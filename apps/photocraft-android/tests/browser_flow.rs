//! The file browser's click flow, replayed off-device (the ui-egui `ios_tap_flow_tests`
//! pattern): a dialog opens over a harness-drawn app, a directory row enters the folder, a
//! file row answers `Paths`, Cancel answers nothing. The emulator round showed the browser
//! rendering but never hit-testing when drawn by the Android shell, so the flow is pinned
//! here where the harness's pointer is authoritative.

use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;
use photocraft_android::browser::{Action, Browser};
use photocraft_ui_egui::{FileDialogAnswer, FileDialogReply};

/// Draws the browser window over an empty app, as the Android shell's `ui` pass does.
struct Shell {
    browser: Arc<Browser>,
}

impl eframe::App for Shell {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // The device's Touch Interface zoom (the shell's default), so the printed row rects
        // match what to tap on the emulator.
        ui.ctx().set_zoom_factor(1.4);
        if let Action::RequestGrant = self.browser.draw(ui) {
            panic!("the grant jump is not wired in the harness");
        }
    }
}

/// A `FileDialogReply` whose answer lands in a channel we can await, built the way
/// `poll_file_dialog` builds one.
fn reply(ctx: &egui::Context) -> (FileDialogReply, mpsc::Receiver<Option<FileDialogAnswer>>) {
    let (tx, rx) = mpsc::channel();
    (FileDialogReply::new(tx, ctx.clone()), rx)
}

fn harness(browser: &Arc<Browser>) -> Harness<'static, Shell> {
    let mut harness: Harness<'static, Shell> =
        Harness::builder().with_size(egui::vec2(914.0, 571.0)).with_pixels_per_point(2.0).build_eframe(|cc| {
            photocraft_ui_egui::PhotocraftApp::setup_context(&cc.egui_ctx, Default::default());
            Shell { browser: browser.clone() }
        });
    harness.run_steps(2);
    harness
}

fn recv(rx: &mpsc::Receiver<Option<FileDialogAnswer>>) -> Option<Option<FileDialogAnswer>> {
    rx.recv_timeout(Duration::from_secs(2)).ok()
}

#[test]
fn tapping_a_file_row_answers_its_path() {
    let dir = std::env::temp_dir().join(format!("pc-browser-test-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("Download")).expect("fixture dir");
    std::fs::write(dir.join("Download").join("picture.png"), b"png").expect("fixture file");

    let browser = Browser::shared();
    let mut harness = harness(&browser);

    // The shell's file_dialog service routes an Open request into the browser; starting in
    // the fixture root lists the Download folder as a row.
    let (reply, rx) = reply(&harness.ctx);
    browser.begin_open(true, Some(dir.to_string_lossy().into_owned()), None, reply);
    harness.run_steps(3);
    assert!(browser.is_active(), "the browser opens");

    harness.get_by_label("[dir]  Download").click();
    harness.run_steps(3);
    let row = harness.get_by_label("       picture.png");
    println!("file-row logical rect: {:?}", row.rect());
    row.click();
    harness.run_steps(5);

    let answer = recv(&rx).expect("an answer arrives");
    assert_eq!(
        answer,
        Some(FileDialogAnswer::Paths(vec![dir.join("Download").join("picture.png").to_string_lossy().into_owned()]))
    );
    assert!(!browser.is_active(), "the browser closed after answering");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cancel_answers_none_and_closes() {
    let dir = std::env::temp_dir().join(format!("pc-browser-cancel-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("fixture dir");

    let browser = Browser::shared();
    let mut harness = harness(&browser);
    let (reply, rx) = reply(&harness.ctx);
    browser.begin_open(false, Some(dir.to_string_lossy().into_owned()), None, reply);
    harness.run_steps(3);

    harness.get_by_label("Cancel").click();
    harness.run_steps(5);

    assert_eq!(recv(&rx), Some(None));
    assert!(!browser.is_active());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn save_mode_answers_the_prefilled_path() {
    // A fresh browser has no last-visited folder, so Save starts at the root (the temp dir
    // off-device) and the prefilled name joins there.
    let browser = Browser::shared();
    let mut harness = harness(&browser);
    let (reply, rx) = reply(&harness.ctx);
    browser.begin_save("artwork.png".into(), reply);
    harness.run_steps(3);
    assert!(browser.is_active());

    // The suggested name is prefilled, so Save answers at once; appending the suggested
    // extension to a bare answer stays the app's job (`pick_save`).
    harness.get_by_label("Save").click();
    harness.run_steps(5);

    assert_eq!(
        recv(&rx),
        Some(Some(FileDialogAnswer::SaveTo(std::env::temp_dir().join("artwork.png").to_string_lossy().into_owned())))
    );
    assert!(!browser.is_active());
}
