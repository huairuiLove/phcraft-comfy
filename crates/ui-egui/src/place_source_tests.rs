//! Place's album-or-files question (`place_source.rs`): asked only where the shell provides a
//! photo album service (the iPad's PHPicker), answered by the album picker or the Files picker,
//! and both continuations place what came back.

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use photocraft_codecs::{ChannelLayout, EncodeOptions, Format, Image};
use serde_json::json;

use crate::PhotocraftApp;

/// A 4×4 opaque PNG, as a picked album photo would arrive.
fn photo_png() -> Vec<u8> {
    let img = Image::from_u8(4, 4, ChannelLayout::Rgba, vec![255; 64]).unwrap();
    photocraft_codecs::encode(&img, Format::Png, &EncodeOptions::default()).unwrap()
}

fn fresh_app() -> PhotocraftApp {
    let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
    app.run("file.new", json!({"width": 64, "height": 64})).unwrap();
    app
}

fn harness(app: PhotocraftApp) -> Harness<'static, PhotocraftApp> {
    let mut h = Harness::builder().with_size(egui::vec2(1100.0, 800.0)).with_max_steps(64).with_accessibility_check(false).build_eframe(move |cc| {
        crate::PhotocraftApp::setup_context(&cc.egui_ctx, crate::theme::ThemeKind::Pro);
        app
    });
    h.run_steps(4);
    h
}

/// The Place menu command with an album service: the question opens, Photo Library places the
/// picked photo, and Files takes the ordinary file dialog instead.
#[test]
fn place_asks_album_or_files_when_the_shell_has_an_album() {
    let mut app = fresh_app();
    // The album answers at once with one photo.
    let png = photo_png();
    app.services.photo_album = Some(Box::new(move |_request, _parent, reply| {
        reply.send(Some(crate::FileDialogAnswer::ContentsMany(vec![("IMG_0001.png".into(), png.clone())])));
    }));
    let mut h = harness(app);
    let ctx = h.ctx.clone();

    // Without the command: no question anywhere.
    assert!(h.state().ui.place_source.is_none());
    crate::menus::invoke(h.state_mut(), &ctx, "file.placeEmbedded", json!({})).unwrap();
    assert!(h.state().ui.place_source.is_some(), "the place waits on its source");
    h.run_steps(2);
    // The question shows both sources; choosing the album runs the album picker.
    let album = h.query_all_by_label_contains("Photo Library").next().expect("the album choice is drawn");
    album.click();
    h.run_steps(6);
    assert!(h.state().ui.place_source.is_none(), "the question is gone");
    let layers = h.state().session.active().map(|d| d.doc.layers.len()).unwrap_or_default();
    assert_eq!(layers, 2, "the photo was placed as a layer");
    assert!(h.state().ui.transform.is_some(), "Free Transform began on the placed layer");

    // And the Files choice goes to the file dialog service, not the album.
    let mut app = fresh_app();
    let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = asked.clone();
    app.services.photo_album = Some(Box::new(|_request, _parent, reply| {
        reply.send(None); // cancelled; this test's choice is Files
    }));
    app.services.file_dialog = Some(Box::new(move |request, _parent, reply| {
        seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(format!("{request:?}"));
        reply.send(None);
    }));
    let mut h = harness(app);
    let ctx = h.ctx.clone();
    crate::menus::invoke(h.state_mut(), &ctx, "file.placeEmbedded", json!({})).unwrap();
    h.run_steps(2);
    let files = h.query_all_by_label_contains("Files…").next().expect("the files choice is drawn");
    files.click();
    h.run_steps(4);
    let asked = asked.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    assert_eq!(asked.len(), 1, "the Files choice asked the file dialog once: {asked:?}");
    assert!(asked[0].contains("Open"), "and it was an Open request: {asked:?}");
}

/// Without an album service (desktop, web, Android today), Place never asks and starts the
/// Files picker directly, as before.
#[test]
fn place_without_an_album_goes_straight_to_files() {
    let mut app = fresh_app();
    let asked = std::sync::Arc::new(std::sync::Mutex::new(0));
    let seen = asked.clone();
    app.services.file_dialog = Some(Box::new(move |_request, _parent, reply| {
        *seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
        reply.send(None);
    }));
    let mut h = harness(app);
    let ctx = h.ctx.clone();
    crate::menus::invoke(h.state_mut(), &ctx, "file.placeEmbedded", json!({})).unwrap();
    h.run_steps(3);
    assert!(h.state().ui.place_source.is_none(), "no question without an album service");
    assert!(h.query_all_by_label_contains("Photo Library").next().is_none(), "the question never drew");
    assert_eq!(*asked.lock().unwrap_or_else(std::sync::PoisonError::into_inner), 1, "the Files picker opened");
}

/// Esc cancels the question: no picker opens, no place happens.
#[test]
fn escape_cancels_the_place_question() {
    let mut app = fresh_app();
    app.services.photo_album = Some(Box::new(|_request, _parent, reply| {
        reply.send(None);
    }));
    let mut h = harness(app);
    let ctx = h.ctx.clone();
    crate::menus::invoke(h.state_mut(), &ctx, "file.placeEmbedded", json!({})).unwrap();
    h.run_steps(2);
    h.key_press(egui::Key::Escape);
    h.run_steps(2);
    assert!(h.state().ui.place_source.is_none(), "the question closed");
    assert!(!h.state().file_dialog_open(), "no dialog is open");
    let layers = h.state().session.active().map(|d| d.doc.layers.len()).unwrap_or_default();
    assert_eq!(layers, 1, "nothing was placed");
}
