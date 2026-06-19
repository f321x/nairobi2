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

./build.sh            # debug APK(s) in the container -> dist/nairobi-debug-<abi>.apk (one per ABI)
./build.sh release    # signed release APKs          -> dist/nairobi-release-<abi>.apk (needs signing env)
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
  - `wallet` — the modular `Wallet` trait (Bitcoin/Lightning), a `MockWallet`, and the LUD-16 /
    LNURL-pay + M-Pesa (`<phone>@bitcoin.co.ke`) cash-out logic. Fire-and-forget like `Pool`
    (`WalletEvent`s come back on a channel). The real Fedimint backend lives in the separate
    `nairobi-wallet-fedimint` crate; a future Nostr-Wallet-Connect backend is a third impl.
  - `burn` — **proof-of-burn anti-sybil** (notary `notary.electrum.org` + client-side Electrum
    verification, paid via `wallet`). Pure, host-tested core: `proof` (leaf/node hashing,
    Merkle-sum root, kind-30021 packing), `tx` (minimal Bitcoin tx/script parse, txid, P2WSH +
    `OP_RETURN`), `verify` (Part B binding), `reputation` (per-pubkey accrual). I/O behind the
    `BurnService` trait (`MockBurnService` + real `NotaryBurnService`), mirroring `Pool`/`Wallet`.
    See `docs/superpowers/specs/2026-06-18-proof-of-burn-antisybil-design.md` +
    `docs/proof-of-burn-api.md`.
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
| 13131 | regular     | Identity bond (immutable proof-of-burn target)|
| 1314  | regular     | Ride-completion attestation (per-ride burn)   |
| 30021 | addressable | Proof-of-burn upvoting event (carries a proof)|

Ride requests carry multiple `g` geohash tags (precision 4–7) + a NIP-40 `expiration` (~90 s,
refreshed every 30 s). Clients also enforce expiry themselves (never trust relays to delete).

### The modular wallet (Bitcoin / Lightning)

`core/src/wallet/` adds a self-custodial Bitcoin/Lightning wallet behind one swappable trait —
the same trait/Mock/real-impl discipline as `Pool`:

- `wallet::Wallet` — fire-and-forget trait (`refresh_balance`, `receive_lightning`,
  `receive_onchain`, `pay_invoice`, `pay_address`, `pay_onchain`); results come back as
  `WalletEvent`s on a channel, folded into the controller's `ViewState` (the engine is untouched).
  `pay_invoice` is the internal "pay a Lightning invoice" API for the rest of the app.
- `wallet::MockWallet` — deterministic, in-memory; used by tests and the desktop sim.
- `wallet::lnaddress` — pure, host-tested LUD-16 / LNURL-pay resolution + the
  `<phone>@bitcoin.co.ke` **M-Pesa** cash-out (`wallet::pay_mpesa`).
- The **real Fedimint backend** is the separate `wallet-fedimint` crate
  (`nairobi_wallet_fedimint::FedimintWallet`, `fedimint-client` 0.11). A future Nostr Wallet
  Connect client would be a third `Wallet` impl.

The UI is a Wallet screen (balance, receive over LN/on-chain, send, M-Pesa payout) reached from a
💰 button on Home; the federation invite is set in Settings (`config.federation_invite`).

**The Fedimint backend is shipped in the APK.** `scripts/build-apk.sh` builds the app with
`--features android,fedimint`, so the real `FedimintWallet` (not the `MockWallet`) backs the
wallet on device. The SDK deps stay optional behind the crate's `fedimint` feature (and the app's
`fedimint` feature), still **off by default**, so the host `cargo test`/`clippy` workspace gate
stays light and ring-only. Building the backend:
- Needs `RUSTFLAGS="--cfg tokio_unstable"` (the build script exports it). Applies to the whole
  app build, which is fine.
- Uses `fedimint-cursed-redb` (pure-Rust DB) — no RocksDB/C++ on Android.
- ⚠️ **Two TLS stacks once enabled.** `fedimint-connectors` 0.11 *hard*-depends on `iroh`/`quinn`
  (QUIC, → `aws-lc-rs`) and on `aws-lc-sys` (with `bindgen`) — neither is behind a feature, so a
  ring-only Fedimint build is not possible with this version. The APK therefore links **both**
  `ring` (nostr-sdk, fedimint-core) and `aws-lc-rs` (iroh/quinn). Two consequences, both handled:
  - **Build:** `aws-lc-sys` needs a CMake/clang/Go/Perl C toolchain (added to `docker/Dockerfile`);
    it cross-compiles to the Android ABIs via the NDK toolchain cargo-ndk sets up.
  - **Runtime:** rustls 0.23 panics on a default `ClientConfig::builder()` when >1 provider is
    linked, so `run_app` pins the process-wide default to `ring` (`app/src/lib.rs`, behind the
    `fedimint` feature). iroh/quinn select `aws-lc-rs` explicitly, so they are unaffected.

## Conventions & gotchas

- **Ring is the default TLS provider; keep new deps on it.** `nostr-sdk` 0.44 is hard-wired to the
  rustls **ring** provider, and the map/HTTP layer reuses the same `tokio-rustls`/ring stack. If you
  add a crate that pulls a rustls provider, force it back: `rustls = { default-features = false,
  features = ["ring"] }`. The **one** sanctioned exception is the Fedimint wallet, whose
  `fedimint-connectors` dep unavoidably links `aws-lc-rs` via iroh/quinn (see the Fedimint section
  above) — the process default is still pinned to `ring`.
- **Feature flags are mutually exclusive backends.** `--features desktop` (winit + femtovg) vs
  `--features android` (android-activity + Skia); the APK build uses
  `--no-default-features --features android`.
- **Secrets never get logged.** Secret keys live inside `keys::SecretString` (redacting `Debug`).
- **The dev sandbox is offline.** Build/test core with `--offline`; the crate versions are pinned
  to the pre-populated cargo cache (`nostr-sdk 0.44.1`, `slint 1.16.1`, …).

## Status

- `core/` — **complete, 136 tests passing, clippy clean**, offline (78 ride + 17 wallet/M-Pesa +
  41 proof-of-burn).
- `burn` (proof-of-burn) — **core complete + host-tested** (hashing/Merkle/tx-parse/verify/
  reputation, the `BurnService` seam, and the engine's bond → proof → reputation → gating
  lifecycle against mocks). The app's `Controller` wires the real `NotaryBurnService` (notary +
  Electrum + wallet); gating + per-ride burn are config-driven and **default off** (permissionless).
  The live notary/Electrum/Lightning path is **not exercised here** (offline; like `SdkPool`). A
  Settings UI action to trigger the identity bond is the remaining step.
- `wallet-fedimint/` — the real Fedimint backend; **compiles** with
  `RUSTFLAGS="--cfg tokio_unstable" cargo check -p nairobi-wallet-fedimint --features fedimint`,
  and is now **compiled into the APK** (`scripts/build-apk.sh` → `--features android,fedimint`).
- `app/` + `android/` + build pipeline — **build end-to-end.** `./build.sh` (podman/Docker)
  produces a valid, signed **`dist/nairobi-debug-arm64-v8a.apk`** (`io.nairobi.app`, minSdk 26):
  `libnairobi_app.so` (Rust+Slint+Skia+nostr-sdk+**Fedimint**) + `libc++_shared.so` + the Java
  shell in `classes.dex`. The build emits one APK per ABI (Gradle ABI split; `ABIS=...` selects
  them, default `arm64-v8a`) so x86_64 (emulator) ships separately from arm64-v8a (phones) rather
  than as one fat APK. The Fedimint dependency tree (iroh/quinn/aws-lc) grows the binary
  noticeably. **On-device runtime is not yet exercised** (no device/emulator here, and the desktop
  build needs GL libs absent from the image).
  See the spec for what's deferred (ratings, pre-request driver map, boot-resume, key backup).
  **Sybil resistance is now addressed** by the `burn` proof-of-burn layer (above).

Build notes: the first `./build.sh` builds the ~4.4 GB toolchain image (needs network); reuse it
with `SKIP_IMAGE_BUILD=1`. A benign `llvm-strip` warning ("unable to strip libc++_shared.so /
libnairobi_app.so") is the known NDK strip quirk — harmless.
