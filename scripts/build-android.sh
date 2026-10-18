#!/usr/bin/env bash
# ==============================================================================
# build-android.sh — build the mathed Android app (APK) with the NDK + SDK
#
# There is no Gradle project. The app is a `cdylib` and a manifest; this script
# is the whole build system:
#
#   1. cross-compile `crates/mathed_android` for the target ABI with the NDK
#      clang as the linker (and as `CC` for the one dependency that ships C,
#      `psm` via `pest` -> `loro`);
#   2. link the manifest and resources into a base APK with `aapt2`;
#   3. add `lib/<abi>/libmathed_android.so` under its in-APK path;
#   4. align and sign it with the platform's `zipalign`/`apksigner` and a
#      generated debug key.
#
# Usage:
#   bash scripts/build-android.sh                 # release, x86_64 (Waydroid)
#   bash scripts/build-android.sh --debug         # debug build
#   ANDROID_ABI=aarch64-linux-android bash scripts/build-android.sh
#
# Environment: ANDROID_SDK_ROOT / ANDROID_HOME and ANDROID_NDK_HOME are honoured
# when set; otherwise the script looks in the usual places.
# ==============================================================================
set -eo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
CRATE="$ROOT/crates/mathed_android"
TARGET="${ANDROID_ABI:-x86_64-linux-android}"
OUT="$ROOT/target/android"
PROFILE="release"
CARGO_PROFILE_FLAG="--release"

for arg in "$@"; do
    case "$arg" in
        --debug) PROFILE="debug"; CARGO_PROFILE_FLAG="" ;;
        -h|--help) sed -n '2,25p' "$0"; exit 0 ;;
        *) echo "❌ unknown argument: $arg"; exit 2 ;;
    esac
done

case "$TARGET" in
    x86_64-linux-android)   ANDROID_ABI_DIR="x86_64";   TARGET_TRIPLE="$TARGET";  NDK_TRIPLE="x86_64-linux-android" ;;
    aarch64-linux-android)  ANDROID_ABI_DIR="arm64-v8a"; TARGET_TRIPLE="$TARGET"; NDK_TRIPLE="aarch64-linux-android" ;;
    i686-linux-android)     ANDROID_ABI_DIR="x86";      TARGET_TRIPLE="$TARGET";  NDK_TRIPLE="i686-linux-android" ;;
    armv7-linux-androideabi) ANDROID_ABI_DIR="armeabi-v7a"; TARGET_TRIPLE="$TARGET"; NDK_TRIPLE="armv7a-linux-androideabi" ;;
    *) echo "❌ unsupported ANDROID_ABI: $TARGET"; exit 2 ;;
esac

# The library is dlopen'd by android.app.NativeActivity; the name must match
# `android.app.lib_name` in the manifest.
LIB_NAME="libmathed_android.so"

fail() { echo "❌ $*"; exit 1; }

# --- SDK discovery ------------------------------------------------------------
SDK="${ANDROID_SDK_ROOT:-${ANDROID_HOME:-}}"
if [[ -z "$SDK" || ! -d "$SDK" ]]; then
    for candidate in "$HOME/Android/Sdk" "$HOME/android-sdk" /usr/lib/android-sdk /opt/android-sdk; do
        if [[ -d "$candidate" ]]; then SDK="$candidate"; break; fi
    done
fi
[[ -n "$SDK" && -d "$SDK" ]] || fail "no Android SDK found; set ANDROID_SDK_ROOT"

NDK="${ANDROID_NDK_HOME:-}"
if [[ -z "$NDK" ]]; then
    NDK=$(ls -d "$SDK"/ndk/* "$SDK"/ndk-bundle 2>/dev/null | sort -V | tail -1)
fi
[[ -n "$NDK" && -d "$NDK" ]] || fail "no NDK found; set ANDROID_NDK_HOME"

BUILD_TOOLS=$(ls -d "$SDK"/build-tools/* 2>/dev/null | sort -V | tail -1)
[[ -n "$BUILD_TOOLS" ]] || fail "no build-tools in $SDK"
PLATFORM=$(ls -d "$SDK"/platforms/* 2>/dev/null | sort -V | tail -1)
[[ -n "$PLATFORM" ]] || fail "no platform (android.jar) in $SDK"

HOST_TAG="linux-x86_64"
case "$(uname -s)" in Darwin) HOST_TAG="darwin-x86_64" ;; esac
NDK_BIN="$NDK/toolchains/llvm/prebuilt/$HOST_TAG/bin"
[[ -d "$NDK_BIN" ]] || fail "no NDK toolchain at $NDK_BIN"

API="${ANDROID_API:-26}"
NDK_CC="$NDK_BIN/${NDK_TRIPLE}${API}-clang"
[[ -x "$NDK_CC" ]] || NDK_CC=$(ls "$NDK_BIN/${NDK_TRIPLE}"*-clang 2>/dev/null | sort -V | tail -1)
[[ -n "$NDK_CC" && -x "$NDK_CC" ]] || fail "no clang for $NDK_TRIPLE in $NDK_BIN"

# apksigner and keytool are Java. Take whatever this machine has, then fall
# back to a JDK out of the nix store (which is where this project's machines
# keep one) rather than failing with "java: not found".
find_java_home() {
    if [[ -n "${JAVA_HOME:-}" && -x "$JAVA_HOME/bin/java" ]]; then echo "$JAVA_HOME"; return; fi
    if command -v java >/dev/null 2>&1; then
        local javac
        javac=$(command -v java)
        echo "$(cd "$(dirname "$javac")/.." && pwd)"
        return
    fi
    local candidate
    for candidate in $(ls -d /nix/store/*openjdk-17*/ /nix/store/*openjdk-21*/ 2>/dev/null); do
        if [[ -x "${candidate}bin/java" ]]; then echo "${candidate%/}"; return; fi
    done
}
JAVA_HOME_DIR=$(find_java_home || true)
[[ -n "$JAVA_HOME_DIR" ]] || fail "no JDK found (apksigner/keytool need java)"
JAVA="$JAVA_HOME_DIR/bin/java"
KEYTOOL="$JAVA_HOME_DIR/bin/keytool"

echo "============================================================="
echo "🤖 building mathed for Android"
echo "============================================================="
echo "   target     : $TARGET ($ANDROID_ABI_DIR), API $API, $PROFILE"
echo "   SDK        : $SDK"
echo "   NDK        : $NDK"
echo "   build-tools: $(basename "$BUILD_TOOLS")"
echo "   platform   : $(basename "$PLATFORM")"
echo "   JDK        : $JAVA_HOME_DIR"
echo ""

# --- 1. the library ----------------------------------------------------------
echo "⏳ [1/5] cargo build --target $TARGET"
# `CC_<triple>`/`AR_<triple>` use the dashed triple spelling cc-rs looks for
# first; `env` is required because a dashed name is not a valid shell
# identifier. -D warnings is not applied here: the crate is built as part of
# the host gates too, and this is the *packaging* step.
mkdir -p "$OUT"
# Keep the Android build tree — and its 60 MB of intermediate objects — inside
# the target directory the repo already ignores, rather than next to the crate
# (this crate is its own workspace, so it would otherwise grow a
# `crates/mathed_android/target/` of its own).
(
    cd "$ROOT"
    env \
        "CC_${TARGET}=$NDK_CC" \
        "CXX_${TARGET}=${NDK_CC}++" \
        "AR_${TARGET}=$NDK_BIN/llvm-ar" \
        "CARGO_TARGET_$(echo "$TARGET" | tr 'a-z-' 'A-Z_')_LINKER=$NDK_CC" \
        "CARGO_TARGET_DIR=$OUT/cargo" \
        cargo build --lib --manifest-path "$CRATE/Cargo.toml" --target "$TARGET" $CARGO_PROFILE_FLAG
)
SO="$OUT/cargo/$TARGET/$PROFILE/$LIB_NAME"
[[ -f "$SO" ]] || fail "expected $SO after the build"

# --- 2. link the manifest ----------------------------------------------------
BASE_APK="$OUT/base.apk"
ALIGNED_APK="$OUT/aligned.apk"
SIGNED_APK="$OUT/${APK_NAME:-mathed-android.apk}"
rm -f "$BASE_APK" "$ALIGNED_APK" "$SIGNED_APK"

echo "⏳ [2/5] aapt2 link"
"$BUILD_TOOLS/aapt2" link \
    --manifest "$CRATE/AndroidManifest.xml" \
    -I "$PLATFORM/android.jar" \
    --min-sdk-version "$API" \
    --target-sdk-version 34 \
    --version-code "${ANDROID_VERSION_CODE:-1}" \
    --version-name "${ANDROID_VERSION_NAME:-0.1.0}" \
    -o "$BASE_APK" \
    || fail "aapt2 link failed"
[[ -f "$BASE_APK" ]] || fail "aapt2 produced no APK"

# --- 3. the native library ---------------------------------------------------
echo "⏳ [3/5] adding lib/$ANDROID_ABI_DIR/$LIB_NAME"
mkdir -p "$OUT/lib/$ANDROID_ABI_DIR"
cp "$SO" "$OUT/lib/$ANDROID_ABI_DIR/$LIB_NAME"
# No `zip` on every machine, but `aapt` ships with the build-tools we already
# require, so it is the one packer guaranteed to be here.
(
    cd "$OUT"
    "$BUILD_TOOLS/aapt" add "$BASE_APK" "lib/$ANDROID_ABI_DIR/$LIB_NAME"
)

# --- 4. align ----------------------------------------------------------------
echo "⏳ [4/5] zipalign"
"$BUILD_TOOLS/zipalign" -f 4 "$BASE_APK" "$ALIGNED_APK" || fail "zipalign failed"

# --- 5. sign -----------------------------------------------------------------
echo "⏳ [5/5] apksigner"
KEYSTORE="${ANDROID_KEYSTORE:-$OUT/debug.keystore}"
if [[ ! -f "$KEYSTORE" ]]; then
    echo "   generating a debug keystore at $KEYSTORE"
    "$KEYTOOL" -genkeypair \
        -keystore "$KEYSTORE" \
        -alias androiddebugkey \
        -storepass android -keypass android \
        -keyalg RSA -keysize 2048 -validity 10000 \
        -dname "CN=Android Debug,O=Android,C=US" >/dev/null 2>&1 \
        || fail "keytool could not create $KEYSTORE"
fi

# `apksigner` is a shell wrapper that `exec java` from PATH, so JAVA_HOME alone
# is not enough to point it at a JDK.
PATH="$JAVA_HOME_DIR/bin:$PATH" JAVA_HOME="$JAVA_HOME_DIR" "$BUILD_TOOLS/apksigner" sign \
    --ks "$KEYSTORE" \
    --ks-pass pass:android \
    --key-pass pass:android \
    --ks-key-alias androiddebugkey \
    --out "$SIGNED_APK" \
    "$ALIGNED_APK" \
    || fail "apksigner failed"

echo ""
echo "============================================================="
echo "✅ $(basename "$SIGNED_APK")"
echo "============================================================="
echo "   path   : $SIGNED_APK"
echo "   size   : $(du -h "$SIGNED_APK" | cut -f1)"
echo "   sha256 : $(sha256sum "$SIGNED_APK" | cut -d' ' -f1)"
echo "   package: dev.velyst.mathed"
echo "   abi    : $ANDROID_ABI_DIR"
echo ""
echo "Install it (Waydroid):"
echo "   waydroid app install \"$SIGNED_APK\""
echo "   waydroid app launch dev.velyst.mathed"
echo "Or on a device with adb:"
echo "   adb install -r \"$SIGNED_APK\""
echo ""
