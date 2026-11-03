//! Live check of the judge panel against the build's provider (Groq by default), run natively (not in
//! OutLayer). Uses the judge's exact prompts and parsing from lib.rs, and
//! reports each model's latency, whether its reply parsed, and the final
//! output size. Never prints the API key.
//!
//!   set GROQ_API_KEY (or NEARAI_API_KEY with --features nearai) in your own terminal, then:
//!   cargo run --example live_panel -- [deliverable_url | local_file]
//! A local file path is read from disk instead of fetched (for demos before
//! the deliverable is hosted).

use escrow_judge::*;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const DEFAULT_URL: &str = "https://raw.githubusercontent.com/astromint-dele/-genlayer-claim-attestor-/main/README.md";

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .tls_connector(std::sync::Arc::new(native_tls::TlsConnector::new().expect("TLS")))
        .timeout(Duration::from_secs(60))
        .build()
}

fn main() {
    let key = match std::env::var(provider::KEY_ENV) {
        Ok(k) if !k.trim().is_empty() => k,
        _ => {
            eprintln!("{} is not set in this terminal.", provider::KEY_ENV);
            std::process::exit(1);
        }
    };
    let url = std::env::args().nth(1).unwrap_or_else(|| DEFAULT_URL.to_string());
    let http = agent();

    let t = Instant::now();
    let mut body = Vec::new();
    if url.starts_with("https://") {
        let resp = http.get(&url).call().unwrap_or_else(|e| panic!("fetch failed: {e}"));
        std::io::Read::read_to_end(&mut resp.into_reader(), &mut body).unwrap();
    } else {
        body = std::fs::read(&url).unwrap_or_else(|e| panic!("read {url}: {e}"));
    }
    let sha = sha256_hex(&body);
    println!("fetched {} bytes in {:?}  sha256={sha}", body.len(), t.elapsed());

    let input = parse_input(
        &json!({
            "job_id": 0, "round": 1,
            "title": "Document a GenLayer Intelligent Contract",
            "spec": "A README that explains what the contract does, how consensus is used, and how to test and deploy it.",
            "rubric": [
                {"name": "Clarity", "description": "Is the purpose clear to a new reader?", "weight": 30},
                {"name": "Depth", "description": "Is the technical design explained substantively?", "weight": 50},
                {"name": "Usability", "description": "Can someone test and deploy it from the README?", "weight": 20}
            ],
            "deliverable_url": if url.starts_with("https://") { url.clone() } else { "https://local.file/".to_string() }, "deliverable_sha256": sha
        })
        .to_string(),
    )
    .unwrap();

    println!("provider: {}", provider::NAME);
    let content = prompt_content(&body);
    let total = Instant::now();
    let mut panel = Vec::new();
    let mut notes = String::new();
    for model in PANEL.iter() {
        let t = Instant::now();
        let req = request_body(model, &input, &content);
        let res = http
            .post(provider::URL)
            .set("Authorization", &format!("Bearer {key}"))
            .set("Content-Type", "application/json")
            .send_json(req);
        let took = t.elapsed();
        match res {
            Ok(r) => {
                let v: Value = r.into_json().unwrap_or(Value::Null);
                let usage = v.get("usage").cloned().unwrap_or(Value::Null);
                match parse_model_response(&v, input.rubric.len()) {
                    Some((scores, n)) => {
                        println!("OK   {:<22} {:>6.1}s scores={:?} tokens={}", model.label, took.as_secs_f32(), scores, usage);
                        if notes.is_empty() {
                            notes = n;
                        }
                        panel.push((model.label.to_string(), scores));
                    }
                    None => {
                        let msg = v.pointer("/choices/0/message").cloned().unwrap_or(v.clone());
                        let snippet: String = msg.to_string().chars().take(300).collect();
                        println!("BAD  {:<22} {:>6.1}s unparseable reply: {snippet}", model.label, took.as_secs_f32());
                    }
                }
            }
            Err(ureq::Error::Status(code, r)) => {
                let snippet: String = r.into_string().unwrap_or_default().chars().take(300).collect();
                println!("HTTP {:<22} {:>6.1}s status {code}: {snippet}", model.label, took.as_secs_f32());
            }
            Err(e) => println!("ERR  {:<22} {:>6.1}s {e}", model.label, took.as_secs_f32()),
        }
    }
    // Probe: Prompt Guard over the same content, raw replies shown so the
    // response format and a threshold can be calibrated.
    if std::env::var("GUARD_PROBE").is_ok() {
        for (i, chunk) in guard_chunks(&content).iter().enumerate() {
            let res = http
                .post(provider::URL)
                .set("Authorization", &format!("Bearer {key}"))
                .set("Content-Type", "application/json")
                .send_json(guard_request_body(chunk));
            let shown = match res {
                Ok(r) => {
                    let v: Value = r.into_json().unwrap_or(Value::Null);
                    v.pointer("/choices/0/message/content").cloned().unwrap_or(v).to_string()
                }
                Err(ureq::Error::Status(code, r)) => format!("HTTP {code}: {}", r.into_string().unwrap_or_default()),
                Err(e) => format!("ERR {e}"),
            };
            let snippet: String = shown.chars().take(200).collect();
            println!("GUARD chunk {i} ({} chars): {snippet}", chunk.chars().count());
        }
    }
    let out = assemble_output(&panel, &sha, &notes);
    println!("\npanel total {:.1}s (OutLayer limit 60s)", total.elapsed().as_secs_f32());
    println!("output {} bytes (limit {MAX_OUTPUT_BYTES}):\n{out}", out.len());
}
