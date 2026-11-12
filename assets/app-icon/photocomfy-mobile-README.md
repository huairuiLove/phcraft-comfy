# PhotoComfy mobile icon

This is the mobile-only icon concept for the iOS and Android shells. The source concept was generated with the locally configured Comfy Desktop OpenAI-compatible image node (`gpt-image-2.5-sunburst`) and then redrawn as deterministic SVG geometry for crisp platform exports.

- `photocomfy-concept-v1.png`: source raster concept.
- `photocomfy-mobile.svg`: canonical vector source, 512-unit rounded square.
- `photocomfy-mobile-1024.png`: rounded preview of the approved vector design.
- [`AppIcon.appiconset`](../../apps/photocraft-ios/ios/PhotoCraft/Assets.xcassets/AppIcon.appiconset/): opaque square iPad icons, masked by iOS itself.
- [`android/res`](../../apps/photocraft-android/android/res/): legacy launcher PNGs and Android 8+ adaptive background/foreground layers. The glyph fits the central 66 dp safe circle on the 108 dp adaptive canvas.

Regenerate the platform exports from the SVG with `python3 packaging/mobile-icons.py`
from the repository root (requires `resvg`, like `packaging/icons.sh`, and Pillow).
Check them with `python3 -m unittest discover -s packaging -p test_mobile_icons.py -v`.

Xcode compiles the catalog in its Resources phase. Standalone iOS builds use
`ios/package-icons.sh` to compile the same catalog and merge the generated icon names
into the bundle's Info.plist. Android's `build.sh` compiles the `res` directory with aapt2.
The desktop icon is managed separately by `packaging/icons.sh`.
