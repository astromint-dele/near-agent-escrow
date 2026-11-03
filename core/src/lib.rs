//! escrow_core: every rule of the AgentEscrow contract, as pure Rust.
//!
//! No chain types and no I/O: callers pass in the caller account, the current
//! time and the judge's raw output, and get back a decision. The NEAR contract
//! is a thin layer that moves money according to these decisions, so the whole
//! decision surface is unit-testable on any machine.
//!
//! The judge (an attested WASM run in a TEE via OutLayer) does no arithmetic.
//! It returns each panel model's raw per-criterion scores plus the sha256 of
//! the content it actually judged. This crate:
//!   * checks that sha256 against the worker's commitment,
//!   * requires a panel of at least MIN_PANEL models,
//!   * takes the median per criterion, rejecting the run as inconclusive
//!     unless a strict majority of the models lies within MAX_SPREAD of that
//!     median on every criterion (one outlier, noisy or fooled, is outvoted;
//!     a genuinely split panel is not),
//!   * computes the weighted 0-100 total itself and decides pass/fail.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const SCORE_MAX: u32 = 10;
pub const MIN_CRITERIA: usize = 2;
pub const MAX_CRITERIA: usize = 6;
pub const MAX_TITLE: usize = 120;
pub const MAX_SPEC: usize = 4000;
pub const MAX_URL: usize = 300;
pub const MAX_CRIT_NAME: usize = 40;
pub const MAX_CRIT_DESC: usize = 300;
pub const MAX_NOTES: usize = 280;
pub const MIN_PANEL: usize = 3;
pub const MAX_PANEL: usize = 7;
/// Max allowed (highest - lowest) score across panel models on any criterion.
pub const MAX_SPREAD: u32 = 4;
pub const MIN_PAYMENT_YOCTO: u128 = 10_000_000_000_000_000_000_000; // 0.01 NEAR
pub const APPEAL_WINDOW_NS: u64 = 24 * 60 * 60 * 1_000_000_000;
pub const MIN_DEADLINE_SECS: u64 = 60;
pub const MAX_DEADLINE_SECS: u64 = 90 * 24 * 60 * 60;
const NS_PER_SEC: u64 = 1_000_000_000;

mod u128_string {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &u128, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
        String::deserialize(d)?.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Criterion {
    pub name: String,
    pub description: String,
    pub weight: u32,
}

#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Score {
    pub name: String,
    pub score: u32,
}

#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Judgement {
    pub round: u8,
    /// Median per criterion across the panel.
    pub scores: Vec<Score>,
    /// Weighted 0-100 total of `scores`, computed here.
    pub total: u32,
    /// Largest (max - min) across models on any single criterion.
    pub max_spread: u32,
    pub models: Vec<String>,
    pub content_sha256: String,
    pub notes: String,
}

#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub enum Status {
    Open,
    Assigned,
    Submitted,
    Judging,
    Failed,
    Paid,
    Refunded,
}

#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Job {
    pub id: u64,
    pub employer: String,
    pub worker: Option<String>,
    pub title: String,
    pub spec: String,
    pub rubric: Vec<Criterion>,
    pub pass_threshold: u32,
    #[serde(with = "u128_string")]
    pub payment: u128,
    pub deadline_ns: u64,
    pub judge: JudgeRef,
    pub status: Status,
    pub deliverable_url: Option<String>,
    pub deliverable_sha256: Option<String>,
    pub judgements: Vec<Judgement>,
    pub final_scores: Option<Vec<Score>>,
    pub final_total: Option<u32>,
    pub failed_at_ns: Option<u64>,
    pub appealed: bool,
}

/// What the contract must do after a judge run returns.
#[derive(Debug, Clone, PartialEq)]
pub enum JudgeOutcome {
    /// Job was not in Judging (stale callback); nothing changed.
    Ignored,
    /// The OutLayer execution itself failed; job returned to its prior state.
    JudgeFailed,
    /// Output unparseable or incomplete; job returned to its prior state.
    Malformed(String),
    /// Panel too small or models disagree too much; job returned to its prior state.
    Inconclusive(String),
    /// Passed: transfer `amount` to `to`.
    Pay { to: String, amount: u128 },
    /// Below threshold (or content mismatch); job is Failed.
    Fail { reason: String },
}

/// The judge program, pinned by content: OutLayer only runs a WASM whose
/// bytes hash to `wasm_sha256`, so a job's judge can never be swapped.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct JudgeRef {
    pub wasm_url: String,
    pub wasm_sha256: String,
}

pub fn validate_judge(j: &JudgeRef) -> R<JudgeRef> {
    ensure(is_https_url(&j.wasm_url), "judge wasm_url must be a single https:// URL")?;
    let sha = normalize_sha256(&j.wasm_sha256).ok_or("judge wasm_sha256 must be 64 hex characters")?;
    Ok(JudgeRef { wasm_url: j.wasm_url.trim().to_string(), wasm_sha256: sha })
}

pub struct NewJob {
    pub title: String,
    pub spec: String,
    pub rubric: Vec<Criterion>,
    pub pass_threshold: u32,
    pub deadline_secs: u64,
    pub worker: Option<String>,
}

type R<T> = Result<T, String>;

fn ensure(cond: bool, msg: &str) -> R<()> {
    if cond { Ok(()) } else { Err(msg.to_string()) }
}

// ---------------------------------------------------------------- validation

pub fn validate_rubric(rubric: &[Criterion]) -> R<()> {
    ensure(
        (MIN_CRITERIA..=MAX_CRITERIA).contains(&rubric.len()),
        "rubric needs 2-6 criteria",
    )?;
    let mut seen: Vec<String> = Vec::new();
    let mut sum = 0u32;
    for c in rubric {
        let ok_name = !c.name.is_empty()
            && c.name.len() <= MAX_CRIT_NAME
            && c.name.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == ' ' || ch == '_' || ch == '-');
        ensure(ok_name, "criterion name must be 1-40 letters, digits, space, _ or -")?;
        ensure(
            !c.description.trim().is_empty() && c.description.len() <= MAX_CRIT_DESC,
            "criterion description must be 1-300 chars",
        )?;
        ensure(c.weight >= 1, "criterion weight must be positive")?;
        let lower = c.name.to_lowercase();
        ensure(!seen.contains(&lower), "criterion names must be unique")?;
        seen.push(lower);
        sum += c.weight;
    }
    ensure(sum == 100, "criterion weights must sum to 100")
}

pub fn is_https_url(u: &str) -> bool {
    u.starts_with("https://") && u.len() > 8 && u.len() <= MAX_URL && !u.contains(char::is_whitespace)
}

pub fn normalize_sha256(h: &str) -> Option<String> {
    let h = h.trim().to_ascii_lowercase();
    let h = h.strip_prefix("sha256:").unwrap_or(&h).to_string();
    (h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit())).then_some(h)
}

// ---------------------------------------------------------------- scoring

/// 0-100 integer, round half up. Never computed by the model.
pub fn weighted_total(scores: &[Score], rubric: &[Criterion]) -> u32 {
    let raw: u32 = rubric
        .iter()
        .map(|c| scores.iter().find(|s| s.name == c.name).map(|s| s.score).unwrap_or(0) * c.weight)
        .sum(); // max 10 * 100 = 1000
    (raw + 5) / 10
}

fn coerce_score(v: &Value) -> Option<u32> {
    let f = match v {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    if !f.is_finite() {
        return None;
    }
    Some((f + 0.5).floor().clamp(0.0, SCORE_MAX as f64) as u32)
}

/// Exactly the rubric's criteria, each clamped to 0-10. Accepts an array in
/// rubric order (compact, used by the judge to stay under OutLayer's output
/// limit) or an object keyed by criterion name (case-insensitive).
pub fn clean_scores(scores: &Value, rubric: &[Criterion]) -> Option<Vec<Score>> {
    if let Some(arr) = scores.as_array() {
        if arr.len() != rubric.len() {
            return None;
        }
        return rubric
            .iter()
            .zip(arr)
            .map(|(c, v)| Some(Score { name: c.name.clone(), score: coerce_score(v)? }))
            .collect();
    }
    let obj = scores.as_object()?;
    rubric
        .iter()
        .map(|c| {
            let v = obj.iter().find(|(k, _)| k.trim().eq_ignore_ascii_case(&c.name)).map(|(_, v)| v)?;
            Some(Score { name: c.name.clone(), score: coerce_score(v)? })
        })
        .collect()
}

fn median(mut v: Vec<u32>) -> u32 {
    v.sort_unstable();
    let n = v.len();
    if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2] + 1) / 2 }
}

/// OutLayer hands back the judge's stdout; accept it raw, JSON-quoted, or wrapped.
/// The judge reports infrastructure problems as {"error": "..."} (possibly
/// wrapped like a verdict). Surfaced in the Malformed reason so a failed run
/// says why; capped so a hostile output cannot bloat the return value.
pub fn judge_error(bytes: &[u8]) -> Option<String> {
    fn find(v: &Value, depth: u8) -> Option<String> {
        if depth > 3 {
            return None;
        }
        match v {
            Value::String(s) => find(&serde_json::from_str(s.trim()).ok()?, depth + 1),
            Value::Object(m) => {
                if let Some(Value::String(e)) = m.get("error") {
                    return Some(e.chars().take(200).collect());
                }
                ["output", "result", "stdout", "data"].iter().find_map(|k| find(m.get(*k)?, depth + 1))
            }
            _ => None,
        }
    }
    find(&serde_json::from_slice::<Value>(bytes).ok()?, 0)
}

pub fn parse_verdict(bytes: &[u8]) -> Option<Value> {
    fn unwrap(v: Value, depth: u8) -> Option<Value> {
        if depth > 3 {
            return None;
        }
        match v {
            Value::String(s) => unwrap(serde_json::from_str(s.trim()).ok()?, depth + 1),
            Value::Object(ref m) if m.contains_key("panel") => Some(v),
            Value::Object(m) => {
                for key in ["output", "result", "stdout", "data"] {
                    if let Some(inner) = m.get(key) {
                        return unwrap(inner.clone(), depth + 1);
                    }
                }
                None
            }
            _ => None,
        }
    }
    if let Ok(v) = serde_json::from_slice::<Value>(bytes) {
        return unwrap(v, 0);
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let (a, b) = (text.find('{')?, text.rfind('}')?);
    if b < a {
        return None;
    }
    unwrap(serde_json::from_str(&text[a..=b]).ok()?, 0)
}

pub enum PanelResult {
    Malformed(String),
    Inconclusive(String),
    Ok(Judgement),
}

/// Turn the judge's panel output into a Judgement for `round`.
pub fn evaluate_panel(verdict: &Value, rubric: &[Criterion], round: u8) -> PanelResult {
    let Some(panel) = verdict.get("panel").and_then(|p| p.as_array()) else {
        return PanelResult::Malformed("missing panel".into());
    };
    let content_sha256 = verdict
        .get("content_sha256")
        .and_then(|s| s.as_str())
        .and_then(normalize_sha256)
        .unwrap_or_default();
    let mut models = Vec::new();
    let mut per_model: Vec<Vec<Score>> = Vec::new();
    for entry in panel.iter().take(MAX_PANEL) {
        let model = entry.get("model").and_then(|m| m.as_str()).unwrap_or("").trim();
        let Some(scores) = entry.get("scores").and_then(|s| clean_scores(s, rubric)) else {
            continue; // a model that answered badly simply doesn't count
        };
        if model.is_empty() || models.iter().any(|m: &String| m == model) {
            continue; // models must be distinct and named
        }
        models.push(model.chars().take(64).collect());
        per_model.push(scores);
    }
    if per_model.len() < MIN_PANEL {
        return PanelResult::Inconclusive(format!(
            "panel has {} valid models, needs {}",
            per_model.len(),
            MIN_PANEL
        ));
    }
    let mut scores = Vec::with_capacity(rubric.len());
    let mut max_spread = 0;
    for (i, c) in rubric.iter().enumerate() {
        let vals: Vec<u32> = per_model.iter().map(|m| m[i].score).collect();
        let spread = vals.iter().max().unwrap() - vals.iter().min().unwrap();
        max_spread = max_spread.max(spread);
        let med = median(vals.clone());
        let agree = vals.iter().filter(|v| v.abs_diff(med) <= MAX_SPREAD).count();
        if agree * 2 <= vals.len() {
            return PanelResult::Inconclusive(format!(
                "no majority within {} points on {} (scores {:?})",
                MAX_SPREAD, c.name, vals
            ));
        }
        scores.push(Score { name: c.name.clone(), score: med });
    }
    let notes = verdict
        .get("notes")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .chars()
        .take(MAX_NOTES)
        .collect();
    let total = weighted_total(&scores, rubric);
    PanelResult::Ok(Judgement { round, scores, total, max_spread, models, content_sha256, notes })
}

// ---------------------------------------------------------------- state machine

pub fn create_job(id: u64, employer: &str, payment: u128, now_ns: u64, judge: &JudgeRef, j: NewJob) -> R<Job> {
    ensure(payment >= MIN_PAYMENT_YOCTO, "attach at least 0.01 NEAR as payment")?;
    let title = j.title.trim().to_string();
    ensure(!title.is_empty() && title.len() <= MAX_TITLE, "title must be 1-120 chars")?;
    ensure(!j.spec.trim().is_empty() && j.spec.len() <= MAX_SPEC, "spec must be 1-4000 chars")?;
    ensure((1..=100).contains(&j.pass_threshold), "pass_threshold must be 1-100")?;
    ensure(
        (MIN_DEADLINE_SECS..=MAX_DEADLINE_SECS).contains(&j.deadline_secs),
        "deadline must be 1 minute to 90 days",
    )?;
    let judge = validate_judge(judge)?;
    validate_rubric(&j.rubric)?;
    if let Some(w) = &j.worker {
        ensure(w != employer, "employer cannot be the worker")?;
    }
    let status = if j.worker.is_some() { Status::Assigned } else { Status::Open };
    Ok(Job {
        id,
        employer: employer.to_string(),
        worker: j.worker,
        title,
        spec: j.spec,
        rubric: j.rubric,
        pass_threshold: j.pass_threshold,
        payment,
        deadline_ns: now_ns + j.deadline_secs * NS_PER_SEC,
        judge,
        status,
        deliverable_url: None,
        deliverable_sha256: None,
        judgements: Vec::new(),
        final_scores: None,
        final_total: None,
        failed_at_ns: None,
        appealed: false,
    })
}

pub fn accept(job: &mut Job, caller: &str, now_ns: u64) -> R<()> {
    ensure(job.status == Status::Open, "job is not open")?;
    ensure(caller != job.employer, "employer cannot be the worker")?;
    ensure(now_ns < job.deadline_ns, "job deadline passed")?;
    job.worker = Some(caller.to_string());
    job.status = Status::Assigned;
    Ok(())
}

/// Returns (refund_to, amount).
pub fn cancel(job: &mut Job, caller: &str) -> R<(String, u128)> {
    ensure(caller == job.employer, "only the employer")?;
    ensure(job.status == Status::Open, "only an open job can be cancelled")?;
    job.status = Status::Refunded;
    Ok((job.employer.clone(), job.payment))
}

/// The worker commits to the exact bytes being judged via their sha256.
pub fn submit(job: &mut Job, caller: &str, now_ns: u64, url: &str, sha256: &str) -> R<()> {
    ensure(job.worker.as_deref() == Some(caller), "only the assigned worker")?;
    ensure(job.status == Status::Assigned, "job is not awaiting a deliverable")?;
    ensure(now_ns < job.deadline_ns, "job deadline passed")?;
    let url = url.trim();
    ensure(is_https_url(url), "deliverable_url must be a single https:// URL")?;
    let sha = normalize_sha256(sha256).ok_or("deliverable_sha256 must be 64 hex characters")?;
    job.deliverable_url = Some(url.to_string());
    job.deliverable_sha256 = Some(sha);
    job.status = Status::Submitted;
    Ok(())
}

/// Round 1: anyone, once submitted. Round 2 (appeal): only the worker, after a
/// fail, within the appeal window. Returns the round number.
pub fn begin_judging(job: &mut Job, caller: &str, now_ns: u64) -> R<u8> {
    let round = match job.status {
        Status::Submitted => 1,
        Status::Failed => {
            ensure(!job.appealed && job.judgements.len() == 1, "appeal already used")?;
            ensure(job.worker.as_deref() == Some(caller), "only the worker can appeal")?;
            ensure(now_ns <= job.failed_at_ns.unwrap_or(0) + APPEAL_WINDOW_NS, "appeal window closed")?;
            2
        }
        _ => return Err("job is not ready for judging".into()),
    };
    job.status = Status::Judging;
    Ok(round)
}

/// The JSON handed to the attested judge on stdin.
pub fn judge_input(job: &Job, round: u8) -> String {
    json!({
        "job_id": job.id,
        "round": round,
        "title": job.title,
        "spec": job.spec,
        "rubric": job.rubric,
        "deliverable_url": job.deliverable_url,
        "deliverable_sha256": job.deliverable_sha256,
    })
    .to_string()
}

/// Apply a judge run. `result` is the raw output, or Err(()) if the
/// execution itself failed.
pub fn finish_judging(job: &mut Job, round: u8, result: Result<&[u8], ()>, now_ns: u64) -> JudgeOutcome {
    if job.status != Status::Judging {
        return JudgeOutcome::Ignored;
    }
    let back = if round == 1 { Status::Submitted } else { Status::Failed };
    let Ok(bytes) = result else {
        job.status = back;
        return JudgeOutcome::JudgeFailed;
    };
    let Some(verdict) = parse_verdict(bytes) else {
        job.status = back;
        let why = match judge_error(bytes) {
            Some(e) => format!("judge error: {}", e),
            None => "unparseable judge output".into(),
        };
        return JudgeOutcome::Malformed(why);
    };
    // The judge must always report the hash of what it fetched. A mismatch is
    // decided before (and without) the panel: the judge skips the models then.
    let Some(judged_sha) = verdict.get("content_sha256").and_then(|s| s.as_str()).and_then(normalize_sha256) else {
        job.status = back;
        return JudgeOutcome::Malformed("missing content_sha256".into());
    };
    if job.deliverable_sha256.as_deref() != Some(judged_sha.as_str()) {
        let zero: Vec<Score> = job.rubric.iter().map(|c| Score { name: c.name.clone(), score: 0 }).collect();
        job.judgements.push(Judgement {
            round,
            scores: zero.clone(),
            total: 0,
            max_spread: 0,
            models: Vec::new(),
            content_sha256: judged_sha,
            notes: "content does not match the committed sha256".into(),
        });
        return conclude(job, round, zero, false, now_ns, "content mismatch");
    }
    let judgement = match evaluate_panel(&verdict, &job.rubric, round) {
        PanelResult::Ok(j) => j,
        PanelResult::Malformed(why) => {
            job.status = back;
            return JudgeOutcome::Malformed(why);
        }
        PanelResult::Inconclusive(why) => {
            job.status = back;
            return JudgeOutcome::Inconclusive(why);
        }
    };

    let scores = judgement.scores.clone();
    job.judgements.push(judgement);
    conclude(job, round, scores, true, now_ns, "")
}

/// Shared tail of a judged round: appeal averaging, total, pay/fail.
/// `may_pass` is false for a content mismatch, which can never pay.
fn conclude(job: &mut Job, round: u8, scores: Vec<Score>, may_pass: bool, now_ns: u64, fail_reason: &str) -> JudgeOutcome {
    let final_scores: Vec<Score> = if round == 2 {
        job.appealed = true;
        let first = &job.judgements[0].scores;
        scores
            .iter()
            .map(|s| {
                let a = first.iter().find(|f| f.name == s.name).map(|f| f.score).unwrap_or(0);
                Score { name: s.name.clone(), score: (a + s.score + 1) / 2 }
            })
            .collect()
    } else {
        scores
    };
    let total = weighted_total(&final_scores, &job.rubric);
    job.final_scores = Some(final_scores);
    job.final_total = Some(total);

    if may_pass && total >= job.pass_threshold {
        job.status = Status::Paid;
        JudgeOutcome::Pay { to: job.worker.clone().unwrap_or_default(), amount: job.payment }
    } else {
        job.status = Status::Failed;
        job.failed_at_ns = Some(now_ns);
        JudgeOutcome::Fail {
            reason: if fail_reason.is_empty() { format!("total {} below {}", total, job.pass_threshold) } else { fail_reason.into() },
        }
    }
}

/// Employer refund: worker missed the deadline, or the work failed and the
/// appeal was used or its window closed. Returns (refund_to, amount).
pub fn reclaim(job: &mut Job, caller: &str, now_ns: u64) -> R<(String, u128)> {
    ensure(caller == job.employer, "only the employer")?;
    let allowed = match job.status {
        Status::Assigned => now_ns >= job.deadline_ns,
        Status::Failed => job.appealed || now_ns > job.failed_at_ns.unwrap_or(0) + APPEAL_WINDOW_NS,
        _ => false,
    };
    ensure(allowed, "refund not available for this job yet")?;
    job.status = Status::Refunded;
    Ok((job.employer.clone(), job.payment))
}
