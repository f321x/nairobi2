# CLAUDE.md

Guidance for Claude Code when working in this repository.

## What this is

**nairobi2** is a permissionless, fully **Nostr-native** ridesharing app for Android,
written in **Rust + [Slint](https://slint.dev)** on **`nostr-sdk`**. A passenger posts an
**escalating-price** ride request (a replaceable Nostr event whose rate climbs every 30 s for
5 min); drivers browse a geohash-scoped, sorted list and take one; the **first taker wins**
deterministically. Payment is **cash / peer-to-peer** in person. The UI is Uber-like but
designed to be usable by **non-literate** users (icon-first, numerals for money, pictogram chat).

**There is no backend and never will be** — all coordination is signed Nostr events over public
relays; scaling comes only from geohash-scoped subscriptions + replaceable events + NIP-40
expiry + client-side sorting. The full design is in
[`docs/superpowers/specs/2026-06-17-nairobi2-ridesharing-design.md`](docs/superpowers/specs/2026-06-17-nairobi2-ridesharing-design.md) — read it before changing the protocol or engine.

Reference project for the Slint+Android pipeline: `../ntrack` (a sibling app this one borrows its
structure from). The unrelated `../nairobi` is a *different* project — do not conflate.

## Commands

Day-to-day core development runs on the host with plain Cargo (offline — see the build-environment
note below). Only the APK build needs the container.

```sh
cargo test  --offline -p nairobi-core                                  # all core unit tests
cargo clippy --offline -p nairobi-core --all-targets -- -D warnings    # the lint gate
cargo test  --offline -p nairobi-core <name>                           # one test by substring

./build.sh            # debug APK in the container  -> dist/nairobi-debug.apk
./build.sh release    # signed release APK          -> dist/nairobi-release.apk (needs signing env)
./build.sh keystore   # generate the release keystore in the image -> release-signing/
./build.sh test       # run the Rust test suite inside the container
./build.sh image      # (re)build the builder image only
./build.sh shell      # interactive shell in the builder container
./build.sh clean      # remove (possibly root-owned) build artifacts
ABIS="arm64-v8a x86_64" ./build.sh   # extra ABIs (x86_64 for the emulator)
DOCKER=podman ./build.sh             # force the container tool (podman/docker auto-detected)
```

The builder image is Ubuntu 24.04 + JDK 17 + Android SDK 34 + NDK r27 + Gradle 8.11.1 + Rust +
cargo-ndk 4.1.2; `minSdk 26`. The build is rootless/SELinux-friendly: bind mounts use `:z` and
artifacts are chowned back to the invoking user.

## Architecture

Three parts (mirrors ntrack):

- **`core/` (`nairobi-core`)** — UI-free, OS-free, fully host-testable. This is the brain and is
  **complete and tested** (`cargo test -p nairobi-core` is the source of truth). Modules:
  - `keys` / `config` — auto-generated Nostr identity (redacting `SecretString`, derived
    name+colour) + atomic `ConfigStore` (corrupt-as-error, never wipes a key).
  - `geo` — hand-rolled `geohash`, `LatLng`/haversine, a minimal rustls/**ring** HTTPS GET
    (`geo::http`, no reqwest/hyper), `geo::routing` (Nominatim + OSRM + haversine fallback), and
    `geo::tiles` (pure Web Mercator/OSM tile math).
  - `auction` — the escalating-rate schedule + fare math (pure, exhaustively tested).
  - `matching` — deterministic first-taker-wins (earliest `created_at`, tie-break smallest id).
  - `protocol` — build/sign/parse/validate the events + subscription filters.
  - `pool` — the `Pool` transport trait, a test `MockPool`, and the real `nostr-sdk` `SdkPool`.
  - `engine` — the single channel-driven task owning all ride state.
- **`app/` (`nairobi-app`)** — Slint UI + `Controller` + `map` renderer + `Platform` glue + desktop
  `sim`. Builds as a `cdylib` (Android) and a desktop binary. **Cannot be host-compiled in the dev
  sandbox** (no fontconfig); it is validated by the `cargo-ndk` Docker build.
- **`android/`** — a thin Java `NativeActivity` shell (`io.nairobi.app`): `MainActivity`,
  `LocationBridge`, a foreground `LocationService` (live location while matched), and the
  external-navigation handoff.

### The channel-driven engine (the core design)

`core/src/engine.rs` is the only owner of ride state. It is decoupled from UI and OS and talks
**only over channels**: `EngineCmd` in (UI actions, a 1 s `Tick{now}`, GPS `Location`, relay
`PoolEvent`), immutable `UiEvent` snapshots out. State crosses the boundary only as snapshots; the
engine never holds a UI handle. It is generic over the `Pool` trait, so the whole relay layer is a
`MockPool` in tests — the entire ride lifecycle (post → escalate → match → trip → complete, plus
the driver browse/take/win/lose path and the deterministic race resolution) is host-tested with no
network. **Time is injected** via `Tick{now}` so auctions are deterministic in tests.

### Nostr event model

| Kind  | Class       | Meaning                                       |
|-------|-------------|-----------------------------------------------|
| 11311 | replaceable | Ride Request (one active per passenger)       |
| 1313  | regular     | Ride Acceptance (a driver's claim, stored)    |
| 21313 | ephemeral   | Location beacon (NIP-44 encrypted)            |
| 1059  | gift-wrap   | NIP-17 private DM (post-match chat)           |

Ride requests carry multiple `g` geohash tags (precision 4–7) + a NIP-40 `expiration` (~90 s,
refreshed every 30 s). Clients also enforce expiry themselves (never trust relays to delete).

## Conventions & gotchas

- **One TLS stack, ring only.** `nostr-sdk` 0.44 is hard-wired to the rustls **ring** provider
  (no aws-lc-rs), and the map/HTTP layer reuses the same `tokio-rustls`/ring stack — deliberately,
  so the Android cross-build gains no second crypto/C dependency. If you add a crate that pulls a
  rustls provider, force it back: `rustls = { default-features = false, features = ["ring"] }`.
- **Feature flags are mutually exclusive backends.** `--features desktop` (winit + femtovg) vs
  `--features android` (android-activity + Skia); the APK build uses
  `--no-default-features --features android`.
- **Secrets never get logged.** Secret keys live inside `keys::SecretString` (redacting `Debug`).
- **The dev sandbox is offline.** Build/test core with `--offline`; the crate versions are pinned
  to the pre-populated cargo cache (`nostr-sdk 0.44.1`, `slint 1.16.1`, …).

## Status

- `core/` — **complete, 78 tests passing, clippy clean**, offline.
- `app/` + `android/` + build pipeline — **build end-to-end.** `./build.sh` (podman/Docker)
  produces a valid, signed **`dist/nairobi-debug.apk`** (~18 MB, `io.nairobi.app`, arm64-v8a,
  minSdk 26): `libnairobi_app.so` (Rust+Slint+Skia+nostr-sdk) + `libc++_shared.so` + the Java
  shell in `classes.dex`. Verified to **compile and package**; **on-device runtime is not yet
  exercised** (no device/emulator here, and the desktop build needs GL libs absent from the image).
  See the spec for what's deferred (sybil resistance, ratings, pre-request driver map, boot-resume,
  key backup).

Build notes: the first `./build.sh` builds the ~4.4 GB toolchain image (needs network); reuse it
with `SKIP_IMAGE_BUILD=1`. A benign `llvm-strip` warning ("unable to strip libc++_shared.so /
libnairobi_app.so") is the known NDK strip quirk — harmless.
