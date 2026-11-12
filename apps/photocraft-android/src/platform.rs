//! The JNI calls the shell needs, plus the `AndroidApp` handle stash.
//!
//! Everything else in the shell is safe Rust (the all-files-access *check* is a plain
//! write probe in `browser`); this module owns the unsafe, in the spirit of
//! `crates/tablet`: OS interop that has no safe wrapper, isolated, commented, panic-free.
//!
//! Besides the settings jump, this is the system gallery (MediaStore over ContentResolver):
//! the system photo picker answers through `onActivityResult`, which a NativeActivity never
//! sees, so the in-app gallery (`browser.rs`'s Gallery tab) queries MediaStore itself and
//! reads the picked item through `openInputStream`. The storage permissions it asks for
//! double as the legacy *file-system* key: with the APK targeting 29 and
//! `requestLegacyExternalStorage`, the granted READ/WRITE pair is raw-path access to the
//! whole shared storage (RAW and .psd included) on every Android version, including the ones
//! that removed MANAGE_EXTERNAL_STORAGE.

use std::sync::OnceLock;

use jni::objects::{JObject, JString};
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

/// The runtime permissions that unlock the gallery and the shared storage: the classic
/// READ/WRITE pair (the legacy full-tree view for this target-29 APK) plus Android 13's
/// READ_MEDIA_IMAGES for the MediaStore query where the legacy view is unavailable.
/// Requesting a permission an OS does not know is filtered out, so one list serves every
/// version.
pub fn storage_permissions() -> &'static [&'static str] {
    &[
        "android.permission.READ_EXTERNAL_STORAGE",
        "android.permission.WRITE_EXTERNAL_STORAGE",
        "android.permission.READ_MEDIA_IMAGES",
    ]
}

/// Whether read access holds (any of [`storage_permissions`] granted): the gallery lists, and
/// with the legacy view the browser walks the whole shared storage.
pub fn storage_permission_granted() -> bool {
    with_activity(|env, activity| permission_granted(env, activity)).unwrap_or(false)
}

fn permission_granted(env: &mut jni::Env<'_>, activity: &JObject<'_>) -> Result<bool, jni::errors::Error> {
    for name in storage_permissions() {
        if name.ends_with("WRITE_EXTERNAL_STORAGE") {
            continue; // write is the save-side grant; reading is what listing needs
        }
        let perm = env.new_string(name.to_owned())?;
        let granted = env
            .call_method(
                activity,
                jni::jni_str!("checkSelfPermission"),
                jni::jni_sig!("(Ljava/lang/String;)I"),
                &[jni::objects::JValue::Object(&perm)],
            )?
            .i()?;
        if granted == 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Show the system dialog that grants [`storage_permissions`]. It answers through
/// `onRequestPermissionsResult`, which NativeActivity never forwards — the grant itself takes
/// effect without it, so the caller re-checks (`storage_permission_granted`) after the user
/// has chosen.
pub fn request_storage_permission() -> Result<(), String> {
    with_activity(|env, activity| {
        let perms = string_array(env, storage_permissions())?;
        env.call_method(
            activity,
            jni::jni_str!("requestPermissions"),
            jni::jni_sig!("([Ljava/lang/String;I)V"),
            &[jni::objects::JValue::Object(&perms), jni::objects::JValue::Int(1)],
        )?;
        Ok(())
    })
}

/// One row of MediaStore's image tables (the union `files` table covers MediaStore.Images
/// and camera RAW indexed with an `image/*` MIME alike).
pub struct MediaRow {
    pub id: i64,
    pub name: String,
    pub mime: String,
    pub size: i64,
    /// Seconds since the epoch, MediaStore's `date_modified`.
    pub modified: i64,
}

/// List the device's images, newest first, camera RAW included. Capped at 500 rows: a
/// gallery can hold six figures of entries and this feeds a scrollable pick list.
pub fn query_media_images() -> Result<Vec<MediaRow>, String> {
    with_activity(|env, activity| {
        let resolver = content_resolver(env, activity)?;
        let uri = parse_uri(env, "content://media/external/files")?;
        let projection = string_array(env, &["_id", "_display_name", "mime_type", "_size", "date_modified"])?;
        let selection = env.new_string("(media_type=? OR mime_type LIKE ?)")?;
        let args = string_array(env, &["1", "image/%"])?;
        let sort = env.new_string("date_modified DESC")?;
        let cursor = env
            .call_method(
                &resolver,
                jni::jni_str!("query"),
                jni::jni_sig!(
                    "(Landroid/net/Uri;[Ljava/lang/String;Ljava/lang/String;[Ljava/lang/String;Ljava/lang/String;)Landroid/database/Cursor;"
                ),
                &[
                    jni::objects::JValue::Object(&uri),
                    jni::objects::JValue::Object(&projection),
                    jni::objects::JValue::Object(&selection),
                    jni::objects::JValue::Object(&args),
                    jni::objects::JValue::Object(&sort),
                ],
            )?
            .l()?;
        if cursor.as_raw().is_null() {
            return Ok(Vec::new());
        }
        let rows = read_cursor(env, &cursor);
        // A cursor holds a native window on some images: close it however the walk ended.
        let _ = env.call_method(&cursor, jni::jni_str!("close"), jni::jni_sig!("()V"), &[]);
        rows
    })
}

/// Walk a MediaStore cursor into rows (newest first — the query's sort order), breaking at
/// 500 entries.
fn read_cursor(env: &mut jni::Env<'_>, cursor: &JObject<'_>) -> Result<Vec<MediaRow>, jni::errors::Error> {
    let id_col = column_index(env, cursor, "_id")?;
    let name_col = column_index(env, cursor, "_display_name")?;
    let mime_col = column_index(env, cursor, "mime_type")?;
    let size_col = column_index(env, cursor, "_size")?;
    let modified_col = column_index(env, cursor, "date_modified")?;
    let mut rows = Vec::new();
    let mut more = env
        .call_method(cursor, jni::jni_str!("moveToFirst"), jni::jni_sig!("()Z"), &[])?
        .i()?
        != 0;
    while more && rows.len() < 500 {
        let id = cursor_long(env, cursor, id_col)?;
        let name = cursor_string(env, cursor, name_col).unwrap_or_else(|| format!("image {id}"));
        let mime = cursor_string(env, cursor, mime_col).unwrap_or_default();
        let size = cursor_long(env, cursor, size_col).unwrap_or(0);
        let modified = cursor_long(env, cursor, modified_col).unwrap_or(0);
        rows.push(MediaRow { id, name, mime, size, modified });
        more = env
            .call_method(cursor, jni::jni_str!("moveToNext"), jni::jni_sig!("()Z"), &[])?
            .i()?
            != 0;
    }
    Ok(rows)
}

/// `cursor.getColumnIndexOrThrow(name)`.
fn column_index(env: &mut jni::Env<'_>, cursor: &JObject<'_>, name: &str) -> Result<i32, jni::errors::Error> {
    let key = env.new_string(name)?;
    Ok(env
        .call_method(
            cursor,
            jni::jni_str!("getColumnIndexOrThrow"),
            jni::jni_sig!("(Ljava/lang/String;)I"),
            &[jni::objects::JValue::Object(&key)],
        )?
        .i()?)
}

/// `cursor.getLong(column)` (zero for null).
fn cursor_long(env: &mut jni::Env<'_>, cursor: &JObject<'_>, column: i32) -> Result<i64, jni::errors::Error> {
    Ok(env
        .call_method(cursor, jni::jni_str!("getLong"), jni::jni_sig!("(I)J"), &[jni::objects::JValue::Int(column)])?
        .j()?)
}

/// `cursor.getString(column)`; `None` for a null value (a null MIME is normal in MediaStore).
fn cursor_string(env: &mut jni::Env<'_>, cursor: &JObject<'_>, column: i32) -> Option<String> {
    let value = env
        .call_method(
            cursor,
            jni::jni_str!("getString"),
            jni::jni_sig!("(I)Ljava/lang/String;"),
            &[jni::objects::JValue::Int(column)],
        )
        .ok()?
        .l()
        .ok()?;
    if value.as_raw().is_null() {
        return None;
    }
    let raw = value.as_raw();
    // SAFETY: `value` is the java.lang.String `getString` just returned (never another
    // type), and the cast borrows the local reference without taking ownership of it.
    let text = unsafe { env.as_cast_raw::<JString>(&raw) }.ok();
    let text = text.and_then(|s| s.try_to_string(env).ok());
    // 500 rows would otherwise pin a thousand local references for the query's lifetime.
    let _ = env.delete_local_ref(value);
    text
}

/// The bytes of one media item (`openInputStream`): a picked photo or RAW, read whole.
/// `Ok(None)` means the item passed the 1.5 GB cap — above any camera file, low enough that
/// a hostile or corrupt index cannot drive an unbounded allocation.
pub fn read_media_bytes(id: i64) -> Result<Vec<u8>, String> {
    const MAX: usize = 1_500_000_000;
    let read = with_activity(|env, activity| {
        let resolver = content_resolver(env, activity)?;
        let uri = parse_uri(env, &format!("content://media/external/files/{id}"))?;
        let stream = env
            .call_method(
                &resolver,
                jni::jni_str!("openInputStream"),
                jni::jni_sig!("(Landroid/net/Uri;)Ljava/io/InputStream;"),
                &[jni::objects::JValue::Object(&uri)],
            )?
            .l()?;
        if stream.as_raw().is_null() {
            return Ok(None);
        }
        let mut bytes = Vec::new();
        let chunk = env.new_byte_array(64 * 1024)?;
        loop {
            let n = env
                .call_method(
                    &stream,
                    jni::jni_str!("read"),
                    jni::jni_sig!("([B)I"),
                    &[jni::objects::JValue::Object(&chunk)],
                )?
                .i()?;
            if n <= 0 {
                break;
            }
            if bytes.len() + n as usize > MAX {
                let _ = env.call_method(&stream, jni::jni_str!("close"), jni::jni_sig!("()V"), &[]);
                return Ok(None);
            }
            let mut buf = vec![0i8; n as usize];
            chunk.get_region(env, 0, &mut buf)?;
            bytes.extend(buf.iter().map(|&b| b as u8));
        }
        let _ = env.call_method(&stream, jni::jni_str!("close"), jni::jni_sig!("()V"), &[]);
        Ok(Some(bytes))
    });
    match read {
        Ok(Some(bytes)) if bytes.len() <= MAX => Ok(bytes),
        Ok(_) => Err("the media item is larger than 1.5 GB".into()),
        Err(e) => Err(format!("can't read the media item: {e}")),
    }
}

/// A thumbnail of one media item as RGBA bytes (`loadThumbnail`, API 29+): `None` where the
/// system has none (the row then shows without a preview). Safe on any thread; each call
/// attaches to the JVM for its duration.
pub fn media_thumbnail(id: i64, max: u32) -> Option<(u32, u32, Vec<u8>)> {
    if sdk_version() < 29 {
        return None;
    }
    with_activity(|env, activity| {
        let resolver = content_resolver(env, activity)?;
        let uri = parse_uri(env, &format!("content://media/external/files/{id}"))?;
        let size = env.new_object(
            jni::jni_str!("android/util/Size"),
            jni::jni_sig!("(II)V"),
            &[jni::objects::JValue::Int(max as i32), jni::objects::JValue::Int(max as i32)],
        )?;
        let no_signal = JObject::null();
        let bitmap = env
            .call_method(
                &resolver,
                jni::jni_str!("loadThumbnail"),
                jni::jni_sig!(
                    "(Landroid/net/Uri;Landroid/util/Size;Landroid/os/CancellationSignal;)Landroid/graphics/Bitmap;"
                ),
                &[
                    jni::objects::JValue::Object(&uri),
                    jni::objects::JValue::Object(&size),
                    jni::objects::JValue::Object(&no_signal),
                ],
            )?
            .l()?;
        if bitmap.as_raw().is_null() {
            return Ok(None);
        }
        let rgba = bitmap_pixels(env, &bitmap);
        let _ = env.call_method(&bitmap, jni::jni_str!("recycle"), jni::jni_sig!("()V"), &[]);
        Ok(rgba)
    })
    .ok()
    .flatten()
}

/// A Bitmap's pixels as RGBA bytes via `getPixels` (ARGB ints — the unambiguous form).
fn bitmap_pixels(env: &mut jni::Env<'_>, bitmap: &JObject<'_>) -> Option<(u32, u32, Vec<u8>)> {
    let width = env.call_method(bitmap, jni::jni_str!("getWidth"), jni::jni_sig!("()I"), &[]).ok()?.i().ok()?;
    let height = env.call_method(bitmap, jni::jni_str!("getHeight"), jni::jni_sig!("()I"), &[]).ok()?.i().ok()?;
    let (width, height) = (u32::try_from(width).ok()?, u32::try_from(height).ok()?);
    // `loadThumbnail` honors the requested bounds; a hostile provider is guarded anyway.
    if width == 0 || height == 0 || width * height > 1024 * 1024 {
        return None;
    }
    let count = (width * height) as usize;
    let pixels = env.new_int_array(count).ok()?;
    env.call_method(
        bitmap,
        jni::jni_str!("getPixels"),
        jni::jni_sig!("([IIIIIII)V"),
        &[
            jni::objects::JValue::Object(&pixels),
            jni::objects::JValue::Int(0),
            jni::objects::JValue::Int(width as i32),
            jni::objects::JValue::Int(0),
            jni::objects::JValue::Int(0),
            jni::objects::JValue::Int(width as i32),
            jni::objects::JValue::Int(height as i32),
        ],
    )
    .ok()?;
    let mut argb = vec![0i32; count];
    pixels.get_region(env, 0, &mut argb).ok()?;
    let mut rgba = Vec::with_capacity(count * 4);
    for c in argb {
        rgba.push(((c >> 16) & 0xff) as u8);
        rgba.push(((c >> 8) & 0xff) as u8);
        rgba.push((c & 0xff) as u8);
        rgba.push(((c >> 24) & 0xff) as u8);
    }
    Some((width, height, rgba))
}

/// Open the system page where the user grants "allow access to manage all files" for this app.
/// Safe to call from any thread that may attach to the JVM (here: the UI thread, inside a
/// frame).
pub fn open_all_files_settings() -> Result<(), String> {
    with_activity(|env, activity| {
        let action = env.new_string("android.settings.MANAGE_ALL_FILES_ACCESS_PERMISSION")?;
        let package = env.new_string("package:com.photocomfy.app")?;
        let uri_class = env.find_class(jni::jni_str!("android/net/Uri"))?;
        let uri = env
            .call_static_method(
                &uri_class,
                jni::jni_str!("parse"),
                jni::jni_sig!("(Ljava/lang/String;)Landroid/net/Uri;"),
                &[jni::objects::JValue::Object(&package)],
            )?
            .l()?;
        let intent = env.new_object(
            jni::jni_str!("android/content/Intent"),
            jni::jni_sig!("(Ljava/lang/String;Landroid/net/Uri;)V"),
            &[jni::objects::JValue::Object(&action), jni::objects::JValue::Object(&uri)],
        )?;
        env.call_method(
            activity,
            jni::jni_str!("startActivity"),
            jni::jni_sig!("(Landroid/content/Intent;)V"),
            &[jni::objects::JValue::Object(&intent)],
        )?;
        Ok(())
    })
}

/// `activity.getContentResolver()`.
fn content_resolver<'a>(env: &mut jni::Env<'a>, activity: &JObject<'_>) -> Result<JObject<'a>, jni::errors::Error> {
    Ok(env
        .call_method(activity, jni::jni_str!("getContentResolver"), jni::jni_sig!("()Landroid/content/ContentResolver;"), &[])?
        .l()?)
}

/// `Uri.parse(text)`.
fn parse_uri<'a>(env: &mut jni::Env<'a>, text: &str) -> Result<JObject<'a>, jni::errors::Error> {
    let text = env.new_string(text)?;
    let uri_class = env.find_class(jni::jni_str!("android/net/Uri"))?;
    Ok(env
        .call_static_method(
            &uri_class,
            jni::jni_str!("parse"),
            jni::jni_sig!("(Ljava/lang/String;)Landroid/net/Uri;"),
            &[jni::objects::JValue::Object(&text)],
        )?
        .l()?)
}

/// A `String[]` for a projection or selection arguments; `storage_permissions` relies on the
/// same shape.
fn string_array<'a>(env: &mut jni::Env<'a>, items: &[&str]) -> Result<jni::objects::JObjectArray<'a>, jni::errors::Error> {
    let string_class = env.find_class(jni::jni_str!("java/lang/String"))?;
    let array = env.new_object_array(items.len() as i32, &string_class, &JObject::null())?;
    for (i, item) in items.iter().enumerate() {
        let value = env.new_string(item.to_owned())?;
        array.set_element(env, i, &value)?;
    }
    Ok(array)
}

/// Attach this thread to the JVM, hand the callback the environment and the activity, and
/// clear any exception a failed call left pending — a pending exception poisons every later
/// JNI call, so no error path may leave one behind. The callback's `Err` travels out as its
/// string form. The activity reference's lifetime is independent of the environment's (it
/// borrows the raw handle, not the frame).
fn with_activity<T>(
    run: impl for<'env, 'obj> FnOnce(&mut jni::Env<'env>, &JObject<'obj>) -> Result<T, jni::errors::Error>,
) -> Result<T, String> {
    let Some(app) = app() else {
        return Err("the Android app handle is not initialized".into());
    };
    // SAFETY: `vm_as_ptr` is the process's single JavaVM, created before `android_main` runs
    // and valid for the process lifetime; `JavaVM::from_raw` only wraps the pointer (its Drop
    // does not destroy the VM), and the ManuallyDrop makes that non-ownership explicit.
    let vm = std::mem::ManuallyDrop::new(unsafe { jni::JavaVM::from_raw(app.vm_as_ptr().cast()) });
    vm.attach_current_thread(move |env| {
        let raw_activity = app.activity_as_ptr() as jni::sys::jobject;
        // SAFETY: the pointer is the global Activity reference `AndroidApp` owns, valid while
        // `app` is alive (it outlives this call); the Cast wrapper borrows it without taking
        // ownership, so it is never deleted. This is android-activity's documented pattern.
        let activity = unsafe { env.as_cast_raw::<JObject>(&raw_activity)? };
        let out = run(env, &activity);
        if out.is_err() {
            env.exception_clear();
        }
        out
    })
    .map_err(|e: jni::errors::Error| e.to_string())
}
