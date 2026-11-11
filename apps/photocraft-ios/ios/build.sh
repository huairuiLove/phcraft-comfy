#!/bin/sh
# Builds the Rust binary for PhotoCraft on iPadOS and copies it into the .app bundle.
#
# Called by the Xcode target's script phase as:
#   build.sh "$BUILT_PRODUCTS_DIR" "$PRODUCT_NAME" "$PLATFORM_NAME" "$ARCHS"
# and usable standalone with the same four arguments, e.g. from the repo root:
#   apps/photocraft-ios/ios/build.sh out PhotoCraft iphonesimulator arm64
#
# The Xcode environment names the SDK (PLATFORM_NAME) and architectures (ARCHS);
# PROFILE selects the Cargo profile, release by default (debug: PROFILE=debug).
set -eu

OUT_DIR="$1"
PRODUCT="$2"
PLATFORM="${3:-iphonesimulator}"
ARCHS="${4:-arm64}"

case "$PLATFORM" in
    iphoneos) TARGET="aarch64-apple-ios" ;;
    iphonesimulator)
        # The simulator runs the host architecture; PhotoCraft builds for arm64 (Apple silicon).
        case "$ARCHS" in
            *arm64*) TARGET="aarch64-apple-ios-sim" ;;
            *) TARGET="x86_64-apple-ios" ;;
        esac
        ;;
    *)
        echo "error: unsupported platform '$PLATFORM' (expected iphoneos or iphonesimulator)" >&2
        exit 1
        ;;
esac
PROFILE="${PROFILE:-release}"

# Xcode's script phases run with a minimal PATH: cargo may live in ~/.cargo/bin, or be managed
# by Homebrew, rustup's own layout, mise or asdf — probe the usual homes instead of failing.
find_cargo() {
    if command -v cargo >/dev/null 2>&1; then
        command -v cargo
        return 0
    fi
    for candidate in "$HOME/.cargo/bin" /opt/homebrew/bin /usr/local/bin "$HOME/.local/bin" "$HOME/.rustup/toolchains"/*/bin "$HOME/.cargo"/*/bin; do
        if [ -x "$candidate/cargo" ]; then
            echo "$candidate/cargo"
            return 0
        fi
    done
    return 1
}

CARGO="$(find_cargo)" || {
    echo "error: cargo not found. Xcode's script phase runs with a minimal PATH; install Rust" \
        "with rustup (https://rustup.rs), or add its bin directory to this script's search" \
        "list (apps/photocraft-ios/ios/build.sh find_cargo)." >&2
    exit 1
}
export PATH="$(dirname "$CARGO"):$PATH"
echo "cargo: $CARGO"

case "${PLATFORM_NAME-}" in
    "") ;; # standalone run: PLATFORM_NAME comes in as $3 below
    iphoneos | iphonesimulator) ;;
    *)
        echo "error: this target builds for iPadOS only. Pick an iPad simulator or a real iPad" \
            "as the run destination in Xcode's toolbar (not 'My Mac'); this platform was:" \
            "'${PLATFORM_NAME}'." >&2
        exit 1
        ;;
esac

REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$REPO"

# Embed the craft-fonts faces like a desktop build (docs/development.md › Fonts): iPadOS, unlike
# macOS, lets no app read the system font files, so without these the UI's Chinese and other
# script text has no glyphs to fall back to. The documented sibling checkout is picked up
# automatically; CRAFT_FONTS_DIR set outside wins.
if [ -z "${CRAFT_FONTS_DIR:-}" ] && [ -d "$REPO/../craft-fonts/fonts" ]; then
    CRAFT_FONTS_DIR="$REPO/../craft-fonts"
    export CRAFT_FONTS_DIR
fi

echo "building photocraft-ios for $TARGET ($PROFILE)…"
cargo build --profile "$PROFILE" --target "$TARGET" -p photocraft-ios

BIN="target/$TARGET/$PROFILE/photocraft-ios"
if [ ! -f "$BIN" ]; then
    echo "error: cargo produced no binary at $BIN" >&2
    exit 1
fi

APP="$OUT_DIR/$PRODUCT.app"
mkdir -p "$APP"
cp "$BIN" "$APP/$PRODUCT"
# Standalone runs (simctl install) need the bundle metadata too; under Xcode the Info.plist
# step provides it, so only fill in what's missing. The placeholders are Xcode build settings;
# standalone they get the same values the target's settings carry.
if [ ! -f "$APP/Info.plist" ]; then
    sed -e "s/\$(EXECUTABLE_NAME)/$PRODUCT/" \
        -e "s/\$(PRODUCT_BUNDLE_IDENTIFIER)/ai.storyteller.photocraft.ios/" \
        -e "s/\$(MARKETING_VERSION)/0.20.1/" \
        -e "s/\$(CURRENT_PROJECT_VERSION)/1/" \
        "$(dirname "$0")/PhotoCraft/Info.plist" > "$APP/Info.plist"
    printf 'APPL????' > "$APP/PkgInfo"
fi
echo "installed $(wc -c < "$APP/$PRODUCT" | tr -d ' ') bytes into $APP/$PRODUCT"
