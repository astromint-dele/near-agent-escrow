//! OutLayer entry point (wasm32-wasip2). stdin: the job (escrow_core::judge_input).
//! stdout: {"panel":[{"model","scores":[...]}],"content_sha256","notes"} or
//! {"error": ...}. The provider API key secret (GROQ_API_KEY, or NEARAI_API_KEY in the
//! `nearai` build) is injected by OutLayer as an env var.
//!
//! Errors print {"error"}: the contract treats that as malformed and returns the
//! job to its previous state, so infrastructure problems never hurt either party.

use escrow_judge::*;
use std::io::{self, Read, Write};

fn emit(s: &str) {
    let mut out = io::stdout();
    let _ = out.write_all(s.as_bytes());
    let _ = out.flush();
}

#[cfg(target_os = "wasi")]
mod net {
    use std::time::Duration;
    use wasi_http_client::Client;

    pub fn get(url: &str) -> Result<Vec<u8>, String> {
        let resp = Client::new()
            .get(url)
            .header("User-Agent", "agent-escrow-judge")
            .connect_timeout(Duration::from_secs(10))
            .send()
            .map_err(|e| format!("fetch failed: {e:?}"))?;
        if !(200..300).contains(&resp.status()) {
            return Err(format!("fetch failed: HTTP {}", resp.status()));
        }
        resp.body().map_err(|e| format!("fetch body failed: {e:?}"))
    }

    pub fn post_json(url: &str, key: &str, body: &str) -> Result<Vec<u8>, String> {
        let resp = Client::new()
            .post(url)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {key}"))
            .connect_timeout(Duration::from_secs(10))
            .body(body.as_bytes())
            .send()
            .map_err(|e| format!("model call failed: {e:?}"))?;
        let status = resp.status();
        let bytes = resp.body().map_err(|e| format!("model body failed: {e:?}"))?;
        if !(200..300).contains(&status) {
            return Err(format!("model HTTP {status}"));
        }
        Ok(bytes)
    }
}

#[cfg(not(target_os = "wasi"))]
mod net {
    pub fn get(_: &str) -> Result<Vec<u8>, String> {
        Err("HTTP is only available inside the OutLayer WASI runtime".into())
    }
    pub fn post_json(_: &str, _: &str, _: &str) -> Result<Vec<u8>, String> {
        Err("HTTP is only available inside the OutLayer WASI runtime".into())
    }
}

fn run() -> Result<String, String> {
    let mut raw = String::new();
    io::stdin().read_to_string(&mut raw).map_err(|e| format!("stdin: {e}"))?;
    let input = parse_input(&raw)?;

    let body = net::get(&input.deliverable_url)?;
    if body.len() > MAX_DELIVERABLE_BYTES {
        return Err("deliverable larger than 1 MiB".into());
    }
    let sha = sha256_hex(&body);
    if sha != input.deliverable_sha256.to_ascii_lowercase() {
        return Ok(mismatch_output(&sha)); // no model calls for mismatched content
    }

    let key = std::env::var(provider::KEY_ENV).map_err(|_| format!("{} secret not set", provider::KEY_ENV))?;
    let content = prompt_content(&body);
    let mut panel = Vec::new();
    let mut notes = String::new();
    for model in PANEL.iter() {
        let req = request_body(model, &input, &content).to_string();
        let Ok(bytes) = net::post_json(provider::URL, &key, &req) else { continue };
        let Ok(resp) = serde_json::from_slice::<serde_json::Value>(&bytes) else { continue };
        if let Some((scores, n)) = parse_model_response(&resp, input.rubric.len()) {
            if notes.is_empty() {
                notes = n;
            }
            panel.push((model.label.to_string(), scores));
        }
    }
    // A short panel is reported as-is; the contract decides (inconclusive).
    Ok(assemble_output(&panel, &sha, &notes))
}

fn main() {
    match run() {
        Ok(out) => emit(&out),
        Err(e) => emit(&error_output(&e)),
    }
}
