//! The UIPasteboard half of the system clipboard bridge
//! (`crates/ui-egui/src/system_clipboard.rs`): egui-winit compiles its OS clipboard (arboard)
//! out on iOS, leaving an in-app fallback, so text copied in Safari or Notes never pasted into a
//! field and PhotoCraft's own copies never reached other apps. The bridge reads and writes the
//! real pasteboard; every paste trigger in the app goes through it
//! (the text field menu, ⌘V, Edit › Paste).
//!
//! iOS 16+ asks "Allow Paste?" when another app's content is read — the system prompt, by
//! design; the call stays on the main thread (a frame) for it.
//!
//! The real implementation is iOS-only; on other hosts — clippy and tests of this bin — the
//! bridge is empty, and paste falls back to the window's own clipboard.

/// The bridge to hand to `Services::system_clipboard`.
#[cfg(target_os = "ios")]
pub fn bridge() -> photocraft_ui_egui::system_clipboard::SystemClipboard {
    real::bridge()
}

/// The bridge to hand to `Services::system_clipboard`.
#[cfg(not(target_os = "ios"))]
pub fn bridge() -> photocraft_ui_egui::system_clipboard::SystemClipboard {
    Default::default()
}

#[cfg(target_os = "ios")]
mod real {
    use std::sync::Arc;

    use objc2_foundation::NSString;
    use objc2_ui_kit::UIPasteboard;

    use photocraft_ui_egui::system_clipboard::SystemClipboard;

    pub fn bridge() -> SystemClipboard {
        SystemClipboard {
            get: Some(Arc::new(|| {
                // SAFETY: the pasteboard's `string` property is only non-atomic, and this runs
                // on the main thread (inside a frame); nothing else in the process touches the
                // pasteboard off-main.
                unsafe { UIPasteboard::generalPasteboard().string() }.map(|text| text.to_string())
            })),
            set: Some(Arc::new(|text: &str| {
                let text = NSString::from_str(text);
                // SAFETY: as above, a main-thread write of the pasteboard's string property.
                unsafe { UIPasteboard::generalPasteboard().setString(Some(&text)) };
            })),
        }
    }

    #[cfg(test)]
    mod tests {
        use photocraft_ui_egui::system_clipboard::SystemClipboard;

        /// The bridge reads and writes without panicking on an empty pasteboard (the simulator
        /// starts with one). What the OS clipboard holds is beyond a unit test; the ui-egui
        /// tests cover the paste flow above the bridge.
        #[test]
        fn the_bridge_reads_without_panicking() {
            let bridge = super::bridge();
            let _ = bridge.get.as_ref().map(|get| get());
            if let Some(set) = bridge.set.as_ref() {
                set("photocraft-clipboard-test");
            }
            assert!(SystemClipboard::default().get.is_none(), "the off-iOS stub stays empty");
        }
    }
}
