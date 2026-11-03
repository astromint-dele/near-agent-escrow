//! AgentEscrow: escrow for AI-agent work on NEAR.
//!
//! Thin NEAR layer over `escrow_core`, which holds every rule. This file only
//! handles what is chain-specific: deposits, storage, gas, the OutLayer
//! cross-contract call (NEAR yield/resume) and transfers.
//!
//! Flow: post_job (payment locked) -> accept_job -> submit_work (URL + sha256
//! commitment) -> request_judging (OutLayer runs the pinned judge WASM in a TEE;
//! the judge asks a panel of models) -> on_judged (core takes the median, checks
//! agreement and the content hash, computes the total) -> pay / fail -> one
//! appeal -> reclaim.

use escrow_core::{self as core, Criterion, Job, JudgeOutcome, JudgeRef, NewJob};
use near_sdk::store::LookupMap;
use near_sdk::{env, near, require, AccountId, Gas, NearToken, PanicOnDefault, Promise};
use serde_json::{json, Value};

const GAS_FOR_OUTLAYER: Gas = Gas::from_tgas(100);
const GAS_FOR_CALLBACK: Gas = Gas::from_tgas(40);
const MIN_PREPAID_GAS: Gas = Gas::from_tgas(200);
/// Cap on judge output read back into the contract.
const MAX_JUDGE_OUTPUT: usize = 16 * 1024;
/// Extra bytes reserved per job for the deliverable and two judgements.
const STORAGE_HEADROOM_BYTES: u64 = 3072;

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct AgentEscrow {
    owner: AccountId,
    outlayer: AccountId,
    /// Default judge for NEW jobs; each job keeps the judge pinned at posting.
    judge: JudgeRef,
    /// OutLayer secret holding NEARAI_API_KEY (bound to the judge WASM hash).
    secrets_profile: String,
    secrets_owner: AccountId,
    jobs: LookupMap<u64, Job>,
    next_id: u64,
}

fn ok<T>(r: Result<T, String>) -> T {
    r.unwrap_or_else(|e| env::panic_str(&e))
}

fn caller() -> String {
    env::predecessor_account_id().to_string()
}

fn transfer(to: &str, amount: u128) -> Promise {
    let to: AccountId = to.parse().unwrap_or_else(|_| env::panic_str("invalid payout account"));
    Promise::new(to).transfer(NearToken::from_yoctonear(amount))
}

#[near]
impl AgentEscrow {
    #[init]
    pub fn new(
        outlayer: AccountId,
        judge_wasm_url: String,
        judge_wasm_sha256: String,
        secrets_profile: String,
        secrets_owner: AccountId,
    ) -> Self {
        let judge = ok(core::validate_judge(&JudgeRef { wasm_url: judge_wasm_url, wasm_sha256: judge_wasm_sha256 }));
        require!(!secrets_profile.trim().is_empty(), "secrets_profile required");
        Self {
            owner: env::predecessor_account_id(),
            outlayer,
            judge,
            secrets_profile,
            secrets_owner,
            jobs: LookupMap::new(b"j"),
            next_id: 0,
        }
    }

    /// Changes the default judge for NEW jobs only; existing jobs keep the
    /// judge (URL + WASM hash) pinned when they were posted.
    pub fn set_judge(&mut self, judge_wasm_url: String, judge_wasm_sha256: String) {
        require!(env::predecessor_account_id() == self.owner, "only the owner");
        self.judge = ok(core::validate_judge(&JudgeRef { wasm_url: judge_wasm_url, wasm_sha256: judge_wasm_sha256 }));
    }

    pub fn set_secrets_ref(&mut self, secrets_profile: String, secrets_owner: AccountId) {
        require!(env::predecessor_account_id() == self.owner, "only the owner");
        require!(!secrets_profile.trim().is_empty(), "secrets_profile required");
        self.secrets_profile = secrets_profile;
        self.secrets_owner = secrets_owner;
    }

    /// Attached deposit = payment + storage reserve. The reserve (storage for
    /// the job plus headroom) is deducted so escrowed payments are never used
    /// to pay for storage.
    #[payable]
    pub fn post_job(
        &mut self,
        title: String,
        spec: String,
        rubric: Vec<Criterion>,
        pass_threshold: u32,
        deadline_secs: u64,
        worker: Option<AccountId>,
    ) -> u64 {
        let deposit = env::attached_deposit().as_yoctonear();
        let id = self.next_id;
        let new = NewJob {
            title,
            spec,
            rubric,
            pass_threshold,
            deadline_secs,
            worker: worker.map(|w| w.to_string()),
        };
        // Validate everything with the full deposit first, then charge storage.
        let job = ok(core::create_job(id, &caller(), deposit, env::block_timestamp(), &self.judge, new));
        let before = env::storage_usage();
        self.jobs.insert(id, job);
        self.jobs.flush();
        let used = env::storage_usage().saturating_sub(before) + STORAGE_HEADROOM_BYTES;
        let reserve = env::storage_byte_cost().as_yoctonear() * used as u128;
        let payment = deposit.saturating_sub(reserve);
        require!(
            payment >= core::MIN_PAYMENT_YOCTO,
            format!("deposit must cover payment (min 0.01 NEAR) plus a storage reserve of {} yoctoNEAR", reserve)
        );
        self.jobs.get_mut(&id).unwrap().payment = payment;
        self.next_id += 1;
        id
    }

    pub fn accept_job(&mut self, job_id: u64) {
        let (who, now) = (caller(), env::block_timestamp());
        ok(core::accept(self.job_mut(job_id), &who, now));
    }

    pub fn cancel_job(&mut self, job_id: u64) -> Promise {
        let who = caller();
        let (to, amount) = ok(core::cancel(self.job_mut(job_id), &who));
        transfer(&to, amount)
    }

    /// `deliverable_sha256` commits to the exact bytes at the URL; the judge
    /// refuses to credit content that does not match it.
    pub fn submit_work(&mut self, job_id: u64, deliverable_url: String, deliverable_sha256: String) {
        let (who, now) = (caller(), env::block_timestamp());
        ok(core::submit(self.job_mut(job_id), &who, now, &deliverable_url, &deliverable_sha256));
    }

    /// The attached deposit pays OutLayer; OutLayer refunds unused deposit to
    /// the caller (payer_account_id).
    #[payable]
    pub fn request_judging(&mut self, job_id: u64) -> Promise {
        require!(!env::attached_deposit().is_zero(), "attach NEAR to pay for the judge execution");
        require!(env::prepaid_gas() >= MIN_PREPAID_GAS, "attach at least 200 Tgas");
        let (who, now) = (caller(), env::block_timestamp());
        let outlayer = self.outlayer.clone();
        let secrets_ref = json!({ "profile": self.secrets_profile, "account_id": self.secrets_owner });
        let job = self.job_mut(job_id);
        let round = ok(core::begin_judging(job, &who, now));
        let args = json!({
            "source": { "WasmUrl": {
                "url": job.judge.wasm_url,
                "hash": job.judge.wasm_sha256,
                "build_target": "wasm32-wasip2",
            } },
            "input_data": core::judge_input(job, round),
            "resource_limits": { "max_instructions": 50_000_000_000u64, "max_memory_mb": 256, "max_execution_seconds": 60 },
            "secrets_ref": secrets_ref,
            "response_format": "Json",
            "payer_account_id": who,
        });
        Promise::new(outlayer)
            .function_call(
                "request_execution".to_string(),
                args.to_string().into_bytes(),
                env::attached_deposit(),
                GAS_FOR_OUTLAYER,
            )
            .then(Self::ext(env::current_account_id()).with_static_gas(GAS_FOR_CALLBACK).on_judged(job_id, round))
    }

    #[private]
    pub fn on_judged(&mut self, job_id: u64, round: u8) -> String {
        let result = env::promise_result_checked(0, MAX_JUDGE_OUTPUT).map_err(|_| ());
        let now = env::block_timestamp();
        match core::finish_judging(self.job_mut(job_id), round, result.as_deref().map_err(|_| ()), now) {
            JudgeOutcome::Pay { to, amount } => {
                transfer(&to, amount).detach();
                "paid".into()
            }
            JudgeOutcome::Fail { reason } => format!("failed: {}", reason),
            JudgeOutcome::Inconclusive(why) => format!("inconclusive: {}", why),
            JudgeOutcome::Malformed(why) => format!("malformed: {}", why),
            JudgeOutcome::JudgeFailed => "judge_failed".into(),
            JudgeOutcome::Ignored => "ignored".into(),
        }
    }

    pub fn reclaim(&mut self, job_id: u64) -> Promise {
        let (who, now) = (caller(), env::block_timestamp());
        let (to, amount) = ok(core::reclaim(self.job_mut(job_id), &who, now));
        transfer(&to, amount)
    }

    // ------------------------------------------------ views

    pub fn get_job(&self, job_id: u64) -> Option<Job> {
        self.jobs.get(&job_id).cloned()
    }

    pub fn get_jobs(&self, from_index: u64, limit: u64) -> Vec<Job> {
        (from_index..self.next_id.min(from_index.saturating_add(limit.min(50))))
            .filter_map(|i| self.jobs.get(&i).cloned())
            .collect()
    }

    pub fn get_config(&self) -> Value {
        json!({
            "owner": self.owner,
            "outlayer": self.outlayer,
            "judge": self.judge,
            "secrets_profile": self.secrets_profile,
            "secrets_owner": self.secrets_owner,
            "jobs": self.next_id,
            "min_panel": core::MIN_PANEL,
            "max_spread": core::MAX_SPREAD,
        })
    }

    fn job_mut(&mut self, job_id: u64) -> &mut Job {
        self.jobs.get_mut(&job_id).unwrap_or_else(|| env::panic_str("unknown job"))
    }
}
