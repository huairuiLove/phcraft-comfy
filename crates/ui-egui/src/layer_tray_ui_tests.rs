//! The Layers footer's layer tray button and popup (panels/layer_tray.rs): the touch answer to
//! moving layers between documents. A click on the button lists the stashed slots (newest
//! first); a click on a row places its layers in the active document, Remove takes the slot out,
//! Clear empties the tray.

use egui_kittest::kittest::Queryable;
use egui_kittest::{Harness, Node};
use serde_json::json;

use crate::PhotocraftApp;

/// Two documents; the tray holds `slots` stashes of distinct layers. Document 1 is active: the
/// Layers panel shows it, and a placement lands there.
fn harness(slots: usize) -> Harness<'static, PhotocraftApp> {
    let mut s = photocraft_engine::Session::new();
    s.execute("file.new", json!({"width": 16, "height": 16})).unwrap();
    s.execute("layer.new.layer", json!({"name": "paint"})).unwrap();
    s.execute("layer.tray.put", json!({})).unwrap();
    for _ in 1..slots {
        s.execute("layer.new.layer", json!({"name": "second"})).unwrap();
        s.execute("layer.tray.put", json!({})).unwrap();
    }
    s.execute("file.new", json!({"width": 32, "height": 32})).unwrap();
    s.set_active(1);
    let mut h = Harness::builder().with_size(egui::vec2(1200.0, 800.0)).with_max_steps(64).build_eframe(|cc| {
        PhotocraftApp::setup_context(&cc.egui_ctx, Default::default());
        PhotocraftApp::new(s, crate::Services::default())
    });
    h.state_mut().session.edit_prefs(|p| p.interface.language = "en".into());
    h.run_steps(4);
    h
}

fn layer_names(h: &Harness<'_, PhotocraftApp>) -> Vec<String> {
    h.state().session.active().unwrap().doc.layers.iter().map(|l| l.name.clone()).collect()
}

/// The tray popup's mode pill labelled `label` — not the quick-actions column's button of the
/// same name, which the touch layout also shows.
fn mode_pill<'a>(h: &'a Harness<'_, PhotocraftApp>, label: &'a str) -> Node<'a> {
    let card = crate::panels::quick_actions::rect(&h.ctx);
    h.query_all_by_label(label)
        .into_iter()
        .find(|n| card.is_none_or(|c| !c.contains(n.rect().center())))
        .expect("the popup's mode pill")
}

#[test]
fn a_click_on_a_tray_row_places_its_layers_in_the_active_document() {
    let mut h = harness(1);
    assert_eq!(layer_names(&h), ["Background"], "the destination starts with its own Background");
    h.get_by_label("Layer Tray").click();
    h.run_steps(2);
    // The slot row: the stashed layer's name and the source document it came from.
    h.query_by_label_contains("From Untitled").expect("the slot row: the stashed name and its source").click();
    h.run_steps(4);
    assert_eq!(layer_names(&h), ["Background", "paint"], "the stashed layer landed above the Background");
    assert_eq!(h.state().session.layer_tray.len(), 1, "placing keeps the slot (Paste semantics)");
}

#[test]
fn remove_takes_one_slot_and_clear_empties_the_tray() {
    let mut h = harness(2);
    h.get_by_label("Layer Tray").click();
    h.run_steps(2);
    // Two rows, newest first: the first Remove drops the "second" slot, the last one "paint".
    let removes: Vec<_> = h.query_all_by_label("Remove").collect();
    assert_eq!(removes.len(), 2, "one Remove per slot");
    removes[0].click();
    h.run_steps(4);
    assert_eq!(h.state().session.layer_tray.len(), 1);
    assert_eq!(h.state().session.layer_tray[0].doc.layers[0].name, "paint", "the oldest slot stayed");
    assert_eq!(layer_names(&h), ["Background"], "nothing was placed");
    // Clear empties what is left.
    h.get_by_label("Clear Tray").click();
    h.run_steps(4);
    assert!(h.state().session.layer_tray.is_empty());
}

/// The rows show what each slot holds: the stashed layer's thumbnail beside its name and source,
/// drawn as the Layers panel draws that layer. A stashed layer lives in a scratch document no tab
/// shows, so without the row drawing it nothing would ever thumbnail it.
#[test]
fn a_tray_row_draws_the_stashed_layers_thumbnail() {
    let mut h = harness(1);
    let stashed = h.state().session.layer_tray[0].doc.layers.last().unwrap().id;
    let key = (stashed, crate::mask_thumbs_ui::THUMB_LAYER);
    assert!(!h.state().thumbs.contains_key(&key), "nothing drew the stashed layer before the popup opened");
    h.get_by_label("Layer Tray").click();
    h.run_steps(2);
    h.query_by_label_contains("From Untitled").expect("the slot row's name and source");
    assert!(h.state().thumbs.contains_key(&key), "the row drew the stashed layer's thumbnail");
    // It frames the stashed document's canvas (the 16×16 document the layers came from).
    assert_eq!(h.state().thumb_frames.get(&stashed).copied(), Some(photocraft_geom::Rect::new(0, 0, 16, 16)));
}

/// A slot holding several layers shows its topmost layer's thumbnail — the layer a placement
/// leaves active — under the "{n} layers" name.
#[test]
fn a_multi_layer_slot_shows_its_topmost_layers_thumbnail() {
    let mut h = harness(1);
    h.state_mut().session.execute("layer.new.layer", serde_json::json!({"name": "lower"})).unwrap();
    h.state_mut().session.execute("layer.new.layer", serde_json::json!({"name": "upper"})).unwrap();
    h.state_mut().session.execute("select.allLayers", serde_json::json!({})).unwrap();
    h.state_mut().session.execute("layer.tray.put", serde_json::json!({})).unwrap();
    h.run_steps(2);
    let slot = &h.state().session.layer_tray[1].doc;
    let (bottom, top) = (slot.layers.first().unwrap().id, slot.layers.last().unwrap().id);
    assert_eq!(slot.layers.last().unwrap().name, "upper", "the slot's topmost layer");
    h.get_by_label("Layer Tray").click();
    h.run_steps(2);
    assert!(h.query_all_by_label_contains("From Untitled").count() >= 2, "both slot rows");
    assert!(h.state().thumbs.contains_key(&(top, crate::mask_thumbs_ui::THUMB_LAYER)), "the topmost layer's thumbnail");
    assert!(!h.state().thumbs.contains_key(&(bottom, crate::mask_thumbs_ui::THUMB_LAYER)), "one thumbnail per slot");
}

/// The Touch Interface harness with one document holding the named layers.
fn touch_harness(layers: &[&str]) -> Harness<'static, PhotocraftApp> {
    let mut s = photocraft_engine::Session::new();
    s.execute("file.new", serde_json::json!({"width": 64, "height": 48})).unwrap();
    for name in layers {
        s.execute("layer.new.layer", serde_json::json!({"name": name})).unwrap();
    }
    let mut h = Harness::builder().with_size(egui::vec2(1200.0, 800.0)).with_max_steps(64).build_eframe(move |cc| {
        PhotocraftApp::setup_context(&cc.egui_ctx, Default::default());
        let mut app = PhotocraftApp::new(s, crate::Services::default());
        app.session.edit_prefs(|p| {
            p.interface.touch_ui = true;
            // The label-based assertions need English whatever a parallel i18n live test did
            // to the global language (the frame re-reads this pref every pass).
            p.interface.language = "en".into();
        });
        app
    });
    h.run_steps(4);
    h
}

/// Press on `from` and drag to `to` through the given waypoints, a step at a time (egui starts a
/// drag once the pressed pointer moves). Leaves the button down at `to`.
fn drag_down(h: &mut Harness<'_, PhotocraftApp>, from: egui::Pos2, to: egui::Pos2) {
    h.hover_at(from);
    h.step();
    h.event(egui::Event::PointerButton { pos: from, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE });
    h.step();
    let steps = 8;
    for i in 1..=steps {
        let p = egui::pos2(from.x + (to.x - from.x) * i as f32 / steps as f32, from.y + (to.y - from.y) * i as f32 / steps as f32);
        h.event(egui::Event::PointerMoved(p));
        h.step();
    }
}

#[test]
fn a_row_dragged_out_summons_the_tray_strip_and_a_drop_stashes_it() {
    let mut h = touch_harness(&["paint"]);
    let row = h.get_by_label("paint").rect().center();
    // No drag, no strip: the footer button is the only exact "Layer Tray".
    assert_eq!(h.query_all_by_label("Layer Tray").count(), 1);
    let canvas = h.state().last_canvas_rect;
    let strip = egui::pos2(canvas.center().x, canvas.top() + 25.0);
    drag_down(&mut h, row, strip);
    assert_eq!(h.query_all_by_label("Layer Tray").count(), 2, "the strip is up during the drag");
    h.event(egui::Event::PointerButton { pos: strip, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE });
    h.step();
    h.run_steps(4);
    assert_eq!(h.state().session.layer_tray.len(), 1, "the drop stashed the row");
    assert_eq!(h.state().session.layer_tray[0].doc.layers[0].name, "paint");
    assert_eq!(layer_names(&h), ["Background", "paint"], "stashing copies, it does not move");
    assert!(h.state().ui.status.contains("tray"), "the status tells the user how to place it");
    // And the strip is gone with the drag (the status message also mentions the tray by name,
    // so exact-label matches only).
    assert_eq!(h.query_all_by_label("Layer Tray").count(), 1);
}

#[test]
fn a_strip_drop_of_one_selected_layer_carries_the_selection() {
    let mut h = touch_harness(&["paint", "second"]);
    h.state_mut().session.execute("select.allLayers", serde_json::json!({})).unwrap();
    h.run_steps(2);
    let row = h.get_by_label("paint").rect().center();
    let canvas = h.state().last_canvas_rect;
    let strip = egui::pos2(canvas.center().x, canvas.top() + 25.0);
    drag_down(&mut h, row, strip);
    h.event(egui::Event::PointerButton { pos: strip, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE });
    h.run_steps(4);
    assert_eq!(h.state().session.layer_tray.len(), 1);
    assert_eq!(h.state().session.layer_tray[0].doc.layers.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), ["paint", "second"], "the whole selection went into the slot");
}

/// The tray popup's Copy | Cut toggle, switched by a tap (iOS has no right-click; a mode that
/// needed one would be unreachable on a finger).
#[test]
fn the_popups_mode_toggle_switches_by_tap() {
    let mut h = harness(1);
    h.get_by_label("Layer Tray").click();
    h.run_steps(2);
    assert!(!h.state().ui.tray_cut, "Copy is the default");
    h.get_by_label("Cut").click();
    h.run_steps(2);
    assert!(h.state().ui.tray_cut, "a tap switches to Cut");
    h.get_by_label("Copy").click();
    h.run_steps(2);
    assert!(!h.state().ui.tray_cut, "and back to Copy");
}

#[test]
fn cut_mode_stashing_via_the_strip_removes_the_layers_from_the_source() {
    let mut h = touch_harness(&["paint"]);
    // Switch the mode in the popup first (a tap, as a finger does).
    h.get_by_label("Layer Tray").click();
    h.run_steps(2);
    mode_pill(&h, "Cut").click();
    h.run_steps(2);
    // Close the popup: a click on a layer row (outside it).
    h.get_by_label("paint").click();
    h.run_steps(2);
    assert!(!egui::Popup::is_any_open(&h.ctx), "the popup closed");
    // Drag the row onto the strip.
    let row = h.get_by_label("paint").rect().center();
    let canvas = h.state().last_canvas_rect;
    let strip = egui::pos2(canvas.center().x, canvas.top() + 25.0);
    drag_down(&mut h, row, strip);
    h.event(egui::Event::PointerButton { pos: strip, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE });
    h.run_steps(4);
    assert_eq!(h.state().session.layer_tray.len(), 1, "the slot holds the layers");
    assert_eq!(h.state().session.layer_tray[0].doc.layers[0].name, "paint");
    assert_eq!(layer_names(&h), ["Background"], "Cut removed them from the source document");
    assert!(h.state().ui.status.contains("Cut") || h.state().ui.status.contains("剪切"), "the status says it was a cut");
}

#[test]
fn cut_mode_placing_by_a_slot_tap_empties_the_slot() {
    let mut h = harness(1);
    // Switch the mode in the popup, then place by tapping the slot row.
    h.get_by_label("Layer Tray").click();
    h.run_steps(2);
    mode_pill(&h, "Cut").click();
    h.run_steps(2);
    h.query_by_label_contains("From Untitled").expect("the slot row").click();
    h.run_steps(4);
    assert_eq!(layer_names(&h), ["Background", "paint"], "the slot's layers were placed");
    assert!(h.state().session.layer_tray.is_empty(), "Cut emptied the slot");
}

/// The footer-button drop (and with it the action-dispatch path the layer menu's "Add to Tray"
/// shares) honours the mode too.
#[test]
fn cut_mode_stashing_via_the_footer_drop_removes_the_layers_too() {
    let mut h = touch_harness(&["paint"]);
    h.get_by_label("Layer Tray").click();
    h.run_steps(2);
    mode_pill(&h, "Cut").click();
    h.run_steps(2);
    h.get_by_label("paint").click();
    h.run_steps(2);
    let row = h.get_by_label("paint").rect().center();
    let footer = h.get_by_label("Layer Tray").rect().center();
    drag_down(&mut h, row, footer);
    h.event(egui::Event::PointerButton { pos: footer, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE });
    h.run_steps(4);
    assert_eq!(h.state().session.layer_tray.len(), 1, "the footer drop stashed the row");
    assert_eq!(layer_names(&h), ["Background"], "Cut removed it from the source document");
}

#[test]
fn a_slot_row_dragged_onto_the_canvas_places_its_layers_there() {
    let mut s = photocraft_engine::Session::new();
    s.execute("file.new", serde_json::json!({"width": 64, "height": 48})).unwrap();
    s.execute("layer.new.layer", serde_json::json!({"name": "paint"})).unwrap();
    s.execute("layer.tray.put", serde_json::json!({})).unwrap();
    let mut h = Harness::builder().with_size(egui::vec2(1200.0, 800.0)).with_max_steps(64).build_eframe(move |cc| {
        PhotocraftApp::setup_context(&cc.egui_ctx, Default::default());
        PhotocraftApp::new(s, crate::Services::default())
    });
    h.state_mut().session.edit_prefs(|p| p.interface.language = "en".into());
    h.run_steps(4);
    assert_eq!(layer_names(&h), ["Background", "paint"], "the stash copies: the layer stays in its document");
    h.get_by_label("Layer Tray").click();
    h.run_steps(2);
    let row = h.query_by_label_contains("From Untitled").expect("the slot row in the popup").rect().center();
    let canvas = h.state().last_canvas_rect;
    drag_down(&mut h, row, canvas.center());
    h.event(egui::Event::PointerButton { pos: canvas.center(), button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE });
    h.run_steps(4);
    // The stashed original stayed, and the placement added the slot's copy of it.
    assert_eq!(layer_names(&h), ["Background", "paint", "paint"], "the slot's layers landed on the canvas");
    assert_eq!(h.state().session.layer_tray.len(), 1, "placing keeps the slot");
}
