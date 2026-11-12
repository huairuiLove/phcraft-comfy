//! The system text clipboard on hosts whose egui-winit has none: egui-winit compiles its
//! OS-clipboard (arboard) out on iOS and Android, leaving an in-app fallback — text copied in
//! Safari or another app could never paste into a field, and PhotoCraft's own copies never
//! reached the system pasteboard. The mobile shells hand the bridge over
//! ([`Services::system_clipboard`](crate::Services::system_clipboard)): reads go through it
//! before the window's paste request, and a plugin mirrors every text copy the app makes to the
//! OS pasteboard. Desktop and web set no bridge and keep egui-winit's own clipboard, untouched.

use std::sync::Arc;

use egui::{Context, Event, FullOutput, RawInput};

/// Read the OS clipboard's text: `None` when it holds none (or none readable).
pub type GetFn = Arc<dyn Fn() -> Option<String> + Send + Sync>;
/// Write text to the OS clipboard.
pub type SetFn = Arc<dyn Fn(&str) + Send + Sync>;

/// The shell's OS clipboard bridge ([`Services::system_clipboard`](crate::Services::system_clipboard)).
/// Cheap to clone; unset (`Default`) wherever egui-winit owns the clipboard itself.
#[derive(Clone, Default)]
pub struct SystemClipboard {
    pub get: Option<GetFn>,
    pub set: Option<SetFn>,
}

/// Where [`SystemClipboard::request_paste`] leaves the text it read, between the request and the
/// frame that delivers it.
fn pending_id() -> egui::Id {
    egui::Id::unique("system-clipboard-paste")
}

impl SystemClipboard {
    /// The bridge reads the OS clipboard at all.
    pub fn reads_os(&self) -> bool {
        self.get.is_some()
    }

    /// Paste into the focused text field. With a bridge, read the OS clipboard now and hand the
    /// text to the next frame as a [`Event::Paste`] — the window's own paste request would read
    /// only egui's in-app fallback there. Without a bridge (desktop, web), ask the window, as
    /// ⌘V does. An OS clipboard with no text falls through to the window's too, so a copy made
    /// inside the app still pastes.
    pub fn request_paste(&self, ctx: &Context) {
        if let Some(text) = self.get.as_ref().and_then(|get| get()).filter(|text| !text.is_empty()) {
            // Keep egui's in-app clipboard in step: the next window-paste (and the undo
            // stack's view of a copy) then agrees with what was pasted.
            ctx.copy_text(text.clone());
            if let Some(set) = &self.set {
                set(&text);
            }
            ctx.data_mut(|d| d.insert_temp(pending_id(), text));
            ctx.request_repaint();
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaste);
    }
}

/// Paste with whatever the host has: the shell's OS bridge when there is one, else the window's
/// paste request (the desktop and web path). Every paste trigger in the app goes through here.
pub fn request_paste(clipboard: Option<&SystemClipboard>, ctx: &Context) {
    match clipboard {
        Some(bridge) => bridge.request_paste(ctx),
        None => ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaste),
    }
}

/// A paste [`request_paste`] read from the OS clipboard, delivered into this frame's input: the
/// focused text field consumes the event in its own pass.
pub fn deliver_pending(ctx: &Context, raw: &mut RawInput) {
    if let Some(text) = ctx.data_mut(|d| d.remove_temp::<String>(pending_id())) {
        raw.events.push(Event::Paste(text));
    }
}

/// ⌘V/⌘C/⌘X into a focused text field, with a bridge taking the paste: the key would otherwise
/// reach the window's paste path, which has no OS clipboard to read. Returns true when the V was
/// taken (the caller drops it; the bridge delivers the paste next frame).
pub fn take_paste_key(bridge: &SystemClipboard, ctx: &Context, raw: &mut RawInput) {
    if !bridge.reads_os() || !ctx.text_edit_focused() {
        return;
    }
    let is_paste_key = |event: &Event| {
        matches!(event, Event::Key { key: egui::Key::V, pressed: true, modifiers, .. }
            if modifiers.command && !modifiers.alt && !modifiers.shift)
    };
    if raw.events.iter().any(is_paste_key) {
        raw.events.retain(|event| !is_paste_key(event));
        bridge.request_paste(ctx);
    }
}

/// Mirrors every text copy the app makes to the OS clipboard ([`SystemClipboard::set`]), which
/// egui-winit cannot do where it has no OS clipboard of its own: without it a copy pasted only
/// inside PhotoCraft. The mobile shell installs it once
/// (`ctx.add_plugin(system_clipboard::CopySync::new(bridge))`); desktop and web need none.
pub struct CopySync {
    bridge: SystemClipboard,
}

impl CopySync {
    pub fn new(bridge: SystemClipboard) -> Self {
        Self { bridge }
    }
}

impl egui::plugin::Plugin for CopySync {
    fn debug_name(&self) -> &'static str {
        "PhotoCraft system clipboard copy sync"
    }

    fn output_hook(&mut self, _ctx: &Context, output: &mut FullOutput) {
        let Some(set) = self.bridge.set.clone() else { return };
        for command in &output.platform_output.commands {
            if let egui::OutputCommand::CopyText(text) = command {
                set(text);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn bridge(text: &'static str) -> (SystemClipboard, Arc<AtomicU32>) {
        let copied = Arc::new(AtomicU32::new(0));
        let count = Arc::clone(&copied);
        (
            SystemClipboard {
                get: Some(Arc::new(move || Some(text.to_string()))),
                set: Some(Arc::new(move |t: &str| {
                    assert_eq!(t, text);
                    count.fetch_add(1, Ordering::Relaxed);
                })),
            },
            copied,
        )
    }

    /// A bridge read becomes the next frame's Paste event, and both egui's clipboard and the OS
    /// pasteboard see the text.
    #[test]
    fn a_bridge_read_is_delivered_as_the_next_paste_event() {
        let ctx = Context::default();
        let (b, copied) = bridge("https://comfy.local");
        b.request_paste(&ctx);
        assert_eq!(copied.load(Ordering::Relaxed), 1, "the OS pasteboard is written");
        let mut raw = RawInput::default();
        deliver_pending(&ctx, &mut raw);
        assert_eq!(raw.events, vec![Event::Paste("https://comfy.local".into())]);
        // Delivered once: a second frame gets nothing.
        let mut raw = RawInput::default();
        deliver_pending(&ctx, &mut raw);
        assert!(raw.events.is_empty());
    }

    /// Without a bridge, or with an OS clipboard holding no text, the window's own paste request
    /// goes out instead (egui-winit answers it from its own clipboard).
    #[test]
    fn no_bridge_or_empty_clipboard_falls_back_to_the_window_paste() {
        let asked = |bridge: &SystemClipboard| {
            let ctx = Context::default();
            let mut out = ctx.run_ui(RawInput::default(), |ui| bridge.request_paste(ui.ctx()));
            // The kittest-less pass leaves font textures unapplied; dropping them by hand.
            out.textures_delta.clear();
            out.viewport_output.values().any(|v| v.commands.contains(&egui::ViewportCommand::RequestPaste))
        };
        assert!(asked(&SystemClipboard::default()), "no bridge: the window is asked");
        let empty = SystemClipboard { get: Some(Arc::new(|| None)), set: None };
        assert!(asked(&empty), "an empty OS clipboard asks the window too");
        let (bridge, _) = bridge("text");
        assert!(!asked(&bridge), "a bridge read never asks the window");
    }

    /// ⌘V in a focused field is taken over by the bridge; other keys pass through.
    #[test]
    fn the_bridge_takes_the_paste_key_in_a_focused_field() {
        let ctx = Context::default();
        let (b, _) = bridge("key");
        let key = |key| Event::Key { key, pressed: true, repeat: false, physical_key: None, modifiers: egui::Modifiers::COMMAND };
        // Without a focused text field the key stays (the document's own paste keeps it).
        let mut raw = RawInput { events: vec![key(egui::Key::V)], ..Default::default() };
        take_paste_key(&b, &ctx, &mut raw);
        assert_eq!(raw.events.len(), 1, "no focused field: the key passes through");
        // A focused text field: the V is taken and the paste is pending; the ⌘C passes.
        let field = egui::Id::unique("field");
        egui::widgets::text_edit::TextEditState::default().store(&ctx, field);
        ctx.memory_mut(|m| m.request_focus(field));
        let mut raw = RawInput { events: vec![key(egui::Key::V), key(egui::Key::C)], ..Default::default() };
        take_paste_key(&b, &ctx, &mut raw);
        assert_eq!(raw.events, vec![key(egui::Key::C)], "only the paste key is taken");
        let mut delivered = RawInput::default();
        deliver_pending(&ctx, &mut delivered);
        assert_eq!(delivered.events, vec![Event::Paste("key".into())]);
    }

    /// The copy-sync plugin mirrors CopyText commands to the OS pasteboard.
    #[test]
    fn the_copy_sync_plugin_mirrors_text_copies() {
        use egui::plugin::Plugin as _;

        let (b, copied) = bridge("hello");
        let mut plugin = CopySync::new(b);
        let mut output = FullOutput::default();
        output.platform_output.commands.push(egui::OutputCommand::CopyText("hello".into()));
        plugin.output_hook(&Context::default(), &mut output);
        assert_eq!(copied.load(Ordering::Relaxed), 1);
    }
}
