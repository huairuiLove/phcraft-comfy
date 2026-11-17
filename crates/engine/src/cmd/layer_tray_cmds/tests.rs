use photocraft_color::{ColorMode, PixelFormat, SampleType};
use photocraft_doc::{Document, LayerId};
use photocraft_geom::Rect;
use serde_json::{Value, json};

use crate::Session;

/// Document 0 (`w`×`h`, `depth`) with a red 10×10 square at (4, 6) on layer "paint" above the
/// Background, and document 1 (60×50, 8-bit RGB). Document 0 is active and "paint" is selected.
fn two_docs(w: u32, h: u32, depth: u32) -> (Session, LayerId) {
    let mut s = Session::new();
    s.execute("file.new", json!({"width": w, "height": h, "depth": depth})).unwrap();
    let paint = LayerId(s.execute("layer.new.layer", json!({"name": "paint"})).unwrap()["layer"].as_u64().unwrap());
    s.execute("select.rect", json!({"x": 4, "y": 6, "width": 10, "height": 10})).unwrap();
    s.execute("edit.fill", json!({"color": "#ff0000"})).unwrap();
    s.execute("select.deselect", json!({})).unwrap();
    s.execute("file.new", json!({"width": 60, "height": 50})).unwrap();
    s.set_active(0);
    (s, paint)
}

fn doc(s: &Session, i: usize) -> &Document {
    &s.documents()[i].doc
}

fn placed(r: &Value) -> LayerId {
    LayerId(r["layers"][0].as_u64().unwrap())
}

fn bounds(s: &Session, i: usize, id: LayerId) -> Rect {
    crate::layer_multi_cmds::layer_bounds(doc(s, i).layer(id).unwrap()).unwrap()
}

#[test]
fn stashed_layers_place_into_another_document_in_one_step() {
    let (mut s, paint) = two_docs(40, 30, 8);
    let put = s.execute("layer.tray.put", json!({})).unwrap();
    assert_eq!(put["slot"], 0);
    assert_eq!(put["count"], 1);
    assert_eq!(put["names"], json!(["paint"]));
    assert_eq!(s.layer_tray.len(), 1);
    assert_eq!(s.layer_tray[0].source, doc(&s, 0).name);
    assert_eq!(doc(&s, 0).layers.len(), 2, "the source keeps its layers");

    let before = doc(&s, 1).clone();
    s.set_active(1);
    let r = s.execute("layer.tray.place", json!({})).unwrap();
    assert_eq!(r["slot"], 0, "the newest slot by default");
    let id = placed(&r);
    assert_ne!(id, paint, "fresh ids on every placement");
    let d = doc(&s, 1);
    assert_eq!(d.layers.len(), 2, "above the destination's Background");
    assert_eq!(d.layers[1].name, "paint");
    assert_eq!(bounds(&s, 1, id), Rect::new(25, 20, 35, 30), "different canvas sizes centre (the tab-drop rule)");
    assert_eq!(d.layers[1].surface().unwrap().read_region(Rect::new(26, 21, 27, 22)), vec![1.0, 0.0, 0.0, 1.0], "the pixels came along");
    assert_eq!(s.active().unwrap().selected_layers(), vec![id], "the placed layers are selected");
    // One history step; the tray keeps its slot.
    assert_eq!(s.layer_tray.len(), 1);
    assert!(s.undo());
    assert_eq!(*doc(&s, 1), before);
    assert_eq!(s.layer_tray.len(), 1, "undo takes back the placement, not the stash");
}

#[test]
fn same_size_canvases_keep_the_stashed_position() {
    let (mut s, _) = two_docs(40, 30, 8);
    s.execute("layer.tray.put", json!({})).unwrap();
    s.set_active(1);
    // Make the second canvas the same size as the first.
    s.execute("image.canvasSize", json!({"width": 40, "height": 30})).unwrap();
    let r = s.execute("layer.tray.place", json!({})).unwrap();
    assert_eq!(bounds(&s, 1, placed(&r)), Rect::new(4, 6, 14, 16), "the stashed canvas position");
    let r = s.execute("layer.tray.place", json!({"center": true})).unwrap();
    assert_eq!(bounds(&s, 1, placed(&r)), Rect::new(15, 10, 25, 20), "forced to the canvas centre");
    let r = s.execute("layer.tray.place", json!({"at": [10.0, 40.0]})).unwrap();
    assert_eq!(bounds(&s, 1, placed(&r)), Rect::new(5, 35, 15, 45), "centred on a point");
}

#[test]
fn placing_converts_to_the_destinations_colour_and_depth() {
    let (mut s, _) = two_docs(40, 30, 8);
    s.execute("layer.tray.put", json!({})).unwrap();
    s.set_active(1);
    s.execute("image.mode.grayscale", json!({})).unwrap();
    s.execute("image.mode.bits16", json!({})).unwrap();
    let r = s.execute("layer.tray.place", json!({})).unwrap();
    let id = placed(&r);
    let surf = doc(&s, 1).layer(id).unwrap().surface().unwrap();
    assert_eq!(surf.format(), PixelFormat::new(ColorMode::Grayscale, SampleType::U16, true));
    let px = surf.read_region(Rect::new(26, 21, 27, 22));
    assert!(px[0] > 0.1 && px[0] < 0.9 && px[1] == 1.0, "red as a mid grey, opaque: {px:?}");
}

#[test]
fn groups_and_masks_travel_too() {
    let (mut s, paint) = two_docs(40, 30, 8);
    s.execute("layer.groupLayers", json!({"layers": [paint.0]})).unwrap();
    let group = s.active().unwrap().active_layer.unwrap();
    s.execute("layer.layerMask.revealAll", json!({"layer": group.0})).unwrap();
    s.execute("layer.tray.put", json!({})).unwrap();
    assert_eq!(s.layer_tray[0].doc.layers.len(), 1, "the group is one stashed top-level layer");
    assert!(s.layer_tray[0].doc.layers[0].is_group());
    s.set_active(1);
    let r = s.execute("layer.tray.place", json!({})).unwrap();
    let id = placed(&r);
    let l = doc(&s, 1).layer(id).unwrap();
    assert!(l.is_group(), "the group keeps its children");
    assert!(l.mask.is_some(), "and its mask");
    let children = l.children().unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].name, "paint");
}

#[test]
fn a_slot_outlives_its_source_document() {
    let (mut s, _) = two_docs(40, 30, 8);
    s.execute("layer.tray.put", json!({})).unwrap();
    s.close(0);
    assert_eq!(s.layer_tray.len(), 1);
    let r = s.execute("layer.tray.place", json!({})).unwrap();
    assert_eq!(doc(&s, 0).layer(placed(&r)).unwrap().name, "paint");
}

#[test]
fn two_placements_of_one_slot_never_share_layer_ids() {
    let (mut s, _) = two_docs(40, 30, 8);
    s.execute("layer.tray.put", json!({})).unwrap();
    s.set_active(1);
    let a = placed(&s.execute("layer.tray.place", json!({})).unwrap());
    let b = placed(&s.execute("layer.tray.place", json!({})).unwrap());
    assert_ne!(a, b, "Paste semantics: the slot keeps its copies, the ids are fresh");
    assert_eq!(doc(&s, 1).layers.len(), 3);
}

#[test]
fn the_tray_fills_up_at_sixteen_slots_and_refuses_more() {
    let (mut s, _) = two_docs(40, 30, 8);
    for _ in 0..crate::layer_tray_cmds::TRAY_SLOTS {
        s.execute("layer.tray.put", json!({})).unwrap();
    }
    let e = s.execute("layer.tray.put", json!({})).unwrap_err().to_string();
    assert!(e.contains("full"), "{e}");
    assert_eq!(s.layer_tray.len(), crate::layer_tray_cmds::TRAY_SLOTS, "no silent eviction");
    assert!(!s.is_enabled("layer.tray.put"));
    // Placing still works, and emptying the tray re-enables putting.
    s.set_active(1);
    let r = s.execute("layer.tray.place", json!({})).unwrap();
    assert_eq!(r["slot"], (crate::layer_tray_cmds::TRAY_SLOTS - 1) as u64, "the newest slot");
    s.execute("layer.tray.clear", json!({})).unwrap();
    assert!(s.layer_tray.is_empty());
    assert!(s.is_enabled("layer.tray.put"));
}

#[test]
fn remove_takes_one_slot_and_clear_reports_what_it_took() {
    let (mut s, _) = two_docs(40, 30, 8);
    s.execute("layer.tray.put", json!({})).unwrap();
    s.execute("layer.new.layer", json!({"name": "second"})).unwrap();
    s.execute("layer.tray.put", json!({"layers": [s.active().unwrap().active_layer.unwrap().0]})).unwrap();
    assert_eq!(s.layer_tray.len(), 2);
    let r = s.execute("layer.tray.remove", json!({"slot": 0})).unwrap();
    assert_eq!(r["names"], json!(["paint"]));
    assert_eq!(s.layer_tray.len(), 1);
    assert_eq!(s.layer_tray[0].doc.layers[0].name, "second", "the rest shift down");
    let r = s.execute("layer.tray.clear", json!({})).unwrap();
    assert_eq!(r["removed"], 1);
    assert!(!s.is_enabled("layer.tray.remove") && !s.is_enabled("layer.tray.clear"));
}

#[test]
fn bad_params_and_states_fail_without_changing_anything() {
    let (mut s, _paint) = two_docs(40, 30, 8);
    // Before anything is stashed: place/remove/clear refuse.
    assert!(!s.is_enabled("layer.tray.place"));
    for (cmd, p) in [
        ("layer.tray.place", json!({})),
        ("layer.tray.place", json!({"slot": 0})),
        ("layer.tray.remove", json!({"slot": 0})),
        ("layer.tray.clear", json!({})),
    ] {
        assert!(s.execute(cmd, p.clone()).is_err(), "{cmd} {p}");
    }
    s.execute("layer.tray.put", json!({})).unwrap();
    s.set_active(1);
    for (cmd, p) in [
        ("layer.tray.place", json!({"slot": 1})),
        ("layer.tray.place", json!({"slot": -1})),
        ("layer.tray.place", json!({"slot": "0"})),
        ("layer.tray.place", json!({"slot": 0, "at": [1.0]})),
        ("layer.tray.place", json!({"slot": 0, "at": [f64::MAX, 0.0]})),
        ("layer.tray.remove", json!({})),
        ("layer.tray.remove", json!({"slot": 9})),
        ("layer.tray.put", json!({"layers": "all"})),
        ("layer.tray.put", json!({"layers": [u64::MAX]})),
        ("layer.tray.put", json!({"layers": []})),
    ] {
        let before = (doc(&s, 1).clone(), s.layer_tray.len());
        assert!(s.execute(cmd, p.clone()).is_err(), "{cmd} {p}");
        assert_eq!(s.layer_tray.len(), before.1, "{cmd} {p}: the tray is unchanged");
        assert_eq!(*doc(&s, 1), before.0, "{cmd} {p}: the document is unchanged");
    }
    // An Indexed destination has no layers to place into.
    s.set_active(1);
    s.edit("indexed", |d, _| {
        d.mode = ColorMode::Indexed;
        Ok(())
    })
    .unwrap();
    assert!(s.execute("layer.tray.place", json!({})).is_err());
}

#[test]
fn enabled_gates_name_the_missing_state() {
    let mut s = Session::new();
    assert!(!s.is_enabled("layer.tray.put"), "no document open");
    assert!(!s.is_enabled("layer.tray.place"));
    s.execute("file.new", json!({"width": 8, "height": 8})).unwrap();
    assert!(s.is_enabled("layer.tray.put"), "the Background is an active layer to stash");
    assert!(!s.is_enabled("layer.tray.place"), "the tray is empty");
    s.execute("layer.tray.put", json!({})).unwrap();
    assert!(s.is_enabled("layer.tray.place"));
    s.execute("layer.tray.clear", json!({})).unwrap();
    assert!(!s.is_enabled("layer.tray.place"));
}
