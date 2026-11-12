//! Pen-only painting (the iPad shell's `PhotocraftApp::pen_only_painting`): a finger's press
//! must not paint with a brush-family tool or any Liquify brush — Freeze and Thaw included —
//! while the Apple Pencil (a touch that reports a force), a mouse, automation, and the same
//! finger with the flag off (every other shell) paint exactly as before.

use egui::{Event, Modifiers, PointerButton, TouchDeviceId, TouchId, TouchPhase, pos2};
use serde_json::json;

use crate::PhotocraftApp;
use crate::canvas::{ToolEvent, tool_event};
use crate::state::Tool;

fn app(tool: Tool) -> PhotocraftApp {
    let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
    app.session.execute("file.new", json!({"width": 200, "height": 200, "background": "white"})).unwrap();
    app.sync_views();
    app.ui.tool = tool;
    app.ui.extras.snap = false;
    app.pen_only_painting = true;
    app
}

/// One press as iOS delivers it: the touch, then the emulated pointer press egui-winit
/// synthesizes from it (`force: None` is a finger, `Some` the Apple Pencil).
fn press(app: &mut PhotocraftApp, force: Option<f32>) {
    app.finger.observe(&[
        Event::Touch { device_id: TouchDeviceId(1), id: TouchId(7), phase: TouchPhase::Start, pos: pos2(20.0, 20.0), force },
        Event::PointerButton { pos: pos2(20.0, 20.0), button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE },
    ]);
}

fn release(app: &mut PhotocraftApp) {
    app.finger.observe(&[Event::PointerButton {
        pos: pos2(60.0, 20.0),
        button: PointerButton::Primary,
        pressed: false,
        modifiers: Modifiers::NONE,
    }]);
}

fn steps(app: &PhotocraftApp) -> usize {
    app.session.active().unwrap().history.entries().len()
}

fn stroke(app: &mut PhotocraftApp) {
    tool_event(app, ToolEvent::Down { x: 20.0, y: 20.0, pressure: 1.0 }, Modifiers::NONE);
    tool_event(app, ToolEvent::Move { x: 60.0, y: 30.0, pressure: 1.0 }, Modifiers::NONE);
    tool_event(app, ToolEvent::Up { x: 60.0, y: 30.0 }, Modifiers::NONE);
}

#[test]
fn a_finger_does_not_paint_but_the_pencil_does() {
    let mut app = app(Tool::Brush);
    press(&mut app, None);
    let n = steps(&app);
    stroke(&mut app);
    assert!(app.drag.is_none() && app.live_stroke.is_none(), "no stroke from a finger");
    assert!(app.ui.status.contains("Apple Pencil"), "{}", app.ui.status);
    assert_eq!(steps(&app), n, "nothing painted, nothing recorded");
    release(&mut app);

    // The pencil on the same canvas paints: its touch reports a force, a featherlight one too.
    let mut painted = 0;
    for force in [Some(0.0), Some(0.4)] {
        press(&mut app, force);
        tool_event(&mut app, ToolEvent::Down { x: 20.0, y: 20.0, pressure: force.unwrap_or(1.0) }, Modifiers::NONE);
        assert!(app.drag.is_some(), "the pencil's stroke runs");
        tool_event(&mut app, ToolEvent::Move { x: 60.0, y: 30.0, pressure: 1.0 }, Modifiers::NONE);
        tool_event(&mut app, ToolEvent::Up { x: 60.0, y: 30.0 }, Modifiers::NONE);
        painted += 1;
        assert_eq!(steps(&app), n + painted, "the pencil's stroke painted");
        release(&mut app);
    }
}

#[test]
fn the_eraser_and_the_retouch_brushes_refuse_a_finger_too() {
    for tool in [Tool::Eraser, Tool::Smudge, Tool::CloneStamp, Tool::HistoryBrush] {
        let mut app = app(tool);
        press(&mut app, None);
        stroke(&mut app);
        assert!(app.drag.is_none() && app.live_stroke.is_none(), "{tool:?}: no stroke from a finger");
    }
}

#[test]
fn a_finger_still_does_everything_that_is_not_painting() {
    // A marquee selection with a finger is not painting: the drag runs, no refusal shows, and
    // the selection lands.
    let mut marquee = app(Tool::RectMarquee);
    press(&mut marquee, None);
    tool_event(&mut marquee, ToolEvent::Down { x: 10.0, y: 10.0, pressure: 1.0 }, Modifiers::NONE);
    assert!(marquee.drag.is_some(), "a finger draws a marquee");
    assert!(!marquee.ui.status.contains("Apple Pencil"), "{}", marquee.ui.status);
    tool_event(&mut marquee, ToolEvent::Up { x: 80.0, y: 80.0 }, Modifiers::NONE);
    assert!(marquee.session.active().unwrap().doc.selection.is_some(), "the marquee selected");
    release(&mut marquee);
}

#[test]
fn with_pen_only_painting_off_the_finger_paints_as_before() {
    let mut app = app(Tool::Brush);
    app.pen_only_painting = false;
    press(&mut app, None);
    let n = steps(&app);
    stroke(&mut app);
    assert!(app.drag.is_none(), "the stroke completed");
    assert_eq!(steps(&app), n + 1, "the desktop, web and Android behaviour is unchanged");
}

#[test]
fn liquify_including_freeze_and_thaw_takes_only_the_pen() {
    // Two moves, so the lasso closes a polygon of at least three points.
    fn warp(app: &mut PhotocraftApp) {
        crate::liquify_ui::pointer(app, ToolEvent::Down { x: 60.0, y: 60.0, pressure: 1.0 }, Modifiers::NONE);
        crate::liquify_ui::pointer(app, ToolEvent::Move { x: 90.0, y: 60.0, pressure: 1.0 }, Modifiers::NONE);
        crate::liquify_ui::pointer(app, ToolEvent::Move { x: 120.0, y: 70.0, pressure: 1.0 }, Modifiers::NONE);
        crate::liquify_ui::pointer(app, ToolEvent::Up { x: 120.0, y: 70.0 }, Modifiers::NONE);
    }
    let strokes = |app: &mut PhotocraftApp| crate::liquify_ui::control(app, &json!({})).unwrap()["strokes"].clone();
    for tool in ["forwardWarp", "freeze", "thaw", "lassoMask"] {
        let mut app = app(Tool::Brush);
        crate::liquify_ui::open(&mut app, &egui::Context::default()).unwrap();
        crate::liquify_ui::control(&mut app, &json!({"tool": tool})).unwrap();
        assert_eq!(strokes(&mut app), json!(0));

        press(&mut app, None);
        warp(&mut app);
        assert_eq!(strokes(&mut app), json!(0), "{tool}: no stroke from a finger");
        release(&mut app);

        press(&mut app, Some(0.5));
        warp(&mut app);
        assert_eq!(strokes(&mut app), json!(1), "{tool}: the pencil warps");
        release(&mut app);
    }
}
