#!/usr/bin/env bash
# nairobi2 build entrypoint — everything runs in a container, so the only host
# requirement is Docker or Podman. Designed to be rootless/SELinux-friendly:
# bind mounts use the ":z" label and artifacts are chowned back to the invoking
# user (so nothing is left root-owned, even under rootful Docker).
#
# Usage:
#   ./build.sh            build the debug APK(s)        -> dist/nairobi-debug-<abi>.apk
#   ./build.sh release    build the signed release APKs -> dist/nairobi-release-<abi>.apk
#   ./build.sh keystore   generate the release signing keystore in the builder
#                         image (no host JDK needed)  -> release-signing/
#   ./build.sh test       run the Rust test suite in the container
#   ./build.sh image      (re)build the builder image only
#   ./build.sh shell      interactive shell in the builder container
#   ./build.sh clean      remove build artifacts (uses the container, since
#                         artifacts may be root-owned under rootful Docker)
#
# Environment:
#   ABIS="arm64-v8a armeabi-v7a x86_64"   ABIs to build (default arm64-v8a;
#                                         use x86_64 for the emulator)
#   SKIP_TESTS=1                          skip tests during a build
#   SKIP_IMAGE_BUILD=1                    reuse an existing builder image
#   DOCKER=podman                         force a specific container tool
#
# Release signing (./build.sh release):
#   NAIROBI_KEYSTORE           host path to the keystore (.jks)
#   NAIROBI_KEYSTORE_PASSWORD  keystore password
#   NAIROBI_KEY_ALIAS          key alias
#   NAIROBI_KEY_PASSWORD       key password
#   NAIROBI_VERSION_NAME / NAIROBI_VERSION_CODE   optional version overrides

set -euo pipefail
cd "$(dirname "$0")"

if [ -z "${DOCKER:-}" ]; then
    if command -v podman >/dev/null 2>&1; then
        DOCKER=podman
    elif command -v docker >/dev/null 2>&1; then
        DOCKER=docker
    else
        echo "error: podman (or docker) is required" >&2
        exit 1
    fi
fi

IMAGE=nairobi-builder

# Rootless podman already maps the container's root to the invoking host user,
# so bind-mounted files come out owned by you. Chowning to your uid would then
# map THROUGH the userns to an unusable subuid and break ownership — so only
# chown back under a rootful daemon (Docker, or rootful podman), where the
# container writes as real root. Empty CHOWN_* => the in-container scripts skip
# the chown entirely.
CHOWN_UID="" CHOWN_GID=""
if [ "$("$DOCKER" info --format '{{.Host.Security.Rootless}}' 2>/dev/null)" != "true" ]; then
    CHOWN_UID="$(id -u)"
    CHOWN_GID="$(id -g)"
fi

build_image() {
    if [ "${SKIP_IMAGE_BUILD:-0}" = "1" ]; then
        echo "==> SKIP_IMAGE_BUILD=1: reusing existing '$IMAGE' image"
        return 0
    fi
    "$DOCKER" build -t "$IMAGE" docker/
}

run_in_container() {
    # Named volumes cache the cargo registry and gradle artifacts across builds;
    # the repo bind mount carries the rust target dir (.docker-target).
    local mounts=(
        -v "$PWD:/work:z"
        -v nairobi-cargo-registry:/opt/cargo/registry
        -v nairobi-gradle-home:/root/.gradle
    )
    local envs=(
        -e ABIS="${ABIS:-arm64-v8a}"
        -e SKIP_TESTS="${SKIP_TESTS:-0}"
        -e BUILD_TYPE="${BUILD_TYPE:-debug}"
        -e NAIROBI_VERSION_NAME="${NAIROBI_VERSION_NAME:-}"
        -e NAIROBI_VERSION_CODE="${NAIROBI_VERSION_CODE:-}"
        -e NAIROBI_KEYSTORE_PASSWORD="${NAIROBI_KEYSTORE_PASSWORD:-}"
        -e NAIROBI_KEY_ALIAS="${NAIROBI_KEY_ALIAS:-}"
        -e NAIROBI_KEY_PASSWORD="${NAIROBI_KEY_PASSWORD:-}"
        -e CHOWN_UID="${CHOWN_UID}"
        -e CHOWN_GID="${CHOWN_GID}"
    )
    # A release build needs the signing keystore inside the container; mount it
    # read-only at a fixed path (never under /work) and point Gradle at it.
    if [ -n "${NAIROBI_KEYSTORE:-}" ]; then
        local ks_abs
        ks_abs="$(readlink -f "$NAIROBI_KEYSTORE")"
        if [ ! -f "$ks_abs" ]; then
            echo "error: NAIROBI_KEYSTORE='$NAIROBI_KEYSTORE' is not a file" >&2
            exit 1
        fi
        mounts+=( -v "$ks_abs:/run/secrets/nairobi-release.jks:ro" )
        envs+=( -e NAIROBI_KEYSTORE=/run/secrets/nairobi-release.jks )
    fi
    "$DOCKER" run --rm "${mounts[@]}" "${envs[@]}" "$IMAGE" "$@"
}

case "${1:-apk}" in
    image)
        build_image
        ;;
    apk)
        build_image
        run_in_container scripts/build-apk.sh
        echo
        echo "Built one APK per ABI in dist/:"
        ls -1 dist/nairobi-debug-*.apk 2>/dev/null || true
        echo "Install with: adb install -r dist/nairobi-debug-arm64-v8a.apk"
        ;;
    release)
        build_image
        BUILD_TYPE=release run_in_container scripts/build-apk.sh
        echo
        echo "Signed release APK(s) (one per ABI):"
        ls -1 dist/nairobi-release-*.apk 2>/dev/null || true
        ;;
    keystore)
        build_image
        run_in_container scripts/gen-release-keystore.sh
        ;;
    test)
        build_image
        run_in_container cargo test --workspace
        ;;
    shell)
        build_image
        "$DOCKER" run --rm -it \
            -v "$PWD:/work:z" \
            -v nairobi-cargo-registry:/opt/cargo/registry \
            -v nairobi-gradle-home:/root/.gradle \
            "$IMAGE" bash
        ;;
    clean)
        build_image
        run_in_container bash -c \
            "rm -rf /work/.docker-target /work/dist /work/android/app/build /work/android/build /work/android/.gradle /work/android/app/src/main/jniLibs"
        ;;
    *)
        echo "unknown command: $1 (expected: apk | release | keystore | test | image | shell | clean)" >&2
        exit 1
        ;;
esac
