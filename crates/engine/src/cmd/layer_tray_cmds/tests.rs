use photocraft_color::{ColorMode, PixelFormat, SampleType};
use photocraft_doc::{Document, LayerContent, LayerId};
use photocraft_geom::Rect;
use serde_json::json;

use crate::Session;

/// Document 0 (`w`×`h`) with a red 10×10 square at (4, 6) on layer "paint" above the Background,
/// and document 1 (60×50, 8-bit RGB). Document 0 is active with "paint" selected.
fn two_docs(w: u32, h: u32) -> (Session, LayerId) {
    let mut s = Session::new();
    s.execute("file.new", json!({"width": w, "height": h})).unwrap();
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

fn bounds(s: &Session, i: usize, id: LayerId) -> Rect {
    crate::layer_multi_cmds::layer_bounds(doc(s, i).layer(id).unwrap()).unwrap()
}

fn placed(r: &serde_json::Value) -> LayerId {
    LayerId(r["layers"][0].as_u64().unwrap())
}

#[test]
fn stashes_into_a_slot_and_places_in_the_other_document_in_one_step() {
    let (mut s, paint) = two_docs(40, 30);
    let source_before = doc(&s, 0).clone();
    let r = s.execute("layer.tray.put", json!({})).unwrap();
    assert_eq!(r["slot"], 0, "the first slot");
    assert_eq!(s.layer_tray.len(), 1);
    assert_eq!(s.layer_tray[0].label, "paint");
    assert_eq!(s.layer_tray[0].source, doc(&s, 0).id);
    assert_eq!(s.layer_tray[0].source_name, doc(&s, 0).name);
    assert_eq!(*doc(&s, 0), source_before, "the source document is untouched, like Copy");
    // Place into the other document, the way a touch user switches documents first.
    s.set_active(1);
    let r = s.execute("layer.tray.place", json!({"slot": 0})).unwrap();
    let id = placed(&r);
    assert_ne!(id, paint, "a copy has its own id");
    assert_eq!(s.active_index(), Some(1), "the destination stays the active document");
    let d = doc(&s, 1);
    assert_eq!(d.layers.len(), 2);
    assert_eq!(d.layers[1].id, id, "above the Background, the destination's active layer");
    assert_eq!(d.layers[1].name, "paint");
    assert_eq!(s.active().unwrap().active_layer, Some(id));
    assert_eq!(s.active().unwrap().selected_layers(), vec![id]);
    // The canvases differ in size, so the layers are centred (the tab-drop rule): the square's
    // centre (9, 11) moves to the destination's (30, 25).
    assert_eq!(bounds(&s, 1, id), Rect::new(25, 20, 35, 30));
    let px = d.layers[1].surface().unwrap().read_region(Rect::new(26, 21, 27, 22));
    assert_eq!(px, vec![1.0, 0.0, 0.0, 1.0], "the pixels came along");
    assert_eq!(s.layer_tray.len(), 1, "placing keeps the slot, like Paste");
    // One history step in the destination.
    assert!(s.undo());
    assert_eq!(doc(&s, 1).layers.len(), 1);
    assert_eq!(s.layer_tray.len(), 1, "undo does not touch the tray");
}

#[test]
fn same_canvas_size_places_at_the_stashed_position() {
    let (mut s, _) = two_docs(40, 30);
    s.execute("file.new", json!({"width": 40, "height": 30})).unwrap();
    s.set_active(0);
    s.execute("layer.tray.put", json!({})).unwrap();
    s.set_active(2);
    let r = s.execute("layer.tray.place", json!({"slot": 0})).unwrap();
    assert_eq!(bounds(&s, 2, placed(&r)), Rect::new(4, 6, 14, 16), "the same canvas position by default");
}

#[test]
fn a_slot_survives_its_source_document_closing() {
    let (mut s, _) = two_docs(40, 30);
    s.execute("layer.tray.put", json!({})).unwrap();
    s.execute("file.close", json!({})).unwrap();
    assert_eq!(s.documents().len(), 1, "the source is gone");
    let r = s.execute("layer.tray.place", json!({"slot": 0})).unwrap();
    assert_eq!(doc(&s, 0).layers[1].name, "paint");
    let _ = placed(&r);
}

#[test]
fn converts_to_the_destinations_depth_and_colour_mode() {
    for depth in [8, 16, 32] {
        let (mut s, _) = two_docs(40, 30);
        s.execute("layer.tray.put", json!({})).unwrap();
        s.set_active(1);
        s.execute("image.mode.grayscale", json!({})).unwrap();
        s.execute("image.mode.bits16", json!({})).unwrap();
        let r = s.execute("layer.tray.place", json!({"slot": 0, "center": true})).unwrap();
        let surf = doc(&s, 1).layer(placed(&r)).unwrap().surface().unwrap();
        assert_eq!(surf.format(), PixelFormat::new(ColorMode::Grayscale, SampleType::U16, true), "from {depth}-bit RGB");
        let px = surf.read_region(Rect::new(30, 25, 31, 26));
        assert!(px[0] > 0.1 && px[0] < 0.9 && px[1] == 1.0, "red as a mid grey, opaque: {px:?}");
    }
}

#[test]
fn keeps_group_children_and_masks() {
    let (mut s, paint) = two_docs(40, 30);
    s.execute("layer.layerMask.hideAll", json!({})).unwrap();
    let other = LayerId(s.execute("layer.new.layer", json!({"name": "paint2"})).unwrap()["layer"].as_u64().unwrap());
    s.execute("layer.select", json!({"layer": paint.0, "mode": "add"})).unwrap();
    s.execute("layer.groupLayers", json!({})).unwrap();
    s.execute("layer.tray.put", json!({})).unwrap();
    s.set_active(1);
    let r = s.execute("layer.tray.place", json!({"slot": 0})).unwrap();
    let id = placed(&r);
    let l = doc(&s, 1).layer(id).unwrap();
    let group = match &l.content {
        LayerContent::Group(g) => g,
        _ => panic!("the stashed group lands as a group"),
    };
    let names: Vec<&str> = group.children.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["paint", "paint2"]);
    assert!(group.children[0].mask.is_some(), "the mask came along");
    assert_ne!(group.children[0].id, paint);
    assert_ne!(group.children[1].id, other);
}

#[test]
fn placing_twice_gives_each_copy_fresh_ids() {
    let (mut s, _) = two_docs(40, 30);
    s.execute("layer.tray.put", json!({})).unwrap();
    s.set_active(1);
    let a = placed(&s.execute("layer.tray.place", json!({"slot": 0})).unwrap());
    let b = placed(&s.execute("layer.tray.place", json!({"slot": 0})).unwrap());
    assert_ne!(a, b);
    assert_eq!(doc(&s, 1).layers.len(), 3, "both copies landed");
}

#[test]
fn remove_and_clear_empty_the_tray() {
    let (mut s, _) = two_docs(40, 30);
    s.execute("layer.tray.put", json!({"layer": "paint"})).unwrap();
    s.execute("layer.tray.put", json!({})).unwrap();
    assert_eq!(s.layer_tray.len(), 2);
    s.execute("layer.tray.remove", json!({"slot": 0})).unwrap();
    assert_eq!(s.layer_tray.len(), 1);
    assert_eq!(s.layer_tray[0].label, "paint", "indices shift down after a remove");
    s.execute("layer.tray.clear", json!({})).unwrap();
    assert!(s.layer_tray.is_empty());
}

#[test]
fn the_sixteenth_slot_is_the_last() {
    let (mut s, _) = two_docs(40, 30);
    for _ in 0..super::MAX_SLOTS {
        s.execute("layer.tray.put", json!({})).unwrap();
    }
    let e = s.execute("layer.tray.put", json!({})).unwrap_err();
    assert!(e.to_string().contains("full"), "{e}");
    assert_eq!(s.layer_tray.len(), super::MAX_SLOTS);
}

#[test]
fn bad_params_are_errors_not_panics() {
    let (mut s, _) = two_docs(40, 30);
    for p in [
        json!({"layers": []}),
        json!({"layers": [999]}),
        json!({"layer": 999}),
        json!({"layer": "paint", "layers": [1]}),
    ] {
        assert!(s.execute("layer.tray.put", p.clone()).is_err(), "put {p}");
    }
    assert!(s.execute("layer.tray.put", json!({"layers": "paint"})).is_err());
    // Place: missing, out-of-range and wrong-typed slot params.
    assert!(s.execute("layer.tray.place", json!({})).is_err());
    assert!(s.execute("layer.tray.place", json!({"slot": 0})).is_err(), "no slot 0 yet");
    s.execute("layer.tray.put", json!({})).unwrap();
    s.set_active(1);
    assert!(s.execute("layer.tray.place", json!({"slot": 9})).is_err());
    assert!(s.execute("layer.tray.place", json!({"slot": -1})).is_err());
    assert!(s.execute("layer.tray.place", json!({"slot": "0"})).is_err());
    assert!(s.execute("layer.tray.place", json!({"slot": 0, "at": [1e9, 0]})).is_err());
    // Remove and clear on an empty tray.
    s.execute("layer.tray.clear", json!({})).unwrap();
    assert!(s.execute("layer.tray.remove", json!({"slot": 0})).is_err());
    assert!(s.execute("layer.tray.clear", json!({})).is_err());
    // No document open.
    s.execute("file.close", json!({})).unwrap();
    s.execute("file.close", json!({})).unwrap();
    assert!(s.execute("layer.tray.put", json!({})).is_err());
    s.execute("file.new", json!({"width": 8, "height": 8})).unwrap();
    assert!(s.execute("layer.tray.place", json!({"slot": 0})).is_err());
}
