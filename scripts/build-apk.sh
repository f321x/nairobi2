#!/usr/bin/env bash
# Builds the nairobi2 APK (debug or release). Runs INSIDE the nairobi-builder
# container (see ../build.sh), with the repository mounted at /work.
#
# Environment:
#   ABIS        space-separated Android ABIs (default: "arm64-v8a";
#               also supported: armeabi-v7a x86_64)
#   SKIP_TESTS  set to 1 to skip the Rust test suite
#   BUILD_TYPE  "debug" (default) or "release"
#   CHOWN_UID/CHOWN_GID  hand artifact ownership back to this host user
#
# A release build (BUILD_TYPE=release) is signed with a key that
# android/app/build.gradle reads from the environment:
#   NAIROBI_KEYSTORE           path to the keystore (.jks) inside the container
#   NAIROBI_KEYSTORE_PASSWORD  keystore password
#   NAIROBI_KEY_ALIAS          key alias
#   NAIROBI_KEY_PASSWORD       key password
# and honours optional version overrides:
#   NAIROBI_VERSION_NAME / NAIROBI_VERSION_CODE

set -euo pipefail
cd /work

ABIS="${ABIS:-arm64-v8a}"
BUILD_TYPE="${BUILD_TYPE:-debug}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/work/.docker-target}"
JNILIBS=android/app/src/main/jniLibs

case "$BUILD_TYPE" in
    debug)   GRADLE_TASK=assembleDebug;   VARIANT=debug ;;
    release) GRADLE_TASK=assembleRelease; VARIANT=release ;;
    *) echo "error: BUILD_TYPE must be 'debug' or 'release' (got '$BUILD_TYPE')" >&2; exit 1 ;;
esac

# A release build without a keystore would silently produce an unsigned APK
# that Android refuses to install — fail loudly instead.
if [ "$BUILD_TYPE" = "release" ] && [ -z "${NAIROBI_KEYSTORE:-}" ]; then
    echo "error: a release build needs a signing key (NAIROBI_KEYSTORE et al.)." >&2
    exit 1
fi

if [ "${SKIP_TESTS:-0}" != "1" ]; then
    echo "==> Running Rust test suite"
    cargo test --workspace
fi

# Native libs are always release-profile regardless of APK variant: a
# debug-profile Slint+Skia build is huge and slow.
echo "==> Building Rust native libs (release profile) for ABIs: $ABIS"
rm -rf "$JNILIBS"

# Slint's android backend compiles a small Java helper at build time. cargo-ndk
# exports ANDROID_PLATFORM=<minSdk>; compile the helper against the installed
# platform jar instead (same model as Gradle's compileSdk).
ANDROID_JAR="$(ls -d "$ANDROID_HOME"/platforms/android-*/android.jar 2>/dev/null | sort -V | tail -1)"
if [ -n "$ANDROID_JAR" ]; then
    export ANDROID_JAR
    echo "    using ANDROID_JAR=$ANDROID_JAR"
fi

# The real Fedimint Bitcoin/Lightning wallet is compiled into the APK (the
# `fedimint` app feature). Its SDK requires `--cfg tokio_unstable`, and its
# dependency tree (fedimint-connectors -> iroh/quinn -> aws-lc-sys) brings a
# CMake/clang C build cross-compiled via the NDK toolchain cargo-ndk configures.
# Append to any caller-provided RUSTFLAGS rather than clobbering them.
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }--cfg tokio_unstable"

TARGET_FLAGS=()
for abi in $ABIS; do TARGET_FLAGS+=(-t "$abi"); done
cargo ndk "${TARGET_FLAGS[@]}" --platform 26 -o "$JNILIBS" \
    build -p nairobi-app --lib --release --no-default-features --features android,fedimint

# Bundle libc++_shared.so: Skia (Slint's Android renderer) links the shared
# C++ STL.
for abi in $ABIS; do
    case "$abi" in
        arm64-v8a)   triple=aarch64-linux-android ;;
        armeabi-v7a) triple=arm-linux-androideabi ;;
        x86_64)      triple=x86_64-linux-android ;;
        *) echo "unsupported ABI: $abi" >&2; exit 1 ;;
    esac
    src="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr/lib/$triple/libc++_shared.so"
    if [ -f "$src" ] && [ ! -f "$JNILIBS/$abi/libc++_shared.so" ]; then
        cp "$src" "$JNILIBS/$abi/"
    fi
done

echo "==> Building $VARIANT APK"
ABIS_CSV="${ABIS// /,}"
(cd android && gradle --no-daemon -PnairobiAbis="$ABIS_CSV" "$GRADLE_TASK")

mkdir -p dist
OUT="dist/nairobi-${VARIANT}.apk"
cp "android/app/build/outputs/apk/${VARIANT}/app-${VARIANT}.apk" "$OUT"

# Bind-mounted builds may run as root (rootful Docker); hand artifacts back.
if [ -n "${CHOWN_UID:-}" ]; then
    chown -R "${CHOWN_UID}:${CHOWN_GID:-$CHOWN_UID}" \
        dist "$JNILIBS" android/app/build android/.gradle 2>/dev/null || true
fi

echo "==> Done: $OUT"
ls -lh "$OUT"
