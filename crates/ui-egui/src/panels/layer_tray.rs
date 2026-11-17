//! The Layers footer's layer tray button, the touch answer to moving layers between documents
//! (engine `layer.tray.*`): a touchscreen shows one document at a time, so the desktop's
//! drag-a-layer-onto-the-other-document's-tab has nothing to aim at. Stash here, switch
//! documents, take them out there. Three ways in:
//!
//! - A layer row dragged out summons a floating tray strip over the canvas's top edge
//!   ([`drag_strip`], Touch Interface only) — release the row on it to stash.
//! - The same drop on the footer's tray button ([`footer_button`]) stashes, as Delete and New
//!   Layer take footer drops.
//! - The button's popup lists the stashed slots, each a row with the stashed layer's thumbnail
//!   and name over the document it came from: a click places a slot's layers, ✕ removes it,
//!   Clear empties the tray; a slot row dragged out onto the canvas places its layers centred
//!   on that point. "Add to Tray" in a layer's long-press menu stashes the selection.
//!
//! Every interaction is a tap or a drag — iPadOS has no right-click, and the shell's long-press
//! (the synthesized right-click) is only needed where the app itself has context menus (the
//! layer rows); the tray keeps to gestures a finger can do directly.
//!
//! The popup's Copy | Cut toggle (`ui.tray_cut`) picks the tray's semantics: Copy keeps the
//! source layers on stash and keeps the slot on place (the desktop drag's copy); Cut removes
//! the stashed layers from the source on stash and empties the slot on place — move semantics,
//! Photoshop's cut-and-paste. [`run_put`] and [`run_place`] compose the engine commands; the
//! Layers panel's action dispatch routes the menu and footer paths through them too.

use super::layer_drag::dropped_on;
use super::{PhotocraftApp, Tokens, Value, icons, json};
use egui::{Align2, Color32, CornerRadius, FontId, Rect, Sense, Stroke, StrokeKind, Vec2, pos2, vec2};

/// The drag of a slot row out of the tray popup: the slot's index.
fn slot_drag_key() -> egui::Id {
    egui::Id::unique("layer-tray-slot-drag")
}

/// A tray row's height: a 34 px thumbnail and two lines of text inside a finger's tap target.
const SLOT_ROW_H: f32 = 44.0;

/// The ✕ target that removes a slot, at the row's right end.
const SLOT_REMOVE: f32 = 26.0;

/// One line of a tray row: `text` in `font`, elided with "…" at `width` so a long layer name
/// stops before the ✕ instead of widening the popup.
fn elided(painter: &egui::Painter, text: String, font: FontId, color: Color32, width: f32) -> std::sync::Arc<egui::Galley> {
    let format = egui::TextFormat { font_id: font, color, ..Default::default() };
    let mut job = egui::text::LayoutJob::single_section(text, format);
    job.wrap = egui::text::TextWrapping::truncate_at_width(width.max(1.0));
    painter.layout_job(job)
}

/// A slot's ghost label: the stashed layer's name, or "{n} layers".
fn slot_label(slot: &photocraft_engine::layer_tray_cmds::TraySlot) -> String {
    match slot.doc.layers.as_slice() {
        [one] => one.name.clone(),
        many => crate::i18n::trn(crate::i18n::current(), many.len() as u64, "{n} layer", "{n} layers"),
    }
}

/// The stash payload of a drop of the dragged layer `dragged`: the whole selection when it is
/// one of the selected, else the dragged layer alone (the footer_drop commands' shape).
fn stash_payload(app: &PhotocraftApp, dragged: u64) -> Value {
    let in_selection = app.session.active().is_some_and(|st| st.selected_layers().iter().any(|s| s.0 == dragged));
    if in_selection {
        json!({})
    } else {
        json!({"layers": [dragged]})
    }
}

/// Run the tray's stash (`layer.tray.put`) in the popup's current mode. Copy just stashes;
/// Cut also deletes the stashed layers from the source document (one undo step — undoing the
/// delete brings the layers back, and the slot keeps its copies, as a real cut's clipboard
/// would). The status says what to do next (or what went wrong).
pub(super) fn run_put(app: &mut PhotocraftApp, payload: &Value) {
    let cut = app.ui.tray_cut;
    // The delete command's shape mirrors the stash's: the explicit one layer, or the selection.
    let delete = match payload.get("layers").and_then(Value::as_array).map(|a| a.as_slice()) {
        Some([one]) => json!({"layer": one.as_u64().unwrap_or(u64::MAX)}),
        _ => json!({}),
    };
    app.reorder_preview = None;
    match app.run("layer.tray.put", payload.clone()) {
        Ok(_) => {
            if cut
                && let Err(e) = app.run("layer.delete", delete)
            {
                // The stash itself succeeded; only the source cleanup failed (cutting the last
                // layer of a document is refused, as in Photoshop). Say so and keep the slot.
                app.ui.status = e;
                app.ui.status_error = true;
                return;
            }
            app.ui.status = if cut {
                tl!("Cut to the tray. Switch documents, then tap the Layer Tray button to place it").into()
            } else {
                tl!("Added to the tray. Switch documents, then tap the Layer Tray button to place it").into()
            };
        }
        Err(e) => {
            app.ui.status = e;
            app.ui.status_error = true;
        }
    }
}

/// Run a placement (`layer.tray.place`) in the popup's current mode. Copy keeps the slot (Paste
/// semantics); Cut empties it after a successful place — the layers have moved, and dragging
/// the slot out again would duplicate what the user just put away. Undo removes the placed
/// layers but cannot bring the slot back (the tray keeps no history), like a consumed cut.
pub(super) fn run_place(app: &mut PhotocraftApp, params: Value) {
    match app.run("layer.tray.place", params.clone()) {
        Ok(_) => {
            if app.ui.tray_cut
                && let Err(e) = app.run("layer.tray.remove", json!({"slot": params.get("slot").cloned().unwrap_or(Value::Null)}))
            {
                app.ui.status = e;
                app.ui.status_error = true;
            }
        }
        Err(e) => {
            app.ui.status = e;
            app.ui.status_error = true;
        }
    }
}

/// One slot's row in the tray popup: the stashed layers' thumbnail — the topmost layer's, the
/// one a placement leaves active — the slot's name (or "{n} layers") over the document it came
/// from, and ✕ to remove the slot. A tap places the slot and a drag off the row places its
/// layers on the canvas; ✕ takes its own clicks, so a removal never places anything. Returns the
/// row's response and whether ✕ was clicked.
fn slot_row(app: &mut PhotocraftApp, ui: &mut egui::Ui, index: usize, enabled: bool) -> (egui::Response, bool) {
    let ctx = ui.ctx().clone();
    let t = Tokens::get(&ctx);
    let (rect, row) = ui.allocate_exact_size(vec2(ui.available_width(), SLOT_ROW_H), Sense::click_and_drag());
    // The stashed document belongs to no open document, so its thumbnail is drawn here straight
    // from the slot, the thumbnail caches borrowed apart from the session that holds it.
    let PhotocraftApp { session, thumbs, thumb_frames, .. } = app;
    let Some(slot) = session.layer_tray.get(index) else { return (row, false) };
    let clip = session.prefs().interface.clip_thumbnails_to_layer_bounds;
    let name = slot_label(slot);
    let from = if slot.source.is_empty() { String::new() } else { crate::i18n::fmt(tl!("From {doc}"), &[("doc", &slot.source)]) };
    let label = if from.is_empty() { name.clone() } else { format!("{name}\n{from}") };
    row.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, label.clone()));
    // The row's tap-or-drag highlight, as a Layers panel row's.
    if enabled && row.hovered() {
        ui.painter().rect_filled(rect, t.radius, t.hover.gamma_multiply(0.5));
        ui.painter().rect_filled(rect, t.radius, t.accent.gamma_multiply(0.12));
    }
    // ✕: remove the slot. Its own target, so the tap that removes does not also place.
    let remove_rect = Rect::from_center_size(pos2(rect.right() - 4.0 - SLOT_REMOVE / 2.0, rect.center().y), Vec2::splat(SLOT_REMOVE));
    let remove = ui.interact(remove_rect, ui.scope_id().with(("layer-tray-remove", index)), Sense::click());
    remove.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, tl!("Remove")));
    if remove.hovered() {
        ui.painter().rect_filled(remove_rect, t.radius_sm, t.hover);
    }
    icons::paint(ui, remove_rect, "x", 12.0, if remove.hovered() { t.text } else { t.text_dim });
    // The thumbnail, as the Layers panel draws a layer's: pixels over a checkerboard, a "T" for
    // type, a folder for a group, and so on.
    let side = SLOT_ROW_H - 10.0;
    let thumb = Rect::from_min_size(pos2(rect.left() + 5.0, rect.center().y - side / 2.0), Vec2::splat(side));
    if let Some(layer) = slot.doc.layers.last() {
        super::layer_row::paint_layer_thumb(thumbs, thumb_frames, clip, None, &ctx, ui, &slot.doc, layer, thumb, false);
    }
    // The name over its source document, elided where the ✕ begins.
    let left = thumb.right() + 7.0;
    let width = remove_rect.left() - 8.0 - left;
    let name_color = if enabled { t.text } else { t.text_dim };
    let name_galley = elided(ui.painter(), name, crate::theme::medium(12.5), name_color, width);
    let from_galley = (!from.is_empty()).then(|| elided(ui.painter(), from, FontId::proportional(10.5), t.text_dim, width));
    let name_h = name_galley.size().y;
    let gap = 1.0;
    let total = name_h + from_galley.as_ref().map_or(0.0, |g| g.size().y + gap);
    let top = rect.center().y - total / 2.0;
    ui.painter().galley(pos2(left, top), name_galley, name_color);
    if let Some(galley) = from_galley {
        ui.painter().galley(pos2(left, top + name_h + gap), galley, t.text_dim);
    }
    (row, remove.clicked())
}

/// The footer button: the tray's popup (with its Copy | Cut mode toggle), its count badge, and
/// the drop that stashes a dragged layer row (`footer_drag`, as Delete and New Layer take
/// drops).
pub(super) fn footer_button(app: &mut PhotocraftApp, ui: &mut egui::Ui, footer_drag: Option<(u64, bool)>, actions: &mut Vec<(String, Value)>) {
    let ctx = ui.ctx().clone();
    let count = app.session.layer_tray.len();
    let tip = tl!("Layer Tray");
    let button = icons::button(ui, "layers", icons::FOOTER_SIZE, false, tip);
    button.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, tip));
    if count > 0 {
        // The count badge: a non-empty tray is one click away, an empty one reads as idle.
        let t = Tokens::get(ui.ctx());
        let digits = count.to_string();
        let badge = Rect::from_min_size(pos2(button.rect.right() - 4.0 - digits.len() as f32 * 5.5, button.rect.top() - 3.0), vec2(4.0 + digits.len() as f32 * 5.5, 12.0));
        ui.painter().rect_filled(badge, 5.0, t.accent);
        ui.painter().text(badge.center(), Align2::CENTER_CENTER, digits, FontId::monospace(9.0), Color32::WHITE);
    }
    // A dragged layer row released here stashes it (the selection when the dragged layer is one
    // of the selected, else the dragged layer alone) — the footer_drop commands' shape.
    if let Some((layer, _)) = footer_drag
        && dropped_on(ui, &button)
    {
        actions.push(("layer.tray.put".into(), stash_payload(app, layer)));
    }
    let mut place = None;
    let mut remove = None;
    let mut clear = false;
    egui::Popup::from_toggle_button_response(&button)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            // One fixed width for the popup: every row's thumbnail, name and ✕ line up, and a
            // long layer name is elided rather than widening the popup past the screen.
            ui.set_width(250.0);
            ui.horizontal(|ui| {
                ui.strong(tl!("Layer Tray"));
                if ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| ui.button(tl!("Clear Tray"))).inner.clicked() {
                    clear = true;
                }
            });
            mode_toggle(app, ui);
            ui.separator();
            if app.session.layer_tray.is_empty() {
                ui.label(
                    egui::RichText::new(tl!("The tray is empty: drag layers onto the button, or use Add to Tray in a layer's menu"))
                        .small()
                        .weak(),
                );
                return;
            }
            let enabled = app.session.active().is_some();
            // Newest first: what was just stashed is the most likely to be placed.
            for index in (0..app.session.layer_tray.len()).rev() {
                let (row, remove_clicked) = slot_row(app, ui, index, enabled);
                // A tap places; the release of a slot drag elsewhere would also read as a click
                // (egui counts it from the press's widget), so a slot drag suppresses it.
                if enabled && !remove_clicked && row.clicked() && ctx.data(|d| d.get_temp::<usize>(slot_drag_key())).is_none() {
                    place = Some(index);
                }
                // Or the slot row is dragged out and released over the canvas: the layers land
                // centred on that point, and the popup stays open for the next slot.
                if enabled && row.drag_started() {
                    ctx.data_mut(|d| d.insert_temp(slot_drag_key(), index));
                }
                if remove_clicked {
                    remove = Some(index);
                }
                if index > 0 {
                    // Air between slots: every row is its own thumbnail, name and source.
                    ui.add_space(2.0);
                }
            }
        });
    if let (Some(slot), Some(p), released) = (ctx.data(|d| d.get_temp::<usize>(slot_drag_key())), ctx.input(|i| i.pointer.interact_pos()), ctx.input(|i| i.pointer.any_released())) {
        let ghost = app.session.layer_tray.get(slot).map(slot_label);
        if let Some(label) = ghost.as_deref() {
            crate::layer_transfer::ghost(&ctx, p, label);
        }
        if released {
            let at = app.last_canvas_rect.contains(p)
                .then_some(crate::canvas::ViewXform::active(app).map(|xf| xf.to_doc(p)))
                .flatten();
            if let Some(at) = at {
                run_place(app, json!({"slot": slot, "at": [at[0].round(), at[1].round()]}));
            }
            ctx.data_mut(|d| d.remove::<usize>(slot_drag_key()));
        }
    }
    if let Some(slot) = place {
        run_place(app, json!({"slot": slot}));
    }
    if let Some(slot) = remove {
        let _ = app.run("layer.tray.remove", json!({"slot": slot}));
    }
    if clear {
        let _ = app.run("layer.tray.clear", json!({}));
    }
}

/// The tray's Copy | Cut mode toggle (`ui.tray_cut`): two tappable pills, the selected one
/// highlighted — a tap each, no modifier keys, nothing a finger cannot do. Copy keeps the
/// stashed layers in their document and keeps placed slots; Cut removes both (move semantics).
fn mode_toggle(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    let cut = app.ui.tray_cut;
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(tl!("Mode")).small().weak());
        for (label, this_cut) in [("Copy", false), ("Cut", true)] {
            if crate::widgets::pill_tab(ui, label, cut == this_cut).clicked() {
                app.ui.tray_cut = this_cut;
            }
        }
    });
    ui.label(
        egui::RichText::new(if cut { tl!("Cut: stashing removes the layers here, placing empties the slot") } else { tl!("Copy: stashing keeps the layers here, placing keeps the slot") })
            .small()
            .weak(),
    );
}

/// While a Layers-panel row drag is under way, the tray announces itself: a strip floating over
/// the canvas's top edge, reading "Layer Tray" (with the slot count) until the dragged row is
/// over it — then "Release to add to the tray" (Copy) or "Release to cut to the tray" (Cut,
/// the popup's mode; the strip says which before the drop). A drop stashes ([`run_put`]);
/// switch documents and take the layers out there. The touch answer to the footer button,
/// which needs no aiming but is easy to miss mid-drag; the drop is pointer math (the row owns
/// the drag, so the strip takes no input). Touch Interface only: with a mouse the footer
/// button takes the drop, and a bar over the canvas would sit in the way of the artwork.
///
/// Runs from `dock_area` before the panels draw, so on the release frame the drag is still in
/// the context's data — the Layers panel clears it later in the same pass.
pub(super) fn drag_strip(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    if !crate::touch::on(app) {
        return;
    }
    let ctx = ui.ctx().clone();
    let key = egui::Id::unique("layer-drag");
    let Some(dragged) = ctx.data(|d| d.get_temp::<u64>(key)) else { return };
    let (down, released, pointer) = ctx.input(|i| (i.pointer.primary_down(), i.pointer.any_released(), i.pointer.interact_pos()));
    if !down && !released {
        // A drag that lost its pointer (its panel never saw the release): retire it, or the
        // strip would hang over the canvas for good.
        ctx.data_mut(|d| d.remove::<u64>(key));
        return;
    }
    let canvas = app.last_canvas_rect;
    if !canvas.is_positive() {
        return;
    }
    let t = Tokens::get(&ctx);
    let count = app.session.layer_tray.len();
    let icon = 14.0;
    let digits = count.to_string();
    let pill = (count > 0).then_some(8.0 + digits.len() as f32 * 5.5);
    let cut = app.ui.tray_cut;
    let idle = tl!("Layer Tray").to_string();
    let hint = if cut { tl!("Release to cut to the tray").to_string() } else { tl!("Release to add to the tray").to_string() };
    let font = FontId::proportional(12.5);
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("layer-tray-strip")));
    let label_w = painter.layout_no_wrap(idle.clone(), font.clone(), t.text).size().x.max(painter.layout_no_wrap(hint.clone(), font.clone(), t.text).size().x);
    // The mode mark: scissors when the drop will cut, no mark for the plain copy.
    let mode_mark = cut.then(|| tl!("Cut").to_string());
    let mark_w = mode_mark.as_ref().map_or(0.0, |m| painter.layout_no_wrap(m.clone(), FontId::proportional(10.0), t.text).size().x + 10.0);
    let w = 10.0 + icon + 6.0 + label_w + pill.map_or(0.0, |p| p + 6.0) + mark_w + 10.0;
    let strip = Rect::from_min_size(pos2(canvas.center().x - w / 2.0, canvas.top() + 10.0), vec2(w, 30.0));
    // A forgiving hit area: a moving finger overshoots.
    let hit = strip.expand(14.0).intersect(canvas);
    let over = pointer.is_some_and(|p| hit.contains(p));
    egui::Area::new(egui::Id::unique("layer-tray-strip"))
        .order(egui::Order::Foreground)
        .fixed_pos(strip.min)
        .interactable(false)
        .show(&ctx, |ui| {
            let resp = ui.interact(strip, ui.scope_id().with("strip"), Sense::hover());
            resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, tl!("Layer Tray")));
            let p = ui.painter();
            let radius = CornerRadius::same(15);
            p.rect_filled(strip, radius, if over { t.accent } else { t.card.gamma_multiply(0.97) });
            p.rect_stroke(strip, radius, Stroke::new(if over { 1.5 } else { 1.0 }, if over { t.accent_text } else { t.card_border }), StrokeKind::Inside);
            let tint = if over { t.accent_text } else { t.accent };
            icons::paint(ui, Rect::from_center_size(pos2(strip.left() + 10.0 + icon / 2.0, strip.center().y), vec2(icon, icon)), "layers", icon, tint);
            let (text, color) = if over { (&hint, t.accent_text) } else { (&idle, t.text) };
            let galley = painter.layout_no_wrap(text.clone(), font, color);
            let at = pos2(strip.left() + 10.0 + icon + 6.0, strip.center().y - galley.size().y / 2.0);
            ui.painter().galley(at, galley, color);
            let mut right = strip.right() - 10.0;
            if let Some(pill_w) = pill {
                let pr = Rect::from_center_size(pos2(right - pill_w / 2.0, strip.center().y), vec2(pill_w, 13.0));
                p.rect_filled(pr, 6.5, if over { t.accent_text } else { t.accent });
                p.text(pr.center(), Align2::CENTER_CENTER, digits.clone(), FontId::monospace(9.0), if over { t.accent } else { Color32::WHITE });
                right -= pill_w + 6.0;
            }
            if let Some(mark) = &mode_mark {
                let g = painter.layout_no_wrap(mark.clone(), FontId::proportional(10.0), if over { t.accent_text } else { t.text_dim });
                let mr = Rect::from_min_max(pos2(right - g.size().x - 8.0, strip.center().y - 8.0), pos2(right, strip.center().y + 8.0));
                p.rect_filled(mr, 8.0, if over { Color32::BLACK.gamma_multiply(0.15) } else { t.hover.gamma_multiply(0.5) });
                ui.painter().galley(pos2(mr.left() + 4.0, mr.center().y - g.size().y / 2.0), g, if over { t.accent_text } else { t.text_dim });
            }
        });
    if released
        && let Some(p) = pointer
        && hit.contains(p)
    {
        run_put(app, &stash_payload(app, dragged));
    }
}
