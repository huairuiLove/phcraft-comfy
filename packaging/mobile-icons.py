#!/usr/bin/env python3
"""Export mobile launcher assets from the approved SVG (requires resvg and Pillow)."""

import copy
import io
import json
from pathlib import Path
import subprocess
import tempfile
import xml.etree.ElementTree as ET

from PIL import Image


ROOT = Path(__file__).resolve().parent.parent
SVG_NS = "http://www.w3.org/2000/svg"
ET.register_namespace("", SVG_NS)


def render(tree, size):
    with tempfile.TemporaryDirectory(prefix="photocraft-mobile-icon-") as directory:
        source = Path(directory) / "icon.svg"
        output = Path(directory) / "icon.png"
        ET.ElementTree(tree).write(source, encoding="utf-8", xml_declaration=True)
        subprocess.run(
            ["resvg", "-w", str(size), "-h", str(size), str(source), str(output)],
            check=True,
        )
        return Image.open(io.BytesIO(output.read_bytes())).convert("RGBA")


def main():
    master = ET.parse(ROOT / "assets/app-icon/photocomfy-mobile.svg").getroot()
    artwork = master.find(f"{{{SVG_NS}}}g")
    if artwork is None:
        raise SystemExit("missing mobile icon artwork")

    # iOS applies its own corner mask. Shipping pre-rounded transparent artwork causes
    # dark corners and also fails the App Store's opaque app-icon requirement.
    square = copy.deepcopy(master)
    square_artwork = square.find(f"{{{SVG_NS}}}g")
    square_artwork.attrib.pop("clip-path", None)
    ios = ROOT / "apps/photocraft-ios/ios/PhotoCraft/Assets.xcassets/AppIcon.appiconset"
    entries = json.loads((ios / "Contents.json").read_text())["images"]
    for entry in entries:
        size = round(float(entry["size"].split("x")[0]) * float(entry["scale"][:-1]))
        render(square, size).convert("RGB").save(ios / entry["filename"])

    android = ROOT / "apps/photocraft-android/android/res"
    for density, size in [("mdpi", 48), ("hdpi", 72), ("xhdpi", 96), ("xxhdpi", 144), ("xxxhdpi", 192)]:
        render(master, size).save(android / f"mipmap-{density}/ic_launcher.png")

    # Adaptive layers fill the 108 dp canvas; keep the entire glyph, including the
    # sparkle, inside the central 66 dp safe circle so every launcher mask preserves it.
    foreground = copy.deepcopy(square)
    foreground_artwork = foreground.find(f"{{{SVG_NS}}}g")
    for background in list(foreground_artwork)[:2]:
        foreground_artwork.remove(background)
    foreground_artwork.set("transform", "translate(256 256) scale(0.74) translate(-256 -256)")
    background = copy.deepcopy(square)
    background_artwork = background.find(f"{{{SVG_NS}}}g")
    for glyph in list(background_artwork)[2:]:
        background_artwork.remove(glyph)
    drawable = android / "drawable-nodpi"
    drawable.mkdir(exist_ok=True)
    render(foreground, 432).save(drawable / "ic_launcher_foreground.png")
    render(background, 432).convert("RGB").save(drawable / "ic_launcher_background.png")


if __name__ == "__main__":
    main()
