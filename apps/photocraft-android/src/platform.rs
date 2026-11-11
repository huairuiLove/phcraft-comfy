//! The one JNI call the shell needs, plus the `AndroidApp` handle stash.
//!
//! Everything else in the shell is safe Rust (the all-files-access *check* is a plain
//! `std::fs::read_dir` probe in `browser`); this module owns the unsafe, in the spirit of
//! `crates/tablet`: OS interop that has no safe wrapper, isolated, commented, panic-free.

use std::sync::OnceLock;

use winit::platform::android::activity::{AndroidApp, WindowManagerFlags};

static APP: OnceLock<AndroidApp> = OnceLock::new();

/// Stash the app handle (a cheap clone) at `android_main`; the shell reads it later for the
/// window flag and the settings jump.
pub fn set_app(app: AndroidApp) {
    let _ = APP.set(app);
}

fn app() -> Option<AndroidApp> {
    APP.get().cloned()
}

/// The app-specific directory on shared storage (`Android/data/<pkg>/files`): readable and
/// writable with no permission on every Android version — the browser's fallback root when
/// all-files access is unavailable (Android 16 removed it for ordinary apps).
pub fn external_files_dir() -> Option<std::path::PathBuf> {
    app().and_then(|app| app.external_data_path())
}

/// The device's Android release (an API level), read once: `getprop` off the UI thread has
/// been observed to wedge the frame loop on the emulator, so the value is cached. 36+ never
/// offers all-files access to a plain app, so the browser banner's advice changes to the
/// USB-transfer path.
pub fn sdk_version() -> i32 {
    static SDK: OnceLock<i32> = OnceLock::new();
    *SDK.get_or_init(AndroidApp::sdk_version)
}

/// Keep the display on while the app is in front, as the iPadOS shell does with UIKit's idle
/// timer: a painting app reads as broken when the screen blanks under a held pen stroke.
pub fn keep_screen_on() {
    if let Some(app) = app() {
        app.set_window_flags(WindowManagerFlags::KEEP_SCREEN_ON, WindowManagerFlags::empty());
    }
}

/// Open the system page where the user grants "allow access to manage all files" for this app.
/// Safe to call from any thread that may attach to the JVM (here: the UI thread, inside a
/// frame).
pub fn open_all_files_settings() -> Result<(), String> {
    let Some(app) = app() else {
        return Err("the Android app handle is not initialized".into());
    };
    // SAFETY: `vm_as_ptr` is the process's single JavaVM, created before `android_main` runs
    // and valid for the process lifetime; `JavaVM::from_raw` only wraps the pointer (its Drop
    // does not destroy the VM), and the ManuallyDrop makes that non-ownership explicit.
    let vm = std::mem::ManuallyDrop::new(unsafe { jni::JavaVM::from_raw(app.vm_as_ptr().cast()) });
    vm.attach_current_thread(|env| -> Result<(), jni::errors::Error> {
        use jni::objects::JValue;
        use jni::{jni_sig, jni_str};
        let action = env.new_string("android.settings.MANAGE_ALL_FILES_ACCESS_PERMISSION")?;
        let package = env.new_string("package:com.photocomfy.app")?;
        let uri_class = env.find_class(jni_str!("android/net/Uri"))?;
        let uri = env
            .call_static_method(
                &uri_class,
                jni_str!("parse"),
                jni_sig!("(Ljava/lang/String;)Landroid/net/Uri;"),
                &[JValue::Object(&package)],
            )?
            .l()?;
        let intent = env.new_object(
            jni_str!("android/content/Intent"),
            jni_sig!("(Ljava/lang/String;Landroid/net/Uri;)V"),
            &[JValue::Object(&action), JValue::Object(&uri)],
        )?;
        let raw_activity = app.activity_as_ptr() as jni::sys::jobject;
        // SAFETY: the pointer is the global Activity reference `AndroidApp` owns, valid while
        // `app` is alive (it outlives this call); the Cast wrapper borrows it without taking
        // ownership, so it is never deleted. This is android-activity's documented pattern.
        let activity = unsafe { env.as_cast_raw::<jni::objects::JObject>(&raw_activity)? };
        env.call_method(
            &activity,
            jni_str!("startActivity"),
            jni_sig!("(Landroid/content/Intent;)V"),
            &[JValue::Object(&intent)],
        )?;
        Ok(())
    })
    .map_err(|e: jni::errors::Error| e.to_string())
}
