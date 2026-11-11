//! The Rust half of the system document picker (`ios/PCDocumentPicker.m`): the picker and
//! its delegate are ObjC (implementing the protocol from Rust needs objc2's unsafe
//! `declare_class!`), and this module is the safe-looking seam — the C callbacks run on the
//! main thread (where egui frames also run, never concurrently), copy their bytes across the
//! ABI, and answer the app's pending `FileDialogReply`. The only unsafe in this crate is the
//! pointer reads those copies require.
//!
//! The real implementation is iOS-only (the pc_* symbols only link there); on other hosts —
//! clippy and tests of this bin — the three entry points are no-ops, and a dropped reply
//! counts as Cancel (`crates/ui-egui/src/file_dialog.rs`).

#[cfg(target_os = "ios")]
use photocraft_ui_egui::FileDialogReply;

/// Wire the callbacks once, before any dialog can start (from `main`).
#[cfg(target_os = "ios")]
pub fn install() {
    real::install();
}

/// File ▸ Open and friends: present the system picker; the reply goes out from the
/// delegate's callbacks.
#[cfg(target_os = "ios")]
pub fn open(multiple: bool, reply: FileDialogReply) {
    real::open(multiple, reply);
}

/// Present the export sheet for a file the `write` service already staged (see `main.rs`).
#[cfg(target_os = "ios")]
pub fn export(path: &std::path::Path) {
    real::export(path);
}

#[cfg(not(target_os = "ios"))]
pub use stubs::{export, install, open};

#[cfg(target_os = "ios")]
mod real {
    use std::ffi::{c_char, c_int, c_uchar, CString};
    use std::sync::{Mutex, PoisonError};

    use photocraft_ui_egui::{FileDialogAnswer, FileDialogReply};

    unsafe extern "C" {
        // `safe`: our own C (PCDocumentPicker.m) — each dispatches async on the main queue and
        // keeps no reference to its arguments past the call.
        safe fn pc_set_callbacks(open: extern "C" fn(*const c_char, *const c_uchar, usize, c_int), cancel: extern "C" fn());
        safe fn pc_pick_open(allow_multiple: c_int);
        safe fn pc_pick_export(path: *const c_char);
    }

    /// The dialog the app started and is waiting on: the answer channel plus the files the
    /// delegate has delivered so far (multi-select delivers one callback per file, `last` flags
    /// the end).
    struct Pending {
        reply: Option<FileDialogReply>,
        files: Vec<(String, Vec<u8>)>,
    }

    static PENDING: Mutex<Option<Pending>> = Mutex::new(None);

    pub fn install() {
        pc_set_callbacks(on_open, on_cancel);
    }

    pub fn open(multiple: bool, reply: FileDialogReply) {
        *PENDING.lock().unwrap_or_else(PoisonError::into_inner) =
            Some(Pending { reply: Some(reply), files: Vec::new() });
        pc_pick_open(c_int::from(multiple));
    }

    /// Present the export sheet for a file the `write` service already staged (see `main.rs`).
    pub fn export(path: &std::path::Path) {
        let Ok(path) = CString::new(path.to_string_lossy().as_bytes()) else {
            return;
        };
        pc_pick_export(path.as_ptr());
    }

    extern "C" fn on_open(name: *const c_char, bytes: *const c_uchar, len: usize, last: c_int) {
        // SAFETY: the pointers are the callback arguments PCDocumentPicker.m passes for the
        // lifetime of the call (the NSData and NSString outlive it), and `len` is that
        // NSData's length — both are copied below before returning.
        let name = if name.is_null() {
            "file".to_string()
        } else {
            unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy().into_owned()
        };
        let data = if bytes.is_null() || len == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(bytes, len) }.to_vec()
        };
        let mut guard = PENDING.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(pending) = guard.as_mut() else { return };
        if !data.is_empty() {
            pending.files.push((name, data));
        }
        if last != 0 {
            let Some(mut done) = guard.take() else { return };
            if let Some(reply) = done.reply.take() {
                let answer = match done.files.len() {
                    0 => None, // opened nothing readable: cancelled, not an error
                    1 => Some(FileDialogAnswer::Contents(done.files[0].0.clone(), std::mem::take(&mut done.files[0].1))),
                    _ => Some(FileDialogAnswer::ContentsMany(std::mem::take(&mut done.files))),
                };
                reply.send(answer);
            }
        }
    }

    extern "C" fn on_cancel() {
        let mut guard = PENDING.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(pending) = guard.as_mut()
            && let Some(reply) = pending.reply.take()
        {
            reply.send(None);
        }
        *guard = None;
    }
}

#[cfg(not(target_os = "ios"))]
mod stubs {
    use photocraft_ui_egui::FileDialogReply;

    pub fn install() {}

    /// No picker off-device: the reply drops unanswered, which the app reads as Cancel.
    pub fn open(_multiple: bool, _reply: FileDialogReply) {}

    pub fn export(_path: &std::path::Path) {}
}
