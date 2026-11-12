#!/bin/sh
# Add the AppIcon catalog and its generated plist entries to a standalone .app bundle.
# Xcode builds use their Resources phase instead, so actool's generated icon names stay
# authoritative in both build paths (the iPad icon uses CFBundleIcons~ipad).
set -eu

APP="$1"
PLATFORM="${2:-iphonesimulator}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
case "$PLATFORM" in
    iphoneos|iphonesimulator) ;;
    *) echo "error: unsupported icon platform '$PLATFORM'" >&2; exit 1 ;;
esac
[ -f "$APP/Info.plist" ] || { echo "error: missing $APP/Info.plist" >&2; exit 1; }
ICON_TMP="$(mktemp -d)"
trap 'rm -r "$ICON_TMP"' EXIT
xcrun actool --compile "$APP" --platform "$PLATFORM" \
    --minimum-deployment-target 16.0 --target-device ipad --app-icon AppIcon \
    --output-partial-info-plist "$ICON_TMP/icons.plist" \
    "$SCRIPT_DIR/PhotoCraft/Assets.xcassets"
# Refresh generated entries so rerunning this helper after a rebuild is safe.
for key in CFBundleIcons 'CFBundleIcons~ipad'; do
    /usr/libexec/PlistBuddy -c "Delete :$key" "$APP/Info.plist" 2>/dev/null || true
done
/usr/libexec/PlistBuddy -c "Merge $ICON_TMP/icons.plist" "$APP/Info.plist"
