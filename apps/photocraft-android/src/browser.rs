//! The in-app file browser behind `services.file_dialog`.
//!
//! Android's system pickers (Storage Access Framework) only answer through
//! `onActivityResult`, which NativeActivity does not forward to native code — wiring it would
//! mean shipping the Games SDK's Java glue. Instead an Open dialog is a three-tab picker
//! (as on a phone: pick from the **Gallery** (MediaStore over JNI, camera RAW included), from
//! **Projects** (the app's own saved documents — nothing there is ever lost), or from
//! **Files**), and a Save dialog is the name field over the Files tab. The Files tab walks
//! the file system directly, rooted as far as the system allows:
//!
//! - With *all files access* granted (`MANAGE_EXTERNAL_STORAGE`, the manifest asks for it) the
//!   tab roots at the whole shared storage — works through Android 15; grant it once in
//!   Settings ▸ Apps ▸ PhotoComfy ▸ All files access.
//! - The APK targets 29 with `requestLegacyExternalStorage`, so the ordinary
//!   READ/WRITE_EXTERNAL_STORAGE pair (the Gallery tab's "Allow photo access" button asks for
//!   it) carries the same full raw-path view on later versions too.
//! - With no grant at all the tab roots at the app's own folder on shared storage —
//!   `/storage/emulated/0/Android/data/com.photocomfy.app/files` — which every Android
//!   version lets the app and **adb over USB** read and write with no permission.
//!
//! Answers carry real paths where the file system has them (replace prompts, in-place saves
//! and the recent-files list work as on the desktop) and contents where the source is a
//! MediaStore stream (the Gallery tab; the iPad shell's picker answers the same way).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use chrono::TimeZone as _;
use photocraft_ui_egui::{FileDialogAnswer, FileDialogReply};

use crate::gallery::{GalleryEvent, GallerySource, GalleryTab};

/// The shared storage root every Android device exposes once all-files access is granted.
pub const STORAGE_ROOT: &str = "/storage/emulated/0";

/// Whether the browser may write the shared storage (the all-files-access grant — or the
/// target-29 legacy view the storage permissions carry — probed by creating a file: no JNI
/// needed for the check itself). Listing is NOT a probe — Android 11+ FUSE lets an app with
/// no storage permission at all enumerate the root (showing only its own entries), so
/// `read_dir` said "granted" on an ungranted device, the browser rooted at the whole shared
/// storage, and every save failed with Permission denied.
pub fn storage_granted() -> bool {
    let probe = std::path::Path::new(STORAGE_ROOT).join(format!("photocomfy-write-probe-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Where the Files tab is rooted: the whole shared storage when a grant holds, else the app's
/// own directory on shared storage (`Android/data/<pkg>/files`), which every Android version
/// lets the app — and adb over USB — read and write with no permission at all. Returns the
/// root and whether a grant held (the session caches both; probing from the frame loop is
/// what the banner must not do). Off-device (tests) the temp dir stands in for the app folder.
fn browser_root() -> (PathBuf, bool) {
    #[cfg(target_os = "android")]
    {
        if storage_granted() {
            return (PathBuf::from(STORAGE_ROOT), true);
        }
        let dir = crate::platform::external_files_dir().unwrap_or_else(|| PathBuf::from(STORAGE_ROOT));
        let _ = std::fs::create_dir_all(&dir);
        (dir, false)
    }
    #[cfg(not(target_os = "android"))]
    {
        (std::env::temp_dir(), true)
    }
}

/// The app's own project library on shared storage: `Android/data/<pkg>/files/Projects`, the
/// default save folder and the Projects tab's source. Off-device the temp dir stands in.
/// Created on demand; a device that hands over no app folder degrades to the storage root.
pub fn projects_root() -> PathBuf {
    let base = {
        #[cfg(target_os = "android")]
        {
            crate::platform::external_files_dir().unwrap_or_else(|| PathBuf::from(STORAGE_ROOT))
        }
        #[cfg(not(target_os = "android"))]
        {
            std::env::temp_dir()
        }
    };
    let dir = base.join("Projects");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// What the shell should do after a frame of the browser UI.
pub enum Action {
    /// Nothing happened worth acting on (the browser may still be open).
    None,
    /// The user tapped the grant button: open the system all-files-access settings page.
    RequestGrant,
}

/// The picker's tabs, in bar order.
#[derive(PartialEq)]
enum Tab {
    Gallery,
    Projects,
    Files,
}

impl Tab {
    fn label(&self, active: bool) -> String {
        // Brackets mark the active tab: plain buttons take taps on the device where
        // selectable labels did not, so the bar stays buttons.
        let name = match self {
            Tab::Gallery => "Gallery",
            Tab::Projects => "Projects",
            Tab::Files => "Files",
        };
        if active { format!("[{name}]") } else { name.into() }
    }
}

/// One saved document the Projects tab lists.
struct ProjectEntry {
    path: PathBuf,
    name: String,
    size: u64,
    modified: Option<SystemTime>,
}

enum Entry {
    Dir(String),
    File(String),
}

/// The Projects tab: every saved document under [`projects_root`], newest first.
struct ProjectsTab {
    entries: Vec<ProjectEntry>,
    error: Option<String>,
    loaded: bool,
}

impl ProjectsTab {
    fn new() -> Self {
        Self { entries: Vec::new(), error: None, loaded: false }
    }

    fn reload(&mut self) {
        self.entries = scan_projects(&projects_root());
        self.error = (self.entries.is_empty()).then(|| "No saved projects yet. Save As puts new ones here.".into());
        self.loaded = true;
    }
}

/// The file extensions the Projects tab lists: the native bundle plus Photoshop's.
const PROJECT_EXTENSIONS: [&str; 3] = ["pcraft", "psd", "psb"];

/// Walk the projects folder for saved documents, newest first. Bounded (depth 8, 2000
/// entries): the folder is the app's own and small, and a hostile tree must not hang the
/// picker.
fn scan_projects(root: &Path) -> Vec<ProjectEntry> {
    let mut found: Vec<ProjectEntry> = Vec::new();
    let mut stack: Vec<(PathBuf, u32)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(listing) = std::fs::read_dir(&dir) else { continue };
        for item in listing {
            if found.len() >= 2000 {
                return finish(found);
            }
            let Some(item) = item.ok() else { continue };
            let Ok(file_type) = item.file_type() else { continue };
            let Some(name) = item.file_name().into_string().ok() else { continue };
            if name.starts_with('.') {
                continue;
            }
            if file_type.is_dir() {
                if depth < 8 {
                    stack.push((dir.join(name), depth + 1));
                }
            } else if file_type.is_file()
                && name.rsplit_once('.').is_some_and(|(_, ext)| {
                    PROJECT_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
                })
                && let Ok(meta) = item.metadata()
            {
                found.push(ProjectEntry {
                    path: dir.join(&name),
                    name,
                    size: meta.len(),
                    modified: meta.modified().ok(),
                });
            }
        }
    }
    finish(found)
}

/// Newest first, then by name: the project just saved is the first row.
fn finish(mut found: Vec<ProjectEntry>) -> Vec<ProjectEntry> {
    found.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.name.cmp(&b.name)));
    found
}

/// One browser session: the request's answer channel is held until a row is picked or the
/// browser is cancelled, exactly like an open system dialog.
struct Active {
    save: bool,
    tab: Tab,
    root: PathBuf,
    dir: PathBuf,
    /// Whether a storage grant held when this dialog opened (the banner's condition).
    all_files: bool,
    filter: Vec<String>,
    reply: Option<FileDialogReply>,
    entries: Vec<Entry>,
    error: Option<String>,
    name: String,
    hint_dismissed: bool,
    gallery: GalleryTab,
    projects: ProjectsTab,
}

/// The shared state behind `Services::file_dialog` (called on the UI thread by the app's
/// frame loop) and the shell's overlay drawing (the same thread, never concurrent).
pub struct Browser {
    active: Mutex<Option<Active>>,
    last_dir: Mutex<Option<PathBuf>>,
    gallery: Arc<dyn GallerySource>,
}

impl Browser {
    /// The browser with the device's gallery (MediaStore on Android; a stub off-device, where
    /// tests inject their own source through [`Browser::with_gallery`]).
    pub fn shared() -> Arc<Self> {
        #[cfg(target_os = "android")]
        let source: Arc<dyn GallerySource> = crate::gallery::AndroidMediaStore::shared();
        #[cfg(not(target_os = "android"))]
        let source: Arc<dyn GallerySource> = Arc::new(crate::gallery::NoGallery);
        Self::with_gallery(source)
    }

    pub fn with_gallery(gallery: Arc<dyn GallerySource>) -> Arc<Self> {
        Arc::new(Self { active: Mutex::new(None), last_dir: Mutex::new(None), gallery })
    }

    fn lock(&self) -> MutexGuard<'_, Option<Active>> {
        self.active.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn is_active(&self) -> bool {
        self.lock().is_some()
    }

    /// File ▸ Open and friends. One file is picked per run; the app's multi-open sorts and
    /// opens each result, so answering with a one-element `Paths` (or one `Contents`) stays
    /// correct. The default tab is the Gallery for an unfiltered Open — the phone's way in —
    /// and Files when the request filters extensions (a script or preset, not a photo).
    pub fn begin_open(
        &self,
        multiple: bool,
        initial_dir: Option<String>,
        extensions: Option<Vec<String>>,
        reply: FileDialogReply,
    ) {
        let _ = multiple;
        let filter = extensions.clone().unwrap_or_default().into_iter().map(|e| e.to_ascii_lowercase()).collect();
        let tab = if extensions.as_ref().is_some_and(|e| !e.is_empty()) { Tab::Files } else { Tab::Gallery };
        let (root, all_files) = browser_root();
        let dir = self.resolve_start(initial_dir.as_deref(), &root);
        let mut active = Active {
            save: false,
            tab,
            root,
            dir,
            all_files,
            filter,
            reply: Some(reply),
            entries: Vec::new(),
            error: None,
            name: String::new(),
            hint_dismissed: false,
            gallery: GalleryTab::new(Arc::clone(&self.gallery)),
            projects: ProjectsTab::new(),
        };
        active.reload_files();
        *self.lock() = Some(active);
    }

    /// Save As and the export flows: a name field over the Files tab. A document that already
    /// has a folder starts there (re-saving stays in place); a new one starts in the app's
    /// Projects folder, so every save accumulates in the app instead of scattering.
    pub fn begin_save(&self, suggested: String, reply: FileDialogReply) {
        let (root, all_files) = browser_root();
        let suggested_path = Path::new(&suggested);
        let dir = suggested_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty() && p.is_dir())
            .map(Path::to_path_buf)
            .unwrap_or_else(projects_root);
        let name = suggested_path
            .file_name()
            .map_or(suggested.clone(), |n| n.to_string_lossy().into_owned());
        let mut active = Active {
            save: true,
            tab: Tab::Files,
            root,
            dir,
            all_files,
            filter: Vec::new(),
            reply: Some(reply),
            entries: Vec::new(),
            error: None,
            name,
            hint_dismissed: false,
            gallery: GalleryTab::new(Arc::clone(&self.gallery)),
            projects: ProjectsTab::new(),
        };
        active.reload_files();
        *self.lock() = Some(active);
    }

    /// Where the Files tab starts: the request's directory, else the last visited one, else
    /// the root's Download folder, else the root; the first that exists wins.
    fn resolve_start(&self, initial_dir: Option<&str>, root: &Path) -> PathBuf {
        let last = self.last_dir.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let download = root.join("Download");
        initial_dir
            .map(PathBuf::from)
            .into_iter()
            .chain(last)
            .chain([download, root.to_path_buf()])
            .find(|p| p.is_dir())
            .unwrap_or_else(|| root.to_path_buf())
    }

    /// Draw the browser over the app (call after `app.ui_frame`, so this modal eats the
    /// input while it is up).
    pub fn draw(&self, ui: &mut egui::Ui) -> Action {
        if !self.is_active() {
            return Action::None;
        }
        let mut guard = self.lock();
        let Some(active) = guard.as_mut() else { return Action::None };
        // Cached per session (a live probe on every frame wedged the frame loop on the
        // emulator): decide the banner once, when the dialog opens.
        let granted = active.all_files;
        // A full-screen panel, not a floating layer: every overlay the shell showed first
        // (Modal, Window, Area) rendered on Android yet never hit-tested — input reached the
        // shell's hook and no widget answered — while the app's own panels and popups always
        // worked. A panel drawn before the app's takes the whole screen for the frame (the
        // app then has no space left), and being the same kind of layer as the app's own UI,
        // it hit-tests like it too.
        let mut outcome = Outcome { answer: None, action: Action::None };
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(ui.visuals().panel_fill).inner_margin(24.0))
            .show(ui, |ui| {
                ui.set_min_width(620.0);
                ui.horizontal(|ui| {
                    ui.heading(if active.save { "Save as" } else { "Open file" });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Cancel").clicked() {
                            outcome.answer = Some(None);
                        }
                        // ASCII only: every glyph here must exist in the embedded font set, a
                        // missing one is a hard epaint panic (the Refresh spiral and the curly
                        // quotes below froze the whole frame loop on device until they became
                        // text).
                        if ui.small_button("Refresh").clicked() {
                            active.reload_current(ui.ctx());
                        }
                    });
                });
                if !active.save {
                    active.draw_tabs(ui);
                }
                if matches!(active.tab, Tab::Files) {
                    draw_files_banner(ui, active, granted, sdk(), &mut outcome);
                    ui.add_space(4.0);
                    ui.monospace(active.dir.to_string_lossy());
                    ui.add_space(8.0);
                    if active.save {
                        active.draw_save_row(ui, &mut outcome);
                        ui.add_space(8.0);
                    }
                    active.draw_files_rows(ui, &mut outcome);
                } else if matches!(active.tab, Tab::Projects) {
                    active.draw_projects(ui, &mut outcome);
                } else if let GalleryEvent::Picked(name, bytes) = active.gallery.draw(ui) {
                    outcome.answer = Some(Some(FileDialogAnswer::Contents(name, bytes)));
                }
                if let Some(error) = active.error.clone() {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
            });
        // Deliver outside the panel closure, then close: the reply channel wakes the app's
        // pending file dialog, which continues on the next frame.
        if let Some(answer) = outcome.answer {
            let dir = active.dir.clone();
            if let Some(reply) = active.reply.take() {
                reply.send(answer);
            }
            *self.last_dir.lock().unwrap_or_else(PoisonError::into_inner) = Some(dir);
            drop(guard);
            *self.lock() = None;
        }
        outcome.action
    }
}

/// What a frame of the browser produced.
struct Outcome {
    answer: Option<Option<FileDialogAnswer>>,
    action: Action,
}

/// The banner's SDK level, pinned off-device so the ≤35 branch (the one with the settings
/// button) is what the tests see.
fn sdk() -> i32 {
    #[cfg(target_os = "android")]
    {
        crate::platform::sdk_version()
    }
    #[cfg(not(target_os = "android"))]
    {
        0
    }
}

/// Without a storage grant the Files tab is rooted at the app's own folder: say why, and how
/// to see more, once per session. On 36+ the all-files grant no longer exists for ordinary
/// apps, so the advice is the Gallery's grant (which carries the legacy view) and the USB
/// transfer path instead of a settings toggle that no longer works.
fn draw_files_banner(ui: &mut egui::Ui, active: &mut Active, granted: bool, sdk: i32, outcome: &mut Outcome) {
    if granted || active.hint_dismissed {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        // Off-device (tests) the ≤35 advice is the branch that needs a button, so the split
        // is pinned instead of cfg-ed away.
        if sdk >= 36 {
            ui.label(
                "Only this app folder is reachable. Tap Gallery and allow photo access to also \
                 browse the device's picture folders here; other files copy over USB \
                 (Android/data/com.photocomfy.app/files).",
            );
        } else {
            ui.label("Only this app folder is listed. 'All files access' in system settings shows the whole device.");
            if ui.small_button("Open settings").clicked() {
                outcome.action = Action::RequestGrant;
            }
        }
        if ui.small_button("Dismiss").clicked() {
            active.hint_dismissed = true;
        }
    });
}

impl Active {
    fn reload_current(&mut self, ctx: &egui::Context) {
        match self.tab {
            Tab::Gallery => self.gallery.refresh(ctx),
            Tab::Projects => self.projects.reload(),
            Tab::Files => self.reload_files(),
        }
    }

    fn draw_tabs(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let mut next = None;
            for tab in [Tab::Gallery, Tab::Projects, Tab::Files] {
                if ui.button(tab.label(tab == self.tab)).clicked() {
                    next = Some(tab);
                }
            }
            if let Some(tab) = next {
                self.tab = tab;
            }
        });
    }

    fn draw_save_row(&mut self, ui: &mut egui::Ui, outcome: &mut Outcome) {
        ui.horizontal(|ui| {
            ui.label("Name");
            let edit = ui.text_edit_singleline(&mut self.name);
            if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
                && let Some(picked) = self.save_answer()
            {
                outcome.answer = Some(picked);
            }
            if ui.button("Save").clicked()
                && let Some(picked) = self.save_answer()
            {
                outcome.answer = Some(picked);
            }
        });
    }

    /// The Files tab's listing, scrolling: a phone's folders overflow one screen and the
    /// finger expects to throw them (the touch UI widens the scroll bar).
    fn draw_files_rows(&mut self, ui: &mut egui::Ui, outcome: &mut Outcome) {
        egui::ScrollArea::vertical()
            .id_salt("files-rows")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let can_go_up = self.dir != self.root;
                if can_go_up
                    && ui.button("..").clicked()
                    && let Some(parent) = self.dir.parent()
                {
                    self.dir = parent.to_path_buf();
                    self.reload_files();
                }
                // Index loop with the row data taken out first: entering a folder mutates the
                // listing mid-iteration.
                for index in 0..self.entries.len() {
                    let Some((label, name, is_dir)) =
                        self.entries.get(index).map(|entry| match entry {
                            // Text prefixes, not emoji: the embedded font set has no glyphs
                            // for them, and a missing glyph is a hard panic in epaint.
                            Entry::Dir(name) => (format!("[dir]  {name}"), name.clone(), true),
                            Entry::File(name) => (format!("       {name}"), name.clone(), false),
                        })
                    else {
                        continue;
                    };
                    // A button, not selectable_label: on the device every button the shell
                    // drew took its tap, every selectable_label row rendered but never did.
                    if ui.button(label).clicked() {
                        if is_dir {
                            self.dir.push(name);
                            self.reload_files();
                            break;
                        }
                        if self.save {
                            // Tapping a file in save mode fills the name field; the replace
                            // prompt (if any) is the app's, driven by `file_stamp`.
                            self.name = name;
                        } else {
                            let path = self.dir.join(name).to_string_lossy().into_owned();
                            outcome.answer = Some(Some(FileDialogAnswer::Paths(vec![path])));
                            break;
                        }
                    }
                }
            });
    }

    /// The Projects tab: every saved document, newest first, one tap to open. Scrolling,
    /// like the Files tab — libraries grow.
    fn draw_projects(&mut self, ui: &mut egui::Ui, outcome: &mut Outcome) {
        if !self.projects.loaded {
            self.projects.reload();
        }
        ui.label(format!(
            "Saved documents stay in the app: {}",
            projects_root().to_string_lossy()
        ));
        ui.add_space(4.0);
        if let Some(error) = self.projects.error.clone() {
            ui.label(error);
            return;
        }
        egui::ScrollArea::vertical()
            .id_salt("projects-rows")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for index in 0..self.projects.entries.len() {
                    let Some(entry) = self.projects.entries.get(index) else { continue };
                    let label = format!(
                        "{}  {}  {}",
                        entry.name,
                        date_text(entry.modified),
                        size_text(entry.size)
                    );
                    if ui.button(label).clicked() {
                        let path = entry.path.to_string_lossy().into_owned();
                        outcome.answer = Some(Some(FileDialogAnswer::Paths(vec![path])));
                        break;
                    }
                }
            });
    }

    fn reload_files(&mut self) {
        self.entries.clear();
        self.error = None;
        match std::fs::read_dir(&self.dir) {
            Ok(listing) => {
                let mut dirs: Vec<String> = Vec::new();
                let mut files: Vec<String> = Vec::new();
                for item in listing {
                    let Some(item) = item.ok() else { continue };
                    let Ok(file_type) = item.file_type() else { continue };
                    let Some(name) = item.file_name().into_string().ok() else { continue };
                    if name.starts_with('.') {
                        continue;
                    }
                    if file_type.is_dir() {
                        dirs.push(name);
                    } else if file_type.is_file() && self.passes_filter(&name) {
                        files.push(name);
                    }
                }
                dirs.sort();
                files.sort();
                self.entries = dirs.into_iter().map(Entry::Dir).chain(files.into_iter().map(Entry::File)).collect();
            }
            Err(e) => self.error = Some(format!("Can't read the folder: {e}")),
        }
    }

    fn passes_filter(&self, name: &str) -> bool {
        if self.filter.is_empty() {
            return true;
        }
        name.rsplit_once('.')
            .is_some_and(|(_, ext)| self.filter.contains(&ext.to_ascii_lowercase()))
    }

    fn save_answer(&mut self) -> Option<Option<FileDialogAnswer>> {
        let name = self.name.trim();
        if name.is_empty() {
            self.error = Some("Enter a file name first".into());
            return None;
        }
        // `pick_save` appends the suggested extension to a bare answer, so the final write
        // path carries one even if the user typed a bare name.
        let path = self.dir.join(name).to_string_lossy().into_owned();
        Some(Some(FileDialogAnswer::SaveTo(path)))
    }
}

/// "2026-11-11"-style date for a file's mtime, local time, blank when unknown.
fn date_text(modified: Option<SystemTime>) -> String {
    let Some(time) = modified else { return String::new() };
    let Ok(secs) = time.duration_since(SystemTime::UNIX_EPOCH) else { return String::new() };
    match chrono::Local.timestamp_opt(secs.as_secs() as i64, 0) {
        chrono::LocalResult::Single(t) => t.format("%Y-%m-%d").to_string(),
        _ => String::new(),
    }
}

/// "1.4 MB"-style size, binary units.
fn size_text(bytes: u64) -> String {
    let units = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < units.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value < 10.0 {
        format!("{value:.1} {}", units[unit])
    } else {
        format!("{value:.0} {}", units[unit])
    }
}
