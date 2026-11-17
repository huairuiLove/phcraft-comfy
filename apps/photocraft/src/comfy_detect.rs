//! Finding a ComfyUI already installed on this machine, so Generate can start one without the
//! user typing a launch command (Preferences ▸ Integrations keeps that command optional).
//!
//! Two shapes are recognised, in this order:
//!
//! - **ComfyUI Desktop.** Its manifest (`installations.json`, in the Electron app's per-user data
//!   directory) names every install; the checkout is `<install>/ComfyUI` and its packages live in
//!   `<install>/ComfyUI/.venv`, next to the bundled interpreter. The bundled `standalone-env`
//!   interpreter alone has no torch, so it is never used.
//! - **A plain checkout with a virtualenv**: `~/ComfyUI-Installs/*/ComfyUI` (Desktop's default
//!   install folder), `~/ComfyUI`, `~/Documents/ComfyUI`, `~/comfy/ComfyUI`, `/opt/ComfyUI`.
//!
//! The command always runs the checkout's own interpreter: a bare `python main.py` is not on a
//! desktop app's PATH and would not see the install's packages. Paths are shell-quoted, because
//! installs can live under a folder with a space in its name.

use std::io::Read;
use std::path::{Path, PathBuf};

/// The port a ComfyUI normally listens on, also the default in the engine's commands.
const DEFAULT_PORT: u16 = 8188;

/// A ComfyUI found on this machine.
pub struct Install {
    /// The checkout that holds `main.py`.
    pub dir: PathBuf,
    /// The interpreter that has the checkout's packages (the virtualenv's).
    pub python: PathBuf,
}

impl Install {
    /// The one-line command that starts this install on `port`, for `sh -c` / `cmd /C`.
    pub fn command(&self, port: u16) -> String {
        #[cfg(windows)]
        {
            format!("cd /d {} && {} main.py --port {port}", quote(&self.dir), quote(&self.python))
        }
        #[cfg(not(windows))]
        {
            format!("cd {} && {} main.py --port {port}", quote(&self.dir), quote(&self.python))
        }
    }
}

/// The directories searched for a checkout, most specific first: every ComfyUI Desktop install,
/// then Desktop's default install folder, then the conventional checkout locations.
pub fn search_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for manifest in desktop_manifests() {
        roots.extend(manifest_roots(&manifest));
    }
    if let Some(home) = home_dir() {
        roots.extend(desktop_installs(&home.join("ComfyUI-Installs")));
        roots.push(home.join("ComfyUI"));
        roots.push(home.join("Documents").join("ComfyUI"));
        roots.push(home.join("comfy").join("ComfyUI"));
    }
    #[cfg(not(windows))]
    roots.push(PathBuf::from("/opt/ComfyUI"));
    roots
}

/// The first root that is a usable checkout.
pub fn find(roots: &[PathBuf]) -> Option<Install> {
    roots.iter().find_map(|dir| install_at(dir))
}

/// Whether `dir` is a checkout whose virtualenv can run it.
fn install_at(dir: &Path) -> Option<Install> {
    if !dir.join("main.py").is_file() {
        return None;
    }
    let python = python_in(dir)?;
    // A Windows command line cannot carry a quote inside a quoted path.
    #[cfg(windows)]
    {
        if dir.to_string_lossy().contains('"') || python.to_string_lossy().contains('"') {
            return None;
        }
    }
    Some(Install { dir: dir.to_path_buf(), python })
}

/// The checkout's own interpreter: the virtualenv's, never a bare `python` from PATH (which has
/// none of the checkout's packages, torch included).
fn python_in(dir: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    let candidates = [
        ".venv/Scripts/python.exe",
        "venv/Scripts/python.exe",
        "../.venv/Scripts/python.exe",
        "python_embeded/python.exe",
    ];
    #[cfg(not(windows))]
    let candidates = [".venv/bin/python3", ".venv/bin/python", "venv/bin/python3", "venv/bin/python", "../.venv/bin/python3", "../.venv/bin/python"];
    candidates.iter().map(|rel| dir.join(rel)).find(|p| p.is_file())
}

/// ComfyUI Desktop's manifest lives in Electron's per-user application-data directory.
fn desktop_manifests() -> Vec<PathBuf> {
    let mut manifests = Vec::new();
    #[cfg(target_os = "macos")]
    if let Some(home) = home_dir() {
        manifests.push(home.join("Library/Application Support/Comfy Desktop/installations.json"));
    }
    #[cfg(windows)]
    if let Some(appdata) = std::env::var_os("APPDATA") {
        manifests.push(PathBuf::from(appdata).join("Comfy Desktop/installations.json"));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| home_dir().map(|h| h.join(".config")));
        if let Some(config) = config {
            manifests.push(config.join("Comfy Desktop/installations.json"));
        }
    }
    manifests
}

/// The `<install>/ComfyUI` checkout of every install the manifest names. The manifest is a JSON
/// array of `{"installPath": "…", …}`; a missing, oversized or malformed file finds nothing (the
/// conventional locations are searched next).
fn manifest_roots(manifest: &Path) -> Vec<PathBuf> {
    // A manifest is a few kilobytes; refuse to read anything that cannot be one.
    const MAX_BYTES: u64 = 1 << 20;
    const MAX_INSTALLS: usize = 16;
    let Ok(file) = std::fs::File::open(manifest) else { return Vec::new() };
    let mut text = String::new();
    if file.take(MAX_BYTES).read_to_string(&mut text).is_err() {
        return Vec::new();
    }
    let Ok(serde_json::Value::Array(installs)) = serde_json::from_str(&text) else { return Vec::new() };
    installs
        .iter()
        .filter_map(|install| install.get("installPath")?.as_str())
        .take(MAX_INSTALLS)
        .map(|path| PathBuf::from(path).join("ComfyUI"))
        .collect()
}

/// ComfyUI Desktop's default install folder: `<install>/ComfyUI` one level down from each entry.
fn desktop_installs(installs_dir: &Path) -> Vec<PathBuf> {
    const MAX_ENTRIES: usize = 16;
    let Ok(entries) = std::fs::read_dir(installs_dir) else { return Vec::new() };
    entries.flatten().take(MAX_ENTRIES).map(|entry| entry.path().join("ComfyUI")).collect()
}

/// This user's home directory, as the platform spells it.
fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let var = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let var = std::env::var_os("HOME");
    var.map(PathBuf::from).filter(|p| !p.as_os_str().is_empty())
}

/// The port in an `http(s)://…` address, or ComfyUI's default. A bare IPv6 address without a
/// port (`http://::1`) keeps the default rather than reading its last group as one.
pub fn url_port(url: &str) -> u16 {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split('/').next().unwrap_or(rest);
    let port = if let Some(after_bracket) = authority.strip_prefix('[').and_then(|a| a.split_once(']')).map(|(_, after)| after) {
        after_bracket.strip_prefix(':').and_then(|p| p.parse().ok())
    } else if authority.matches(':').count() == 1 {
        authority.rsplit_once(':').and_then(|(_, p)| p.parse().ok())
    } else {
        None
    };
    port.unwrap_or(DEFAULT_PORT)
}

/// Shell-quote one path. Single quotes survive everything a path can hold on POSIX (`'` itself
/// closes the quote, escapes and reopens it); Windows paths are double-quoted.
fn quote(path: &Path) -> String {
    let text = path.to_string_lossy();
    #[cfg(windows)]
    {
        format!("\"{text}\"")
    }
    #[cfg(not(windows))]
    {
        let mut out = String::with_capacity(text.len() + 2);
        out.push('\'');
        for ch in text.chars() {
            if ch == '\'' {
                out.push_str("'\\''");
            } else {
                out.push(ch);
            }
        }
        out.push('\'');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A checkout shaped like ComfyUI Desktop's: `main.py` beside `.venv/`.
    fn fake_install(base: &Path) -> PathBuf {
        let dir = base.join("ComfyUI");
        std::fs::create_dir_all(dir.join(".venv/bin")).unwrap();
        std::fs::write(dir.join("main.py"), b"# ComfyUI").unwrap();
        std::fs::write(dir.join(".venv/bin/python"), b"#!/bin/sh\n").unwrap();
        dir
    }

    #[cfg(not(windows))]
    #[test]
    fn finds_a_checkout_and_runs_its_own_interpreter() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = fake_install(tmp.path());
        let install = find(&[tmp.path().join("missing"), dir.clone()]).expect("the checkout is found");
        let cmd = install.command(8189);
        assert_eq!(install.python, dir.join(".venv/bin/python"));
        assert!(cmd.starts_with(&format!("cd '{}' && ", dir.display())), "{cmd}");
        assert!(cmd.ends_with("main.py --port 8189"), "{cmd}");
        // Ported from the server address, so a non-default port is honoured.
        assert_eq!(url_port("http://127.0.0.1:8189"), 8189);
    }

    #[cfg(not(windows))]
    #[test]
    fn needs_both_main_py_and_a_virtualenv() {
        let tmp = tempfile::tempdir().unwrap();
        // main.py without an interpreter (the bundled standalone-env has no torch: never run it).
        let bare = tmp.path().join("bare/ComfyUI");
        std::fs::create_dir_all(&bare).unwrap();
        std::fs::write(bare.join("main.py"), b"# ComfyUI").unwrap();
        assert!(find(std::slice::from_ref(&bare)).is_none());
        // An interpreter without main.py is some other project.
        let other = tmp.path().join("other");
        std::fs::create_dir_all(other.join(".venv/bin")).unwrap();
        std::fs::write(other.join(".venv/bin/python"), b"").unwrap();
        assert!(find(std::slice::from_ref(&other)).is_none());
        assert!(find(&[]).is_none());
    }

    #[test]
    fn reads_the_comfy_desktop_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let install = tmp.path().join("ComfyUI-Installs/ComfyUI");
        fake_install(&install);
        let manifest = tmp.path().join("Comfy Desktop/installations.json");
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        std::fs::write(
            &manifest,
            format!(r#"[{{"id":"inst-1","name":"ComfyUI","installPath":{:?},"status":"installed"}}]"#, install.display().to_string()),
        )
        .unwrap();
        let roots = manifest_roots(&manifest);
        assert_eq!(roots, vec![install.join("ComfyUI")]);
    }

    #[cfg(not(windows))]
    #[test]
    fn a_manifest_checkout_is_usable() {
        let tmp = tempfile::tempdir().unwrap();
        let install = tmp.path().join("ComfyUI-Installs/ComfyUI");
        fake_install(&install);
        let manifest = tmp.path().join("Comfy Desktop/installations.json");
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        std::fs::write(&manifest, format!(r#"[{{"installPath":{:?}}}]"#, install.display().to_string())).unwrap();
        let roots = manifest_roots(&manifest);
        assert!(find(&roots).is_some());
    }

    #[test]
    fn a_broken_manifest_finds_nothing_and_never_panics() {
        let tmp = tempfile::tempdir().unwrap();
        for (name, body) in [
            ("not-json.json", "{oops"),
            ("object.json", r#"{"installPath": "/tmp/x"}"#),
            ("no-path.json", r#"[{"id": "x"}]"#),
            ("wrong-type.json", r#"[{"installPath": 7}]"#),
        ] {
            let path = tmp.path().join(name);
            std::fs::write(&path, body).unwrap();
            assert!(manifest_roots(&path).is_empty(), "{name}");
        }
        assert!(manifest_roots(&tmp.path().join("absent.json")).is_empty());
        // Too many installs: the first few are enough, the rest are not read.
        let many = (0..40).map(|i| format!(r#"{{"installPath":"/nope/{i}"}}"#)).collect::<Vec<_>>().join(",");
        let path = tmp.path().join("many.json");
        std::fs::write(&path, format!("[{many}]")).unwrap();
        assert_eq!(manifest_roots(&path).len(), 16);
    }

    #[cfg(not(windows))]
    #[test]
    fn quotes_a_path_with_spaces_and_an_apostrophe() {
        // A checkout under a folder a user really could name.
        let dir = Path::new("/Users/some one/Comfy's UI");
        let quoted = quote(dir);
        assert_eq!(quoted, "'/Users/some one/Comfy'\\''s UI'");
        assert_eq!(quote(Path::new("/opt/ComfyUI")), "'/opt/ComfyUI'");
    }

    #[test]
    fn url_port_reads_the_server_address() {
        for (url, port) in [
            ("http://127.0.0.1:8188", 8188),
            ("http://127.0.0.1:8199/system_stats", 8199),
            ("http://localhost", 8188),
            ("https://comfy.example.com", 8188),
            ("http://[::1]:8188", 8188),
            ("http://[::1]:9000/", 9000),
            // A bare IPv6 address is not a port number.
            ("http://::1", 8188),
        ] {
            assert_eq!(url_port(url), port, "{url}");
        }
    }
}
