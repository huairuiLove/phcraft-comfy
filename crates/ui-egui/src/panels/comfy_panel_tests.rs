//! ComfyUI Generation panel: the Window menu toggle, the panel's first draw (workflow listing
//! and the queue empty state), and the engine-command route that opens it.

use egui_kittest::kittest::Queryable;
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

/// Typing into the Image API connection fields must land in the preferences as it is typed.
/// The fields re-clone from the preferences every frame and egui keeps no text of its own, so
/// anything not written through in its own frame is dropped — and a touch keyboard (the only
/// keyboard the mobile shells have) never presses Enter, the sole commit the fields had.
#[test]
fn image_api_fields_keep_what_is_typed_without_enter() {
    let mut app = fresh_app();
    app.session.comfy.image_api = true;
    app.ui.panels.comfy = true;
    let mut h = harness(app);
    h.run_steps(6);
    use egui::accesskit::Role;
    // The field beside its row label, not the first text input in the window (that one belongs
    // to the toolbar's colour readout), and not by tree nesting (the accesskit rows are not one
    // level deep): the input that shares the label's row and sits to its right. A password
    // field carries its own role. Re-queried every time — a node is a snapshot of one step.
    fn field<'t>(h: &'t Harness<'static, PhotocraftApp>, label: &str) -> egui_kittest::Node<'t> {
        let lr = h.get_by_label(label).rect();
        let on_row = |n: &egui_kittest::Node| {
            let r = n.rect();
            r.min.x > lr.max.x && r.min.y < lr.max.y && r.max.y > lr.min.y
        };
        h.query_all_by_role(Role::TextInput)
            .chain(h.query_all_by_role(Role::PasswordInput))
            .find(|n| on_row(n))
            .unwrap_or_else(|| panic!("the {label} field"))
    }
    let type_into = |h: &mut Harness<'static, PhotocraftApp>, label: &str, text: &str| {
        field(h, label).focus();
        h.run_steps(1);
        // Type like a keyboard does: one character per frame, no Enter ever pressed.
        for c in text.chars() {
            let one = c.to_string();
            field(h, label).type_text(&one);
            h.run_steps(1);
        }
        // Idle frames afterwards: a field that only commits on Enter reverts here.
        h.run_steps(4);
    };
    type_into(&mut h, "Endpoint", "https://api.example.com");
    assert_eq!(h.state().session.prefs().integrations.image_api_base, "https://api.example.com");
    type_into(&mut h, "Key", "sk-test-123");
    assert_eq!(h.state().session.prefs().integrations.image_api_key, "sk-test-123");
    // The Endpoint field still shows what was typed (not reverted to the saved, empty value);
    // the value sits on more than one accesskit node (the field and its text), so ask for at
    // least one. The Key field is a password input — accesskit does not expose its value, so
    // the preference above is its proof.
    assert!(!h.query_all_by_value("https://api.example.com").collect::<Vec<_>>().is_empty(), "the Endpoint field kept its text");
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
    // A stale desktop workflow name in the saved panel state: the mobile panel has no picker,
    // so a run must never be routed into that ComfyUI graph.
    app.ui.comfy_panel.workflow = "FLUX.3 Klein Image Edit".into();
    app.ui.comfy_panel.prompt = "a red dress".into();
    let mut h = harness(app);
    h.run_steps(6);
    {
        let s = h.state();
        let names: Vec<&str> = s.comfy_queue.workflows.iter().map(|w| w.0.as_str()).collect();
        assert!(names.contains(&"Image API Edit") && names.contains(&"FLUX.3 Klein Image Edit"), "{names:?}");
    }
    // No workflow feature on the Image API shells: no picker row, no importer, and none of the
    // controls the one direct call does not take.
    for absent in ["Workflow", "Import…", "Negative", "Seed", "Steps"] {
        assert!(h.query_by_label(absent).is_none(), "no {absent} row on the Image API shell");
    }
    // Add to Queue runs the fixed workflow (never the stale name) and lands the image. The
    // button sits below the fold of the panel's scroll area, so the click goes through
    // accesskit, which reaches widgets a clipped pointer event cannot. The queued task's row
    // (and the gallery it fills) use "✕", which the test font stack has no glyph for — allow
    // tofu for this harness instead of dodging the frames that draw them.
    h.ctx.set_missing_glyph_policy(egui::MissingGlyphPolicy::Tofu);
    h.get_by_label("Add to Queue").click_accesskit();
    h.run_steps(8);
    {
        let s = h.state();
        let t = s.comfy_queue.tasks.first().expect("the queued task");
        assert_eq!(t.workflow, "Image API Edit", "the pinned workflow, not the stale one");
        assert_eq!(t.state, "done", "{}", t.error);
        assert_eq!(s.session.comfy.results.len(), 1, "the generated image is on the shelf");
    }
    let calls = seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    assert!(!calls.is_empty() && calls.iter().all(|api| *api), "only Image API calls happened: {calls:?}");
}
