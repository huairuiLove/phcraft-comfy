//! Pen-only painting (the iPad shell's `PhotocraftApp::pen_only_painting`): a finger — a touch
//! contact that began without a force, where the Apple Pencil's always carries one — never
//! starts a brush stroke or a Liquify push. The pencil (any force, 0.0 included), a mouse and
//! automation paint as before; every other tool keeps its touch behaviour; the flag off is the
//! old behaviour everywhere (desktop, web, Android).

use egui::{Modifiers, Pos2, TouchDeviceId, TouchId, TouchPhase, pos2, vec2};
use egui_kittest::Harness;
use photocraft_algo::liquify::LiquifyTool;
use serde_json::json;

use crate::canvas::{ToolEvent, document_area};
use crate::state::Tool;
use crate::PhotocraftApp;

/// The app over a transparent 64×64 document with a hard 8 px black brush, `pen_only` policy.
fn app(pen_only: bool, tool: Tool) -> PhotocraftApp {
    let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
    app.run("file.new", json!({"width": 64, "height": 64, "background": "transparent"})).unwrap();
    // An unlocked layer: the Background's position lock refuses a marquee before the tool runs.
    app.run("layer.new.layer", json!({})).unwrap();
    app.run("tools.setBrush", json!({"brush": {"size": 8, "hardness": 1.0, "smoothing": {"amount": 0.0}}})).unwrap();
    app.run("tools.setColors", json!({"foreground": [0.0, 0.0, 0.0, 1.0]})).unwrap();
    app.sync_views();
    app.ui.tool = tool;
    app.pen_only_painting = pen_only;
    app
}

/// The whole-app canvas harness (as the two-finger gesture tests build it).
fn canvas(app: PhotocraftApp) -> (Harness<'static, (PhotocraftApp, bool)>, Pos2) {
    let mut h = Harness::builder()
        .with_size(egui::vec2(480.0, 360.0))
        .with_step_dt(1.0 / 60.0)
        .build_ui_state(
            |ui, (app, ready): &mut (PhotocraftApp, bool)| {
                if *ready {
                    egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| document_area(app, ui));
                }
            },
            (app, false),
        );
    PhotocraftApp::setup_context(&h.ctx, crate::theme::ThemeKind::Pro);
    h.state_mut().1 = true;
    h.run_steps(3);
    let centre = h.state().0.last_canvas_rect.center();
    (h, pos2(centre.x, centre.y))
}

fn touch(id: u64, phase: TouchPhase, pos: Pos2, force: Option<f32>) -> egui::Event {
    egui::Event::Touch { device_id: TouchDeviceId(0), id: TouchId(id), phase, pos, force }
}
fn press(pos: Pos2, pressed: bool) -> egui::Event {
    egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE }
}

/// One contact goes down at `at` — a finger (`force: None`) or the pencil.
fn contact_down(h: &mut Harness<'_, (PhotocraftApp, bool)>, at: Pos2, force: Option<f32>) {
    h.input_mut().events = vec![touch(1, TouchPhase::Start, at, force), press(at, true)];
    h.step();
    h.run_steps(1);
}

/// The contact moves and lifts.
fn contact_up(h: &mut Harness<'_, (PhotocraftApp, bool)>, to: Pos2, force: Option<f32>) {
    h.input_mut().events = vec![touch(1, TouchPhase::Move, to, force), egui::Event::PointerMoved(to)];
    h.step();
    h.input_mut().events = vec![touch(1, TouchPhase::End, to, force), press(to, false)];
    h.step();
    h.run_steps(2);
}

#[test]
fn a_finger_never_starts_a_stroke_with_the_brush_family() {
    for tool in [Tool::Brush, Tool::Eraser, Tool::Smudge, Tool::CloneStamp, Tool::HistoryBrush] {
        let (mut h, at) = canvas(app(true, tool));
        let history = h.state().0.session.active().unwrap().history.past_len();
        contact_down(&mut h, at, None);
        assert!(h.state().0.drag.is_none(), "{tool:?}: refused at the press");
        contact_up(&mut h, at + vec2(12.0, 0.0), None);
        assert_eq!(h.state().0.session.active().unwrap().history.past_len(), history, "{tool:?}: no stroke committed");
        assert_eq!(h.state().0.ui.status, "Painting needs the Apple Pencil", "{tool:?}: the hint says why");
    }
}

#[test]
fn the_pencil_paints_at_every_pressure() {
    for force in [0.0, 0.4] {
        let (mut h, at) = canvas(app(true, Tool::Brush));
        let base = h.state().0.session.active().unwrap().history.past_len();
        contact_down(&mut h, at, Some(force));
        assert!(h.state().0.drag.is_some(), "force {force}: the pencil paints");
        contact_up(&mut h, at + vec2(12.0, 0.0), Some(force));
        assert_eq!(h.state().0.session.active().unwrap().history.past_len(), base + 1, "force {force}: one stroke");
    }
}

#[test]
fn a_finger_still_runs_the_other_tools_and_the_move_shortcut() {
    // A marquee with a finger works as before (its drag starts on egui's drag threshold, so
    // the proof is the selection the released drag leaves).
    let (mut h, at) = canvas(app(true, Tool::RectMarquee));
    contact_down(&mut h, at, None);
    assert_ne!(h.state().0.ui.status, "Painting needs the Apple Pencil", "the gate does not take the marquee");
    h.input_mut().events = vec![touch(1, TouchPhase::Move, at + vec2(20.0, 5.0), None), egui::Event::PointerMoved(at + vec2(20.0, 5.0))];
    h.step();
    assert!(h.state().0.drag.is_some(), "the selection drag started");
    contact_up(&mut h, at + vec2(20.0, 10.0), None);
    assert!(h.state().0.session.active().unwrap().doc.selection.is_some(), "the finger selected");
}

#[test]
fn the_flag_off_is_the_old_behaviour() {
    let (mut h, at) = canvas(app(false, Tool::Brush));
    contact_down(&mut h, at, None);
    assert!(h.state().0.drag.is_some(), "a finger paints when the policy is off");
}

/// A finger's ToolEvents never reach Liquify; the pencil's push, freeze, thaw and lasso do.
#[test]
fn liquify_refuses_a_finger_for_every_tool() {
    let pencil = ToolEvent::Down { x: 32.0, y: 32.0, pressure: 0.5 };
    let opened = || {
        let mut app = app(true, Tool::Move);
        // Liquify's workspace (Filter ▸ Liquify…), with pixels to push.
        app.run("select.all", json!({})).unwrap();
        app.run("edit.fill", json!({"color": "#808080"})).unwrap();
        app.run("select.deselect", json!({})).unwrap();
        crate::liquify_ui::open(&mut app, &egui::Context::default()).unwrap();
        assert!(app.distort.liquify.is_some(), "the Liquify workspace opened");
        app
    };
    let finger = || {
        let mut app = opened();
        app.fingers.frame(&[touch(7, TouchPhase::Start, pos2(10.0, 10.0), None)]);
        app
    };
    let pencil_app = |tool: LiquifyTool| {
        let mut app = opened();
        app.distort.liquify.as_mut().unwrap().opts.tool = tool;
        app
    };
    for tool in [LiquifyTool::ForwardWarp, LiquifyTool::Freeze, LiquifyTool::Thaw, LiquifyTool::LassoMask] {
        // A finger's full gesture: nothing starts, nothing is recorded.
        let mut app = finger();
        app.distort.liquify.as_mut().unwrap().opts.tool = tool;
        let before = app.distort.liquify.as_ref().unwrap().strokes.len();
        for ev in [
            ToolEvent::Down { x: 20.0, y: 20.0, pressure: 1.0 },
            ToolEvent::Move { x: 40.0, y: 20.0, pressure: 1.0 },
            ToolEvent::Up { x: 40.0, y: 20.0 },
            ToolEvent::Down { x: 20.0, y: 40.0, pressure: 1.0 },
            ToolEvent::Move { x: 40.0, y: 40.0, pressure: 1.0 },
            ToolEvent::Up { x: 40.0, y: 40.0 },
        ] {
            crate::liquify_ui::pointer(&mut app, ev, Modifiers::NONE);
        }
        assert_eq!(app.distort.liquify.as_ref().unwrap().strokes.len(), before, "{tool:?}: the finger recorded nothing");
        assert_eq!(app.ui.status, "Painting needs the Apple Pencil", "{tool:?}: the hint says why");

        // The pencil's press starts the tool's own gesture.
        let mut app = pencil_app(tool);
        crate::liquify_ui::pointer(&mut app, pencil, Modifiers::NONE);
        let d = app.distort.liquify.as_ref().unwrap();
        let started = match tool {
            LiquifyTool::LassoMask => d.lasso.is_some(),
            _ => d.cur.is_some(),
        };
        assert!(started, "{tool:?}: the pencil works");
        let _ = before;
    }
}
