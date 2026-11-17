//! The ClipboardManager half of the system clipboard bridge
//! (`crates/ui-egui/src/system_clipboard.rs`): egui-winit compiles its OS clipboard (arboard)
//! out on Android, leaving an in-app fallback, so text copied in another app never pasted into a
//! field — the Generate panel's Image API endpoint, model and key are the ones that matter — and
//! the app's own copies never reached other apps. The bridge reads and writes the real clipboard
//! through the activity's `ClipboardManager` (`platform.rs`); every paste trigger in the app goes
//! through it (the touch edit menu's Paste, the quick-actions column's).
//!
//! Android 10+ only shows the clipboard to the focused app, which the app is while the user is
//! pasting; the call stays on the UI thread (inside a frame), as the iPad bridge stays on the
//! main thread for iOS's "Allow Paste?" prompt.
//!
//! The real implementation is Android-only; on other hosts — clippy and the host tests of this
//! crate — the bridge is empty, and paste falls back to the window's own clipboard.

/// The bridge to hand to `Services::system_clipboard` and to
/// [`photocraft_ui_egui::system_clipboard::CopySync`].
#[cfg(target_os = "android")]
pub fn bridge() -> photocraft_ui_egui::system_clipboard::SystemClipboard {
    real::bridge()
}

/// The bridge to hand to `Services::system_clipboard` and to
/// [`photocraft_ui_egui::system_clipboard::CopySync`].
#[cfg(not(target_os = "android"))]
pub fn bridge() -> photocraft_ui_egui::system_clipboard::SystemClipboard {
    Default::default()
}

#[cfg(target_os = "android")]
mod real {
    use std::sync::Arc;

    use photocraft_ui_egui::system_clipboard::SystemClipboard;

    pub fn bridge() -> SystemClipboard {
        SystemClipboard {
            get: Some(Arc::new(crate::platform::clipboard_text)),
            set: Some(Arc::new(|text: &str| crate::platform::set_clipboard_text(text))),
        }
    }
}

#[cfg(test)]
mod tests {
    /// Off the device the bridge is the empty stub: the host test build has no Android
    /// `ClipboardManager`. What the OS clipboard holds is beyond a unit test (as on iPadOS);
    /// ui-egui's tests cover the paste flow above the bridge.
    #[cfg(not(target_os = "android"))]
    #[test]
    fn the_bridge_is_empty_off_the_device() {
        let bridge = super::bridge();
        assert!(bridge.get.is_none());
        assert!(bridge.set.is_none());
    }
}
