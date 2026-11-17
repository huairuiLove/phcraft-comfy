//! The touch quick-actions column (panels/quick_actions.rs): Undo, Redo, Copy, Cut, Paste at
//! the canvas's top-right corner, left of the end dock — taps only, dispatching the engine's
//! own edit commands.

use egui_kittest::kittest::Queryable;
use egui_kittest::{Harness, Node};
use serde_json::json;

use crate::PhotocraftApp;

fn touch_harness() -> Harness<'static, PhotocraftApp> {
    let mut s = photocraft_engine::Session::new();
    s.execute("file.new", json!({"width": 64, "height": 48})).unwrap();
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

fn layer_names(h: &Harness<'_, PhotocraftApp>) -> Vec<String> {
    h.state().session.active().unwrap().doc.layers.iter().map(|l| l.name.clone()).collect()
}

/// The column's button labelled `label`, the one inside the column's card (the Edit menu carries
/// the same names).
fn column_button<'a>(h: &'a Harness<'_, PhotocraftApp>, label: &'a str) -> Option<Node<'a>> {
    let card = crate::panels::quick_actions::rect(&h.ctx).expect("the column drew");
    h.query_all_by_label(label).into_iter().find(|n| card.contains(n.rect().center()))
}

#[test]
fn the_column_sits_at_the_canvas_top_right_and_only_with_the_touch_interface() {
    let h = touch_harness();
    let card = crate::panels::quick_actions::rect(&h.ctx).expect("the column drew");
    let canvas = h.state().last_canvas_rect;
    let top_right = egui::pos2(canvas.right(), canvas.top());
    assert!(card.right() <= canvas.right() && card.left() >= canvas.right() - 80.0, "the column hugs the canvas's right edge");
    assert!(card.top() >= canvas.top() && card.top() <= canvas.top() + 40.0, "and its top");
    assert!(card.right() < top_right.x - 4.0, "inset from the corner");
    for label in ["Undo", "Redo", "Copy", "Cut", "Paste"] {
        assert!(column_button(&h, label).is_some(), "{label} is in the column");
    }
    // Without the Touch Interface there is no column (the desktop keeps its shortcuts).
    let mut s = photocraft_engine::Session::new();
    s.execute("file.new", json!({"width": 64, "height": 48})).unwrap();
    let mut h = Harness::builder().with_size(egui::vec2(1200.0, 800.0)).with_max_steps(64).build_eframe(move |cc| {
        PhotocraftApp::setup_context(&cc.egui_ctx, Default::default());
        PhotocraftApp::new(s, crate::Services::default())
    });
    h.run_steps(4);
    assert!(crate::panels::quick_actions::rect(&h.ctx).is_none(), "no column in the pointer-first layout");
}

#[test]
fn a_tap_on_undo_and_redo_round_trips_a_new_layer() {
    let mut h = touch_harness();
    h.state_mut().session.execute("layer.new.layer", json!({"name": "extra"})).unwrap();
    h.run_steps(2);
    assert_eq!(layer_names(&h), ["Background", "extra"]);
    column_button(&h, "Undo").expect("Undo").click();
    h.run_steps(3);
    assert_eq!(layer_names(&h), ["Background"], "the column's Undo undid the new layer");
    column_button(&h, "Redo").expect("Redo").click();
    h.run_steps(3);
    assert_eq!(layer_names(&h), ["Background", "extra"], "and Redo brought it back");
}

/// Copy then Paste through the column: the selection's pixels land as a new layer, with the
/// greyed-out Paste enabled only after the copy — the Edit menu's own enablement.
/// Copy then Paste through the column: the active layer's pixels land as a new layer, with
/// Paste greyed until the copy — the Edit menu's own enablement. (The layer copied is the
/// white Background: a fresh empty layer holds no pixels to copy.)
#[test]
fn copy_then_paste_through_the_column_lands_a_new_layer() {
    let mut h = touch_harness();
    h.state_mut().session.execute("layer.new.layer", json!({"name": "paint"})).unwrap();
    h.state_mut().session.execute("layer.select", json!({"name": "Background"})).unwrap();
    h.run_steps(2);
    assert!(!h.state().session.is_enabled("edit.paste"), "Paste is greyed with an empty clipboard");
    column_button(&h, "Copy").expect("Copy").click();
    h.run_steps(3);
    assert!(h.state().session.clipboard.is_some(), "the copy filled the session clipboard");
    assert!(h.state().session.is_enabled("edit.paste"), "Paste is enabled after the copy");
    column_button(&h, "Paste").expect("Paste").click();
    h.run_steps(3);
    assert_eq!(layer_names(&h).len(), 3, "the paste landed the copy as a new layer");
    // And the column's own Undo takes it back off.
    column_button(&h, "Undo").expect("Undo").click();
    h.run_steps(3);
    assert_eq!(layer_names(&h).len(), 2, "Undo removed the pasted layer");
}
