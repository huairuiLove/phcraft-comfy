//! The ComfyUI HTTP transport (see `photocraft_engine::comfy_cmds`): blocking ureq calls, one
//! request per call, loopback by default. The engine's `ai.comfyui.*` commands call into this
//! from their worker thread, so a slow server never blocks the UI.

use std::io::Read;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use photocraft_engine::comfy_cmds::{ComfyCall, ComfyFn, ComfyReply, ComfyRequest};
use serde_json::Value;

/// JSON calls connect and answer in seconds; a big image download may legitimately take minutes.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);
/// Never read more than this from one reply (a broken server must not exhaust memory).
const REPLY_MAX: u64 = 2 * 1024 * 1024 * 1024;
/// One readiness probe while a launched ComfyUI is still loading.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a started ComfyUI gets to come up (model imports can make a cold start slow).
const START_TIMEOUT: Duration = Duration::from_secs(180);
const START_POLL: Duration = Duration::from_millis(500);
/// The tail of the server's console log an error shows, so a failing launch says why.
const LOG_TAIL: u64 = 400;

/// The ComfyUI this shell started, so a second Start waits instead of spawning a twin, and the
/// handle reaps the child when it exits. Poisoning just means another start raced us: the child
/// it holds is as good as ours.
static STARTED: Mutex<Option<std::process::Child>> = Mutex::new(None);

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
        // Auto-start: probe, launch, wait. Loopback only — the engine gates it too, but a
        // transport must never run a launch command pointed at somebody else's machine.
        ComfyCall::Start { command } => start(url, command),
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

// ------------------------------------------------------------------ auto-start

/// The host of `url` (an `http(s)://…` address): between the scheme and the first `/`, minus
/// the port. IPv6 brackets come off, so `http://[::1]:8188` reads as `::1`.
fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let host = rest.split('/').next().unwrap_or(rest);
    // An IPv6 host is the part in brackets; anything else ends at its port.
    if let Some(inner) = host.strip_prefix('[') {
        return inner.split(']').next().unwrap_or(inner);
    }
    host.split(':').next().unwrap_or(host)
}

/// True when the host names this machine: only such a server may be started from here.
fn is_loopback(url: &str) -> bool {
    matches!(host_of(url), "127.0.0.1" | "localhost" | "::1")
}

/// Is the server at `url` answering `/system_stats`? A short, separate probe: a connection
/// refused or a loading server's silence is a normal answer here, not an error to report.
fn probe(url: &str) -> Option<Value> {
    let resp = ureq::get(&format!("{url}/system_stats")).config().timeout_global(Some(PROBE_TIMEOUT)).build().call();
    read_json(resp).ok()
}

/// The file a started ComfyUI's console output goes to, truncated per start: a failing launch
/// says why from there, and a user can read it after a crash. Under the temp dir, not the
/// config dir, so it never mixes into synced settings.
fn server_log() -> std::path::PathBuf {
    std::env::temp_dir().join("photocraft-comfyui.log")
}

/// The shell wrapper a launch command runs through: the user writes one line, arguments and
/// `cd … &&` included, so it needs a shell to interpret it.
fn shell_command(command: &str) -> std::process::Command {
    #[cfg(target_os = "windows")]
    let mut cmd = std::process::Command::new("cmd");
    #[cfg(not(target_os = "windows"))]
    let mut cmd = std::process::Command::new("sh");
    #[cfg(target_os = "windows")]
    cmd.arg("/C").arg(command);
    #[cfg(not(target_os = "windows"))]
    cmd.arg("-c").arg(command);
    cmd
}

/// Run the user's launch command and wait until this machine's ComfyUI answers. An empty
/// command asks for a ComfyUI found on this machine ([`crate::comfy_detect`]: a ComfyUI Desktop install,
/// or a checkout with a virtualenv). The server stays running after the app exits (it is the
/// user's server, not a child we own); the Child handle is kept only to notice an early exit and
/// reap it.
fn start(url: &str, command: &str) -> Result<ComfyReply, String> {
    start_in(url, command, &crate::comfy_detect::search_roots())
}

/// [`start`] against explicit search roots, so a test never starts this machine's real install.
fn start_in(url: &str, command: &str, roots: &[std::path::PathBuf]) -> Result<ComfyReply, String> {
    if !is_loopback(url) {
        return Err(format!("{url} is not on this machine; only a local server (like http://127.0.0.1:8188) can be started"));
    }
    let command = match command.trim() {
        "" => crate::comfy_detect::find(roots).map(|install| install.command(crate::comfy_detect::url_port(url))).ok_or_else(|| {
            "no local ComfyUI was found to start; set the ComfyUI launch command in Preferences, Integrations (for example `cd /opt/ComfyUI && python main.py --port 8188`)".to_string()
        })?,
        command => command.to_string(),
    };
    // Already up (or another window's start finished first): no spawn, just confirm.
    if let Some(stats) = probe(url) {
        return Ok(ComfyReply::Json(stats));
    }
    let mut guard = STARTED.lock().unwrap_or_else(PoisonError::into_inner);
    // Reap a previous child that has exited since, so the slot is meaningful.
    if let Some(child) = guard.as_mut()
        && child.try_wait().ok().flatten().is_some()
    {
        *guard = None;
    }
    if guard.is_none() {
        let mut log = std::fs::File::create(server_log());
        let mut cmd = shell_command(&command);
        if let Ok(f) = &mut log {
            use std::io::Write as _;
            let _ = writeln!(f, "$ {command}");
            cmd.stdout(f.try_clone().map_err(|e| format!("duplicating the ComfyUI log handle: {e}"))?).stderr(f.try_clone().map_err(|e| format!("duplicating the ComfyUI log handle: {e}"))?);
        }
        match cmd.spawn() {
            Ok(child) => *guard = Some(child),
            // The log stays meaningful even without handles: the shell itself wrote nothing.
            Err(e) => return Err(format!("could not run `{command}`: {e}")),
        }
    }
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Some(stats) = probe(url) {
            return Ok(ComfyReply::Json(stats));
        }
        // A quick exit means the command itself failed (bad path, missing python, port clash):
        // say so with the console tail instead of waiting out the whole timeout.
        if let Some(child) = guard.as_mut() {
            let status = child.try_wait().map_err(|e| format!("checking the started ComfyUI: {e}"))?;
            if let Some(status) = status {
                *guard = None;
                return Err(format!("`{command}` exited with {status}. Last output: {}", log_tail()));
            }
        }
        if Instant::now() >= deadline {
            return Err(format!("ComfyUI did not answer at {url} within {} s. Last output: {}", START_TIMEOUT.as_secs(), log_tail()));
        }
        std::thread::sleep(START_POLL);
    }
}

/// The end of the started server's console log, trimmed to a line the status bar can show.
fn log_tail() -> String {
    let mut text = std::fs::read(server_log()).map(|b| String::from_utf8_lossy(&b).trim().to_string()).unwrap_or_default();
    if text.chars().count() > LOG_TAIL as usize {
        let skip = text.char_indices().nth_back(LOG_TAIL as usize - 1).map_or(0, |(i, _)| i);
        // Start at a character boundary at or after `skip` (from_utf8_lossy output is valid UTF-8).
        text = text[skip..].trim_start().to_string();
    }
    text
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

    #[test]
    fn host_of_handles_ports_paths_and_ipv6() {
        assert_eq!(host_of("http://127.0.0.1:8188"), "127.0.0.1");
        assert_eq!(host_of("http://localhost:8188/system_stats"), "localhost");
        assert_eq!(host_of("https://comfy.example.com"), "comfy.example.com");
        assert_eq!(host_of("http://[::1]:8188"), "::1");
        // Not a URL at all: the whole string is the host, as unreadable rather than wrong.
        assert_eq!(host_of("localhost"), "localhost");
    }

    #[test]
    fn only_this_machine_is_startable() {
        for url in ["http://127.0.0.1:8188", "http://localhost", "http://localhost:8188/", "http://[::1]:8188"] {
            assert!(is_loopback(url), "{url}");
        }
        for url in ["http://192.168.1.5:8188", "https://comfy.example.com", "http://[2001:db8::2]:8188", "http://0.0.0.0:8188", "file:///tmp"] {
            assert!(!is_loopback(url), "{url}");
        }
    }

    #[test]
    fn start_refuses_what_it_must_not_run() {
        // A remote server is refused before anything is spawned.
        let e = start_in("https://comfy.example.com", "python main.py", &[]).err().unwrap_or_default();
        assert!(e.contains("not on this machine"), "{e}");
        // An empty command means "start a ComfyUI found here": with none, the error says where
        // to set one, and nothing was spawned.
        let e = start_in("http://127.0.0.1:8188", "   ", &[]).err().unwrap_or_default();
        assert!(e.contains("no local ComfyUI") && e.contains("Integrations"), "{e}");
        // The shell wrapper carries the command as one argument, quotes and all.
        let cmd = shell_command("cd /opt/ComfyUI && python main.py");
        assert!(format!("{cmd:?}").contains("cd /opt/ComfyUI && python main.py"));
    }
}
