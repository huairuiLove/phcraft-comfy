#!/bin/sh
# Cross-compiles libphotocraft_android.so and assembles the signed PhotoComfy APK — no Gradle,
# no Java: the manifest names the system android.app.NativeActivity, which loads the library
# directly (see android/AndroidManifest.xml).
#
#   apps/photocraft-android/android/build.sh [OUT_DIR]     # from the repo root; default: out/
#
# PROFILE selects the Cargo profile, release by default (debug: PROFILE=debug). The SDK lives
# in ANDROID_HOME or ~/Library/Android/sdk and needs cmdline-tools, one NDK, build-tools and a
# platforms;android-* package (see README › Build); the signing key is generated on first run.
set -eu

OUT_DIR="${1:-out}"
PROFILE="${PROFILE:-release}"
LIB="photocraft_android"
ABI="arm64-v8a"
TARGET="aarch64-linux-android"
API="26"       # minimum: Android 8.0
API_TARGET="29"

# Xcode-style minimal PATHs are not the problem here, but the same probe keeps the scripts
# interchangeable: cargo may live in ~/.cargo/bin, or be managed by Homebrew, rustup, mise/asdf.
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
    echo "error: cargo not found. Install Rust with rustup (https://rustup.rs), or add its bin" \
        "directory to this script's search list (apps/photocraft-android/android/build.sh find_cargo)." >&2
    exit 1
}
export PATH="$(dirname "$CARGO"):$PATH"
command -v cargo-ndk >/dev/null 2>&1 || {
    echo "error: cargo-ndk not found (it wires the NDK toolchain for Rust). Install it with" \
        "'cargo install cargo-ndk --locked' and add the aarch64-linux-android target" \
        "with 'rustup target add aarch64-linux-android'." >&2
    exit 1
}

SDK="${ANDROID_HOME:-$HOME/Library/Android/sdk}"
[ -d "$SDK" ] || {
    echo "error: Android SDK not found at $SDK. Set ANDROID_HOME, or install cmdline-tools" \
        "(README › Build): sdkmanager 'build-tools' 'platform-tools' 'platforms;android-36' ndk emulator." >&2
    exit 1
}
NDK="${ANDROID_NDK_HOME:-$(ls -d "$SDK/ndk"/* 2>/dev/null | sort | tail -1)}"
BT="$(ls -d "$SDK/build-tools"/* 2>/dev/null | sort | tail -1)"
PLATFORM="$(ls -d "$SDK/platforms"/android-* 2>/dev/null | sort | tail -1)"
[ -n "$NDK" ] && [ -d "$BT" ] && [ -d "$PLATFORM" ] || {
    echo "error: the SDK at $SDK lacks an NDK, build-tools or a platforms;android-* package" \
        "(README › Build lists the sdkmanager invocation)." >&2
    exit 1
}
export ANDROID_HOME="$SDK" ANDROID_NDK_HOME="$NDK"

REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$REPO"

# A caller's CARGO_TARGET_DIR silently ships a stale .so below (cargo builds elsewhere, the
# cp finds the old library) — this script owns its layout.
unset CARGO_TARGET_DIR

# Embed the craft-fonts faces like a desktop build (docs/development.md › Fonts): Android, like
# iPadOS, gives a NativeActivity app no usable fontconfig, so without these the UI's Chinese and
# other script text has no glyphs to fall back to (readable system Noto paths are still tried
# first; see crates/text/src/cjk.rs). The documented sibling checkout is picked up
# automatically; CRAFT_FONTS_DIR set outside wins.
if [ -z "${CRAFT_FONTS_DIR:-}" ] && [ -d "$REPO/../craft-fonts/fonts" ]; then
    CRAFT_FONTS_DIR="$REPO/../craft-fonts"
    export CRAFT_FONTS_DIR
fi

VERSION="$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
[ -n "$VERSION" ] || VERSION="0.0.0"

echo "building lib${LIB}.so for $TARGET ($PROFILE)…"
cargo ndk -t "$ABI" -P "$API" build --profile "$PROFILE" -p photocraft-android

SO="target/$TARGET/$PROFILE/lib${LIB}.so"
[ -f "$SO" ] || {
    echo "error: cargo produced no library at $SO" >&2
    exit 1
}

echo "assembling the APK with aapt2 ($BT)…"
# Absolute: the .so is zipped into the APK from inside $WORK/pkg below, after a cd.
WORK="$(cd "$(dirname "$0")" && pwd)/build"
rm -rf "$WORK"
mkdir -p "$WORK/pkg/lib/$ABI" "$OUT_DIR"
cp "$SO" "$WORK/pkg/lib/$ABI/"
"$BT/aapt2" compile --dir "$(dirname "$0")/res" -o "$WORK/resources.zip"
"$BT/aapt2" link \
    -I "$PLATFORM/android.jar" \
    -R "$WORK/resources.zip" \
    --manifest "$(dirname "$0")/AndroidManifest.xml" \
    --min-sdk-version "$API" \
    --target-sdk-version "$API_TARGET" \
    --version-code 1 \
    --version-name "$VERSION" \
    -o "$WORK/base.apk"
# extractNativeLibs="true" in the manifest, so the library may ride compressed in the zip.
cd "$WORK/pkg" && zip -qr "$WORK/base.apk" lib
cd "$REPO"

"$BT/zipalign" -f 4 "$WORK/base.apk" "$WORK/aligned.apk"

# A local signing key, generated once: Android has no provisioning-profile expiry — teammates
# install the APK and keep it. Keep the keystore to sign updates with the same identity, or
# Android makes everyone uninstall first.
KEYSTORE="$(dirname "$0")/debug.keystore"
if [ ! -f "$KEYSTORE" ]; then
    echo "generating a signing key at $KEYSTORE (storepass/keypass 'android')…"
    keytool -genkeypair -keystore "$KEYSTORE" -storepass android -keypass android \
        -alias photocomfy -keyalg RSA -keysize 2048 -validity 10000 \
        -dname "CN=PhotoComfy Team, O=PhotoCraft, C=US"
fi

APK="$OUT_DIR/PhotoComfy.apk"
"$BT/apksigner" sign --ks "$KEYSTORE" --ks-pass pass:android --key-pass pass:android \
    --ks-key-alias photocomfy --out "$APK" "$WORK/aligned.apk"
"$BT/apksigner" verify "$APK"
echo "installed $(wc -c < "$APK" | tr -d ' ') bytes into $APK ($(basename "$NDK") + build-tools $(basename "$BT"))"
echo "note: only $APK is signed — the intermediates in $(dirname "$0")/build/ (base.apk,"
echo "      aligned.apk) have no signature and will not install on any device."
