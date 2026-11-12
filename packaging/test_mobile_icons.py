"""Launcher resource regressions; run with python3 -m unittest discover -s packaging -p test_mobile_icons.py."""

import json
from pathlib import Path
import plistlib
import shutil
import subprocess
import tempfile
import unittest
import xml.etree.ElementTree as ET

from PIL import Image


ROOT = Path(__file__).resolve().parent.parent
IOS = ROOT / "apps/photocraft-ios/ios"
ANDROID = ROOT / "apps/photocraft-android/android"


class MobileIconTests(unittest.TestCase):
    def test_ios_exports_have_expected_size_and_no_alpha(self):
        catalog = IOS / "PhotoCraft/Assets.xcassets/AppIcon.appiconset"
        for entry in json.loads((catalog / "Contents.json").read_text())["images"]:
            with self.subTest(icon=entry["filename"]), Image.open(catalog / entry["filename"]) as image:
                pixels = round(float(entry["size"].split("x")[0]) * float(entry["scale"][:-1]))
                self.assertEqual(image.size, (pixels, pixels))
                self.assertEqual(image.mode, "RGB", "iOS launcher icons must be opaque")

    @unittest.skipUnless(shutil.which("plutil"), "needs Apple's plutil to read the Xcode project")
    def test_xcode_target_really_builds_the_icon_catalog(self):
        result = subprocess.run(
            ["plutil", "-convert", "json", "-o", "-", str(IOS / "PhotoCraft.xcodeproj/project.pbxproj")],
            check=True, capture_output=True,
        )
        objects = json.loads(result.stdout)["objects"]
        target = next(obj for obj in objects.values() if obj["isa"] == "PBXNativeTarget")
        catalogs = []
        for phase_id in target["buildPhases"]:
            phase = objects[phase_id]
            if phase["isa"] == "PBXResourcesBuildPhase":
                catalogs.extend(objects[objects[item]["fileRef"]]["path"] for item in phase["files"])
        self.assertIn("PhotoCraft/Assets.xcassets", catalogs)

    def test_android_launcher_uses_mask_safe_adaptive_layers(self):
        ns = "{http://schemas.android.com/apk/res/android}"
        application = ET.parse(ANDROID / "AndroidManifest.xml").getroot().find("application")
        self.assertEqual(application.get(ns + "icon"), "@mipmap/ic_launcher")
        self.assertEqual(application.get(ns + "roundIcon"), "@mipmap/ic_launcher")
        adaptive = ET.parse(ANDROID / "res/mipmap-anydpi-v26/ic_launcher.xml").getroot()
        self.assertEqual(adaptive.tag, "adaptive-icon")
        for layer in adaptive:
            name = layer.get(ns + "drawable").split("/")[-1]
            self.assertTrue((ANDROID / f"res/drawable-nodpi/{name}.png").is_file())
        with Image.open(ANDROID / "res/drawable-nodpi/ic_launcher_foreground.png") as image:
            self.assertEqual(image.mode, "RGBA")
            radius = image.width * 33 / 108
            for y in range(image.height):
                for x in range(image.width):
                    if image.getpixel((x, y))[3] >= 240:
                        self.assertLessEqual((x + 0.5 - image.width / 2) ** 2 + (y + 0.5 - image.height / 2) ** 2,
                                             radius ** 2, "glyph is cropped by an Android launcher mask")

    @unittest.skipUnless(shutil.which("xcrun"), "needs Xcode's asset compiler")
    def test_standalone_bundle_gets_actual_compiled_icon_names(self):
        with tempfile.TemporaryDirectory(prefix="photocraft-icons-test-") as directory:
            bundle = Path(directory) / "PhotoCraft.app"
            bundle.mkdir()
            plist = bundle / "Info.plist"
            plist.write_bytes(plistlib.dumps({"CFBundleIdentifier": "ai.storyteller.photocraft.ios"}))
            for _ in range(2):
                subprocess.run(["sh", str(IOS / "package-icons.sh"), str(bundle), "iphonesimulator"],
                               check=True, capture_output=True)
                metadata = plistlib.loads(plist.read_bytes())
                icon = metadata["CFBundleIcons~ipad"]["CFBundlePrimaryIcon"]
                self.assertEqual(icon["CFBundleIconName"], "AppIcon")
                self.assertTrue(icon["CFBundleIconFiles"])
                self.assertTrue(list(bundle.glob("AppIcon*.png")))
                self.assertTrue((bundle / "Assets.car").is_file())


if __name__ == "__main__":
    unittest.main()
