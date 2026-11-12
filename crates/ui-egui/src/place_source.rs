//! The question Place asks on a device with a photo album (the iPad shell presents PHPicker
//! through [`Services::photo_album`](crate::Services::photo_album)): read the image from the
//! album, or from files? The album answer runs the same continuation as the Files picker's —
//! only where the bytes come from differs. Hosts without the service never ask, and Place
//! starts straight at the Files picker, as before.

use serde_json::Value;

use crate::{DocId, PhotocraftApp};

/// A Place waiting on its source choice, held in [`crate::UiState::place_source`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Ask {
    /// Place Linked (`file.placeLinked`) rather than Place Embedded.
    pub linked: bool,
    /// The document to place into, restored before the placed bytes land.
    pub doc: DocId,
}

/// What the shared place continuation does once a source delivered the file
/// (the body the Files picker's closure had): refocus the document, place the
/// bytes, and start Free Transform when the layer lands synchronously.
pub(crate) fn place_picked(
    app: &mut PhotocraftApp,
    name: &str,
    bytes: Vec<u8>,
    linked: bool,
    doc: DocId,
    ctx: &egui::Context,
) -> Result<Value, String> {
    if app.ui.transform.is_some() {
        return Err("Commit or cancel the transform first".into());
    }
    app.refocus(doc)?;
    let link = linked.then(|| name.to_string());
    let result = app.place_bytes(name, bytes, link)?;
    // In the background, Free Transform starts when the layer has landed.
    if result.get("frame").and_then(Value::as_u64).is_none() && !crate::jobs_ui::track_place(app, name, &result) {
        crate::transform_tool::begin_placed(app, ctx)?;
    }
    Ok(result)
}

/// Draw the source question while one waits ([`Ask`], set by the Place commands). Answering
/// starts the album picker or the Files picker; Cancel (or Esc) drops the place. Called first
/// thing in the frame's UI, before any panel could open another dialog.
pub(crate) fn show(app: &mut PhotocraftApp, ctx: &egui::Context) {
    let Some(ask) = app.ui.place_source else { return };
    // Something else took over while the question waited (a canvas drop placed, a transform
    // began): the place is off rather than fighting for the dialog.
    if app.ui.transform.is_some() || app.jobs.place.is_some() || app.file_dialog_open() {
        app.ui.place_source = None;
        return;
    }
    enum Answer {
        Album,
        Files,
        Cancel,
    }
    let mut answer = None;
    egui::Modal::new(egui::Id::unique("place-source")).show(ctx, |ui| {
        ui.set_width(240.0);
        ui.heading(tl!("Place From"));
        ui.add_space(6.0);
        if ui.button(tl!("Photo Library")).clicked() {
            answer = Some(Answer::Album);
        }
        if ui.button(tl!("Files…")).clicked() {
            answer = Some(Answer::Files);
        }
        if ui.button(tl!("Cancel")).clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            answer = Some(Answer::Cancel);
        }
    });
    let Some(answer) = answer else { return };
    app.ui.place_source = None;
    if matches!(answer, Answer::Cancel) {
        return;
    }
    let (linked, doc) = (ask.linked, ask.doc);
    let (ctx, wake) = (ctx.clone(), ctx.clone());
    let then = move |app: &mut PhotocraftApp, name: String, bytes: Vec<u8>| place_picked(app, &name, bytes, linked, doc, &ctx);
    let result = match answer {
        Answer::Album => app.pick_album_bytes(then),
        Answer::Files => app.pick_file_bytes(then),
        Answer::Cancel => return,
    };
    if let Err(e) = result
        && e != crate::file_dialog::CANCELLED
    {
        app.ui.status = e;
        app.ui.status_error = true;
    }
    wake.request_repaint();
}
