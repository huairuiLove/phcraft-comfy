# photocraft-ios

PhotoCraft for iPadOS: the same `ui-egui` shell and engine as the desktop, run by eframe's
native runner on Metal. A plain Rust `[[bin]]` — winit's iOS backend calls `UIApplicationMain`
itself from `EventLoop::run_app`, so no Objective-C bootstrap and no exported symbols are
needed; the binary is the app bundle's `CFBundleExecutable`.

Touch input is egui input: winit's iOS backend delivers taps (and Apple Pencil force, as
`Force::Calibrated`) as egui `Touch`/pointer events, `input/stylus.rs` feeds the pressure, and
`touch_twins` de-duplicates double-delivered taps in `raw_input_hook`. Two fingers pan and pinch
the canvas and stand the tool down (Photoshop on iPad). The interface runs in Preferences ›
Interface › Touch Interface (on by default on iPadOS): magnified targets, always-visible scroll
bars, and the safe area respected at the screen edges. Services the OS doesn't provide an iPad
app (desktop file dialogs, the clipboard bridge, crash recovery) stay unset and the shell
degrades gracefully, as in the browser; preferences persist in the app container's
`Application Support/PhotoCraft` directory.

## Fonts

iPadOS, unlike macOS, lets no app read the system font files, so the UI's script fallback needs
the embedded craft-fonts faces: `ios/build.sh` picks up the documented sibling checkout
(`../craft-fonts`) automatically, exactly like a desktop build with `CRAFT_FONTS_DIR`. Without
it the build works but non-Latin interface text shows missing glyphs. The system locale (for the
interface language and the CJK script choice) is read from `NSLocale` at startup.

## What the Xcode project contains

`ios/PhotoCraft.xcodeproj` looks nearly empty when you open it, and that is the design, not a
broken project. It has one target, `PhotoCraft`, with one build phase, "Build the PhotoCraft
binary with Cargo", which runs `ios/build.sh` (a `cargo build` for the chosen iOS target) and
copies the resulting binary into `PhotoCraft.app`; Xcode signs the finished bundle. There are no
Swift or Objective-C sources in the target: the app is the Rust crate next to `ios/`, and the two
small ObjC bridges (`PCSceneDelegate.m`, `PCDocumentPicker.m`) are compiled by `build.rs` with
`cc`.

So the navigator holds references, not the build: `Info.plist`, `Products/PhotoCraft.app`, and
the "PhotoCraft (Rust crate)" group with the files the cargo phase builds. Edit any of them in
the group (or in your Rust editor), press Run. The phase runs on every build and lets cargo's
fingerprint decide what to rebuild: an unchanged tree costs a couple of seconds (cargo no-op,
copy, re-sign). The executable is declared as the phase's output, which is what makes Xcode
re-sign the bundle after the copy — with no output declared, Xcode skipped CodeSign and the
`.app` stayed unsigned. Do not move the `.m` files into the target's Compile Sources phase:
`build.rs` already compiles them, and compiling them twice fails to link.

## Build

One-time: `rustup target add aarch64-apple-ios aarch64-apple-ios-sim` (a C compiler for the
tiny ObjC scene delegate comes with Xcode; `build.rs` compiles it via `cc`).

```sh
# Simulator (from the repo root; installs into an .app bundle you can `simctl install`):
apps/photocraft-ios/ios/build.sh out PhotoCraft iphonesimulator arm64

# Device:
PROFILE=release apps/photocraft-ios/ios/build.sh out PhotoCraft iphoneos arm64
```

Or open `ios/PhotoCraft.xcodeproj` in Xcode: the target's script phase runs `ios/build.sh`
(`PROFILE` env selects the Cargo profile; release is the default) and Xcode signs the bundle.
For a real iPad with a free Apple ID, set the target's Signing & Capabilities team to your
Personal Team, connect the iPad, and press Run. The free profile expires after 7 days —
press Run again to re-sign. See `docs/development.md` for the full walkthrough.

## Sideload to a real iPad with a free Apple ID (no App Store review)

1. Connect the iPad and open `ios/PhotoCraft.xcodeproj` in Xcode.
2. Select the PhotoCraft target › Signing & Capabilities › check "Automatically manage
   signing" and pick your **Personal Team** (sign in with your free Apple ID under Xcode ›
   Settings › Accounts if prompted).
3. On the iPad, trust the developer: Settings › General › VPN & Device Management › your
   Apple ID › Trust (first run only).
4. Select your iPad as the run destination and press Run. Xcode builds through
   `ios/build.sh`, signs, and installs.
5. The free profile expires after **7 days**: run again from Xcode to re-sign (data is kept).
   A free account signs at most 3 apps at once. A paid Apple Developer Program membership
   ($99/year) lifts the expiry to a year and adds TestFlight, but is not needed for personal
   use.

## Opening and saving files

File ▸ Open presents the system document picker (`ios/PCDocumentPicker.m`, driven from safe
Rust through the C ABI in `src/picker.rs`; the picker reads the files — security scope and
all — inside its delegate and hands `(name, bytes)` to the shell, which answers the app's
pending dialog with the contents). Save answers at once with the suggested name (the web
download's shape) and `write` stages the bytes in tmp and presents the export sheet that
copies them wherever the user picks — so `write_unconfirmed` stays true: cancelling the
sheet saved nothing, and the document honestly stays marked unsaved (#4962 semantics).
Folder pickers are not offered (as on the web). `src/picker.rs` holds this crate's only
unsafe: the pointer reads that copy the callback's bytes across the ABI.

Apple Pencil pressure/tilt arrives through winit's iOS touch events and the stylus feed, like
a pen on the desktop.

## Troubleshooting

- **Nothing appears when you Run (Xcode 26+)**: this Xcode has no Simulator.app — simulator
  windows live in **DeviceHub** (`Xcode ▸ Open Developer Tool ▸ DeviceHub`, or it opens on
  Run). If the device shows "Not running" there, double-click its row to open its window; the
  app may already be running on the device (check with `xcrun simctl io booted screenshot`).
- **Xcode says "Unable to launch … No such process" on Run**: the app is fine — the debugger
  attaching to this ~127 MB binary on a simulator times out and Xcode reports the launch as
  failed while the process comes up moments later (the launch trail in `Documents/launch.log`
  completes and the UI renders). The shared scheme already runs without a debugger attached
  (`selectedDebuggerIdentifier` empty in `PhotoCraft.xcscheme`), which keeps Run from reporting
  this at all. If you ever see it anyway, switch to the Simulator and look — the app is there.
- **UIScene adoption (iPadOS 26+/27)**: iOS 27 traps at launch for apps that don't adopt the
  UIScene lifecycle (`EXC_BREAKPOINT` in
  `UIApplicationEvaluateRuntimeIssueForNoSceneLifecycleAdoption`; iOS 26 only warns). The app
  adopts it without touching the winit fork or writing unsafe Rust: `build.rs` compiles an
  empty ObjC scene delegate (`ios/PCSceneDelegate.m`, force-loaded past the Apple linker's
  dead-strip of unreferenced members) into the binary, and Info.plist's
  `UIApplicationSceneManifest` names it (`UISceneDelegateClassName`). winit keeps driving the
  window through the classic path — its plain UIWindow attaches to the foreground scene the
  manifest creates. The delegate class name is the load-bearing part: a manifest without one
  leaves no foreground scene and every OS shows a black screen.
- **Relaunch rendered blank (first launch after install fine)**: fixed — once Info.plist
  adopts scenes, UIKit saves the scene session to `Library/Saved Application State` and
  restores it on the next launch, and the restored scene never attaches winit's freshly
  created window (egui's viewport stays its 10000x10000 default). The shell now drops its own
  saved scene state before the UI starts (`discard_saved_scene_state` in `src/main.rs`), so
  every launch takes the fresh-install path. Verified: four consecutive relaunches on the
  iOS 27.2 simulator render identically; iOS 27.0 likewise; xcodebuild install-and-run too.
- **Blank screen for a while, then the app appears**: that is the cold start, not a fault — a
  freshly booted simulator's first launch takes ~13 s from launch to first frame (loading the
  ~127 MB binary and compiling the GPU pipelines); every later launch is ~1 s. The launch
  screen is dark to match the app, so it reads as loading rather than a white screen. Wait out
  the first launch after booting a simulator or a device restart.
- **Opens to a white screen on the device and quits at once**: usually the expired signing
  profile — a free profile lasts 7 days, and an expired app is killed the moment it launches.
  Run again from Xcode to re-sign; check Settings › General › VPN & Device Management.
- **Anything else**: the build writes a startup trail to `Documents/launch.log`
  (`launched / app created / first frame` — the missing step names where it stopped) and every
  Rust panic with a backtrace to `Documents/crash.log`, both readable over Finder file sharing
  or Files › On My iPad › PhotoCraft. Send them and the step names the failure.
