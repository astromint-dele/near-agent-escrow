use escrow_core::*;
use serde_json::json;

const NEAR: u128 = 1_000_000_000_000_000_000_000_000;
const SEC: u64 = 1_000_000_000;
const T0: u64 = 1_000 * SEC;
const EMPLOYER: &str = "employer.testnet";
const WORKER: &str = "worker.testnet";
const OTHER: &str = "other.testnet";
const SHA: &str = "aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11";
const URL: &str = "https://example.org/deliverable.md";

fn judge_ref() -> JudgeRef {
    JudgeRef { wasm_url: "https://example.org/judge.wasm".into(), wasm_sha256: "dd".repeat(32) }
}

fn rubric() -> Vec<Criterion> {
    vec![
        Criterion { name: "Clarity".into(), description: "Is it clear?".into(), weight: 30 },
        Criterion { name: "Depth".into(), description: "Is it substantive?".into(), weight: 50 },
        Criterion { name: "Docs".into(), description: "Is it documented?".into(), weight: 20 },
    ]
}

fn new_job(rubric: Vec<Criterion>, threshold: u32, deadline_secs: u64, worker: Option<&str>) -> NewJob {
    NewJob {
        title: "Write a README".into(),
        spec: "Document the CLI".into(),
        rubric,
        pass_threshold: threshold,
        deadline_secs,
        worker: worker.map(String::from),
    }
}

fn job() -> Job {
    create_job(0, EMPLOYER, NEAR, T0, &judge_ref(), new_job(rubric(), 60, 3600, None)).unwrap()
}

fn submitted() -> Job {
    let mut j = job();
    accept(&mut j, WORKER, T0).unwrap();
    submit(&mut j, WORKER, T0 + SEC, URL, SHA).unwrap();
    j
}

/// Judge stdout: one entry per model, scores for [Clarity, Depth, Docs].
fn panel(models: &[(&str, [u32; 3])], sha: &str) -> Vec<u8> {
    let entries: Vec<_> = models
        .iter()
        .map(|(m, s)| json!({"model": m, "scores": {"Clarity": s[0], "Depth": s[1], "Docs": s[2]}}))
        .collect();
    json!({"panel": entries, "content_sha256": sha, "notes": "ok"}).to_string().into_bytes()
}

fn good_panel() -> Vec<u8> {
    // medians 8, 6, 5 -> 8*30 + 6*50 + 5*20 = 640 -> 64
    panel(&[("model-a", [8, 6, 5]), ("model-b", [9, 7, 6]), ("model-c", [7, 6, 4])], SHA)
}

fn judge(j: &mut Job, caller: &str, now: u64, out: &[u8]) -> JudgeOutcome {
    let round = begin_judging(j, caller, now).unwrap();
    finish_judging(j, round, Ok(out), now)
}

// --------------------------------------------------------------- creation

#[test]
fn create_job_validates_inputs() {
    let p = &judge_ref();
    let bad = |nj: NewJob, pay: u128| create_job(0, EMPLOYER, pay, T0, p, nj).unwrap_err();
    assert!(bad(new_job(rubric(), 60, 3600, None), NEAR / 1000).contains("0.01 NEAR"));
    let mut r = rubric();
    r[0].weight = 31;
    assert!(bad(new_job(r, 60, 3600, None), NEAR).contains("sum to 100"));
    let mut r = rubric();
    r[1].name = "clarity".into();
    assert!(bad(new_job(r, 60, 3600, None), NEAR).contains("unique"));
    let mut r = rubric();
    r[0].name = "Clarity!".into();
    assert!(bad(new_job(r, 60, 3600, None), NEAR).contains("criterion name"));
    let one = vec![Criterion { name: "All".into(), description: "x".into(), weight: 100 }];
    assert!(bad(new_job(one, 60, 3600, None), NEAR).contains("2-6 criteria"));
    assert!(bad(new_job(rubric(), 0, 3600, None), NEAR).contains("pass_threshold"));
    assert!(bad(new_job(rubric(), 101, 3600, None), NEAR).contains("pass_threshold"));
    assert!(bad(new_job(rubric(), 60, 59, None), NEAR).contains("deadline"));
    assert!(bad(new_job(rubric(), 60, 91 * 24 * 3600, None), NEAR).contains("deadline"));
    assert!(bad(new_job(rubric(), 60, 3600, Some(EMPLOYER)), NEAR).contains("employer cannot"));
}

#[test]
fn create_job_pins_judge_and_assigns_named_worker() {
    let j = create_job(7, EMPLOYER, NEAR, T0, &judge_ref(), new_job(rubric(), 60, 3600, Some(WORKER))).unwrap();
    assert_eq!(j.status, Status::Assigned);
    assert_eq!(j.judge, judge_ref());
    assert_eq!(j.deadline_ns, T0 + 3600 * SEC);
    let open = job();
    assert_eq!(open.status, Status::Open);
}

#[test]
fn payment_serializes_as_string() {
    let v = serde_json::to_value(job()).unwrap();
    assert_eq!(v["payment"], json!(NEAR.to_string()));
}

// --------------------------------------------------------------- accept / cancel / submit

#[test]
fn accept_rules() {
    let mut j = job();
    assert!(accept(&mut j, EMPLOYER, T0).unwrap_err().contains("employer cannot"));
    assert!(accept(&mut j, WORKER, T0 + 3600 * SEC).unwrap_err().contains("deadline"));
    accept(&mut j, WORKER, T0).unwrap();
    assert_eq!((j.status, j.worker.as_deref()), (Status::Assigned, Some(WORKER)));
    assert!(accept(&mut j, OTHER, T0).unwrap_err().contains("not open"));
}

#[test]
fn cancel_only_open_by_employer() {
    let mut j = job();
    assert!(cancel(&mut j, OTHER).unwrap_err().contains("only the employer"));
    assert_eq!(cancel(&mut j, EMPLOYER).unwrap(), (EMPLOYER.to_string(), NEAR));
    assert_eq!(j.status, Status::Refunded);
    let mut j = job();
    accept(&mut j, WORKER, T0).unwrap();
    assert!(cancel(&mut j, EMPLOYER).unwrap_err().contains("only an open job"));
}

#[test]
fn submit_rules_and_commitment() {
    let mut j = job();
    accept(&mut j, WORKER, T0).unwrap();
    assert!(submit(&mut j, OTHER, T0, URL, SHA).unwrap_err().contains("only the assigned worker"));
    assert!(submit(&mut j, WORKER, T0, "http://x.org", SHA).unwrap_err().contains("https://"));
    assert!(submit(&mut j, WORKER, T0, URL, "abc").unwrap_err().contains("64 hex"));
    assert!(submit(&mut j, WORKER, T0 + 3600 * SEC, URL, SHA).unwrap_err().contains("deadline"));
    submit(&mut j, WORKER, T0, URL, &format!("SHA256:{}", SHA.to_uppercase())).unwrap();
    assert_eq!(j.deliverable_sha256.as_deref(), Some(SHA)); // normalised
    assert_eq!(j.status, Status::Submitted);
    let input: serde_json::Value = serde_json::from_str(&judge_input(&j, 1)).unwrap();
    assert_eq!(input["deliverable_sha256"], json!(SHA));
    assert_eq!(input["deliverable_url"], json!(URL));
    assert_eq!(input["rubric"].as_array().unwrap().len(), 3);
}

// --------------------------------------------------------------- judging

#[test]
fn passing_panel_pays_worker_with_median_scores() {
    let mut j = submitted();
    let out = judge(&mut j, OTHER, T0, &good_panel()); // anyone may trigger round 1
    assert_eq!(out, JudgeOutcome::Pay { to: WORKER.into(), amount: NEAR });
    assert_eq!(j.status, Status::Paid);
    assert_eq!(j.final_total, Some(64));
    let jd = &j.judgements[0];
    assert_eq!(jd.scores.iter().map(|s| s.score).collect::<Vec<_>>(), vec![8, 6, 5]);
    assert_eq!((jd.max_spread, jd.models.len()), (2, 3));
}

#[test]
fn judge_does_no_arithmetic_totals_are_recomputed() {
    let mut j = submitted();
    let mut v: serde_json::Value = serde_json::from_slice(&good_panel()).unwrap();
    v["total"] = json!(100); // a lying judge field is simply ignored
    let out = judge(&mut j, OTHER, T0, v.to_string().as_bytes());
    assert!(matches!(out, JudgeOutcome::Pay { .. }));
    assert_eq!(j.final_total, Some(64));
}

#[test]
fn below_threshold_fails() {
    let mut j = submitted();
    let out = judge(&mut j, OTHER, T0, &panel(&[("a", [5, 5, 5]), ("b", [5, 5, 5]), ("c", [5, 5, 5])], SHA));
    assert_eq!(out, JudgeOutcome::Fail { reason: "total 50 below 60".into() });
    assert_eq!((j.status, j.failed_at_ns), (Status::Failed, Some(T0)));
}

#[test]
fn one_outlier_is_outvoted_by_the_majority() {
    // Live Groq data: one noisy judge gave 3 where the others gave 8 and 9.
    let mut j = submitted();
    let out = judge(&mut j, OTHER, T0, &panel(&[("a", [8, 3, 5]), ("b", [8, 8, 5]), ("c", [8, 9, 5])], SHA));
    assert!(matches!(out, JudgeOutcome::Pay { .. }));
    let jd = &j.judgements[0];
    assert_eq!(jd.scores[1].score, 8);
    assert_eq!(jd.max_spread, 6); // still recorded for transparency
}

#[test]
fn agreement_boundary_is_inclusive() {
    // 4 points from the median still agrees; 5 does not.
    let mut j = submitted();
    let ok = panel(&[("a", [8, 1, 5]), ("b", [8, 5, 5]), ("c", [8, 9, 5])], SHA);
    assert!(!matches!(judge(&mut j, OTHER, T0, &ok), JudgeOutcome::Inconclusive(_)));
    let mut j = submitted();
    let split = panel(&[("a", [8, 0, 5]), ("b", [8, 5, 5]), ("c", [8, 10, 5])], SHA);
    assert!(matches!(judge(&mut j, OTHER, T0, &split), JudgeOutcome::Inconclusive(_)));
}

#[test]
fn disagreeing_panel_is_inconclusive_and_retryable() {
    let mut j = submitted();
    let out = judge(&mut j, OTHER, T0, &panel(&[("a", [8, 0, 5]), ("b", [8, 5, 5]), ("c", [8, 10, 5])], SHA));
    assert!(matches!(out, JudgeOutcome::Inconclusive(ref w) if w.contains("no majority") && w.contains("Depth")));
    assert_eq!(j.status, Status::Submitted);
    assert!(j.judgements.is_empty());
    assert!(matches!(judge(&mut j, OTHER, T0, &good_panel()), JudgeOutcome::Pay { .. }));
}

#[test]
fn small_or_duplicate_panel_is_inconclusive() {
    let mut j = submitted();
    let two = panel(&[("a", [8, 6, 5]), ("b", [8, 6, 5])], SHA);
    assert!(matches!(judge(&mut j, OTHER, T0, &two), JudgeOutcome::Inconclusive(_)));
    let dup = panel(&[("a", [8, 6, 5]), ("a", [8, 6, 5]), ("b", [8, 6, 5])], SHA);
    assert!(matches!(judge(&mut j, OTHER, T0, &dup), JudgeOutcome::Inconclusive(_)));
    // one model with missing criteria doesn't count either
    let mut v: serde_json::Value = serde_json::from_slice(&good_panel()).unwrap();
    v["panel"][2]["scores"] = json!({"Clarity": 7});
    assert!(matches!(judge(&mut j, OTHER, T0, v.to_string().as_bytes()), JudgeOutcome::Inconclusive(_)));
    assert_eq!(j.status, Status::Submitted);
}

#[test]
fn malformed_and_failed_runs_change_nothing() {
    let mut j = submitted();
    assert!(matches!(judge(&mut j, OTHER, T0, b"not json at all"), JudgeOutcome::Malformed(_)));
    assert!(matches!(judge(&mut j, OTHER, T0, b"{\"scores\":{}}"), JudgeOutcome::Malformed(_)));
    let round = begin_judging(&mut j, OTHER, T0).unwrap();
    assert_eq!(finish_judging(&mut j, round, Err(()), T0), JudgeOutcome::JudgeFailed);
    assert_eq!(j.status, Status::Submitted);
    assert!(j.judgements.is_empty());
}

#[test]
fn stale_callback_is_ignored() {
    let mut j = submitted();
    assert_eq!(finish_judging(&mut j, 1, Ok(&good_panel()), T0), JudgeOutcome::Ignored);
    assert_eq!(j.status, Status::Submitted);
}

#[test]
fn cannot_judge_before_submission() {
    let mut j = job();
    assert!(begin_judging(&mut j, OTHER, T0).unwrap_err().contains("not ready"));
}

#[test]
fn content_mismatch_fails_with_zero() {
    let mut j = submitted();
    let other_sha = "bb".repeat(32);
    let out = judge(&mut j, OTHER, T0, &panel(&[("a", [9, 9, 9]), ("b", [9, 9, 9]), ("c", [9, 9, 9])], &other_sha));
    assert_eq!(out, JudgeOutcome::Fail { reason: "content mismatch".into() });
    assert_eq!(j.final_total, Some(0));
    assert!(j.judgements[0].notes.contains("committed sha256"));
}

#[test]
fn verdict_accepts_wrapped_output() {
    let inner = String::from_utf8(good_panel()).unwrap();
    for out in [
        serde_json::to_vec(&json!(inner)).unwrap(),                       // JSON string
        serde_json::to_vec(&json!({"output": inner.clone()})).unwrap(),   // wrapped string
        format!("log line\n{}\n", inner).into_bytes(),                    // noisy stdout
    ] {
        let mut j = submitted();
        assert!(matches!(judge(&mut j, OTHER, T0, &out), JudgeOutcome::Pay { .. }), "{:?}", String::from_utf8_lossy(&out));
    }
}

#[test]
fn scores_are_coerced_and_clamped() {
    let mut j = submitted();
    let entries: Vec<_> = ["a", "b", "c"]
        .iter()
        .map(|m| json!({"model": m, "scores": {"clarity": "7.6", "DEPTH": 14, "Docs": -3}}))
        .collect();
    let out = json!({"panel": entries, "content_sha256": SHA}).to_string();
    judge(&mut j, OTHER, T0, out.as_bytes());
    assert_eq!(j.judgements[0].scores.iter().map(|s| s.score).collect::<Vec<_>>(), vec![8, 10, 0]);
}

#[test]
fn weighted_total_rounds_half_up() {
    let s = |a, b, c| vec![
        Score { name: "Clarity".into(), score: a },
        Score { name: "Depth".into(), score: b },
        Score { name: "Docs".into(), score: c },
    ];
    assert_eq!(weighted_total(&s(10, 10, 10), &rubric()), 100);
    assert_eq!(weighted_total(&s(0, 0, 0), &rubric()), 0);
    // 5*30 + 6*50 + 7*20 = 150 + 300 + 140 = 590 -> 59 ; 645 -> 65 (half up)
    assert_eq!(weighted_total(&s(5, 6, 7), &rubric()), 59);
    let r = vec![
        Criterion { name: "A".into(), description: "a".into(), weight: 55 },
        Criterion { name: "B".into(), description: "b".into(), weight: 45 },
    ];
    let half = vec![Score { name: "A".into(), score: 5 }, Score { name: "B".into(), score: 8 }]; // 275+360=635 -> 64
    assert_eq!(weighted_total(&half, &r), 64);
}

// --------------------------------------------------------------- appeal

fn failed() -> Job {
    let mut j = submitted();
    judge(&mut j, OTHER, T0, &panel(&[("a", [6, 5, 5]), ("b", [6, 5, 5]), ("c", [6, 5, 5])], SHA)); // 53
    assert_eq!(j.status, Status::Failed);
    j
}

#[test]
fn appeal_averages_and_can_pass() {
    let mut j = failed();
    // round 2 medians 8,7,6 -> averaged with 6,5,5: (6+8+1)/2=7, (5+7+1)/2=6, (5+6+1)/2=6 -> 210+300+120=630 -> 63
    let out = judge(&mut j, WORKER, T0 + SEC, &panel(&[("a", [8, 7, 6]), ("b", [8, 7, 6]), ("c", [8, 7, 6])], SHA));
    assert_eq!(out, JudgeOutcome::Pay { to: WORKER.into(), amount: NEAR });
    assert_eq!(j.final_scores.unwrap().iter().map(|s| s.score).collect::<Vec<_>>(), vec![7, 6, 6]);
    assert_eq!((j.final_total, j.appealed, j.judgements.len()), (Some(63), true, 2));
}

#[test]
fn appeal_rules() {
    let mut j = failed();
    assert!(begin_judging(&mut j, OTHER, T0).unwrap_err().contains("only the worker"));
    assert!(begin_judging(&mut j, WORKER, T0 + APPEAL_WINDOW_NS + 1).unwrap_err().contains("window closed"));
    let bad = panel(&[("a", [5, 5, 5]), ("b", [5, 5, 5]), ("c", [5, 5, 5])], SHA);
    assert!(matches!(judge(&mut j, WORKER, T0, &bad), JudgeOutcome::Fail { .. }));
    assert!(j.appealed);
    assert!(begin_judging(&mut j, WORKER, T0).unwrap_err().contains("appeal already used"));
}

#[test]
fn failed_appeal_run_returns_to_failed() {
    let mut j = failed();
    let round = begin_judging(&mut j, WORKER, T0).unwrap();
    assert_eq!(round, 2);
    assert_eq!(finish_judging(&mut j, round, Err(()), T0), JudgeOutcome::JudgeFailed);
    assert_eq!((j.status, j.appealed), (Status::Failed, false));
}

// --------------------------------------------------------------- reclaim

#[test]
fn reclaim_after_missed_deadline() {
    let mut j = job();
    accept(&mut j, WORKER, T0).unwrap();
    assert!(reclaim(&mut j, EMPLOYER, T0).unwrap_err().contains("not available"));
    assert!(reclaim(&mut j, OTHER, T0 + 3600 * SEC).unwrap_err().contains("only the employer"));
    assert_eq!(reclaim(&mut j, EMPLOYER, T0 + 3600 * SEC).unwrap(), (EMPLOYER.into(), NEAR));
    assert_eq!(j.status, Status::Refunded);
}

#[test]
fn reclaim_after_fail_waits_for_appeal_window() {
    let mut j = failed();
    assert!(reclaim(&mut j, EMPLOYER, T0 + SEC).unwrap_err().contains("not available"));
    assert!(reclaim(&mut j, EMPLOYER, T0 + APPEAL_WINDOW_NS + 1).is_ok());
}

#[test]
fn reclaim_immediately_after_failed_appeal() {
    let mut j = failed();
    judge(&mut j, WORKER, T0, &panel(&[("a", [5, 5, 5]), ("b", [5, 5, 5]), ("c", [5, 5, 5])], SHA));
    assert!(reclaim(&mut j, EMPLOYER, T0 + SEC).is_ok());
}

#[test]
fn paid_or_submitted_jobs_cannot_be_reclaimed() {
    let mut j = submitted();
    assert!(reclaim(&mut j, EMPLOYER, T0 + 9999 * SEC).is_err()); // submitted: worker delivered in time
    judge(&mut j, OTHER, T0, &good_panel());
    assert!(reclaim(&mut j, EMPLOYER, T0 + 9999 * SEC).is_err());
}

// --------------------------------------------------------------- judge output format

#[test]
fn mismatch_fails_even_without_a_panel() {
    // The judge skips the models when the hash doesn't match; that must be a
    // Fail (worker's fault), never Inconclusive (free retry).
    let mut j = submitted();
    let out = json!({"panel": [], "content_sha256": "cc".repeat(32)}).to_string();
    assert_eq!(judge(&mut j, OTHER, T0, out.as_bytes()), JudgeOutcome::Fail { reason: "content mismatch".into() });
    assert_eq!((j.status, j.final_total), (Status::Failed, Some(0)));
    assert!(j.judgements[0].models.is_empty());
}

#[test]
fn missing_content_hash_is_malformed() {
    let mut j = submitted();
    let mut v: serde_json::Value = serde_json::from_slice(&good_panel()).unwrap();
    v.as_object_mut().unwrap().remove("content_sha256");
    assert!(matches!(judge(&mut j, OTHER, T0, v.to_string().as_bytes()), JudgeOutcome::Malformed(ref w) if w.contains("content_sha256")));
    assert_eq!(j.status, Status::Submitted);
}

#[test]
fn compact_array_scores_in_rubric_order() {
    let mut j = submitted();
    let out = json!({
        "panel": [
            {"model": "glm", "scores": [8, 6, 5]},
            {"model": "qwen", "scores": [9, 7, 6]},
            {"model": "deepseek", "scores": [7, 6, 4]},
            {"model": "short", "scores": [9, 9]}            // wrong length: ignored
        ],
        "content_sha256": SHA
    })
    .to_string();
    assert!(out.len() < 900, "fits OutLayer's 900-byte output limit");
    assert!(matches!(judge(&mut j, OTHER, T0, out.as_bytes()), JudgeOutcome::Pay { .. }));
    assert_eq!(j.final_total, Some(64));
    assert_eq!(j.judgements[0].models, vec!["glm", "qwen", "deepseek"]);
}

#[test]
fn judge_must_be_pinned_by_url_and_hash() {
    let bad_url = JudgeRef { wasm_url: "http://x.org/j.wasm".into(), wasm_sha256: "dd".repeat(32) };
    assert!(create_job(0, EMPLOYER, NEAR, T0, &bad_url, new_job(rubric(), 60, 3600, None)).unwrap_err().contains("wasm_url"));
    let bad_sha = JudgeRef { wasm_url: "https://x.org/j.wasm".into(), wasm_sha256: "nothex".into() };
    assert!(create_job(0, EMPLOYER, NEAR, T0, &bad_sha, new_job(rubric(), 60, 3600, None)).unwrap_err().contains("wasm_sha256"));
    let upper = JudgeRef { wasm_url: "https://x.org/j.wasm".into(), wasm_sha256: format!("SHA256:{}", "DD".repeat(32)) };
    let j = create_job(0, EMPLOYER, NEAR, T0, &upper, new_job(rubric(), 60, 3600, None)).unwrap();
    assert_eq!(j.judge.wasm_sha256, "dd".repeat(32)); // normalised
}

#[test]
fn a_tie_is_not_a_majority() {
    // 4 models: median of [1,1,10,10] is 6; only the two 10s are within 4.
    let mut j = submitted();
    let tie = panel(&[("a", [8, 1, 8]), ("b", [8, 1, 8]), ("c", [8, 10, 8]), ("d", [8, 10, 8])], SHA);
    assert!(matches!(judge(&mut j, OTHER, T0, &tie), JudgeOutcome::Inconclusive(_)));
}

#[test]
fn judge_errors_are_surfaced_and_harmless() {
    let mut j = submitted();
    let out = judge(&mut j, OTHER, T0, br#"{"error":"GROQ_API_KEY secret not set"}"#);
    assert_eq!(out, JudgeOutcome::Malformed("judge error: GROQ_API_KEY secret not set".into()));
    assert_eq!(j.status, Status::Submitted); // job untouched, can be retried
    let wrapped = serde_json::json!({"output": "{\"error\":\"x\"}"}).to_string();
    assert_eq!(judge(&mut j, OTHER, T0, wrapped.as_bytes()), JudgeOutcome::Malformed("judge error: x".into()));
    let long = format!(r#"{{"error":"{}"}}"#, "a".repeat(500));
    let JudgeOutcome::Malformed(why) = judge(&mut j, OTHER, T0, long.as_bytes()) else { panic!() };
    assert!(why.len() <= 220);
    assert_eq!(judge(&mut j, OTHER, T0, b"garbage"), JudgeOutcome::Malformed("unparseable judge output".into()));
}
