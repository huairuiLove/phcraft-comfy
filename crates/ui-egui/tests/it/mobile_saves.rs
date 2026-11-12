//! The mobile shells' saves: both the Android and iPadOS shells run `background_jobs` with NO
//! `save_file` worker service (their `write` answers inline — the iPad one stages the file and
//! presents the system export sheet), and their save dialogs answer at once (the iPad shell
//! with the suggested bare name, Android's in-app browser with a full path). These tests hold
//! that shape: a Save As and a plain Save must complete inline, without a worker, through the
//! shell's own `export`/`write` services, and the document must come out saved.

use photocraft_doc::Document;
use photocraft_engine::Session;
use photocraft_ui_egui::{FileDialogAnswer, FileDialogRequest, PhotocraftApp, Services, menus};
use serde_json::json;

/// The shells' services (`apps/photocraft-android/src/lib.rs`, `apps/photocraft-ios/src/main.rs`):
/// import/export/file_dialog/write, no `save_file` (no background save worker), `background_jobs`
/// on. `ios` picks the save dialog's answer shape: the bare suggested name (iPad) or a full path
/// under the shared storage root (Android).
fn mobile_app(ios: bool) -> PhotocraftApp {
    let mut app = PhotocraftApp::new(
        Session::new(),
        Services {
            import: Some(Box::new(|name: &str, bytes: &[u8], opts: &photocraft_io::ImportOptions| {
                photocraft_io::import_with_options(name, bytes, opts).map(|r| (r.document, r.warnings)).map_err(|e| e.to_string())
            })),
            export: Some(Box::new(|doc: &Document, path: &str, settings: &photocraft_ui_egui::ExportSettings| {
                photocraft_io::export(doc, path, &settings.io_options()).map(|r| (r.bytes, r.warnings)).map_err(|e| e.to_string())
            })),
            file_dialog: Some(Box::new(move |request, _parent, reply| match request {
                FileDialogRequest::Open { .. } | FileDialogRequest::Folder { .. } => reply.send(None),
                FileDialogRequest::Save { suggested } => {
                    let name = if ios {
                        std::path::Path::new(&suggested).file_name().map_or_else(|| suggested.clone(), |n| n.to_string_lossy().into_owned())
                    } else {
                        format!("/storage/emulated/0/Download/{suggested}")
                    };
                    reply.send(Some(FileDialogAnswer::SaveTo(name)));
                }
            })),
            write: Some(Box::new(|_path: &str, _bytes: &[u8]| Ok(()))),
            encode_png: Some(Box::new(|w, h, rgba| {
                use photocraft_codecs::{ChannelLayout, EncodeOptions, Image};
                let img = Image::from_u8(w, h, ChannelLayout::Rgba, rgba.to_vec()).map_err(|e| e.to_string())?;
                photocraft_codecs::encode(&img, photocraft_codecs::Format::Png, &EncodeOptions::default()).map_err(|e| e.to_string())
            })),
            write_unconfirmed: ios,
            ..Default::default()
        },
    );
    app.background_jobs = true;
    app
}

/// Whether the active document completed the save. The iPad shell keeps
/// `write_unconfirmed` (a cancelled export sheet saved nothing, #4962), so its document only
/// gains the file; Android's write is the file on disk, so its revision comes out saved too.
fn saved(app: &PhotocraftApp, unconfirmed: bool) -> bool {
    let st = app.session.active().expect("document");
    st.path.is_some() && (unconfirmed || st.saved_revision == st.revision)
}

#[test]
fn mobile_save_as_writes_inline_without_a_worker() {
    for ios in [true, false] {
        let ctx = egui::Context::default();
        let mut app = mobile_app(ios);
        app.run("file.new", json!({"width": 64, "height": 48, "background": "white"})).unwrap();
        // File > Save As asks the dialog, which answers on the next poll; the save then runs
        // inline in the same poll (no save_file service to hand it to a worker).
        menus::invoke(&mut app, &ctx, "file.saveAs", json!({})).unwrap();
        app.poll_file_dialog(&ctx, None);
        assert!(saved(&app, ios), "ios={ios}: the document is saved, status={}", app.ui.status);

        // File > Save after the document has a file writes back through the same services.
        app.run("layer.new.layer", json!({})).unwrap();
        menus::invoke(&mut app, &ctx, "file.save", json!({})).unwrap();
        app.poll_file_dialog(&ctx, None);
        assert!(saved(&app, ios), "ios={ios}: status={}", app.ui.status);
        assert!(!app.ui.status_error, "ios={ios}: status={}", app.ui.status);
    }
}

#[test]
fn mobile_plain_save_on_an_untitled_document_asks_then_writes() {
    for ios in [true, false] {
        let ctx = egui::Context::default();
        let mut app = mobile_app(ios);
        app.run("file.new", json!({"width": 64, "height": 48})).unwrap();
        menus::invoke(&mut app, &ctx, "file.save", json!({})).unwrap();
        app.poll_file_dialog(&ctx, None);
        assert!(saved(&app, ios), "ios={ios}: status={}", app.ui.status);
    }
}

/// The save must record a real, re-openable path: the iPad shell saves into its Documents
/// folder (a bare staged tmp name once left every Open Recent entry unopenable —
/// `open_paths` resolves a bare name against the working directory, `/` on a device), and
/// Android's browser answers a full path under the shared storage. Whichever shape, the
/// saved file must exist on disk where the recent list says it is, and opening it from
/// there must bring the document back.
#[test]
fn a_mobile_save_records_a_path_open_recent_can_reopen() {
    let root = std::env::temp_dir().join(format!(
        "photocraft-mobile-recent-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0)
    ));
    let documents = root.join("Documents");
    std::fs::create_dir_all(&documents).unwrap();
    let ctx = egui::Context::default();
    let mut app = mobile_app(true);
    // The shell's own services: the save dialog answers the Documents path, `write` writes it.
    let answered = documents.clone();
    app.services.file_dialog = Some(Box::new(move |request, _parent, reply| match request {
        FileDialogRequest::Save { suggested } => {
            let name = std::path::Path::new(&suggested)
                .file_name()
                .map_or_else(|| suggested.clone(), |n| n.to_string_lossy().into_owned());
            reply.send(Some(FileDialogAnswer::SaveTo(answered.join(&name).to_string_lossy().into_owned())));
        }
        _ => reply.send(None),
    }));
    app.services.write = Some(Box::new(|path: &str, bytes: &[u8]| std::fs::write(path, bytes).map_err(|e| e.to_string())));
    app.run("file.new", json!({"width": 64, "height": 48, "name": "art"})).unwrap();
    menus::invoke(&mut app, &ctx, "file.saveAs", json!({})).unwrap();
    app.poll_file_dialog(&ctx, None);

    let recorded = app.ui.recent_files.first().expect("the save reached Open Recent").clone();
    let file = std::path::PathBuf::from(&recorded);
    assert!(file.is_file(), "the recent entry names a file on disk: {recorded}");
    assert_eq!(file.parent(), Some(documents.as_path()), "the entry carries the full path, not a bare name");

    // Open Recent re-opens it: the saved document is closed first (as after a relaunch), then
    // its recent entry is opened — a document comes back and no error reaches the status bar.
    app.run("file.close", json!({"discard": true})).unwrap();
    app.run("file.new", json!({"width": 8, "height": 8})).unwrap();
    let before = app.session.documents().len();
    app.open_paths(&[recorded]);
    // The open runs as a background job (`background_jobs`, as on the shells): pump it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while app.session.documents().len() <= before && std::time::Instant::now() < deadline {
        photocraft_ui_egui::jobs_ui::tick(&mut app, &ctx);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert_eq!(app.session.documents().len(), before + 1, "reopening the recent entry opens the document");
    assert!(!app.ui.status_error, "status={}", app.ui.status);
    let _ = std::fs::remove_dir_all(&root);
}
