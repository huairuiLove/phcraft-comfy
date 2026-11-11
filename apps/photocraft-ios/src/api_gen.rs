//! The Image API transport (see `photocraft_engine::comfy_cmds`): the iPad app's generation
//! backend, injected as `Services::comfy_transport`. No ComfyUI server is involved — every
//! `ai.comfyui.*` call that would speak the ComfyUI protocol explains that instead, and the two
//! API calls do the work directly:
//!
//! - `ApiImage` → `POST {base}/images/edits` (multipart: the source PNG, the model, the prompt)
//!   or `POST {base}/images/generations` with no source — one OpenAI-compatible round trip, the
//!   reply's single image back as bytes. A reply naming a URL instead of inline bytes is fetched
//!   with the same key.
//! - `ApiCheck` → `GET {base}/models`, the connection check.
//!
//! The base arrives normalized to end in `/v1` (`comfy_cmds::image_api_base_url`); normalizing
//! again is free and keeps the transport correct for any caller. The key travels only in the
//! Authorization header — never in an error or the log.

use std::io::Read;
use std::time::Duration;

use photocraft_engine::comfy_cmds::{
    image_api_base_url, image_api_images, ComfyCall, ComfyFn, ComfyReply, ComfyRequest, ImageApiImage,
};
use serde_json::{Value, json};

/// A JSON call (the model list) answers in seconds.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Never read more than this from one reply (a broken endpoint must not exhaust memory). A
/// 512 MiB image is the engine's cap; its base64 form fits well under this.
const REPLY_MAX: u64 = 1024 * 1024 * 1024;
/// The relay's images may be megabytes; keep URL-fetched bytes bounded too.
const URL_IMAGE_TIMEOUT: Duration = Duration::from_secs(120);

/// The one-line refusal every ComfyUI-protocol call gets: there is no server behind this shell.
const NO_SERVER: &str = "the iPad app generates through the Image API (Preferences ▸ Integrations), not a ComfyUI server";

pub fn transport() -> ComfyFn {
    std::sync::Arc::new(|req: &ComfyRequest| call(req))
}

fn call(req: &ComfyRequest) -> std::result::Result<ComfyReply, String> {
    match &req.call {
        ComfyCall::ApiImage { base, key, model, prompt, image, timeout_s } => {
            let timeout = Duration::from_secs((*timeout_s).clamp(10, 3600)).max(CALL_TIMEOUT);
            let base = image_api_base_url(base)?;
            let v = match image {
                Some(png) => post_edits(&base, key, model, prompt, png, timeout)?,
                None => post_generations(&base, key, model, prompt, timeout)?,
            };
            // One image per call, as asked; a reply with several keeps the first.
            let images = image_api_images(&v)?;
            match images.first() {
                Some(ImageApiImage::Bytes(bytes)) => Ok(ComfyReply::Bytes(bytes.clone())),
                Some(ImageApiImage::Url(url)) => {
                    let bytes = get_url(url, key)?;
                    Ok(ComfyReply::Bytes(bytes))
                }
                None => Err("the Image API replied without an image".into()),
            }
        }
        ComfyCall::ApiCheck { base, key } => {
            let base = image_api_base_url(base)?;
            Ok(ComfyReply::Json(get_json(&base, "/models", key, CALL_TIMEOUT)?))
        }
        ComfyCall::SystemStats | ComfyCall::ObjectInfo | ComfyCall::Queue { .. } | ComfyCall::Interrupt | ComfyCall::History { .. }
        | ComfyCall::Download { .. } | ComfyCall::Upload { .. } => Err(NO_SERVER.into()),
    }
}

/// `POST /images/edits`: the source image, the model and the prompt as multipart form data (the
/// OpenAI-compatible shape every relay accepts for image editing).
fn post_edits(
    base: &str,
    key: &str,
    model: &str,
    prompt: &str,
    png: &[u8],
    timeout: Duration,
) -> std::result::Result<Value, String> {
    use ureq::unversioned::multipart::{Form, Part};
    let part = Part::bytes(png).file_name("photocraft.png").mime_str("image/png").map_err(|e| format!("internal multipart error: {e}"))?;
    let form = Form::new().text("model", model).text("prompt", prompt).part("image", part);
    let request = ureq::post(&format!("{base}/images/edits")).config().timeout_global(Some(timeout)).build();
    let request = if key.is_empty() { request } else { request.header("Authorization", format!("Bearer {key}")) };
    let resp = request.send(form);
    read_json(resp, "the image edit")
}

/// `POST /images/generations`: text only, no source image.
fn post_generations(base: &str, key: &str, model: &str, prompt: &str, timeout: Duration) -> std::result::Result<Value, String> {
    let body = json!({"model": model, "prompt": prompt});
    let request = ureq::post(&format!("{base}/images/generations")).config().timeout_global(Some(timeout)).build();
    let request = if key.is_empty() { request } else { request.header("Authorization", format!("Bearer {key}")) };
    let resp = request.send_json(&body);
    read_json(resp, "the image generation")
}

fn get_json(base: &str, path: &str, key: &str, timeout: Duration) -> std::result::Result<Value, String> {
    let request = ureq::get(&format!("{base}{path}")).config().timeout_global(Some(timeout)).build();
    let request = if key.is_empty() { request } else { request.header("Authorization", format!("Bearer {key}")) };
    let resp = request.call();
    read_json(resp, "the model list")
}

/// A reply image by URL (some relays send a link instead of inline bytes), fetched with the key.
fn get_url(url: &str, key: &str) -> std::result::Result<Vec<u8>, String> {
    let request = ureq::get(url).config().timeout_global(Some(URL_IMAGE_TIMEOUT)).build();
    let request = if key.is_empty() { request } else { request.header("Authorization", format!("Bearer {key}")) };
    let resp = request.call();
    let resp = resp.map_err(|e| format!("{e}"))?;
    let status = resp.status();
    let body = read_body(resp)?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {}", preview(&body)));
    }
    if body.is_empty() {
        return Err("the endpoint returned an empty image".into());
    }
    Ok(body)
}

/// A non-2xx still carries the endpoint's reason in its body; `what` names the step for the
/// message the panel shows.
fn read_json(resp: std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>, what: &str) -> std::result::Result<Value, String> {
    let resp = resp.map_err(|e| format!("{what}: {e}"))?;
    let status = resp.status();
    let body = read_body(resp)?;
    if !status.is_success() {
        return Err(format!("{what}: HTTP {status}: {}", preview(&body)));
    }
    serde_json::from_slice(&body).map_err(|e| format!("{what}: the reply is not JSON: {e}"))
}

fn read_body(resp: ureq::http::Response<ureq::Body>) -> std::result::Result<Vec<u8>, String> {
    let mut body = Vec::new();
    resp.into_body()
        .into_reader()
        .take(REPLY_MAX)
        .read_to_end(&mut body)
        .map_err(|e| format!("reading the reply: {e}"))?;
    Ok(body)
}

/// The first characters of an error body, enough to act on without flooding the status bar.
fn preview(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let cut = text.char_indices().take_while(|(i, _)| *i < 400).last().map_or(0, |(i, c)| i + c.len_utf8());
    text.get(..cut).unwrap_or_default().trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // The ComfyUI protocol is refused with guidance, never a panic — the engine's other
    // commands stay honest about why they cannot run here.
    #[test]
    fn comfyui_protocol_calls_are_refused_with_guidance() {
        let t = transport();
        for call in [
            ComfyCall::SystemStats,
            ComfyCall::ObjectInfo,
            ComfyCall::Queue { workflow: json!({}), client_id: "c".into() },
            ComfyCall::Interrupt,
            ComfyCall::History { prompt_id: "p".into() },
            ComfyCall::Download { filename: "f".into(), subfolder: String::new(), kind: "output".into() },
            ComfyCall::Upload { name: "n".into(), bytes: Vec::new() },
        ] {
            let Err(e) = t(&ComfyRequest { url: "https://x.example".into(), call }) else {
                panic!("the ComfyUI protocol must be refused, not answered")
            };
            assert!(e.contains("Image API"), "{e}");
        }
    }

    #[test]
    fn a_bad_base_is_rejected_before_any_request() {
        let t = transport();
        let call = ComfyCall::ApiImage {
            base: "ftp://nope".into(),
            key: String::new(),
            model: "gpt-image-2".into(),
            prompt: "p".into(),
            image: Some(vec![1, 2, 3]),
            timeout_s: 30,
        };
        let r = t(&ComfyRequest { url: "ftp://nope".into(), call });
        let Err(e) = r else { panic!("a non-http(s) base must be refused before any request") };
        assert!(e.contains("not an http(s) address"), "{e}");
        let call = ComfyCall::ApiCheck { base: "api.example.com".into(), key: String::new() };
        let r = t(&ComfyRequest { url: String::new(), call });
        let Err(e) = r else { panic!("a base without a scheme must be refused before any request") };
        assert!(e.contains("not an http(s) address"), "{e}");
    }

    // One live round trip against a thread-local HTTP server: the transport posts the edit as
    // multipart with the bearer key, and returns the reply's inline image as bytes.
    #[test]
    fn api_image_round_trips_through_a_local_server() {
        use base64::Engine as _;
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").ok();
        let Some(listener) = listener else { return };
        let addr = listener.local_addr().ok();
        let Some(addr) = addr else { return };
        let png = vec![0x89, b'P', b'N', b'G', 1, 2, 3];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let reply = format!("{{\"data\":[{{\"b64_json\":\"{b64}\"}}]}}");
        let server = std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else { return String::new() };
            // The multipart body can arrive in several TCP segments; read until the image part
            // (the last one) is through, or the peer stops.
            let mut head = String::new();
            loop {
                let mut buf = [0u8; 8192];
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => head.push_str(&String::from_utf8_lossy(&buf[..n])),
                }
                if head.contains("name=\"image\"") {
                    break;
                }
            }
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len());
            let _ = stream.write_all(response.as_bytes());
            head
        });
        let t = transport();
        let call = ComfyCall::ApiImage {
            base: format!("http://{addr}"),
            key: "sk-test".into(),
            model: "gpt-image-2".into(),
            prompt: "a red dress".into(),
            image: Some(vec![1, 2, 3, 4]),
            timeout_s: 30,
        };
        let r = t(&ComfyRequest { url: format!("http://{addr}"), call });
        let head = server.join().unwrap_or_default();
        match r {
            Ok(ComfyReply::Bytes(bytes)) => assert_eq!(bytes, png),
            Ok(ComfyReply::Json(v)) => panic!("expected image bytes, got JSON {v}"),
            Err(e) => panic!("expected image bytes, got the error {e}"),
        }
        // The request hit the edits endpoint (the base gains `/v1`), as multipart, with the key
        // in the header (ureq sends header names lower-case).
        let head_lower = head.to_ascii_lowercase();
        assert!(head.starts_with("POST /v1/images/edits"), "{head}");
        assert!(head_lower.contains("authorization: bearer sk-test"), "the key rides the header: {head}");
        assert!(head_lower.contains("multipart/form-data"), "{head}");
        assert!(head.contains("name=\"model\""), "{head}");
        assert!(head.contains("name=\"prompt\""), "{head}");
        assert!(head.contains("name=\"image\""), "{head}");
    }
}
