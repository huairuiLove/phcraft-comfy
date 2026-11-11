//! The in-app file browser behind `services.file_dialog`.
//!
//! Android's system pickers (Storage Access Framework) only answer through
//! `onActivityResult`, which NativeActivity does not forward to native code — wiring it would
//! mean shipping the Games SDK's Java glue. Instead the browser walks the file system
//! directly, rooted as far as the system allows: the whole shared storage when the user has
//! granted *all files access* (`MANAGE_EXTERNAL_STORAGE` in settings — works through Android
//! 15), else the app's own folder on shared storage (`Android/data/com.photocomfy.app/files`),
//! which every Android version lets the app and adb-over-USB read and write with no permission
//! (Android 16 removed the grant for ordinary apps, so there this is the only root). Real
//! paths either way: replace prompts, File ▸ Save in place and the recent-files list work as
//! on the desktop.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use photocraft_ui_egui::{FileDialogAnswer, FileDialogReply};

/// The shared storage root every Android device exposes once all-files access is granted.
pub const STORAGE_ROOT: &str = "/storage/emulated/0";

/// Whether the browser may list the shared storage (the all-files-access grant, probed by
/// reading the root: no JNI needed for the check itself). Android 16 removed the grant for
/// ordinary apps; there the browser roots at the app's own folder instead (`browser_root`).
pub fn storage_granted() -> bool {
    std::fs::read_dir(STORAGE_ROOT).is_ok()
}

/// Where the browser is rooted: the whole shared storage with all-files access, else the
/// app's own directory on shared storage (`Android/data/<pkg>/files`), which every Android
/// version lets the app — and adb over USB — read and write with no permission at all.
/// Returns the root and whether the all-files grant held (the session caches both; probing
/// from the frame loop is what the banner must not do). Off-device (tests) the temp dir
/// stands in for the app folder.
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

/// What the shell should do after a frame of the browser UI.
pub enum Action {
    /// Nothing happened worth acting on (the browser may still be open).
    None,
    /// The user tapped the grant button: open the system all-files-access settings page.
    RequestGrant,
}

/// One browser session: the request's answer channel is held until a row is picked or the
/// browser is cancelled, exactly like an open system dialog.
struct Active {
    save: bool,
    root: PathBuf,
    dir: PathBuf,
    /// Whether all-files access held when this dialog opened (the banner's condition).
    all_files: bool,
    filter: Vec<String>,
    reply: Option<FileDialogReply>,
    entries: Vec<Entry>,
    error: Option<String>,
    name: String,
    hint_dismissed: bool,
}

enum Entry {
    Dir(String),
    File(String),
}

/// The shared state behind `Services::file_dialog` (called on the UI thread by the app's
/// frame loop) and the shell's overlay drawing (the same thread, never concurrent).
pub struct Browser {
    active: Mutex<Option<Active>>,
    last_dir: Mutex<Option<PathBuf>>,
}

impl Browser {
    pub fn shared() -> Arc<Self> {
        Arc::new(Self { active: Mutex::new(None), last_dir: Mutex::new(None) })
    }

    fn lock(&self) -> MutexGuard<'_, Option<Active>> {
        self.active.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn is_active(&self) -> bool {
        self.lock().is_some()
    }

    /// File ▸ Open and friends. One file is picked per run; the app's multi-open sorts and
    /// opens each result, so answering with a one-element `Paths` stays correct.
    pub fn begin_open(
        &self,
        multiple: bool,
        initial_dir: Option<String>,
        extensions: Option<Vec<String>>,
        reply: FileDialogReply,
    ) {
        let _ = multiple;
        let filter = extensions.unwrap_or_default().into_iter().map(|e| e.to_ascii_lowercase()).collect();
        let (root, all_files) = browser_root();
        let dir = self.resolve_start(initial_dir.as_deref(), &root);
        let mut active = Active {
            save: false,
            root,
            dir,
            all_files,
            filter,
            reply: Some(reply),
            entries: Vec::new(),
            error: None,
            name: String::new(),
            hint_dismissed: false,
        };
        active.reload();
        *self.lock() = Some(active);
    }

    /// Save As and the export flows: a name field over the same browser.
    pub fn begin_save(&self, suggested: String, reply: FileDialogReply) {
        let (root, all_files) = browser_root();
        let dir = self.resolve_start(None, &root);
        let name = Path::new(&suggested)
            .file_name()
            .map_or(suggested.clone(), |n| n.to_string_lossy().into_owned());
        let mut active = Active {
            save: true,
            root,
            dir,
            all_files,
            filter: Vec::new(),
            reply: Some(reply),
            entries: Vec::new(),
            error: None,
            name,
            hint_dismissed: false,
        };
        active.reload();
        *self.lock() = Some(active);
    }

    /// Where a new browser starts: the request's directory, else the last visited one, else
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
        let mut answer: Option<Option<FileDialogAnswer>> = None;
        let mut action = Action::None;
        // Cached per session (a live probe on every frame wedged the frame loop on the
        // emulator): decide the banner once, when the dialog opens.
        let granted = active.all_files;
        #[cfg(target_os = "android")]
        let sdk = crate::platform::sdk_version();
        // A full-screen panel, not a floating layer: every overlay the shell showed first
        // (Modal, Window, Area) rendered on Android yet never hit-tested — input reached the
        // shell's hook and no widget answered — while the app's own panels and popups always
        // worked. A panel drawn before the app's takes the whole screen for the frame (the
        // app then has no space left), and being the same kind of layer as the app's own UI,
        // it hit-tests like it too.
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(ui.visuals().panel_fill).inner_margin(24.0))
            .show(ui, |ui| {
                ui.set_min_width(620.0);
                ui.horizontal(|ui| {
                    ui.heading(if active.save { "Save as" } else { "Open file" });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Cancel").clicked() {
                            answer = Some(None);
                        }
                        // ASCII only: every glyph here must exist in the embedded font set, a
                            // missing one is a hard epaint panic (the Refresh spiral and the curly
                            // quotes below froze the whole frame loop on device until they became
                            // text).
                            if ui.small_button("Refresh").clicked() {
                                active.reload();
                            }
                        });
                    });
                // Without all-files access the browser is rooted at the app's own folder:
                // say why, and how to see everything, once per session. Android 16 removed
                // the grant for ordinary apps, so there the advice is the USB transfer
                // path instead of a settings toggle that no longer exists.
                if !granted && !active.hint_dismissed {
                    ui.horizontal_wrapped(|ui| {
                        // Off-device (tests) the ≤35 advice is the branch that needs a
                        // button, so pin the split instead of cfg-ing the whole banner.
                        #[cfg(target_os = "android")]
                        let new_android = sdk >= 36;
                        #[cfg(not(target_os = "android"))]
                        let new_android = false;
                        if new_android {
                            ui.label(
                                "Android 16: only this app folder is reachable. Copy files here over \
                                 USB (Android/data/com.photocomfy.app/files), or save here and fetch \
                                 them from the same folder.",
                            );
                        } else {
                            ui.label("Only this app folder is listed. 'All files access' in system settings shows the whole device.");
                            if ui.small_button("Open settings").clicked() {
                                action = Action::RequestGrant;
                            }
                        }
                        if ui.small_button("Dismiss").clicked() {
                            active.hint_dismissed = true;
                        }
                    });
                }
                ui.add_space(4.0);
                ui.monospace(active.dir.to_string_lossy());
                ui.add_space(8.0);
                if active.save {
                    ui.horizontal(|ui| {
                        ui.label("Name");
                        let edit = ui.text_edit_singleline(&mut active.name);
                        if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
                            && let Some(picked) = active.save_answer()
                        {
                            answer = Some(picked);
                        }
                        if ui.button("Save").clicked()
                            && let Some(picked) = active.save_answer()
                        {
                            answer = Some(picked);
                        }
                    });
                    ui.add_space(8.0);
                }
                // No ScrollArea: on the device its content swallowed taps on the rows (buttons outside
                // it hit, rows inside never did); the panel is tall enough for a folder's first
                // screen and Refresh re-lists after changes.
                {
                    let can_go_up = active.dir != active.root;
                    if can_go_up
                        && ui.button("..").clicked()
                        && let Some(parent) = active.dir.parent()
                    {
                        active.dir = parent.to_path_buf();
                        active.reload();
                    }
                    // Index loop with the row data taken out first: entering a folder
                    // mutates the listing mid-iteration.
                    for index in 0..active.entries.len() {
                        let Some((label, name, is_dir)) = active.entries.get(index).map(|entry| match entry {
                            // Text prefixes, not emoji: the embedded font set has no glyphs
                            // for them, and a missing glyph is a hard panic in epaint — the
                            // Save browser died on open until these became text.
                            Entry::Dir(name) => (format!("[dir]  {name}"), name.clone(), true),
                            Entry::File(name) => (format!("       {name}"), name.clone(), false),
                        }) else {
                            continue;
                        };
                        // A button, not selectable_label: on the device every button the shell drew took
                        // its tap, every selectable_label row rendered but never did.
                        if ui.button(label).clicked() {
                            if is_dir {
                                active.dir.push(name);
                                active.reload();
                                break;
                            }
                            if active.save {
                                // Tapping a file in save mode fills the name field; the
                                // replace prompt (if any) is the app's, driven by
                                // `file_stamp`.
                                active.name = name;
                            } else {
                                let path = active.dir.join(name).to_string_lossy().into_owned();
                                answer = Some(Some(FileDialogAnswer::Paths(vec![path])));
                                break;
                            }
                        }
                    }
                }
                if let Some(error) = &active.error {
                    ui.colored_label(ui.visuals().error_fg_color, error.clone());
                }
            });
        // Deliver outside the panel closure, then close: the reply channel wakes the app's
        // pending file dialog, which continues on the next frame.
        if let Some(answer) = answer {
            let dir = active.dir.clone();
            if let Some(reply) = active.reply.take() {
                reply.send(answer);
            }
            *self.last_dir.lock().unwrap_or_else(PoisonError::into_inner) = Some(dir);
            drop(guard);
            *self.lock() = None;
        }
        action
    }
}

impl Active {
    fn reload(&mut self) {
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
