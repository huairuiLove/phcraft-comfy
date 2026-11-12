//! The Layers panel footer's tray button and its popup: layers stashed with `layer.tray.put`
//! from one open document, placed into another with `layer.tray.place`. A touch screen shows one
//! document at a time, so the desktop's drag-a-layer-onto-the-other-document's-tab has nothing
//! to aim at; the tray is the two-step replacement. A click on the button opens the popup, a
//! click on a slot places its layers (the slot stays, like Paste), and the ✕ empties a slot.
//! A layer row dragged onto the button stashes the selection, like the footer's other drops.

use egui::{Popup, PopupCloseBehavior, Sense, SetOpenCommand};
use photocraft_doc::{Document, LayerContent};
use serde_json::{Value, json};

use crate::theme::Tokens;
use crate::{PhotocraftApp, icons, widgets};

/// The tray's footer button: opens the tray popup, takes a dragged layer row (`drag`, the same
/// payload the footer's Delete and New Layer drops take), and says how many slots are held.
pub(super) fn footer_button(app: &mut PhotocraftApp, ui: &mut egui::Ui, actions: &mut Vec<(String, Value)>, drag: Option<(u64, bool)>) {
    let n = app.session.layer_tray.len();
    let tip = tl!("Layer tray: stash layers, then place them in another document");
    let btn = icons::button(ui, "layers", 26.0, false, tip);
    if n > 0 {
        // The count sits on the button's corner, like a badge.
        let t = Tokens::get(ui.ctx());
        let d = 13.0;
        let c = btn.rect.right_top() + egui::vec2(-3.0, 3.0);
        ui.painter().circle_filled(c, d / 2.0, t.accent);
        ui.painter().text(c, egui::Align2::CENTER_CENTER, n.to_string(), egui::FontId::proportional(9.0), t.card);
    }
    actions.extend(super::layer_drag::footer_drop(ui, &btn, drag, "layer.tray.put"));
    let popup = Popup::default_response_id(&btn);
    let open = Popup::is_id_open(ui.ctx(), popup);
    let toggle = btn.clicked().then_some(SetOpenCommand::Bool(!open));
    // Closing on a click outside only: the ✕ removes slot after slot without reopening.
    Popup::menu(&btn).open_memory(toggle).close_behavior(PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
        ui.set_min_width(270.0);
        body(app, ui, actions);
    });
}

/// The popup's contents: one row per slot, newest last, with a thumbnail of the topmost stashed
/// layer, what the tray calls it, and the document it came from.
fn body(app: &mut PhotocraftApp, ui: &mut egui::Ui, actions: &mut Vec<(String, Value)>) {
    ui.horizontal(|ui| {
        ui.strong(tl!("Layer tray"));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button(tl!("Clear")).clicked() && !app.session.layer_tray.is_empty() {
                actions.push(("layer.tray.clear".into(), json!({})));
            }
        });
    });
    if app.session.layer_tray.is_empty() {
        ui.add_space(4.0);
        ui.label(tl!("Drag layers here, or long-press a layer and choose “Add to tray”"));
        ui.add_space(4.0);
        return;
    }
    // The thumbnails need `&mut app` (their cache), which cannot alias the tray's borrow: take
    // the slots out for drawing. A `Document` clone is a pointer copy per layer, not per pixel.
    let rows: Vec<(Document, String)> = app
        .session
        .layer_tray
        .iter()
        .map(|s| ((*s.scratch).clone(), s.source_name.clone()))
        .collect();
    let t = Tokens::get(ui.ctx());
    for (i, (scratch, source_name)) in rows.iter().enumerate() {
        let top = scratch.layers.last();
        let h = 34.0;
        // The row is one click target (like the footer's icon buttons: an allocated rect whose
        // contents are painted, not child widgets — a child would take the pointer hit); the ✕
        // beside it is its own button. A click on the row places the slot, the ✕ empties it.
        let mut place = false;
        ui.horizontal(|ui| {
            let (rect, row) = ui.allocate_exact_size(egui::vec2(ui.available_width() - 26.0, h), Sense::click());
            let p = ui.painter_at(rect);
            if row.hovered() {
                p.rect_filled(rect, t.radius_sm, t.hover);
            }
            // The thumbnail: the topmost stashed layer's, the size of the Layers panel's.
            let cell = egui::Rect::from_min_size(rect.min + egui::vec2(4.0, 2.0), egui::vec2(40.0, h - 4.0));
            let icon = match top.map(|l| &l.content) {
                Some(LayerContent::Group(_)) => Some("folder"),
                Some(LayerContent::Text(_)) => Some("type"),
                Some(LayerContent::Adjustment(_)) => Some("contrast"),
                _ => None,
            };
            if let (Some(l), None) = (top, icon) {
                let (tex, frame) = app.layer_thumb(ui.ctx(), scratch, l);
                let (fitted, uv) = super::layer_row::layer_thumb_fit(cell, frame.width(), frame.height());
                widgets::checker(&p, fitted, 5.0);
                p.image(tex, fitted, uv, egui::Color32::WHITE);
            } else {
                p.rect_filled(cell, 5.0, t.field);
                icons::paint(ui, cell, icon.unwrap_or("contrast"), 16.0, t.icon);
            }
            // What the row says: the layer's name (or how many layers) and where they came from,
            // painted (not Labels) so they cannot take the row's click.
            let label = match scratch.layers.as_slice() {
                [one] => one.name.clone(),
                many => crate::i18n::trn(crate::i18n::current(), many.len() as u64, "{n} layer", "{n} layers").to_string(),
            };
            let text = egui::Rect::from_min_max(egui::pos2(cell.right() + 8.0, rect.min.y), egui::pos2(rect.max.x, rect.max.y));
            let name = p.layout_no_wrap(label, egui::FontId::proportional(13.0), t.text);
            p.with_clip_rect(text).galley(egui::pos2(text.min.x, rect.min.y + 1.0), name, t.text);
            let from = p.layout_no_wrap(source_name.clone(), egui::FontId::proportional(11.0), t.text_dim);
            p.with_clip_rect(text).galley(egui::pos2(text.min.x, rect.max.y - 16.0), from, t.text_dim);
            row.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, tl!("Place from tray")));
            if row.clicked() {
                place = true;
            }
            let x = icons::button(ui, "x", 22.0, false, tl!("Remove from tray"));
            if x.clicked() {
                actions.push(("layer.tray.remove".into(), json!({"slot": i})));
            }
        });
        if place {
            actions.push(("layer.tray.place".into(), json!({"slot": i})));
            ui.close();
        }
        if i + 1 < rows.len() {
            widgets::hairline(ui);
        }
    }
}
