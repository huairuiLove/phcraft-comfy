# photocraft-ios

PhotoComfy for iPadOS: the same `ui-egui` shell and engine as the desktop, run by eframe's
native runner on Metal. A plain Rust `[[bin]]` — winit's iOS backend calls `UIApplicationMain`
itself from `EventLoop::run_app`, so no Objective-C bootstrap and no exported symbols are
needed; the binary is the app bundle's `CFBundleExecutable`.

Touch input is egui input: winit's iOS backend delivers taps (and Apple Pencil force, as
`Force::Calibrated`) as egui `Touch`/pointer events, `input/stylus.rs` feeds the pressure, and
`touch_twins` de-duplicates double-delivered taps in `raw_input_hook`. Painting is pen-only, as
Photoshop on iPad: a finger (a contact without a force) pans, pinches and taps the UI but never
starts a brush stroke or a Liquify push — the hint says "Painting needs the Apple Pencil" —
while the pencil, a mouse and automation paint (the `pen_only_painting` flag is each shell's
own choice: Android and the web leave it off). Long-press is the touch
right-click: a finger held still for half a second opens every context menu, including the text
fields' edit menu (Cut/Copy/Paste — Paste reads the system pasteboard through the clipboard
bridge). Two fingers pan and pinch the canvas and stand the tool down (Photoshop on iPad).
Moving layers to another document works without the desktop's tab drag: the **layer tray** —
drag a layer row out and a tray strip appears over the canvas's top edge ("Layer Tray · n");
release the row on it (or use "Add to Tray" in a layer's long-press menu, or drop the row on
the tray button in the Layers footer) and the selection is stashed. Switch documents, then
tap the tray button: its popup lists the slots, each row showing the stashed layer's thumbnail
and name over the document it came from, and a tap places a slot — or drag a slot row out of
the popup onto the canvas to place its layers centred on that point, converted to the
destination's colour and depth.
The popup's **Copy | Cut** toggle (a tap; iPadOS has no right-click, so nothing in the tray
needs one) picks the semantics: Copy keeps the layers in the source document and keeps the
slot after placing; Cut removes them from the source on stash and empties the slot on place
— move. A **quick-actions column** (Undo, Redo, Copy, Cut, Paste) floats at the canvas's
top-right corner, left of the end dock — the touch answer to ⌘Z/⌘⇧Z/⌘C/⌘X/⌘V; each button
runs the engine's own edit command, so enablement and the OS-clipboard paste behave exactly
as the Edit menu's. And because an iPad has neither an Esc nor a Tab key, a full-screen
mode (View › Screen Mode) keeps a small **exit chip** at the canvas's top-left corner: one
tap returns to the Standard screen mode, where the tools, drawer and menus come back. The
interface runs in Preferences ›
Interface › Touch Interface (on by default on iPadOS): magnified targets, always-visible scroll
bars, and the safe area respected at the screen edges. Services the OS doesn't provide an iPad
app (desktop file dialogs, the clipboard bridge, crash recovery) stay unset and the shell
degrades gracefully, as in the browser; preferences persist in the app container's
`Application Support/PhotoComfy` directory.

## Fonts

iPadOS, unlike macOS, lets no app read the system font files, so the UI's script fallback needs
the embedded craft-fonts faces: `ios/build.sh` picks up the documented sibling checkout
(`../craft-fonts`) automatically, exactly like a desktop build with `CRAFT_FONTS_DIR`. Without
it the build works but non-Latin interface text shows missing glyphs. The system locale (for the
interface language and the CJK script choice) is read from `NSLocale` at startup.

## Build

One-time: `rustup target add aarch64-apple-ios aarch64-apple-ios-sim` (a C compiler for the
tiny ObjC scene delegate comes with Xcode; `build.rs` compiles it via `cc`).

```sh
# Simulator (from the repo root; installs into an .app bundle you can `simctl install`):
apps/photocraft-ios/ios/build.sh out PhotoComfy iphonesimulator arm64

# Device:
PROFILE=release apps/photocraft-ios/ios/build.sh out PhotoComfy iphoneos arm64
```

Or open `ios/PhotoComfy.xcodeproj` in Xcode: the target's script phase runs `ios/build.sh`
(`PROFILE` env selects the Cargo profile; release is the default) and Xcode signs the bundle.
For a real iPad with a free Apple ID, set the target's Signing & Capabilities team to your
Personal Team, connect the iPad, and press Run. The free profile expires after 7 days —
press Run again to re-sign. See `docs/development.md` for the full walkthrough.

## Sideload to a real iPad with a free Apple ID (no App Store review)

1. Connect the iPad and open `ios/PhotoComfy.xcodeproj` in Xcode.
2. Select the PhotoComfy target › Signing & Capabilities › check "Automatically manage
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
  (`selectedDebuggerIdentifier` empty in `PhotoComfy.xcscheme`), which keeps Run from reporting
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
  or Files › On My iPad › PhotoComfy. Send them and the step names the failure.
