//! The touch quick-actions column: Undo, Redo, Copy, Cut and Paste as small buttons in one
//! vertical strip floating at the canvas's top-right corner, left of the end dock (or at the
//! screen's right edge in the portrait drawer layout) — the touch answer to ⌘Z/⌘⇧Z/⌘C/⌘X/⌘V,
//! which an iPad's software keyboard cannot keep at hand. Touch Interface only: the desktop
//! keeps its shortcuts, menu bar and edit menu, and a floating column over the artwork would
//! only block it.
//!
//! Every button dispatches the engine's own edit command through `app.run`, so enablement
//! (greyed with nothing to undo, nothing selected, an empty clipboard), the OS-clipboard
//! import a paste does (`app/commands.rs`), history and status messages are exactly the Edit
//! menu's — one behaviour, two affordances. The interactions are taps only (iPadOS has no
//! right-click), and the card is drawn behind its buttons so the column reads as chrome over
//! the artwork, like the layer-tray strip.

use super::{PhotocraftApp, Tokens, icons, json, pos2, vec2};
use egui::{Order, Rect, Stroke, StrokeKind};

/// One button: its glyph, its command id, and the menu label it shares (translations and
/// screen-reader names come from the catalog the Edit menu uses).
const ACTIONS: [(&str, &str, &str); 5] = [
    ("undo-2", "edit.undo", "Undo"),
    ("redo-2", "edit.redo", "Redo"),
    ("copy", "edit.copy", "Copy"),
    ("scissors", "edit.cut", "Cut"),
    ("clipboard", "edit.paste", "Paste"),
];

/// The button's touch target.
const SIZE: f32 = 40.0;
/// Padding inside the column's card, and the gap between buttons.
const PAD: f32 = 4.0;
const GAP: f32 = 2.0;
/// Distance from the canvas's top-right corner.
const MARGIN: f32 = 12.0;

/// The column, over the canvas's top-right corner. An `Area` in the Foreground order, so it
/// floats above the canvas and takes taps only on its buttons; anchored to `last_canvas_rect`,
/// which is the space the dock/rail/toolbars leave the artwork — exactly "left of the end
/// dock". Called from the frame layout before the canvas draws (both the docked and the plain
/// layout, never on the Home screen), so the first frame after a layout change positions off
/// the previous rect — a one-frame settle, like the layer-tray strip.
pub fn column(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    if !crate::touch::on(app) {
        return;
    }
    let canvas = app.last_canvas_rect;
    if !canvas.is_positive() {
        return;
    }
    let ctx = ui.ctx().clone();
    let t = Tokens::get(&ctx);
    let w = SIZE + 2.0 * PAD;
    let h = 2.0 * PAD + ACTIONS.len() as f32 * SIZE + (ACTIONS.len() - 1) as f32 * GAP;
    let min = pos2(canvas.right() - w - MARGIN, canvas.top() + MARGIN);
    let mut actions: Vec<&str> = Vec::new();
    egui::Area::new(egui::Id::unique("quick-actions"))
        .order(Order::Foreground)
        .fixed_pos(min)
        .show(&ctx, |ui| {
            // The card behind its buttons: painted before them, on the same layer, so the
            // buttons (drawn later) sit on top of it.
            let card = Rect::from_min_size(ui.cursor().min, vec2(w, h));
            ui.painter().rect_filled(card, t.radius_lg, t.card.gamma_multiply(0.97));
            ui.painter().rect_stroke(card, t.radius_lg, Stroke::new(1.0, t.card_border), StrokeKind::Inside);
            ui.spacing_mut().item_spacing = vec2(0.0, GAP);
            ui.add_space(PAD);
            for (icon, cmd, label) in ACTIONS {
                let enabled = app.session.is_enabled(cmd);
                let button = ui.add_enabled_ui(enabled, |ui| icons::button(ui, icon, SIZE, false, label)).inner;
                button.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, tl!(label)));
                if button.clicked() {
                    actions.push(cmd);
                }
            }
            // The card is sized for the buttons; the layout under it must not exceed it.
            debug_assert!(ui.min_rect().height() <= h + 0.5, "the quick-actions column outgrew its card");
        });
    for cmd in actions {
        if let Err(e) = app.run(cmd, json!({})) {
            app.ui.status = e;
            app.ui.status_error = true;
        }
    }
}

/// The column's card rect last frame, where the layout put it (tests, and overlap checks).
/// `None` before it first drew.
#[cfg(test)]
pub(crate) fn rect(ctx: &egui::Context) -> Option<Rect> {
    ctx.memory(|m| m.area_rect(egui::Id::unique("quick-actions")))
}
