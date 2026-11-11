# Local fork patches (MUST land upstream in storytold/winit and storytold/egui)

These three fixes currently live only in the local cargo checkouts of the pinned forks
(`~/.cargo/git/checkouts/…`). They are applied there by hand; any `cargo clean`/`cargo update`
or fresh clone loses them. Until each lands as a commit in its fork with a Cargo.toml rev bump:

```sh
patch -d ~/.cargo/git/checkouts/winit-0b02aa607dc0343f/a517b38 -p1 < 0001-winit-ios-inner-size-and-outer-size.patch
patch -d ~/.cargo/git/checkouts/egui-057e9a0485eda407/6d518fc -p1 < 0002-egui-insets-per-frame-and-no-zoom-rescale.patch
cargo clean -p winit -p egui -p egui-winit --release --target aarch64-apple-ios-sim
```

1. **winit iOS `inner_size`** returns the safe-area-inset-subtracted size; eframe sizes the wgpu
   surface from it, so the full-screen layer presented the surface anisotropically stretched
   (6.6% vertically on iPad Pro 11) and taps landed progressively below their targets.
2. **egui-winit safe-area insets** were raw UIKit points (32) fed into egui-point math
   (true value 32/zoom) and were only read on launch events (zoom still 1.0); refreshed per
   frame with the current zoom now.
3. **egui `begin_pass` zoom rescale** overwrote `raw_input.screen_rect` with the previous
   content rect scaled by the zoom ratio; the Touch Interface scale derives zoom FROM that
   rect, so the two chased each other forever — the infinite zoom/jitter on any non-full-screen
   window. The block is removed.
