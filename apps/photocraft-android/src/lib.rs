//! The Android shell, distributed as **PhotoComfy**: the same ui-egui app on wgpu (Vulkan, with
//! GLES as the fallback). winit's Android backend delivers touches as egui input, so the canvas
//! and tools work unchanged; platform services the OS doesn't give a plain NativeActivity app
//! (desktop file dialogs, the clipboard bridge, crash recovery) stay unset and the shell
//! degrades gracefully, as in the browser and on iPadOS. Everything mirrors
//! `apps/photocraft-ios/src/main.rs`.
//!
//! The APK carries no Java code (`hasCode="false"`): its `android.app.NativeActivity` loads
//! `libphotocraft_android.so`, android-activity brings up the JVM thread and calls
//! `android_main` below, which hands the `AndroidApp` to eframe (`NativeOptions::android_app`)
//! and runs the same `run_native` path as every other shell.

// The file browser is platform-independent egui over std::fs (kittest-tested on every host);
// only its Android glue (the external-dir fallback and the settings jump) is gated below.
pub mod browser;

#[cfg(target_os = "android")]
mod platform;

#[cfg(target_os = "android")]
mod shell {
    use std::path::PathBuf;
    use std::sync::{Arc, OnceLock};

    use photocraft_codecs::{ChannelLayout, EncodeOptions, Image};
    use photocraft_doc::Document;
    use photocraft_ui_egui::long_press::LongPress;
    use photocraft_ui_egui::theme::ThemeKind;
    use photocraft_ui_egui::{FileDialogRequest, FileStamp, PhotocraftApp, Services};
    use winit::platform::android::activity::AndroidApp;

    use crate::browser::Browser;

    /// The app's private files directory (`/data/data/com.photocomfy.app/files`), the one place
    /// a NativeActivity app may write without asking. Set once from `android_main` before
    /// eframe starts; `None` (the shell then runs without persisted preferences or logs) when
    /// the OS doesn't hand one over.
    static FILES_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();

    fn files_dir() -> Option<PathBuf> {
        FILES_DIR.get().cloned().flatten()
    }

    /// android-activity 0.6's entry convention: a plain `#[no_mangle]` `android_main` that the
    /// glue crate calls on a dedicated thread after the JVM is up. The unsafe attribute is the
    /// symbol export itself; there is no unsafe code in this crate beyond it.
    #[unsafe(no_mangle)]
    fn android_main(app: AndroidApp) {
        let _ = log::set_logger(&StderrLogger).map(|()| log::set_max_level(log::LevelFilter::Info));
        let _ = FILES_DIR.set(app.internal_data_path());
        // The handle for the window flag and the settings jump (see `platform`).
        crate::platform::set_app(app.clone());
        // KEEP_SCREEN_ON via set_window_flags wedged input on the API 36 emulator (everything
        // rendered, nothing reached the widgets); revisit with a later android-activity.
        // crate::platform::keep_screen_on();
        breadcrumb("launched");
        install_locale();
        install_panic_log();
        let browser = Browser::shared();
        let mut options = native_options();
        options.android_app = Some(app);
        let shell_browser = browser.clone();
        let result = eframe::run_native("PhotoComfy", options, Box::new(move |cc| {
            breadcrumb("app created");
            Ok(Box::new(AndroidShell::new(cc, shell_browser)))
        }));
        breadcrumb(&format!("run_native returned: {result:?}"));
    }

    /// Startup trail, for a device where stderr goes nowhere (logcat shows nothing from native
    /// stdout/stderr): each step appends to `files/launch.log`, pulled with
    /// `adb run-as com.photocomfy.app cat files/launch.log` on the debuggable team build. A
    /// crash names the step that never happened; no entries at all means the library never ran
    /// (packaging or a load error, visible in `adb logcat`).
    fn breadcrumb(step: &str) {
        let line = format!(
            "[{:?}] {step}\n",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
        );
        log::info!("{step}");
        if let Some(dir) = files_dir() {
            use std::io::Write as _;
            if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("launch.log")) {
                let _ = write!(file, "{line}");
            }
        }
    }

    /// The trail above, but logged once per process per step (input diagnostics).
    fn breadcrumb_once(step: &'static str) {
        static SEEN: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
        let mut seen = SEEN.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !seen.contains(&step) {
            seen.push(step);
            drop(seen);
            breadcrumb(step);
        }
    }

    /// A panic on a device is invisible, so each one lands in `files/crash.log` — readable over
    /// `adb run-as` — with a backtrace, before the default hook reports it.
    fn install_panic_log() {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let text = format!("PhotoComfy panicked\n{info}\n\n{:?}\n", std::backtrace::Backtrace::force_capture());
            log::error!("{text}");
            if let Some(dir) = files_dir() {
                use std::io::Write as _;
                if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("crash.log")) {
                    let _ = writeln!(file, "{text}");
                }
            }
            default(info);
        }));
    }

    /// The interface language and the CJK script default come from the system locale. The text
    /// engine normally reads `PHOTOCRAFT_LOCALE` (`crates/text/src/cjk.rs`), and no launcher
    /// sets environment variables for an Android app; `sys_locale` reads it over JNI (as the
    /// web build's language auto path already does), and failure falls back to English.
    fn install_locale() {
        if let Some(tag) = sys_locale::get_locales().next() {
            photocraft_text::cjk::set_shell_locale(tag);
        }
    }

    /// The smallest logger that gets a startup error onto stderr (`adb logcat` on a debug
    /// build shows eprintln from `am start`-launched processes on some images).
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

    /// Android: the window is the screen, so eframe's window geometry options don't apply.
    /// PhotoComfy owns its fallback fonts (`cjk_fonts`, craft-fonts), so eframe's system-font
    /// provider stays off, as on the desktop; the saved layout (panel positions) lives beside
    /// the preferences.
    fn native_options() -> eframe::NativeOptions {
        let mut options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_title("PhotoComfy"),
            system_font_fallback: false,
            persistence_path: files_dir().map(|dir| dir.join("ui.ron")),
            ..Default::default()
        };
        // egui's default limits exceed what an Android emulator GPU allows (the iOS simulator
        // hits the same wall) and its 8192 px texture cap would push big documents to the CPU:
        // request the adapter's own limits, clamped, as the browser build does.
        photocraft_ui_egui::gpu_canvas::use_adapter_limits(&mut options.wgpu_options.wgpu_setup);
        options
    }

    /// Wraps [`PhotocraftApp`] to hand it each frame, as the web and iPad runners do
    /// (`apps/photocraft-web/src/web.rs`), with Android's input quirk handled first. The
    /// in-app file browser draws over the app when a dialog is pending.
    struct AndroidShell {
        app: PhotocraftApp,
        browser: Arc<Browser>,
        frames: u32,
        /// Long-press → the synthesized right-click that opens the context menus, the same
        /// gesture as iPadOS (`photocraft_ui_egui::long_press`).
        long_press: LongPress,
    }

    impl AndroidShell {
        fn new(cc: &eframe::CreationContext<'_>, browser: Arc<Browser>) -> Self {
            PhotocraftApp::setup_context(&cc.egui_ctx, ThemeKind::Pro);
            let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), services(browser.clone()));
            // Testing aid (a launcher sets no CLI args for an Android app): open a document at
            // start, to reproduce the canvas states a reviewer sees.
            if std::env::var_os("PHOTOCRAFT_OPEN_NEW").is_some()
                && let Err(e) = app.run("file.new", serde_json::json!({"width": 1024, "height": 768, "background": "white"}))
            {
                log::warn!("file.new: {e}");
            }
            // Testing aid: the portrait panel drawer starts open on Layers, for visual checks.
            if std::env::var_os("PHOTOCRAFT_OPEN_DRAWER").is_some() {
                cc.egui_ctx.data_mut(|d| {
                    d.insert_temp(egui::Id::unique("touch-drawer"), Some(photocraft_ui_egui::dock::Group::Layers))
                });
            }
            // Long commands and file opens run as background jobs with progress, as on the desktop.
            app.background_jobs = true;
            if let Some(rs) = cc.wgpu_render_state.clone() {
                // The GPU canvas: Vulkan on real devices, GLES where the adapter says so, and a
                // device loss is watched, not fatal (see gpu.rs).
                app.set_wgpu(rs);
                app.start_compositor(&cc.egui_ctx);
            }
            Self { app, browser, frames: 0, long_press: LongPress::default() }
        }
    }

    impl eframe::App for AndroidShell {
        fn on_exit(&mut self) {
            eframe::App::on_exit(&mut self.app);
        }

        fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
            // A finger held still for half a second is the touch right-click: synthesize it, and
            // swallow the finger's own events until it lifts so the release cannot click through
            // the menu that opened — the same gesture as iPadOS (photocraft_ui_egui::long_press).
            self.long_press.frame(&mut raw_input.events, raw_input.time);
            if let Some(wait) = self.long_press.fires_within(raw_input.time) {
                ctx.request_repaint_after(wait);
            }
            // A tap can reach the app as both a Touch and an emulated pointer press; keep one,
            // or the twin closes the menu the first one opened (#2095). Android's winit backend
            // synthesizes no separate system zoom recognizer (both fingers arrive as Touch
            // events that egui's `zoom_delta` reports), so there is no `Event::Zoom` to drop,
            // unlike iPadOS.
            if raw_input.events.iter().any(|e| matches!(e, egui::Event::Touch { .. })) {
                static TOUCHES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                let n = TOUCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                breadcrumb(&format!(
                    "touch #{n} at {:?} screen {:?}",
                    raw_input.events.iter().find_map(|e| match e {
                        egui::Event::Touch { pos, .. } => Some(pos),
                        _ => None,
                    }),
                    raw_input.screen_rect
                ));
            }
            photocraft_ui_egui::touch_twins::drop_twins(ctx, raw_input);
            eframe::App::raw_input_hook(&mut self.app, ctx, raw_input);
        }

        fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
            if self.frames == 0 {
                breadcrumb("first frame");
            }
            self.frames += 1;
            self.app.logic_frame(ctx, frame);
            self.app.finish_type_reset_logic(ctx);
        }

        fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
            // The browser's full-screen panel first: it takes the whole screen for the
            // frame (the app then has no space left and paints nothing), and being the same
            // kind of layer as the app's own panels it hit-tests on Android, where every
            // floating layer the shell showed rendered but never took input.
            if let crate::browser::Action::RequestGrant = self.browser.draw(ui)
                && let Err(e) = crate::platform::open_all_files_settings()
            {
                log::error!("open settings: {e}");
            }
            self.app.ui_frame(ui, frame);
            self.app.finish_type_reset_pass(ui.ctx());
        }
    }

    fn services(browser: Arc<Browser>) -> Services {
        Services {
            import: Some(Box::new(|name: &str, bytes: &[u8], opts: &photocraft_io::ImportOptions| {
                photocraft_io::import_with_options(name, bytes, opts).map(|r| (r.document, r.warnings)).map_err(|e| e.to_string())
            })),
            export: Some(Box::new(|doc: &Document, path: &str, settings: &photocraft_ui_egui::ExportSettings| {
                photocraft_io::export(doc, path, &settings.io_options()).map(|r| (r.bytes, r.warnings)).map_err(|e| e.to_string())
            })),
            // The in-app browser (see `browser`): answers carry real paths on the shared
            // storage, so replace prompts and File ▸ Save in place work as on the desktop.
            file_dialog: Some(Box::new(move |request, _parent, reply| match request {
                FileDialogRequest::Open { multiple, initial_dir, extensions, .. } => {
                    browser.begin_open(multiple, initial_dir, extensions, reply)
                }
                // Folder pickers have no browser counterpart (as on the web): back out.
                FileDialogRequest::Folder { .. } => reply.send(None),
                FileDialogRequest::Save { suggested } => browser.begin_save(suggested, reply),
            })),
            write: Some(Box::new(|path: &str, bytes: &[u8]| write_atomic(path, bytes))),
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
            ..Default::default()
        }
    }

    /// Write beside the file and rename, so a crash mid-write can't corrupt it (the pattern
    /// the preferences use; `photocraft_format::atomic_write`'s desktop twin).
    fn write_atomic(path: &str, bytes: &[u8]) -> Result<(), String> {
        let path = std::path::Path::new(path);
        let name = path.file_name().ok_or("the path names no file")?;
        let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
        let mut tmp = std::ffi::OsString::from(name);
        tmp.push(".pc-tmp");
        let tmp = dir.join(tmp);
        std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
        match std::fs::rename(&tmp, path) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e.to_string())
            }
        }
    }

    const PREFS_FILE: &str = "preferences.json";

    fn read_prefs() -> Option<String> {
        std::fs::read_to_string(files_dir()?.join(PREFS_FILE)).ok()
    }

    fn write_prefs(text: &str) -> Result<(), String> {
        let dir = files_dir().ok_or("no files directory")?;
        // Write beside the file and rename, so a crash mid-write can't corrupt the preferences.
        let tmp = dir.join(format!("{PREFS_FILE}.tmp"));
        std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, dir.join(PREFS_FILE)).map_err(|e| e.to_string())
    }
}
