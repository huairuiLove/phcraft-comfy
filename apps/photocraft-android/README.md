# photocraft-android (PhotoComfy)

PhotoCraft for Android, distributed as **PhotoComfy**: the same `ui-egui` shell and engine as the
desktop, run by eframe's native runner on wgpu (Vulkan, with GLES where the adapter says so).
The APK carries **no Java code** (`hasCode="false"`): the manifest names the system
`android.app.NativeActivity`, which loads `libphotocraft_android.so`; android-activity brings up
the JVM thread and calls `android_main` (`src/lib.rs`), which hands eframe its `AndroidApp` and
runs the same `run_native` path as every other shell. The package id is `com.photocomfy.app`.

Touch input is egui input: winit's Android backend delivers taps as `Touch` events
(`input/stylus.rs` feeds the pressure for styli that report it), `touch_twins` de-duplicates
double-delivered taps in `raw_input_hook`. Two fingers pan and pinch the canvas and stand the
tool down, as on iPadOS. The Touch Interface preference (`crates/engine/src/prefs.rs`
`default_touch_ui`) is on by default on Android: magnified targets, always-visible scroll bars.
Android, unlike iPadOS, reports no safe-area insets to egui (winit has no support yet), so the
activity runs fullscreen and the UI owns the whole screen. Preferences persist in the app's
private `files` directory; clipboard bridge and crash recovery stay unset and the shell
degrades gracefully, as in the browser and on iPadOS.

## Opening and saving files

The system SAF pickers can't answer a NativeActivity (`onActivityResult` is not forwarded to
native code anywhere in the stack), so File ▸ Open / Save As open an **in-app picker**
(`src/browser.rs`, an egui panel over the app) with three tabs:

- **Gallery** — the device's photos, queried straight from MediaStore over JNI
  (`src/platform.rs`), newest first, with streaming thumbnails and camera RAW files included
  (MediaStore's `image/*` MIMEs cover ARW/CR2/CR3/DNG/NEF and friends). The first use shows
  an *Allow photo access* button (`Activity.requestPermissions` over JNI); the grant lands
  without a callback — NativeActivity gets none — so the button says to tap *Refresh* after
  choosing. A pick reads the file through `ContentResolver.openInputStream` on a worker
  thread and answers with its contents, the same shape as the iPad picker's answer.
- **Projects** — the app's own library at `Android/data/com.photocomfy.app/files/Projects`,
  listed recursively (`.pcraft`, `.psd`, `.psb`), newest first, one tap to reopen. **Save As
  starts here**: every save accumulates in the app, so no project is ever lost, and the
  folder is reachable over USB (`adb push/pull`, or any MTP file browser) for bulk moves.
- **Files** — a plain folder browser over std::fs, rooted as far as the system allows:
  - With *all files access* granted (`MANAGE_EXTERNAL_STORAGE`, the manifest asks for it) the
    tab roots at the whole shared storage — works through Android 15; grant it once in
    Settings ▸ Apps ▸ PhotoComfy ▸ All files access (the browser's banner links there on
    older systems).
  - The APK targets 29 with `requestLegacyExternalStorage="true"`, so the **ordinary
    READ/WRITE_EXTERNAL_STORAGE pair — the Gallery tab's same Allow button — carries the
    legacy full-tree view on later versions too** (verified on the API 36 emulator:
    `storage granted: true` in `files/launch.log` after `pm grant`): raw-path reads and
    writes of the whole shared storage, RAW and `.psd` included, on Android 16 where
    MANAGE_EXTERNAL_STORAGE is gone.
  - With nothing granted the tab roots at the app's own folder on shared storage, which
    every Android version lets the app and **adb over USB** read and write with no
    permission.

Answers carry real paths where the file system has them (replace prompts, in-place saves and
the recent-files list work as on the desktop) and contents where the source is a MediaStore
stream. Known gap on the emulator (API 36 image): the listing rows render but a tap on them
is not registered by the egui fork's Android input path (the Cancel/Refresh buttons work; the
same flows pass in the off-device `tests/browser_flow.rs` and `tests/gallery_flow.rs` kittest
suites). Real-device behaviour is the open question — if rows also ignore taps there, the
next stop is the egui fork's touch-to-click conversion, not this shell.

AccessKit (screen-reader support) is compiled out on Android: eframe only offers it with the
`android-game-activity` backend, which would pull the Games SDK's Java glue into the APK.
`ui-egui`'s eframe dependency enables the `accesskit` feature everywhere else.

## AI image generation

The Generate panel (the drawer's sparkles button) is the same shared ComfyUI workspace as the
desktop — the prompt templates (生图 · 扩图, 大气透视), the send modes (composite / active layer /
selection crop), the result placement and the shelf — with one difference: there is no local
ComfyUI server to talk to, so generation goes straight to an OpenAI-compatible Image API
(`src/api_gen.rs`, the iPad app's backend). Set the endpoint, model and key once in Preferences ▸
Integrations (or right in the panel; the Check button asks the endpoint for its model list);
results persist in the app's private `files/Generations`, so the shelf survives a relaunch.

## Build

One-time (`rustup` and Homebrew assumed; all SDK parts live under `~/Library/Android/sdk`):

```sh
rustup target add aarch64-linux-android
cargo install cargo-ndk --locked

# Android SDK pieces (accept the licenses when asked), then from the repo root:
apps/photocraft-android/android/build.sh out
```

`build.sh` needs `ANDROID_HOME` (or the default `~/Library/Android/sdk`) with: cmdline-tools,
one NDK (`sdkmanager ndk`), `build-tools` (aapt2/zipalign/apksigner) and a `platforms;android-*`
package. It cross-compiles through `cargo ndk` (aapt2 needs no Gradle), assembles and signs the
APK — generating a local keystore on first run (storepass/keypass `android`) — and writes
`out/PhotoComfy.apk` for `arm64-v8a` (every modern phone and tablet; the Apple-silicon emulator
runs the same ABI). `PROFILE=debug` selects the Cargo profile. The sibling `../craft-fonts`
checkout is picked up exactly like an iPadOS build: without it the build works, but CJK and other
non-Latin interface text shows missing glyphs unless the device's Noto files are readable.

The build also compiles `android/res` with aapt2: the manifest's launcher icon uses the
mobile design from `assets/app-icon/photocomfy-mobile.svg`. Android 8+ uses adaptive
background/foreground layers, preserving the mark under round and rounded-square masks.
Regenerate platform exports with `python3 packaging/mobile-icons.py`.

## Install and share

```sh
adb install -r out/PhotoComfy.apk            # over USB/wireless debugging
adb shell am start -n com.photocomfy.app/android.app.NativeActivity
```

Or send the APK itself (chat, drive, mail): the receiver opens it and allows "install unknown
apps" for that source once. **Nothing expires** — Android has no Apple-style provisioning
profile, so one install keeps working; sign updates with the same keystore
(`android/debug.keystore`, already gitignored) or every update forces an uninstall first.
`versionCode` is still 1 for every build; raise it in `android/build.sh` when you want Android
to treat a rebuild as an update over an existing install. Note that the keystore is generated
per checkout: an APK built on another machine signs differently, so installing it over an
existing install fails ("app not installed") until the old one is uninstalled. Chat apps also
mangle APK transfers (renamed `.apk.1`, stripped); copy over USB, a drive app, or a cable.

## Diagnostics

The build writes a startup trail to `files/launch.log` (`launched / app created / first frame` —
the missing step names where it stopped) and every Rust panic with a backtrace to
`files/crash.log`. The team manifest keeps `android:debuggable="true"` so both are reachable:

```sh
adb run-as com.photocomfy.app cat files/launch.log
adb run-as com.photocomfy.app cat files/crash.log
adb logcat -s RustStdoutStderr:d   # native stderr, on images that route it
```

On the emulator, `adb exec-out screencap -p > shot.png` and `adb shell input tap/swipe` drive
the UI for visual checks. The `PHOTOCRAFT_OPEN_NEW` / `PHOTOCRAFT_OPEN_DRAWER` aids from
`src/lib.rs` read process environment variables, which `am start` does not carry on Android —
use taps instead, or temporarily default them in the shell.

## Emulator (Apple silicon)

The arm64-v8a system images run natively on M-series Macs:

```sh
sdkmanager "system-images;android-36;google_apis;arm64-v8a"   # once
avdmanager create avd -n photocomfy -k "system-images;android-36;google_apis;arm64-v8a"
emulator -avd photocomfy -no-window -no-audio &               # or with a window
adb install -r out/PhotoComfy.apk
adb shell am start -n com.photocomfy.app/android.app.NativeActivity
adb exec-out screencap -p > shot.png
```

Cold start after a fresh emulator boot takes a while (loading the ~127 MB library and compiling
the GPU pipelines, as on the iPad simulator); later launches are fast. If wgpu finds no Vulkan
on the emulator image, it falls back to GLES; on real devices Vulkan is the norm.

## Troubleshooting

- **"Install" is greyed out or a risk dialog blocks it (Xiaomi/HyperOS, MIUI, and other
  Chinese ROMs)**: these ROMs intercept sideloaded APKs on top of stock Android.
  - HyperOS/MIUI's **pure mode** (纯净模式, on by default) blocks non-store apps with a
    security prompt and a greyed button — search 设置 for "纯净模式" and turn it off (some
    builds ask you to sign in to a Xiaomi account first).
  - The app you open the APK *from* needs **install unknown apps** permission once:
    设置 › 隐私保护 › 特殊权限 › 安装未知应用 › that file manager / browser.
  - MIUI's developer-options **"install via USB"** toggle can require a signed-in Xiaomi
    account and a SIM; plain `adb install` from a booted `adb` session is the least-gated
    path on a desk.
  - "应用未安装" (app not installed) on *update* means a signature change (per-checkout
    keystore; see Install and share) — uninstall the old app first.
- **Fails to install with `INSTALL_FAILED_NO_MATCHING_ABIS`**: the device is not arm64-v8a
  (an x86_64 emulator on an Intel PC, or a very old 32-bit device). Build that ABI too:
  `cargo ndk -t x86_64 ...` and drop the .so into `lib/x86_64/` before the zip step.
- **Blank screen / instant quit**: pull `files/launch.log` and `files/crash.log` (Diagnostics);
  with no entries at all, the library never ran — `adb logcat` around the launch shows the load
  error.
- **Text renders as boxes**: built without the craft-fonts sibling (`../craft-fonts`) on a
  device whose `/system/fonts` (or the fonts APEX) has no readable CJK collection.
