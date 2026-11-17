//! The full-screen exit chip: a small floating button at the canvas's top-left corner that
//! returns to the Standard screen mode, shown in the Touch Interface whenever a full-screen
//! mode is active. An iPad has no Esc and no Tab key — the desktop's two ways out of View ›
//! Screen Mode › Full Screen (shortcuts.rs) — and in the bare full-screen mode the menu bar is
//! hidden too, so without a visible button the artwork would be a trap the finger cannot
//! leave. One tap here does what Esc does there: the View menu's own
//! `view.screenMode.standard`, so the menu's checkmark follows. The button mirrors the
//! quick-actions column's card, at the opposite corner.

use super::{PhotocraftApp, Tokens, icons, json, pos2, vec2};
use egui::{Order, Rect, Stroke, StrokeKind};

/// The button's touch target and the card padding, as in the quick-actions column.
const SIZE: f32 = 40.0;
const PAD: f32 = 4.0;
/// Distance from the canvas's top-left corner.
const MARGIN: f32 = 12.0;

/// The chip, over the canvas's top-left corner. Anchored to `last_canvas_rect`, so it sits
/// below the menu bar in the with-menu-bar mode and at the safe-area edge in the bare one.
pub fn chip(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    if !crate::touch::on(app) || app.ui.view.screen_mode == "standard" {
        return;
    }
    let canvas = app.last_canvas_rect;
    if !canvas.is_positive() {
        return;
    }
    let ctx = ui.ctx().clone();
    let t = Tokens::get(&ctx);
    let w = SIZE + 2.0 * PAD;
    let min = pos2(canvas.left() + MARGIN, canvas.top() + MARGIN);
    let mut go_standard = false;
    egui::Area::new(egui::Id::unique("screen-mode-chip"))
        .order(Order::Foreground)
        .fixed_pos(min)
        .show(&ctx, |ui| {
            let card = Rect::from_min_size(ui.cursor().min, vec2(w, w));
            ui.painter().rect_filled(card, t.radius_lg, t.card.gamma_multiply(0.97));
            ui.painter().rect_stroke(card, t.radius_lg, Stroke::new(1.0, t.card_border), StrokeKind::Inside);
            ui.add_space(PAD);
            // The screen-mode glyph the toolbar's own screen-mode buttons use; the tip is the
            // destination mode, translated from the View menu's catalog entry.
            let button = icons::button(ui, "maximize-2", SIZE, false, tl!("Standard Screen Mode"));
            button.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, tl!("Standard Screen Mode")));
            if button.clicked() {
                go_standard = true;
            }
        });
    if go_standard
        && let Err(e) = crate::menus::invoke(app, &ctx, "view.screenMode.standard", json!({}))
    {
        app.ui.status = e;
        app.ui.status_error = true;
    }
}

/// The chip's card rect last frame (tests). `None` before it first drew, and in the standard
/// mode.
#[cfg(test)]
pub(crate) fn rect(ctx: &egui::Context) -> Option<Rect> {
    ctx.memory(|m| m.area_rect(egui::Id::unique("screen-mode-chip")))
}
