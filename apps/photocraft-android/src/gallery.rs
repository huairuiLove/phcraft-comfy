//! The system gallery behind the browser's Gallery tab.
//!
//! Android's photo picker answers through `onActivityResult`, which a NativeActivity never
//! sees, so the gallery is queried, listed and read right here: MediaStore over JNI (the
//! Android adapter at the bottom; the platform module owns those calls) hands the rows to
//! this tab, which renders them as a scrollable list with streaming thumbnails and answers a
//! pick with the file's *contents* — the same answer shape the iPad shell's document picker
//! gives (`FileDialogAnswer::Contents`), so File ▸ Open places the image as on iPadOS.
//!
//! The [`GallerySource`] seam keeps the tab host-testable: off-device tests inject a fake
//! (rows, bytes, pixels) and drive the permission gate, the listing and a pick through the
//! same draw code the device runs.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, PoisonError};

/// One image the system indexed: a photo, a downloaded JPEG, or a camera RAW (an `image/*`
/// MIME in MediaStore, whatever the extension).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GalleryItem {
    pub id: i64,
    pub name: String,
    pub mime: String,
    pub size: i64,
    /// Seconds since the epoch.
    pub modified: i64,
}

/// Whether the source may list at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GalleryPermission {
    Granted,
    Missing,
}

/// The device's gallery. Implemented over MediaStore on Android ([`AndroidMediaStore`]) and by
/// fakes in the tests; every method may run on a worker thread.
pub trait GallerySource: Send + Sync {
    fn permission(&self) -> GalleryPermission;
    /// Show the system grant dialog; the grant lands without a callback (NativeActivity gets
    /// none), so the caller re-checks [`GallerySource::permission`] after the user chose.
    fn request_permission(&self) -> Result<(), String>;
    /// The gallery's images, newest first.
    fn list(&self) -> Result<Vec<GalleryItem>, String>;
    /// One item's bytes, whole.
    fn read(&self, item: &GalleryItem) -> Result<Vec<u8>, String>;
    /// One item's preview as RGBA bytes, when the system has one.
    fn thumbnail(&self, item: &GalleryItem, max: u32) -> Option<(u32, u32, Vec<u8>)>;
}

/// What the tab hands the browser while drawing.
pub enum GalleryEvent {
    /// Nothing to act on.
    None,
    /// The user picked an image: its display name and bytes, ready for
    /// `FileDialogAnswer::Contents`.
    Picked(String, Vec<u8>),
}

/// Pixels a worker decoded, keyed by item id; `None` = this item has no thumbnail.
type Pixels = Arc<Mutex<HashMap<i64, Option<(u32, u32, Vec<u8>)>>>>;

/// A finished read: the item's display name and bytes, or why it couldn't be read.
type ReadOut = Result<(String, Vec<u8>), String>;

/// The Gallery tab's state: the listing, the streaming thumbnails and the in-flight pick.
/// Created when the tab first draws; dropped (ending its worker thread) when the dialog
/// closes.
pub struct GalleryTab {
    source: Arc<dyn GallerySource>,
    items: Arc<Mutex<Vec<GalleryItem>>>,
    /// Worker-decoded thumbnails, shared with the worker thread.
    pixels: Pixels,
    /// UI-side textures built from `pixels` (textures are main-thread objects).
    textures: HashMap<i64, egui::TextureHandle>,
    /// Item ids whose thumbnail was already requested.
    wanted: HashSet<i64>,
    thumb_jobs: Option<std::sync::mpsc::Sender<i64>>,
    list_inbox: Option<Receiver<Result<Vec<GalleryItem>, String>>>,
    read_inbox: Option<Receiver<ReadOut>>,
    /// The name of the item being read, for the busy row.
    reading: Option<String>,
    error: Option<String>,
    /// The grant dialog was already shown once; re-asking is the Refresh button's job.
    asked: bool,
    permission: GalleryPermission,
    /// Whether a listing was started (once per dialog; a failed one waits for Refresh, it
    /// must not retry every frame).
    listed: bool,
}

impl GalleryTab {
    pub fn new(source: Arc<dyn GallerySource>) -> Self {
        let permission = source.permission();
        Self {
            source,
            items: Arc::new(Mutex::new(Vec::new())),
            pixels: Arc::new(Mutex::new(HashMap::new())),
            textures: HashMap::new(),
            wanted: HashSet::new(),
            thumb_jobs: None,
            list_inbox: None,
            read_inbox: None,
            reading: None,
            error: None,
            asked: false,
            permission,
            listed: false,
        }
    }

    /// Start listing (the first draw, and every Refresh): a worker thread asks the source and
    /// a later frame drains the answer.
    pub fn refresh(&mut self, ctx: &egui::Context) {
        self.listed = false;
        self.reload(ctx);
    }

    fn reload(&mut self, ctx: &egui::Context) {
        self.error = None;
        self.permission = self.source.permission();
        let (tx, rx) = std::sync::mpsc::channel();
        self.list_inbox = Some(rx);
        let source = Arc::clone(&self.source);
        let ctx = ctx.clone();
        // Detached by design: the channel wakes the frame loop when the answer lands.
        let _ = std::thread::Builder::new().spawn(move || {
            let _ = tx.send(source.list());
            ctx.request_repaint();
        });
    }

    /// Pick `item`: read its bytes on a worker thread (a camera RAW can be a hundred MB over
    /// binder) and hand them to the browser when they land.
    fn pick(&mut self, ctx: &egui::Context, item: &GalleryItem) {
        const MAX: i64 = 1_500_000_000; // mirrors the platform reader's cap
        if item.size > MAX {
            self.error = Some(format!("{} is larger than 1.5 GB", item.name));
            return;
        }
        self.reading = Some(item.name.clone());
        self.error = None;
        let (tx, rx) = std::sync::mpsc::channel();
        self.read_inbox = Some(rx);
        let source = Arc::clone(&self.source);
        let ctx = ctx.clone();
        let item = item.clone();
        let _ = std::thread::Builder::new().spawn(move || {
            let out = source.read(&item).map(|bytes| (item.name.clone(), bytes));
            let _ = tx.send(out);
            ctx.request_repaint();
        });
    }

    /// The thumbnail worker starts with the first listing and dies with the dialog: jobs flow
    /// until the sender side of the channel drops.
    fn ensure_worker(&mut self, ctx: &egui::Context) {
        if self.thumb_jobs.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.thumb_jobs = Some(tx);
        let source = Arc::clone(&self.source);
        let items = Arc::clone(&self.items);
        let pixels = Arc::clone(&self.pixels);
        let ctx = ctx.clone();
        // One worker: thumbnail decoding is serial JNI, and a steady drip of finished
        // previews beats a burst of jank.
        let _ = std::thread::Builder::new().spawn(move || {
            while let Ok(id) = rx.recv() {
                let item = items
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .iter()
                    .find(|i| i.id == id)
                    .cloned();
                if let Some(item) = item {
                    let thumb = source.thumbnail(&item, 256);
                    pixels.lock().unwrap_or_else(PoisonError::into_inner).insert(id, thumb);
                }
                ctx.request_repaint();
            }
        });
    }

    /// Draw the tab: the permission gate, then the scrolling image list. The first frame
    /// starts the listing and the thumbnail worker.
    pub fn draw(&mut self, ui: &mut egui::Ui) -> GalleryEvent {
        self.ensure_worker(ui.ctx());
        if !self.listed {
            self.listed = true;
            self.reload(ui.ctx());
        }
        if let Some(picked) = self.drain_inboxes() {
            return picked;
        }
        if self.permission != GalleryPermission::Granted {
            return self.draw_permission_gate(ui);
        }
        if let Some(name) = self.reading.clone() {
            ui.label(format!("Opening {name}…"));
            ui.add_space(4.0);
        }
        ui.horizontal(|ui| {
            ui.label("Newest first. Camera RAW files are included.");
            if ui.small_button("Refresh").clicked() {
                self.reload(ui.ctx());
            }
        });
        self.draw_rows(ui);
        if let Some(error) = self.error.clone() {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        GalleryEvent::None
    }

    /// Move the workers' answers into the tab's state; a finished read becomes the pick.
    fn drain_inboxes(&mut self) -> Option<GalleryEvent> {
        if let Some(rx) = &self.list_inbox
            && let Ok(listed) = rx.try_recv()
        {
            self.list_inbox = None;
            match listed {
                Ok(items) => *self.items.lock().unwrap_or_else(PoisonError::into_inner) = items,
                Err(e) => self.error = Some(e),
            }
        }
        if let Some(rx) = &self.read_inbox
            && let Ok(read) = rx.try_recv()
        {
            self.read_inbox = None;
            self.reading = None;
            match read {
                Ok((name, bytes)) => return Some(GalleryEvent::Picked(name, bytes)),
                Err(e) => self.error = Some(e),
            }
        }
        None
    }

    fn draw_permission_gate(&mut self, ui: &mut egui::Ui) -> GalleryEvent {
        ui.add_space(8.0);
        ui.label("PhotoComfy needs photo access to list the gallery.");
        ui.label("The grant also opens the device's folders in the Files tab.");
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button("Allow photo access").clicked() {
                match self.source.request_permission() {
                    Ok(()) => self.asked = true,
                    Err(e) => self.error = Some(e),
                }
            }
            if ui.small_button("Refresh").clicked() {
                self.reload(ui.ctx());
            }
        });
        // The system dialog takes the foreground and the grant lands without a callback, so
        // say what to do once it is answered.
        if self.asked && self.permission != GalleryPermission::Granted {
            ui.small("Choose Allow in the system dialog, then Refresh.");
        }
        if let Some(error) = self.error.clone() {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        GalleryEvent::None
    }

    fn draw_rows(&mut self, ui: &mut egui::Ui) {
        let items = self.items.lock().unwrap_or_else(PoisonError::into_inner).clone();
        if items.is_empty() && self.list_inbox.is_none() && self.error.is_none() {
            ui.label("No images in the gallery. Save or copy photos onto the device first.");
            return;
        }
        // Scroll, not paging: the gallery holds hundreds of rows and the finger expects to
        // throw them. The touch UI's widened scroll bar comes with the shell's preference.
        let row_height = 56.0_f32;
        egui::ScrollArea::vertical()
            .id_salt("gallery-rows")
            .auto_shrink([false, false])
            .show_rows(ui, row_height, items.len(), |ui, range| {
                for item in items[range].iter() {
                    self.draw_row(ui, item);
                }
            });
    }

    /// One full-width button row: the thumbnail, the name, the kind and the size. A button
    /// (not a selectable label): on the device every button the shell draws takes its tap.
    fn draw_row(&mut self, ui: &mut egui::Ui, item: &GalleryItem) {
        let disabled = self.reading.is_some();
        let label = row_label(item);
        let clicked = match self.thumbnail_texture(ui, item) {
            Some(texture) => {
                let image = egui::Image::new(egui::load::SizedTexture::new(texture.id(), egui::vec2(48.0, 48.0)))
                    .fit_to_exact_size(egui::vec2(48.0, 48.0));
                ui.add_enabled(!disabled, egui::Button::image_and_text(image, label)).clicked()
            }
            None => ui.add_enabled(!disabled, egui::Button::new(label)).clicked(),
        };
        if clicked {
            self.pick(ui.ctx(), item);
        }
    }

    /// The item's texture: built here once its pixels arrived, requested from the worker when
    /// first wanted. Callers get the handle only after the pixels exist.
    fn thumbnail_texture(&mut self, ui: &mut egui::Ui, item: &GalleryItem) -> Option<egui::TextureHandle> {
        if let Some(handle) = self.textures.get(&item.id) {
            return Some(handle.clone());
        }
        let arrived = self.pixels.lock().unwrap_or_else(PoisonError::into_inner).get(&item.id).cloned();
        match arrived {
            Some(Some((w, h, rgba))) => {
                let handle = ui.ctx().load_texture(
                    format!("gallery-thumb-{}", item.id),
                    egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba),
                    egui::TextureOptions::LINEAR,
                );
                self.textures.insert(item.id, handle.clone());
                Some(handle)
            }
            Some(None) => None,
            None => {
                if self.wanted.insert(item.id)
                    && let Some(jobs) = &self.thumb_jobs
                {
                    let _ = jobs.send(item.id);
                }
                None
            }
        }
    }
}

/// Camera RAW extensions worth naming in the row label (MediaStore's `image/x-*` MIMEs cover
/// these already; the names are for the user).
const RAW_EXTENSIONS: [&str; 8] = [".arw", ".cr2", ".cr3", ".dng", ".nef", ".raf", ".rw2", ".orf"];

/// The row's text: name, RAW-or-image, size.
fn row_label(item: &GalleryItem) -> String {
    let lower = item.name.to_ascii_lowercase();
    let raw = item.mime.starts_with("image/x") || RAW_EXTENSIONS.iter().any(|e| lower.ends_with(e));
    format!("{}  {}  {}", item.name, if raw { "RAW" } else { "image" }, size_text(item.size))
}

/// "1.4 MB"-style size, binary units.
fn size_text(bytes: i64) -> String {
    let units = ["B", "KB", "MB", "GB"];
    let mut value = bytes.max(0) as f64;
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

/// The MediaStore adapter (the JNI lives in `platform`, with the rest of the shell's).
#[cfg(target_os = "android")]
pub struct AndroidMediaStore;

#[cfg(target_os = "android")]
impl AndroidMediaStore {
    pub fn shared() -> Arc<Self> {
        Arc::new(Self)
    }
}

#[cfg(target_os = "android")]
impl GallerySource for AndroidMediaStore {
    fn permission(&self) -> GalleryPermission {
        if crate::platform::storage_permission_granted() {
            GalleryPermission::Granted
        } else {
            GalleryPermission::Missing
        }
    }

    fn request_permission(&self) -> Result<(), String> {
        crate::platform::request_storage_permission()
    }

    fn list(&self) -> Result<Vec<GalleryItem>, String> {
        crate::platform::query_media_images().map(|rows| {
            rows.into_iter()
                .map(|row| GalleryItem {
                    id: row.id,
                    name: row.name,
                    mime: row.mime,
                    size: row.size,
                    modified: row.modified,
                })
                .collect()
        })
    }

    fn read(&self, item: &GalleryItem) -> Result<Vec<u8>, String> {
        crate::platform::read_media_bytes(item.id)
    }

    fn thumbnail(&self, item: &GalleryItem, max: u32) -> Option<(u32, u32, Vec<u8>)> {
        crate::platform::media_thumbnail(item.id, max)
    }
}

/// The off-device stand-in: no gallery to reach (host tests inject their own fake), so every
/// entry point answers "not here" instead of pretending.
#[cfg(not(target_os = "android"))]
pub struct NoGallery;

#[cfg(not(target_os = "android"))]
impl GallerySource for NoGallery {
    fn permission(&self) -> GalleryPermission {
        GalleryPermission::Missing
    }
    fn request_permission(&self) -> Result<(), String> {
        Err("the gallery needs a device".into())
    }
    fn list(&self) -> Result<Vec<GalleryItem>, String> {
        Err("the gallery needs a device".into())
    }
    fn read(&self, _item: &GalleryItem) -> Result<Vec<u8>, String> {
        Err("the gallery needs a device".into())
    }
    fn thumbnail(&self, _item: &GalleryItem, _max: u32) -> Option<(u32, u32, Vec<u8>)> {
        None
    }
}
