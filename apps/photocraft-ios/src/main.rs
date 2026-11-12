//! The iPad shell: the same ui-egui app on Metal. winit's iOS backend delivers touches as egui
//! input (Apple Pencil force included), the canvas and tools work unchanged; platform services
//! the OS doesn't give an iPad app (desktop file dialogs, the clipboard bridge, crash recovery)
//! stay unset and the shell degrades gracefully, as in the browser.

use std::path::{Path, PathBuf};

use photocraft_codecs::{ChannelLayout, EncodeOptions, Image};
use photocraft_doc::Document;
use photocraft_ui_egui::long_press::LongPress;
use photocraft_ui_egui::theme::ThemeKind;

// The last tap's received position, painted as a crosshair under `PHOTOCRAFT_DEBUG_IOS`.
thread_local! {
    static LAST_TAP: std::cell::RefCell<Option<egui::Pos2>> = const { std::cell::RefCell::new(None) };
}
use photocraft_ui_egui::{FileDialogAnswer, FileStamp, FileDialogRequest, PhotocraftApp, Services};
#[cfg(target_os = "ios")]
use objc2_ui_kit::UICoordinateSpace;

// Unconditional: the services() below references it on every host (clippy, tests); the
// extern "C" pc_* symbols only link on iOS, where the ObjC file is compiled in.
mod picker;
// Unconditional too: the transport links everywhere and its tests run on the host.
mod api_gen;

fn main() -> eframe::Result {
    // Route records to stderr (`simctl launch --console-pty`): eframe quits with exit(0) on a
    // startup error, and without a logger that error is dropped silently.
    let _ = log::set_logger(&StderrLogger).map(|()| log::set_max_level(log::LevelFilter::Info));
    breadcrumb("launched");
    discard_saved_scene_state();
    install_locale();
    install_panic_log();
    picker::install();
    let result = eframe::run_native("PhotoCraft", native_options(), Box::new(|cc| {
        breadcrumb("app created");
        Ok(Box::new(IosShell::new(cc)))
    }));
    breadcrumb(&format!("run_native returned: {result:?}"));
    result
}

/// Startup trail, for a device where stderr is invisible: each step appends to
/// `Documents/launch.log` (Finder file sharing). A white-screen crash names the step that never
/// happened — or, with no entries at all, that the process never ran our binary at all
/// (signing/build), since the trail's first line is written before anything else.
fn breadcrumb(step: &str) {
    let line = format!("[{:?}] {step}\n", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0));
    log::info!("{step}");
    if let Some(dir) = documents_dir() {
        use std::io::Write as _;
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("launch.log")) {
            let _ = write!(file, "{line}");
        }
    }
}

/// winit recreates its window from scratch every launch (classic lifecycle), but once the
/// Info.plist adopts scenes, UIKit persists the scene session in `Library/Saved Application
/// State` and restores it on the next launch — and the restored scene never attaches winit's
/// new window: every relaunch rendered blank (egui's viewport stuck at its 10000x10000
/// default) while a fresh install worked. PhotoCraft has no scene state worth restoring, so
/// the saved state is dropped before the UI starts, making every launch the fresh-install
/// path.
fn discard_saved_scene_state() {
    #[cfg(target_os = "ios")]
    if let Some(home) = std::env::var_os("HOME") {
        let _ = std::fs::remove_dir_all(std::path::Path::new(&home).join("Library/Saved Application State"));
    }
}

/// A panic on a device is invisible (stderr goes nowhere), so each one lands in
/// `Documents/crash.log` — readable over Finder file sharing — with a backtrace, before the
/// default hook reports it.
fn install_panic_log() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let text = format!("PhotoCraft panicked\n{info}\n\n{:?}\n", std::backtrace::Backtrace::force_capture());
        log::error!("{text}");
        if let Some(dir) = documents_dir() {
            use std::io::Write as _;
            if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("crash.log")) {
                let _ = writeln!(file, "{text}");
            }
        }
        default(info);
    }));
}

/// The app container's Documents folder (Finder file sharing exposes it).
fn documents_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("HOME")?).join("Documents");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// The interface language and CJK script default come from the system locale: the text engine
/// normally reads `PHOTOCRAFT_LOCALE` (`crates/text/src/cjk.rs`), and no launcher sets
/// environment variables for an iPad app. Safe Foundation calls only (objc2's methods are safe
/// wrappers).
#[cfg(target_os = "ios")]
fn install_locale() {
    let identifier = objc2_foundation::NSLocale::currentLocale().languageIdentifier().to_string();
    photocraft_text::cjk::set_shell_locale(identifier);
}

#[cfg(not(target_os = "ios"))]
fn install_locale() {}

/// The smallest logger that gets a startup error onto stderr.
struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record) {
        eprintln!("[{}] {}", record.level(), record.args());
    }

    fn flush(&self) {}
}

/// iPad: the window is the screen, so eframe's window geometry options don't apply. PhotoCraft
/// owns its fallback fonts (`cjk_fonts`, craft-fonts), so eframe's system-font provider stays
/// off, as on the desktop; the saved layout (panel positions) lives beside the preferences.
fn native_options() -> eframe::NativeOptions {
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("PhotoCraft"),
        system_font_fallback: false,
        persistence_path: app_support_dir().map(|dir| dir.join("ui.ron")),
        ..Default::default()
    };
    // egui's default limits exceed what the iOS simulator GPU allows (max_inter_stage_shader_
    // variables 16 > 15) and its 8192 px texture cap would push big documents to the CPU: request
    // the adapter's own limits, clamped, as the browser build does.
    photocraft_ui_egui::gpu_canvas::use_adapter_limits(&mut options.wgpu_options.wgpu_setup);
    options
}

/// Wraps [`PhotocraftApp`] to hand it each frame, as the web runner does
/// (`apps/photocraft-web/src/web.rs`), with iPad input quirks handled first.
struct IosShell {
    app: PhotocraftApp,
    frames: u32,
    /// Long-press → the synthesized right-click that opens the context menus (the shared
    /// `photocraft_ui_egui::long_press`).
    long_press: LongPress,
}

impl IosShell {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // A painting app keeps the display on while it is in front: the OS auto-locks an idle
        // iPad after two minutes, which reads on a review device as the screen going black with
        // the app still running. Safe UIKit call, main thread by construction here.
        #[cfg(target_os = "ios")]
        if let Some(mtm) = objc2::MainThreadMarker::new() {
            objc2_ui_kit::UIApplication::sharedApplication(mtm).setIdleTimerDisabled(true);
        }
        PhotocraftApp::setup_context(&cc.egui_ctx, ThemeKind::Pro);
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), services());
        // The iPad is a pen-first canvas: the brush-family tools and every Liquify brush take
        // only the Apple Pencil (`input::finger`); a finger points, pans, picks and taps menus.
        app.pen_only_painting = true;
        // Testing aid (a launcher sets no CLI args for an iPad app): open a document at start,
        // to reproduce the canvas states a reviewer sees.
        if std::env::var_os("PHOTOCRAFT_OPEN_NEW").is_some()
            && let Err(e) = app.run("file.new", serde_json::json!({"width": 1024, "height": 768, "background": "white"}))
        {
            log::warn!("file.new: {e}");
        }
        // Testing aid: Save As at start (the save flow with the shell's own services, the
        // reviewer's first save), replayed like the menu item.
        if std::env::var_os("PHOTOCRAFT_SAVE_NEW").is_some() {
            if let Err(e) = app.run("file.new", serde_json::json!({"width": 64, "height": 48, "background": "white"})) {
                log::warn!("file.new: {e}");
            }
            match photocraft_ui_egui::menus::invoke(&mut app, &cc.egui_ctx, "file.saveAs", serde_json::json!({})) {
                Ok(_) => log::info!("file.saveAs: asked"),
                Err(e) => log::warn!("file.saveAs: {e}"),
            }
        }
        // Testing aid: the portrait panel drawer starts open on Layers, for visual checks.
        if std::env::var_os("PHOTOCRAFT_OPEN_DRAWER").is_some() {
            cc.egui_ctx.data_mut(|d| {
                d.insert_temp(egui::Id::unique("touch-drawer"), Some(photocraft_ui_egui::dock::Group::Layers))
            });
        }
        // Testing aid: present the system document picker at start (the native sheet is the
        // one thing a screenshot can prove without a finger).
        if std::env::var_os("PHOTOCRAFT_OPEN_PICKER").is_some()
            && let Err(e) = app.run("file.open", serde_json::json!({}))
        {
            log::warn!("file.open: {e}");
        }
        // Long commands and file opens run as background jobs with progress, as on the desktop.
        app.background_jobs = true;
        if let Some(rs) = cc.wgpu_render_state.clone() {
            // The GPU canvas: Metal always, and a device loss is watched, not fatal (see gpu.rs).
            app.set_wgpu(rs);
            app.start_compositor(&cc.egui_ctx);
        }
        Self { app, frames: 0, long_press: LongPress::default() }
    }
}

impl eframe::App for IosShell {
    fn on_exit(&mut self) {
        eframe::App::on_exit(&mut self.app);
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        #[cfg(target_os = "ios")]
        repair_missing_screen_rect(raw_input);
        // A finger held still for half a second is the touch right-click: synthesize it, and
        // swallow the finger's own events until it lifts so the release cannot click through
        // the menu that opened (long_press.rs).
        self.long_press.frame(&mut raw_input.events, raw_input.time);
        if let Some(wait) = self.long_press.fires_within(raw_input.time) {
            ctx.request_repaint_after(wait);
        }
        // A tap can reach the app as both a Touch and an emulated pointer press; keep one, or
        // the twin closes the menu the first one opened (#2095).
        photocraft_ui_egui::touch_twins::drop_twins(ctx, raw_input);
        // One pinch, two channels: winit's iOS system recognizer delivers it as `Event::Zoom`,
        // and the same fingers arrive as Touch events that egui's `zoom_delta` already reports.
        // Keeping both double-applies the pinch, and the recognizer's trailing events keep
        // zooming after the fingers lift — the endless zoom wobble. The touch channel is the
        // accurate one, so the recognizer's zooms are dropped (a trackpad pinch on an iPad is
        // the one input that loses its zoom; its scroll still pans).
        if self.app.session.prefs().interface.touch_ui {
            raw_input.events.retain(|event| !matches!(event, egui::Event::Zoom(_)));
        }
        // `PHOTOCRAFT_DEBUG_IOS`: every touch press lands in the log with its egui position and
        // the pointer position the same frame synthesized — comparing the two with where the
        // finger actually was names any input offset (the "tap lands a row off" diagnosis).
        if std::env::var_os("PHOTOCRAFT_DEBUG_IOS").is_some() {
            for event in &raw_input.events {
                match event {
                    egui::Event::Touch { phase: egui::TouchPhase::Start, pos, .. } => {
                        breadcrumb(&format!("touch press at {pos:?} (zoom {:.3} ppp {:.3}, screen {:?})", ctx.zoom_factor(), ctx.pixels_per_point(), ctx.input(|i| i.content_rect())));
                    }
                    egui::Event::PointerButton { pressed: true, pos, .. } => {
                        breadcrumb(&format!("pointer press at {pos:?} (zoom {:.3} ppp {:.3}, screen {:?})", ctx.zoom_factor(), ctx.pixels_per_point(), ctx.input(|i| i.content_rect())));
                        LAST_TAP.with(|t| *t.borrow_mut() = Some(*pos));
                    }
                    _ => {}
                }
            }
        }
        eframe::App::raw_input_hook(&mut self.app, ctx, raw_input);
    }

    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if self.frames == 0 {
            breadcrumb("first frame");
        }
        // `PHOTOCRAFT_DEBUG_IOS`: the view/layer/drawable geometry once, to name any present-path
        // mismatch (the anisotropic-stretch diagnosis).
        if std::env::var_os("PHOTOCRAFT_DEBUG_IOS").is_some() && (self.frames == 2 || self.frames.is_multiple_of(30)) {
            #[cfg(target_os = "ios")]
            unsafe extern "C" {
                safe fn pc_log_geometry();
            }
            #[cfg(target_os = "ios")]
            unsafe {
                pc_log_geometry();
            }
            let vp = ctx.input(|i| (i.viewport().inner_rect, i.viewport().outer_rect, i.viewport().monitor_size));
            breadcrumb(&format!("viewport inner={vp:?}"));
        }
        self.frames += 1;
        // `PHOTOCRAFT_DEBUG_IOS=1`: a throttled sample of everything the touch layout and the
        // canvas zoom read, for the wobble/crash diagnoses (a device has no visible stderr; use
        // the log next to the breadcrumbs).
        if std::env::var_os("PHOTOCRAFT_DEBUG_IOS").is_some() && self.frames.is_multiple_of(30) {
            let screen = ctx.input(|i| i.content_rect());
            let insets = ctx.input(|i| i.safe_area_insets().0);
            let view = self.app.ui.views.first().map(|v| (v.zoom, v.center));
            let forced = std::env::var_os("PHOTOCRAFT_FORCE_PORTRAIT").is_some();
            let touch = self.app.session.prefs().interface.touch_ui;
            breadcrumb(&format!(
                "frame {} ppp {:.3} zoom {:.3} screen {}x{} insets t{} b{} view {view:?} forced={forced} touch_ui={touch}",
                self.frames,
                ctx.pixels_per_point(),
                ctx.zoom_factor(),
                screen.width(),
                screen.height(),
                insets.top,
                insets.bottom,
            ));
        }
        self.app.logic_frame(ctx, frame);
        self.app.finish_type_reset_logic(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.app.ui_frame(ui, frame);
        self.app.finish_type_reset_pass(ui.ctx());
        // `PHOTOCRAFT_DEBUG_IOS`: a crosshair at the position the last tap was RECEIVED at,
        // for ~3 s — one screenshot then shows the finger's intent and the app's belief.
        if std::env::var_os("PHOTOCRAFT_DEBUG_IOS").is_some() {
            // A 100 pt grid of labelled dots: tap one, and the crosshair's distance to the dot
            // you aimed at is the input offset, read straight off the screenshot.
            let screen = ui.input(|i| i.content_rect());
            let p = ui.ctx().debug_painter();
            let mut x = 100.0;
            while x < screen.right() {
                let mut y = 100.0;
                while y < screen.bottom() {
                    let at = egui::pos2(x, y);
                    p.circle_filled(at, 3.0, egui::Color32::from_rgb(0, 200, 255));
                    p.text(at + egui::vec2(4.0, 4.0), egui::Align2::LEFT_TOP, format!("{x:.0},{y:.0}"), egui::FontId::monospace(11.0), egui::Color32::from_rgb(0, 170, 255));
                    y += 100.0;
                }
                x += 100.0;
            }
        }
        if std::env::var_os("PHOTOCRAFT_DEBUG_IOS").is_some()
            && let Some(pos) = LAST_TAP.with(|t| *t.borrow())
        {
            let p = ui.ctx().debug_painter();
            p.line_segment([pos - egui::vec2(40.0, 0.0), pos + egui::vec2(40.0, 0.0)], egui::Stroke::new(3.0, egui::Color32::RED));
            p.line_segment([pos - egui::vec2(0.0, 40.0), pos + egui::vec2(0.0, 40.0)], egui::Stroke::new(3.0, egui::Color32::RED));
            p.circle_filled(pos, 6.0, egui::Color32::YELLOW);
            p.text(pos + egui::vec2(8.0, 8.0), egui::Align2::LEFT_TOP, format!("{pos:?}"), egui::FontId::monospace(14.0), egui::Color32::RED);
        }
    }
}

#[cfg(target_os = "ios")]
fn repair_missing_screen_rect(raw_input: &mut egui::RawInput) {
    let invalid = raw_input.screen_rect.is_none_or(|rect| rect.width() >= 9999.0 || rect.height() >= 9999.0);
    if !invalid {
        return;
    }

    let Some(mtm) = objc2::MainThreadMarker::new() else {
        return;
    };
    let scenes = objc2_ui_kit::UIApplication::sharedApplication(mtm)
        .connectedScenes()
        .allObjects();
    let scene_size = (0..scenes.count()).find_map(|index| {
        scenes
            .objectAtIndex(index)
            .downcast::<objc2_ui_kit::UIWindowScene>()
            .ok()
            .map(|scene| {
                #[allow(deprecated)]
                let bounds = scene.coordinateSpace().bounds();
                egui::vec2(bounds.size.width as f32, bounds.size.height as f32)
            })
    });
    #[allow(deprecated)]
    let fallback_bounds = objc2_ui_kit::UIScreen::mainScreen(mtm).bounds();
    let size = scene_size.unwrap_or_else(|| {
        egui::vec2(fallback_bounds.size.width as f32, fallback_bounds.size.height as f32)
    });
    if size.is_finite() && size.x > 0.0 && size.y > 0.0 {
        raw_input.screen_rect = Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size));
    }
}

fn services() -> Services {
    Services {
        import: Some(Box::new(|name: &str, bytes: &[u8], opts: &photocraft_io::ImportOptions| {
            photocraft_io::import_with_options(name, bytes, opts).map(|r| (r.document, r.warnings)).map_err(|e| e.to_string())
        })),
        export: Some(Box::new(|doc: &Document, path: &str, settings: &photocraft_ui_egui::ExportSettings| {
            photocraft_io::export(doc, path, &settings.io_options()).map(|r| (r.bytes, r.warnings)).map_err(|e| e.to_string())
        })),
        // The system document picker (ios/PCDocumentPicker.m + src/picker.rs). Open reads the
        // picked files in the delegate (security scope) and answers with contents. Save answers
        // at once with a real path in the app's Documents folder — the container's Documents is
        // visible in Files › On My iPad › PhotoCraft (UIFileSharingEnabled), so a save lands
        // where the app can always re-read it (Open Recent, Save in place, thumbnails) and
        // where the user can move it onward from the Files app. The earlier design answered a
        // bare staged name and presented the system export sheet: the document then remembered
        // a path that existed only in tmp, and every Open Recent entry was unopenable.
        file_dialog: Some(Box::new(|request, _parent, reply| match request {
            FileDialogRequest::Open { multiple, .. } => picker::open(multiple, reply),
            // Folder pickers have no counterpart (as on the web): back out.
            FileDialogRequest::Folder { .. } => reply.send(None),
            FileDialogRequest::Save { suggested } => {
                let name = Path::new(&suggested)
                    .file_name()
                    .map_or_else(|| suggested.clone(), |n| n.to_string_lossy().into_owned());
                let path = documents_dir().map(|dir| dir.join(&name)).map_or_else(|| name.clone(), |p| p.to_string_lossy().into_owned());
                reply.send(Some(FileDialogAnswer::SaveTo(path)));
            }
        })),
        write: Some(Box::new(|path: &str, bytes: &[u8]| {
            // Write beside the file and rename, so a crash mid-write can't corrupt it. The
            // write runs inline with the frame; it is a plain container path, fast and small.
            let file = PathBuf::from(path);
            if let Some(dir) = file.parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            let mut tmp = file.clone().into_os_string();
            tmp.push(".pc-tmp");
            std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
            match std::fs::rename(&tmp, &file) {
                Ok(()) => Ok(()),
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    Err(e.to_string())
                }
            }
        })),
        // File stamps and same-file checks, so replacing an existing Documents file asks
        // first and a save over the document's own file is recognised, as on the desktop.
        file_stamp: Some(Box::new(|path: &str| {
            std::fs::metadata(path).ok().map(|m| FileStamp { len: m.len(), modified: m.modified().ok() })
        })),
        same_file: Some(Box::new(|a: &str, b: &str| {
            use std::os::unix::fs::MetadataExt as _;
            match (std::fs::metadata(a), std::fs::metadata(b)) {
                (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
                _ => false,
            }
        })),
        encode_png: Some(Box::new(|w, h, rgba| {
            let img = Image::from_u8(w, h, ChannelLayout::Rgba, rgba.to_vec()).map_err(|e| e.to_string())?;
            photocraft_codecs::encode(&img, photocraft_codecs::Format::Png, &EncodeOptions::default()).map_err(|e| e.to_string())
        })),
        load_prefs: Some(Box::new(read_prefs)),
        save_prefs: Some(Box::new(write_prefs)),
        // Generation goes straight to the user's OpenAI-compatible Image API (src/api_gen.rs),
        // not through a ComfyUI server: the endpoint, model and key are Preferences ▸ Integrations
        // (editable in the generation panel), and results persist in the container like the
        // desktop's Generations folder, so the shelf survives a relaunch.
        comfy_transport: Some(api_gen::transport()),
        comfy_image_api: true,
        comfy_generations_dir: app_support_dir().map(|dir| dir.join("Generations")),
        ..Default::default()
    }
}

const PREFS_FILE: &str = "preferences.json";

/// The app container's Application Support folder. The iOS sandbox's `HOME` is the container
/// (the simulator's likewise); created on first use, `None` where storage is unavailable.
fn app_support_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("HOME")?).join("Library/Application Support/PhotoCraft");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn read_prefs() -> Option<String> {
    let text = std::fs::read_to_string(app_support_dir()?.join(PREFS_FILE)).ok()?;
    Some(migrate_recent_files(&text))
}

/// Open Recent entries recorded by the export-sheet save design were bare file names (the
/// staged tmp name, never openable: `open_paths` resolves them against the app's working
/// directory, `/`). Point them at the Documents folder the save now uses, so an entry either
/// opens or fails with the document's real name — anything with a `/` is a real path already.
fn migrate_recent_files(prefs: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(prefs) else {
        return prefs.to_string();
    };
    let Some(list) = value.get_mut("fileHandling").and_then(|f| f.get_mut("recentFiles")).and_then(|r| r.as_array_mut()) else {
        return prefs.to_string();
    };
    let mut changed = false;
    for entry in list.iter_mut() {
        let Some(path) = entry.as_str() else { continue };
        if path.contains('/') || path.is_empty() {
            continue;
        }
        if let Some(dir) = documents_dir() {
            *entry = serde_json::Value::String(dir.join(path).to_string_lossy().into_owned());
            changed = true;
        }
    }
    if changed { value.to_string() } else { prefs.to_string() }
}

fn write_prefs(text: &str) -> Result<(), String> {
    let dir = app_support_dir().ok_or("no Application Support directory")?;
    // Write beside the file and rename, so a crash mid-write can't corrupt the preferences.
    let tmp = dir.join(format!("{PREFS_FILE}.tmp"));
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, dir.join(PREFS_FILE)).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::migrate_recent_files;

    #[test]
    fn bare_recent_names_move_into_documents_and_real_paths_stay() {
        let prefs = r#"{"fileHandling":{"recentFiles":["Untitled.psd","/var/mobile/Documents/real.psd",""]},"interface":{}}"#;
        let moved = migrate_recent_files(prefs);
        let value: serde_json::Value = serde_json::from_str(&moved).unwrap();
        let recent = value["fileHandling"]["recentFiles"].as_array().unwrap();
        assert!(recent[0].as_str().unwrap().contains("Documents/Untitled.psd"), "{}", recent[0]);
        assert_eq!(recent[1].as_str().unwrap(), "/var/mobile/Documents/real.psd");
        assert_eq!(recent[2].as_str().unwrap(), "");
        // Corrupt preferences pass through untouched, and nothing is rewritten when it must not be.
        assert_eq!(migrate_recent_files("not json"), "not json");
        let nothing = r#"{"fileHandling":{"recentFiles":["/a.psd"]}}"#;
        assert_eq!(migrate_recent_files(nothing), nothing);
    }
}
