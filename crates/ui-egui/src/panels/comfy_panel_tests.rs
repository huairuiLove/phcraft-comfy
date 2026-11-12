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
    // build_eframe, like the i18n live tests: the panel draws Chinese template labels, which
    // need the full font stack (system CJK fallback) setup_context installs.
    // The dock tests' build_ui_state path runs no accessibility check; on the eframe path the
    // check wants per-widget names the egui fork's builders do not offer yet.
    let mut h = Harness::builder().with_size(egui::vec2(1100.0, 800.0)).with_max_steps(64).with_accessibility_check(false).build_eframe(move |cc| {
        crate::PhotocraftApp::setup_context(&cc.egui_ctx, crate::theme::ThemeKind::Pro);
        app
    });
    h.state_mut().ui.theme = crate::theme::ThemeKind::Pro;
    h.run_steps(4);
    h
}

#[test]
fn window_menu_toggles_the_comfy_panel() {
    let app = fresh_app();
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

/// The Image API backend (the iPad app's transport shape): the panel lists the fixed API
/// workflow beside the ComfyUI built-in, queues a task, and the direct call lands on the shelf.
#[test]
fn image_api_backend_lists_and_runs_the_api_workflow() {
    use photocraft_codecs::{ChannelLayout, EncodeOptions, Format, Image};
    use photocraft_engine::comfy_cmds::{ComfyCall, ComfyReply, ComfyRequest};
    // A 4×4 opaque PNG, as the endpoint would return one.
    let png = {
        let img = Image::from_u8(4, 4, ChannelLayout::Rgba, vec![255; 64]).unwrap();
        photocraft_codecs::encode(&img, Format::Png, &EncodeOptions::default()).unwrap()
    };
    let mut app = fresh_app();
    app.session.comfy.image_api = true;
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = seen.clone();
    app.session.comfy.transport = Some(std::sync::Arc::new(move |req: &ComfyRequest| {
        let api = matches!(req.call, ComfyCall::ApiImage { .. } | ComfyCall::ApiCheck { .. });
        record.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(api);
        match &req.call {
            ComfyCall::ApiCheck { .. } => Ok(ComfyReply::Json(json!({"data": [{"id": "gpt-image-2"}]}))),
            ComfyCall::ApiImage { .. } => Ok(ComfyReply::Bytes(png.clone())),
            _ => Ok(ComfyReply::Json(serde_json::Value::Null)),
        }
    }));
    app.session.prefs.edit(|p| {
        p.integrations.image_api_base = "https://api.example.com".into();
        p.integrations.image_api_key = "sk-test".into();
        p.integrations.image_api_model = "gpt-image-2".into();
    });
    app.ui.panels.comfy = true;
    let mut h = harness(app);
    h.run_steps(6);
    {
        let s = h.state();
        let names: Vec<&str> = s.comfy_queue.workflows.iter().map(|w| w.0.as_str()).collect();
        assert!(names.contains(&"Image API Edit") && names.contains(&"FLUX.3 Klein Image Edit"), "{names:?}");
    }
    // One direct fill through the app (the queue's scheduler runs the same command): the API
    // call lands on the shelf. The queue rows themselves are not drawn here — their ✕ marks
    // have no glyph in the test font stack.
    let r = h.state_mut().run("ai.comfyui.fill", json!({"workflow": "Image API Edit", "prompt": "a red dress", "place": "none"})).expect("fill runs");
    assert!(r["promptId"].as_str().is_some_and(|id| id.starts_with("api-")), "{r}");
    // No repaint after the fill: the gallery's ✕ remove buttons have no glyph in the test font
    // stack (a pre-existing harness gap; the desktop falls back to system fonts).
    assert_eq!(h.state().session.comfy.results.len(), 1, "the generated image is on the shelf");
    let calls = seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    assert!(!calls.is_empty() && calls.iter().all(|api| *api), "only Image API calls happened: {calls:?}");
}

/// A paste into the connection fields (Endpoint, Model, Key) commits without Enter: on a touch
/// device there is no Enter between a paste and the next tap, and the fields used to discard
/// what they were holding. The paste is the OS-clipboard bridge's delivery
/// (see `system_clipboard`).
#[test]
fn a_paste_into_the_image_api_fields_commits_without_enter() {
    use egui_kittest::kittest::Queryable;

    let mut app = fresh_app();
    app.session.comfy.image_api = true;
    app.session.prefs.edit(|p| p.integrations.image_api_base = "old-endpoint".into());
    app.ui.panels.comfy = true;
    app.comfy_queue.check = Some("ComfyUI 0.44".into());
    let mut h = harness(app);
    h.run_steps(6);
    // The Endpoint field, told apart from Model and Key by its current value.
    let endpoint = h
        .query_all_by_role(egui::accesskit::Role::TextInput)
        .into_iter()
        .find(|n| n.value().as_deref() == Some("old-endpoint"))
        .expect("the Endpoint field is drawn");
    endpoint.focus();
    h.run_steps(1);
    // Select all first, as a user replacing the whole endpoint would.
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    h.run_steps(1);
    h.event(egui::Event::Paste("https://ipad.local/api".into()));
    h.run_steps(2);
    assert_eq!(h.state().session.prefs().integrations.image_api_base, "https://ipad.local/api", "the paste committed");
    assert_eq!(h.state().comfy_queue.check, None, "and the connection check was reset");
}
