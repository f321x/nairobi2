#!/usr/bin/env bash
# Reclaim disk on a stock `ubuntu-latest` GitHub runner before an APK build.
#
# The release/debug APK jobs build everything *inside* the nairobi-builder
# container, so the host runner's preinstalled toolchains (its own Android SDK,
# .NET, GHC/Haskell, CodeQL, Swift, PowerShell) are dead weight. Meanwhile the
# build stacks up the ~4.4 GB builder image, the GHA docker layer cache, the
# Fedimint dependency tree (iroh/quinn/aws-lc-sys) in the cargo target dir, and
# Gradle artifacts — together they overflow the ~14 GB free on a fresh runner,
# and the build dies at the very last step ("cp ... : No space left on device").
#
# Deleting the unused host toolchains frees ~25 GB, which is plenty of headroom.
set -euo pipefail

echo "==> Disk usage before cleanup"
df -h /

# Each path is removed independently so a layout change on the runner image
# (a missing directory) can't abort the whole cleanup.
for path in \
    /usr/local/lib/android \
    /usr/share/dotnet \
    /opt/ghc \
    /usr/local/.ghcup \
    /opt/hostedtoolcache/CodeQL \
    /usr/local/share/powershell \
    /usr/share/swift \
    /usr/local/share/chromium \
    /usr/local/lib/node_modules; do
    sudo rm -rf "$path" || true
done

# Drop the runner's preinstalled Docker images; the builder image is rebuilt
# (layer-cached) by the job that follows.
sudo docker image prune -af || true

echo "==> Disk usage after cleanup"
df -h /
