//! ComfyUI Generation panel: the Window menu toggle, the panel's first draw (workflow listing
//! and the queue empty state), and the engine-command route that opens it.

use egui_kittest::Harness;
use serde_json::json;

use crate::PhotocraftApp;

fn fresh_app() -> PhotocraftApp {
    let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
    app.run("file.new", json!({"width": 64, "height": 64})).unwrap();
    app
}

fn harness(app: PhotocraftApp) -> Harness<'static, PhotocraftApp> {
    let mut h = Harness::builder().with_size(egui::vec2(1100.0, 800.0)).build_ui_state(
        |ui, app: &mut PhotocraftApp| {
            let ctx = ui.ctx().clone();
            if !ctx.fonts(|f| f.families().contains(&egui::FontFamily::Name("medium".into()))) {
                return;
            }
            crate::panels::dock_area(app, ui);
            egui::CentralPanel::default().show(ui, |_| {});
        },
        app,
    );
    crate::PhotocraftApp::setup_context(&h.ctx, crate::theme::ThemeKind::Pro);
    h.state_mut().ui.theme = crate::theme::ThemeKind::Pro;
    h.run_steps(4);
    h
}

#[test]
fn window_menu_toggles_the_comfy_panel() {
    let mut app = fresh_app();
    let ctx = {
        let mut h = harness(app);
        assert!(!h.state().ui.panels.comfy);
        // Off by default (like Character: not in the default workspace) and checkmarked when open.
        assert_eq!(crate::view_cmds::checked(h.state(), "window.panel.comfy"), Some(false));
        let ctx = h.ctx.clone();
        crate::menus::invoke(h.state_mut(), &ctx, "window.panel.comfy", json!({})).unwrap();
        assert!(h.state().ui.panels.comfy && h.state().ui.dock_tabs.comfy == 0);
        assert_eq!(crate::view_cmds::checked(h.state(), "window.panel.comfy"), Some(true));
        // The panel drew: the workflow listing ran and shows the built-in preset.
        h.run_steps(6);
        assert!(h.state().comfy_queue.listed, "the panel listed workflows on first draw");
        let names: Vec<String> = h.state().comfy_queue.workflows.iter().map(|w| w.0.clone()).collect();
        assert!(names.iter().any(|n| n == "FLUX.3 Klein Image Edit"), "{names:?}");
        h.run_steps(4);
        drop(h);
        ctx
    };
    let _ = ctx;
    // Toggling again closes it.
    let mut h = {
        let mut app = fresh_app();
        app.ui.panels.comfy = true;
        harness(app)
    };
    let ctx = h.ctx.clone();
    crate::menus::invoke(h.state_mut(), &ctx, "window.panel.comfy", json!({})).unwrap();
    assert!(!h.state().ui.panels.comfy);
}

#[test]
fn fill_without_params_reports_the_engine_reason() {
    // The panel route runs the engine's enabled check first (like ai.comfyui.generate): without
    // a transport (tests, the web) it says why instead of opening anything.
    let mut h = harness(fresh_app());
    let ctx = h.ctx.clone();
    let e = crate::menus::invoke(h.state_mut(), &ctx, "ai.comfyui.fill", json!({})).unwrap_err();
    assert!(e.contains("desktop app"), "{e}");
    assert!(!h.state().ui.panels.comfy);
}
