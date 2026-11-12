//! The Layers footer's tray button: layers stashed from one document (`layer.tray.put`) list in
//! its popup, and a click on a slot places them into the active document (`layer.tray.place`) —
//! the touch answer to dragging a layer onto another document's tab. The ✕ on a row empties the
//! slot without closing the popup.

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

use crate::PhotocraftApp;

/// Two documents: the first (active) with a layer "paint", the second 60×50 for placements.
fn panel() -> Harness<'static, PhotocraftApp> {
    let mut s = photocraft_engine::Session::new();
    s.execute("file.new", json!({"width": 40, "height": 30})).unwrap();
    s.execute("layer.new.layer", json!({"name": "paint"})).unwrap();
    s.execute("file.new", json!({"width": 60, "height": 50})).unwrap();
    s.set_active(0);
    let mut h = Harness::builder().with_size(egui::vec2(1200.0, 800.0)).with_max_steps(64).build_eframe(|cc| {
        PhotocraftApp::setup_context(&cc.egui_ctx, Default::default());
        PhotocraftApp::new(s, crate::Services::default())
    });
    h.run_steps(4);
    h
}

#[test]
fn a_stashed_layer_places_into_the_active_document_from_the_tray_popup() {
    let mut h = panel();
    h.state_mut().run("layer.tray.put", json!({})).unwrap();
    // The touch flow: switch to the other document, then open the tray.
    h.state_mut().session.set_active(1);
    h.run_steps(2);
    h.get_by_label("Add to Tray").click();
    h.run_steps(2);
    h.get_by_label("Place from tray").click();
    h.run_steps(2);
    let st = h.state().session.active().unwrap();
    assert_eq!(st.doc.layers.len(), 2, "the stashed layer landed in the destination");
    assert_eq!(st.doc.layers[1].name, "paint");
    assert_eq!(st.active_layer, Some(st.doc.layers[1].id), "the copy is the active layer");
    assert_eq!(h.state().session.layer_tray.len(), 1, "placing keeps the slot, like Paste");
}

#[test]
fn the_x_on_a_row_empties_the_slot() {
    let mut h = panel();
    h.state_mut().run("layer.tray.put", json!({})).unwrap();
    h.get_by_label("Add to Tray").click();
    h.run_steps(2);
    h.get_by_label("Remove from tray").click();
    h.run_steps(2);
    assert!(h.state().session.layer_tray.is_empty());
}
