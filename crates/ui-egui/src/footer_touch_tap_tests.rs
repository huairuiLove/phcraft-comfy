//! A finger on the Layers panel footer: the fill/adjustment and fx buttons are press menus
//! (#2071), built for the mouse's press-drag-release. A finger drifts a few points between
//! touching down and lifting, and the menu opens flush beside the contact, so a menu that opened
//! on the touch-down let the lift run the item beside the finger — one tap on the button and a
//! fill or adjustment layer of whichever kind was under the finger appeared. A finger now taps:
//! the menu opens (and closes) on the lift, and only a tap that pressed on an item runs it.

use egui::{Event, Modifiers, PointerButton, Pos2, TouchDeviceId, TouchId, TouchPhase, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

use crate::PhotocraftApp;

fn panel() -> Harness<'static, PhotocraftApp> {
    let mut s = photocraft_engine::Session::new();
    s.execute("file.new", json!({"width": 8, "height": 8})).unwrap();
    let mut h = Harness::builder().with_size(egui::vec2(1200.0, 800.0)).with_max_steps(64).build_eframe(|cc| {
        PhotocraftApp::setup_context(&cc.egui_ctx, Default::default());
        PhotocraftApp::new(s, crate::Services::default())
    });
    h.run_steps(4);
    h
}

/// One finger tap shaped as egui-winit's `on_touch` delivers it: a `Touch` event plus the
/// emulated pointer pair (the shells' `touch_twins::drop_twins` keeps both as they are here).
/// `release_at` is where the finger lifts, which on a real screen is never exactly the contact.
fn finger_tap(h: &mut Harness<'static, PhotocraftApp>, press_at: Pos2, release_at: Pos2) {
    let ctx = h.ctx.clone();
    let touch = |phase, pos| Event::Touch { device_id: TouchDeviceId(1), id: TouchId(1), phase, pos, force: None };
    let button = |pos, pressed| Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
    h.input_mut().events = vec![touch(TouchPhase::Start, press_at), Event::PointerMoved(press_at), button(press_at, true)];
    crate::touch_twins::drop_twins(&ctx, h.input_mut());
    h.step();
    if release_at != press_at {
        h.input_mut().events = vec![Event::PointerMoved(release_at)];
        h.step();
    }
    h.input_mut().events = vec![touch(TouchPhase::End, release_at), button(release_at, false), Event::PointerGone];
    crate::touch_twins::drop_twins(&ctx, h.input_mut());
    h.step();
    h.run_steps(2);
}

fn layers(h: &Harness<'static, PhotocraftApp>) -> usize {
    h.state().session.active().unwrap().doc.layers.len()
}

fn adj_button(h: &mut Harness<'static, PhotocraftApp>) -> egui::Rect {
    h.get_by_label("Create new fill or adjustment layer").rect()
}

#[test]
fn a_finger_tap_opens_the_menu_on_the_lift_and_chooses_nothing() {
    let mut h = panel();
    let at = adj_button(&mut h).center();
    finger_tap(&mut h, at, at);
    assert!(h.query_by_label_contains("Solid Color").is_some(), "the menu opened");
    assert_eq!(layers(&h), 1, "the tap that opened the menu chose nothing");
    // A second tap on the button closes it again, on its lift.
    finger_tap(&mut h, at, at);
    h.run_steps(2);
    assert!(h.query_by_label_contains("Solid Color").is_none(), "the second tap closed the menu");
    assert_eq!(layers(&h), 1);
}

#[test]
fn a_finger_tap_on_an_item_chooses_it() {
    let mut h = panel();
    let at = adj_button(&mut h).center();
    finger_tap(&mut h, at, at);
    let invert = h.query_by_label("Invert").expect("the Invert item").rect().center();
    finger_tap(&mut h, invert, invert);
    h.run_steps(2);
    assert_eq!(layers(&h), 2, "the tapped Invert made its adjustment layer");
    assert!(h.query_by_label_contains("Solid Color").is_none(), "and closed the menu");
}

/// The tap that opens the menu may drift toward it as the finger rocks before lifting: the lift
/// is still a click, so the menu opens, but it must choose nothing.
#[test]
fn a_finger_tap_drifting_toward_the_menu_opens_and_chooses_nothing() {
    let mut h = panel();
    let rect = adj_button(&mut h);
    let at = rect.center();
    // 4pt towards the popup above the button: inside egui's click slop (6pt), so the lift is
    // still a click on the button, but past its edge into the menu's own padding.
    finger_tap(&mut h, at, at - vec2(0.0, 4.0));
    h.run_steps(2);
    assert!(h.query_by_label_contains("Solid Color").is_some(), "the menu opened");
    assert_eq!(layers(&h), 1, "the drifting lift chose nothing");
}

/// The fx button behaves the same way.
#[test]
fn a_finger_tap_opens_the_fx_menu_and_chooses_nothing() {
    let mut h = panel();
    let at = h.get_by_label("Add a layer style").rect().center();
    finger_tap(&mut h, at, at);
    assert!(h.query_by_label_contains("Drop Shadow").is_some(), "the fx menu opened");
    assert_eq!(layers(&h), 1, "and chose nothing");
}

/// The mouse keeps Photoshop's press-drag-release (#2071): press the button, drag onto an item,
/// release to choose it.
#[test]
fn a_mouse_press_drag_release_still_chooses_an_item() {
    let mut h = panel();
    let at = adj_button(&mut h).center();
    h.hover_at(at);
    h.run_steps(1);
    h.drag_at(at);
    h.run_steps(2);
    let invert = h.query_by_label("Invert").expect("the menu opened on the press").rect().center();
    for k in 1..=6 {
        h.hover_at(at + (invert - at) * (k as f32 / 6.0));
        h.run_steps(1);
    }
    h.drop_at(invert);
    h.run_steps(3);
    assert_eq!(layers(&h), 2, "releasing on Invert chose it");
    assert!(h.query_by_label_contains("Solid Color").is_none(), "and closed the menu");
}
