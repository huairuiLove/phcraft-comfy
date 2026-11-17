//! The full-screen exit chip (panels/screen_mode_chip.rs): a floating button that returns to
//! the Standard screen mode, because an iPad has neither of the desktop's two ways out of a
//! full-screen mode (Esc, Tab).

use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;
use serde_json::json;

use crate::PhotocraftApp;

fn touch_harness(screen_mode: &str) -> Harness<'static, PhotocraftApp> {
    let mut s = photocraft_engine::Session::new();
    s.execute("file.new", json!({"width": 64, "height": 48})).unwrap();
    let mode = screen_mode.to_string();
    let mut h = Harness::builder().with_size(egui::vec2(1200.0, 800.0)).with_max_steps(64).build_eframe(move |cc| {
        PhotocraftApp::setup_context(&cc.egui_ctx, Default::default());
        let mut app = PhotocraftApp::new(s, crate::Services::default());
        app.session.edit_prefs(|p| {
            p.interface.touch_ui = true;
            // The label-based assertions need English whatever a parallel i18n live test did
            // to the global language (the frame re-reads this pref every pass).
            p.interface.language = "en".into();
        });
        app.ui.view.screen_mode = mode;
        app
    });
    h.run_steps(4);
    h
}

fn chip<'a>(h: &'a Harness<'_, PhotocraftApp>) -> Option<egui_kittest::Node<'a>> {
    h.query_all_by_label("Standard Screen Mode")
        .into_iter()
        .find(|n| {
            crate::panels::screen_mode_chip::rect(&h.ctx)
                .is_some_and(|card| card.contains(n.rect().center()))
        })
}

#[test]
fn the_chip_shows_in_a_full_screen_mode_and_a_tap_returns_to_standard() {
    let mut h = touch_harness("fullScreen");
    let card = crate::panels::screen_mode_chip::rect(&h.ctx).expect("the chip drew in full screen");
    let canvas = h.state().last_canvas_rect;
    assert!(card.left() >= canvas.left() && card.left() <= canvas.left() + 40.0, "the chip hugs the canvas's left edge");
    assert!(card.top() <= canvas.top() + 40.0, "and its top");
    chip(&h).expect("the chip's button").click();
    h.run_steps(3);
    assert_eq!(h.state().ui.view.screen_mode, "standard", "a tap did what Esc does on the desktop");
    // The card rect lingers in egui's memory after the chip stops drawing, so the gone-check
    // judges by the per-frame node.
    assert!(chip(&h).is_none(), "the chip is gone in the standard mode");
}

#[test]
fn the_chip_shows_in_the_with_menu_bar_mode_too_but_not_in_standard() {
    let h = touch_harness("fullScreenWithMenuBar");
    assert!(chip(&h).is_some(), "the with-menu-bar mode hides the tools as well, so it keeps the chip");
    let h = touch_harness("standard");
    assert!(crate::panels::screen_mode_chip::rect(&h.ctx).is_none(), "no chip in the standard mode");
}

#[test]
fn no_chip_without_the_touch_interface() {
    let mut s = photocraft_engine::Session::new();
    s.execute("file.new", json!({"width": 64, "height": 48})).unwrap();
    let mut h = Harness::builder().with_size(egui::vec2(1200.0, 800.0)).with_max_steps(64).build_eframe(move |cc| {
        PhotocraftApp::setup_context(&cc.egui_ctx, Default::default());
        let mut app = PhotocraftApp::new(s, crate::Services::default());
        app.ui.view.screen_mode = "fullScreen".into();
        app
    });
    h.run_steps(4);
    assert!(crate::panels::screen_mode_chip::rect(&h.ctx).is_none(), "the desktop exits with Esc, Tab and the View menu");
}
