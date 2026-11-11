//! The ComfyUI HTTP transport (see `photocraft_engine::comfy_cmds`): blocking ureq calls, one
//! request per call, loopback by default. The engine's `ai.comfyui.*` commands call into this
//! from their worker thread, so a slow server never blocks the UI.

use std::io::Read;
use std::time::Duration;

use photocraft_engine::comfy_cmds::{ComfyCall, ComfyFn, ComfyReply, ComfyRequest};
use serde_json::Value;

/// JSON calls connect and answer in seconds; a big image download may legitimately take minutes.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);
/// Never read more than this from one reply (a broken server must not exhaust memory).
const REPLY_MAX: u64 = 2 * 1024 * 1024 * 1024;

pub fn transport() -> ComfyFn {
    std::sync::Arc::new(|req: &ComfyRequest| call(&req.url, &req.call))
}

fn call(url: &str, call: &ComfyCall) -> Result<ComfyReply, String> {
    match call {
        ComfyCall::SystemStats => Ok(ComfyReply::Json(get_json(url, "/system_stats")?)),
        // The whole document at once: the per-node endpoint returns `{}` on current servers.
        ComfyCall::ObjectInfo => Ok(ComfyReply::Json(get_json(url, "/object_info")?)),
        ComfyCall::Queue { workflow, client_id } => {
            let body = serde_json::json!({"prompt": workflow, "client_id": client_id});
            Ok(ComfyReply::Json(post_json(url, "/prompt", &body)?))
        }
        // Fire and forget: the answer, if any, is nothing we act on.
        ComfyCall::Interrupt => {
            let _ = post_json(url, "/interrupt", &serde_json::json!({}));
            Ok(ComfyReply::Json(Value::Null))
        }
        ComfyCall::History { prompt_id } => {
            let path = format!("/history/{}", path_segment(prompt_id)?);
            Ok(ComfyReply::Json(get_json(url, &path)?))
        }
        ComfyCall::Download { filename, subfolder, kind } => Ok(ComfyReply::Bytes(get_bytes(
            url,
            "/view",
            &[("filename", filename), ("subfolder", subfolder), ("type", kind)],
        )?)),
        ComfyCall::Upload { name, bytes } => Ok(ComfyReply::Json(upload(url, name, bytes)?)),
        // The Image API backend is the iPad app's transport; this one only speaks ComfyUI.
        ComfyCall::ApiImage { .. } => Err("the desktop app generates through a ComfyUI server; the direct Image API runs on the iPad".into()),
        ComfyCall::ApiCheck { .. } => Err("the desktop app checks a ComfyUI server; the direct Image API runs on the iPad".into()),
    }
}

/// A path segment must be server-generated (a prompt id); anything else is refused, not encoded.
fn path_segment(s: &str) -> Result<&str, String> {
    if !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        Ok(s)
    } else {
        Err(format!("ComfyUI sent an unusable prompt id {s:?}"))
    }
}

fn get_json(url: &str, path: &str) -> Result<Value, String> {
    let resp = ureq::get(&format!("{url}{path}")).config().timeout_global(Some(CALL_TIMEOUT)).build().call();
    read_json(resp)
}

fn post_json(url: &str, path: &str, body: &Value) -> Result<Value, String> {
    let resp = ureq::post(&format!("{url}{path}")).config().timeout_global(Some(CALL_TIMEOUT)).build().send_json(body);
    read_json(resp)
}

/// A non-2xx still carries the server's reason (a rejected workflow's node errors) in its body.
fn read_json(resp: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Result<Value, String> {
    let resp = resp.map_err(|e| format!("{e}"))?;
    let status = resp.status();
    let body = read_body(resp)?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {}", preview(&body)));
    }
    serde_json::from_slice(&body).map_err(|e| format!("ComfyUI's reply is not JSON: {e}"))
}

fn get_bytes(url: &str, path: &str, queries: &[(&str, &str)]) -> Result<Vec<u8>, String> {
    let mut request = ureq::get(&format!("{url}{path}")).config().timeout_global(Some(DOWNLOAD_TIMEOUT)).build();
    for (k, v) in queries {
        request = request.query(k, v);
    }
    let resp = request.call().map_err(|e| format!("{e}"))?;
    let status = resp.status();
    let body = read_body(resp)?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {}", preview(&body)));
    }
    Ok(body)
}

fn upload(url: &str, name: &str, bytes: &[u8]) -> Result<Value, String> {
    use ureq::unversioned::multipart::{Form, Part};
    let part = Part::bytes(bytes)
        .file_name(name)
        .mime_str("image/png")
        .map_err(|e| format!("internal multipart error: {e}"))?;
    let form = Form::new().text("overwrite", "true").part("image", part);
    let resp = ureq::post(&format!("{url}/upload/image"))
        .config()
        .timeout_global(Some(DOWNLOAD_TIMEOUT))
        .build()
        .send(form);
    read_json(resp)
}

fn read_body(resp: ureq::http::Response<ureq::Body>) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    resp.into_body().into_reader().take(REPLY_MAX).read_to_end(&mut body).map_err(|e| format!("reading ComfyUI's reply: {e}"))?;
    Ok(body)
}

/// The first characters of an error body, enough to act on without flooding the status bar.
fn preview(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let cut = text.char_indices().take_while(|(i, _)| *i < 400).last().map_or(0, |(i, c)| i + c.len_utf8());
    text.get(..cut).unwrap_or_default().trim().to_string()
}
