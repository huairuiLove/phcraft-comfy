//! The Gallery and Projects tabs' flow, replayed off-device with an injected fake gallery
//! (the `browser_flow` suite's pattern): the Gallery's permission gate answers through the
//! fake's grant, the listing renders, a row pick answers `Contents`, and the Projects tab
//! lists the app's saved documents newest first and opens one by path.

use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;
use photocraft_android::browser::{projects_root, Action, Browser};
use photocraft_android::gallery::{GalleryItem, GalleryPermission, GallerySource};
use photocraft_ui_egui::{FileDialogAnswer, FileDialogReply};

/// Draws the browser window over an empty app, as the Android shell's `ui` pass does.
struct Shell {
    browser: Arc<Browser>,
}

impl eframe::App for Shell {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if let Action::RequestGrant = self.browser.draw(ui) {
            panic!("the grant jump is not wired in the harness");
        }
    }
}

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

/// Pump frames until `done` holds or two seconds pass: the tabs' listings and picks land
/// through worker threads, which answer between frames.
fn wait_for(harness: &mut Harness<'static, Shell>, done: impl Fn(&Harness<'static, Shell>) -> bool) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        harness.run_steps(2);
        std::thread::sleep(Duration::from_millis(10));
        if done(harness) {
            return true;
        }
    }
    false
}

fn recv(rx: &mpsc::Receiver<Option<FileDialogAnswer>>) -> Option<Option<FileDialogAnswer>> {
    rx.recv_timeout(Duration::from_secs(2)).ok()
}

/// The fake gallery: one lockable state, granted only after the app asked (as the system
/// dialog's Allow would), fixed rows, bytes and a 2x2 thumbnail.
struct FakeGallery {
    granted: Mutex<bool>,
    asked: Mutex<bool>,
}

impl FakeGallery {
    fn new() -> Arc<Self> {
        Arc::new(Self { granted: Mutex::new(false), asked: Mutex::new(false) })
    }

    fn items() -> Vec<GalleryItem> {
        vec![
            GalleryItem {
                id: 7,
                name: "IMG_0001.jpg".into(),
                mime: "image/jpeg".into(),
                size: 2048,
                modified: 1_799_000_000,
            },
            GalleryItem {
                id: 8,
                name: "DSC002.arw".into(),
                mime: "image/x-sony-arw".into(),
                size: 45_000_000,
                modified: 1_798_000_000,
            },
        ]
    }
}

impl GallerySource for FakeGallery {
    fn permission(&self) -> GalleryPermission {
        if *self.granted.lock().unwrap_or_else(PoisonError::into_inner) {
            GalleryPermission::Granted
        } else {
            GalleryPermission::Missing
        }
    }

    fn request_permission(&self) -> Result<(), String> {
        *self.asked.lock().unwrap_or_else(PoisonError::into_inner) = true;
        *self.granted.lock().unwrap_or_else(PoisonError::into_inner) = true; // the user tapped Allow
        Ok(())
    }

    fn list(&self) -> Result<Vec<GalleryItem>, String> {
        if self.permission() == GalleryPermission::Missing {
            return Err("no permission".into());
        }
        Ok(Self::items())
    }

    fn read(&self, item: &GalleryItem) -> Result<Vec<u8>, String> {
        Ok(format!("bytes of {}", item.name).into_bytes())
    }

    fn thumbnail(&self, _item: &GalleryItem, _max: u32) -> Option<(u32, u32, Vec<u8>)> {
        Some((2, 2, vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255]))
    }
}

/// An unfiltered Open lands on the Gallery tab: the permission gate first, then the listing
/// (the RAW named as such), and a pick answers with the item's contents.
#[test]
fn gallery_gate_lists_and_picks_by_contents() {
    let gallery = FakeGallery::new();
    let browser = Browser::with_gallery(gallery.clone());
    let mut harness = harness(&browser);
    let (reply, rx) = reply(&harness.ctx);
    browser.begin_open(true, None, None, reply);
    harness.run_steps(3);
    assert!(browser.is_active(), "the browser opens");

    // The gate is the first thing an unfiltered Open shows.
    assert!(
        harness.query_by_label("PhotoComfy needs photo access to list the gallery.").is_some(),
        "the permission gate shows first"
    );
    harness.get_by_label("Allow photo access").click();
    harness.run_steps(2);
    // The grant lands without a callback (as on the device): Refresh re-checks and lists.
    // Two Refresh buttons share the screen while the gate is up (the header's and the gate's);
    // either reloads, so the first found is enough.
    harness.get_all_by_label("Refresh").next().expect("a Refresh button").click();
    harness.run_steps(2);
    let listed = wait_for(&mut harness, |h| h.query_by_label("IMG_0001.jpg  image  2.0 KB").is_some());
    assert!(listed, "the jpeg row lists after the grant");
    assert!(harness.query_by_label("DSC002.arw  RAW  43 MB").is_some(), "the RAW row lists with its kind");
    assert!(*gallery.asked.lock().unwrap_or_else(PoisonError::into_inner), "the source was asked");

    // A pick reads on a worker thread and answers with the item's contents.
    harness.get_by_label("IMG_0001.jpg  image  2.0 KB").click();
    let answered = wait_for(&mut harness, |_| !browser.is_active());
    assert!(answered, "the browser closed after the pick");
    assert_eq!(
        recv(&rx),
        Some(Some(FileDialogAnswer::Contents("IMG_0001.jpg".into(), b"bytes of IMG_0001.jpg".to_vec())))
    );
}

/// The Projects tab lists the app's saved documents, newest first, and opens one by path.
#[test]
fn projects_tab_lists_and_opens_newest_first() {
    let projects = projects_root();
    let _ = std::fs::remove_file(projects.join("old.psd"));
    let _ = std::fs::remove_dir_all(projects.join("sub"));
    let _ = std::fs::remove_file(projects.join("ignored.txt"));
    std::fs::create_dir_all(projects.join("sub")).expect("fixture dir");
    std::fs::write(projects.join("old.psd"), b"old").expect("old fixture");
    std::fs::write(projects.join("sub").join("nested.pcraft"), b"nested").expect("nested fixture");
    std::fs::write(projects.join("ignored.txt"), b"no").expect("ignored fixture");
    // Deterministic order: the nested one is the newest.
    set_modified(&projects.join("old.psd"), SystemTime::now() - Duration::from_secs(3600));
    set_modified(&projects.join("sub").join("nested.pcraft"), SystemTime::now());

    let browser = Browser::shared();
    let mut harness = harness(&browser);
    let (reply, rx) = reply(&harness.ctx);
    browser.begin_open(true, None, None, reply);
    harness.run_steps(3);
    harness.get_by_label("Projects").click();
    harness.run_steps(3);

    // The nested project leads (newest first); the non-project file never lists. The rows
    // carry name, date and size, so match on the name part.
    let nested = harness.get_by_label_contains("nested.pcraft");
    let old = harness.get_by_label_contains("old.psd");
    assert!(harness.query_by_label("ignored.txt").is_none(), "a non-project file stays out");
    assert!(nested.rect().min.y <= old.rect().min.y, "the newest project is the first row");

    nested.click();
    harness.run_steps(5);
    assert_eq!(
        recv(&rx),
        Some(Some(FileDialogAnswer::Paths(vec![
            projects.join("sub").join("nested.pcraft").to_string_lossy().into_owned()
        ])))
    );
    assert!(!browser.is_active());

    let _ = std::fs::remove_file(projects.join("old.psd"));
    let _ = std::fs::remove_dir_all(projects.join("sub"));
    let _ = std::fs::remove_file(projects.join("ignored.txt"));
}

/// A bare Save starts in the app's Projects folder: every save accumulates in the app.
#[test]
fn save_defaults_to_the_projects_folder() {
    let browser = Browser::shared();
    let mut harness = harness(&browser);
    let (reply, rx) = reply(&harness.ctx);
    browser.begin_save("gallery-flow-save.psd".into(), reply);
    harness.run_steps(3);
    assert!(browser.is_active());

    harness.get_by_label("Save").click();
    harness.run_steps(5);
    match recv(&rx) {
        Some(Some(FileDialogAnswer::SaveTo(path))) => assert_eq!(
            path,
            projects_root().join("gallery-flow-save.psd").to_string_lossy().into_owned(),
            "the save lands in the Projects folder"
        ),
        other => panic!("no save path answered: {other:?}"),
    }
    assert!(!browser.is_active());
}

/// Set a file's mtime the way the listing orders by (`std`'s FileTimes, no extra dep).
fn set_modified(path: &std::path::Path, when: SystemTime) {
    let file = std::fs::File::options().write(true).open(path).expect("open fixture");
    file.set_times(std::fs::FileTimes::new().set_modified(when)).expect("set mtime");
}
