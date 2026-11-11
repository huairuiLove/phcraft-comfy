//! A finger-shaped tap walks File › New › Create in the portrait Touch Interface layout — the
//! flow a real iPad exercises ("can't create a file"). On iOS, winit and egui-winit deliver one
//! contact as a `Touch` event plus a synthesized pointer press in the same frame (and the pair
//! again on release), and the iOS shell runs `touch_twins::drop_twins` on the raw input before
//! the frame — this test replays exactly that shape through the whole app, in the portrait touch
//! chrome, where the menu bar, the menu popup, the New Document dialog and its Create button
//! all have to answer a tap.

use eframe::egui::{Event, Modifiers, PointerButton, Pos2, TouchDeviceId, TouchId, TouchPhase, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

use crate::PhotocraftApp;

fn harness() -> Harness<'static, PhotocraftApp> {
    // pixels_per_point 2 as on the iPad: the Touch Interface zoom then leaves ~760 pt of
    // design width, less than the New Document dialog's 800 pt design minimum.
    let mut h = Harness::builder().with_size(vec2(834.0, 1194.0)).with_pixels_per_point(2.0).with_max_steps(16).build_eframe(|cc| {
        PhotocraftApp::setup_context(&cc.egui_ctx, Default::default());
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        app.session.edit_prefs(|p| p.interface.touch_ui = true);
        app
    });
    h.run_steps(4);
    h
}

/// One iOS-shaped tap: the touch and its synthesized pointer press arrive together, the shell's
/// `drop_twins` runs on the raw input (as `IosShell::raw_input_hook` does), then the frame.
fn finger_tap(h: &mut Harness<'static, PhotocraftApp>, at: Pos2) {
    let touch = |phase, pos| Event::Touch {
        device_id: TouchDeviceId(1),
        id: TouchId(1),
        phase,
        pos,
        force: None,
    };
    let pointer = |pos, pressed| Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
    let ctx = h.ctx.clone();
    h.input_mut().events = vec![touch(TouchPhase::Start, at), pointer(at, true)];
    crate::touch_twins::drop_twins(&ctx, h.input_mut());
    h.step();
    h.input_mut().events = vec![touch(TouchPhase::End, at), pointer(at, false), Event::PointerGone];
    crate::touch_twins::drop_twins(&ctx, h.input_mut());
    h.step();
    h.run_steps(2);
}

/// The same walk with plain pointer clicks, as the desktop and the simulator's mouse see it —
/// the control the finger shape is compared against.
fn click(h: &mut Harness<'static, PhotocraftApp>, at: Pos2) {
    h.event(Event::PointerButton { pos: at, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE });
    h.step();
    h.event(Event::PointerButton { pos: at, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE });
    h.step();
    h.run_steps(2);
}

fn walk_new_document(h: &mut Harness<'static, PhotocraftApp>, tap: fn(&mut Harness<'static, PhotocraftApp>, Pos2)) {
    // Home is up (no documents, Auto Show on): its rail carries the New file button.
    assert!(h.state().session.documents().is_empty());
    assert!(h.state_mut().home_screen_up(), "the Home screen is up");

    // File › New… opens the New Document dialog.
    let file = h.get_by_label("File").rect().center();
    tap(h, file);
    let new_item = h
        .query_by_label_contains("New…")
        .expect("the File menu opened and shows New…")
        .rect()
        .center();
    tap(h, new_item);
    assert_eq!(h.state().ui.dialogs.len(), 1, "the New Document dialog opened");

    // Create makes a document. The button must be on the screen: an 11-inch portrait at the
    // Touch Interface scale has less room than the dialog's 800 pt design minimum, which used
    // to push it past the right edge (invisible, untappable — "can't create a file").
    let create_rect = h.get_by_label("Create").rect();
    let screen = h.ctx.input(|i| i.content_rect());
    assert!(create_rect.right() <= screen.right() && create_rect.left() >= screen.left() && create_rect.bottom() <= screen.bottom(), "Create fully on the screen: {create_rect:?} vs {screen:?}");
    let create = create_rect.center();
    tap(h, create);
    assert_eq!(h.state().session.documents().len(), 1, "Create opened a document");
}

#[test]
fn a_mouse_click_creates_a_document_from_the_menu() {
    let mut h = harness();
    walk_new_document(&mut h, click);
}

#[test]
fn a_finger_tap_creates_a_document_from_the_menu() {
    let mut h = harness();
    walk_new_document(&mut h, finger_tap);
}
