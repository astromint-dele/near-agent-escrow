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
    //! Minimal wasi:http client. Written directly against the WASI 0.2
    //! bindings because the wasi-http-client crate writes the whole request
    //! body in one blocking-write-and-flush call, which WASI caps at 4096
    //! bytes: any larger POST (every model prompt) traps the component.
    use wasi::http::outgoing_handler;
    use wasi::http::types::{Fields, Method, OutgoingBody, OutgoingRequest, RequestOptions, Scheme};
    use wasi::io::streams::StreamError;

    /// WASI's per-call limit for blocking-write-and-flush.
    const WRITE_CHUNK: usize = 4096;
    const CONNECT_TIMEOUT_NS: u64 = 10_000_000_000;
    /// Model replies are small; anything bigger is refused.
    const MAX_MODEL_RESPONSE: usize = 256 * 1024;

    fn request(
        method: Method,
        url: &str,
        headers: &[(&str, String)],
        body: Option<&[u8]>,
        max_response: usize,
    ) -> Result<(u16, Vec<u8>), String> {
        let rest = url.strip_prefix("https://").ok_or("only https URLs are allowed")?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let fields = Fields::new();
        for (k, v) in headers {
            fields.append(&k.to_string(), &v.as_bytes().to_vec()).map_err(|e| format!("header {k}: {e:?}"))?;
        }
        let req = OutgoingRequest::new(fields);
        req.set_method(&method).map_err(|_| "set method")?;
        req.set_scheme(Some(&Scheme::Https)).map_err(|_| "set scheme")?;
        req.set_authority(Some(authority)).map_err(|_| "set authority")?;
        req.set_path_with_query(Some(path)).map_err(|_| "set path")?;

        let out_body = req.body().map_err(|_| "request body")?;
        if let Some(bytes) = body {
            let stream = out_body.write().map_err(|_| "request stream")?;
            for chunk in bytes.chunks(WRITE_CHUNK) {
                stream.blocking_write_and_flush(chunk).map_err(|e| format!("request write: {e:?}"))?;
            }
            drop(stream); // child stream must go before finishing its body
        }
        OutgoingBody::finish(out_body, None).map_err(|e| format!("request finish: {e:?}"))?;

        let options = RequestOptions::new();
        options.set_connect_timeout(Some(CONNECT_TIMEOUT_NS)).map_err(|_| "set timeout")?;
        let future = outgoing_handler::handle(req, Some(options)).map_err(|e| format!("send: {e:?}"))?;
        let response = loop {
            match future.get() {
                Some(r) => break r.map_err(|_| "response already taken")?.map_err(|e| format!("http: {e:?}"))?,
                None => future.subscribe().block(),
            }
        };
        let status = response.status();
        let in_body = response.consume().map_err(|_| "response body")?;
        let stream = in_body.stream().map_err(|_| "response stream")?;
        let mut out = Vec::new();
        loop {
            match stream.blocking_read(64 * 1024) {
                Ok(chunk) => {
                    out.extend_from_slice(&chunk);
                    if out.len() > max_response {
                        return Err(format!("response larger than {max_response} bytes"));
                    }
                }
                Err(StreamError::Closed) => break,
                Err(e) => return Err(format!("response read: {e:?}")),
            }
        }
        drop(stream); // child stream must go before its body
        drop(in_body);
        Ok((status, out))
    }

    pub fn get(url: &str) -> Result<Vec<u8>, String> {
        let ua = [("User-Agent", "agent-escrow-judge".to_string())];
        // One byte over the cap is enough to know it is too big.
        let (status, body) = request(Method::Get, url, &ua, None, super::MAX_DELIVERABLE_BYTES + 1)
            .map_err(|e| format!("fetch failed: {e}"))?;
        if !(200..300).contains(&status) {
            return Err(format!("fetch failed: HTTP {status}"));
        }
        Ok(body)
    }

    pub fn post_json(url: &str, key: &str, body: &str) -> Result<Vec<u8>, String> {
        let headers = [
            ("Content-Type", "application/json".to_string()),
            ("Authorization", format!("Bearer {key}")),
        ];
        let (status, bytes) = request(Method::Post, url, &headers, Some(body.as_bytes()), MAX_MODEL_RESPONSE)
            .map_err(|e| format!("model call failed: {e}"))?;
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
