# nairobi2

A **permissionless, fully Nostr-native ridesharing app** for Android, in **Rust + [Slint](https://slint.dev)**.

No company, no server, no accounts, no platform fee — just riders and drivers meeting over the
open [Nostr](https://nostr.com) network and paying **cash, peer-to-peer**, in person.

<p align="center">
  <img src="docs/screenshots/home.png" width="240" alt="Home — rider or driver">
  <img src="docs/screenshots/request.png" width="240" alt="Request a ride — OSM map, tap to set drop-off, escalating rate">
  <img src="docs/screenshots/driver.png" width="240" alt="Nearby rides — driver offer list">
</p>

<p align="center"><sub>Running under a virtual display (Slint software renderer). Left→right: Home, the map-first ride request (real OpenStreetMap tiles + GPS pin + escalating-rate steppers), and the driver's nearby-rides list.</sub></p>

## How it works

1. A **passenger** posts a ride request (pickup → dropoff) with a **starting** rate per km and a
   **maximum** rate they're willing to pay. The offered rate **climbs every 30 seconds for 5
   minutes** until a driver accepts or the maximum is reached — a reverse auction in the rider's
   favour.
2. **Drivers** nearby see a live list of requests, sorted by **distance to pickup**, **total
   earnings**, **rate**, or **trip distance**, and **take** one. If several drivers take the same
   ride at once, the **first one wins**, decided deterministically with no referee.
3. Once matched, each side sees the other as a **moving dot on the map**, the driver gets a
   one-tap **Navigate** handoff to their usual maps app, and they can exchange **end-to-end
   encrypted** messages (with one-tap pictogram replies). They meet at the pin and settle in cash.

Distances and the map come from **OpenStreetMap** (Nominatim + OSRM, with OSM tiles). The UI is
Uber-like but **icon-first and numeral-based**, so it's usable by people who can't read.

> **No backend, ever.** Every interaction is a signed Nostr event over public relays. The app
> scales by scoping subscriptions with geohashes, not by adding servers.

## Wallet & M-Pesa cash-out

The app carries an optional **self-custodial Bitcoin / Lightning wallet**. It can be funded over
**Lightning** (the app shows an invoice) or **on-chain** (a deposit address), spend by paying a
Lightning invoice or sending on-chain, and — the headline feature for Kenya — **cash out to
M-Pesa**: enter a phone number and an amount, and the app pays `<phone>@bitcoin.co.ke` over
Lightning, which converts the sats to **KES** and pushes them to that M-Pesa wallet.

The wallet sits behind one small, swappable trait (`nairobi_core::wallet::Wallet`), so the same UI
and the same internal "pay this Lightning invoice" API work over any backend:

- a deterministic **`MockWallet`** (tests + desktop simulator),
- a **[Fedimint](https://fedimint.org) e-cash wallet** (`nairobi-wallet-fedimint`), funded from a
  federation, and
- — later — a **Nostr Wallet Connect** (NIP-47) link to a remote wallet, a drop-in third backend.

The LUD-16 lightning-address / LNURL-pay resolution behind the M-Pesa payout is pure and unit-tested
(no network in tests). See [`CLAUDE.md`](CLAUDE.md) for how to enable the Fedimint backend.

## Sybil resistance — proof of burn

A permissionless network has no accounts, so identities are free (`Keys::generate`) and a single
attacker can spin up thousands of throwaway pubkeys to flood the geohash feeds with fake requests
or fake acceptances. nairobi2 answers this **without a backend or a gatekeeper**, using
**proof of burn**: a publicly verifiable Bitcoin commitment that a number of satoshis was
*irreversibly sacrificed to the miners*, attached to a Nostr event. Spam becomes linearly
expensive; honest users pay little and build durable, portable reputation.

The mechanism follows T. Voegtlin's *"The Price of Attention"* and the
[`spesmilo/notary`](https://github.com/spesmilo/notary) protocol. A burn is produced by a
**notary** (`notary.electrum.org`) that batches many requests into one Bitcoin transaction via a
Merkle-sum tree — the summed amount goes to a timelocked anyone-can-spend (i.e. miner-swept) output
and the Merkle root is committed in an `OP_RETURN`. The app pays the notary's Lightning invoice
**from its own wallet**, then **verifies the resulting proof client-side against Electrum servers** —
so the notary is trusted only for *liveness* (to actually burn the funds), never for *validity*.
Proofs are carried as standard kind-`30021` Nostr events; nothing about the Nostr protocol changes.

Reputation is **layered**, so the expensive part stays off the ride's critical path:

- **L1 — identity bond** *(one-time, confirmed)*: burn a configurable amount against a stable,
  immutable identity-bond event you author and sign as the burn's upvoter. N Sybil identities now
  cost `N × bond + fees`. The Nostr key *is* the BIP340 key that signs the burn leaf — no second
  keypair, and only the key-holder can claim the burn as their own.
- **L2 — proof of ride** *(per completed ride, confirmed)*: after a trip, each party may burn
  ~1 % of the fare against a ride-completion attestation that references the real request,
  acceptance, and counterparty — so reputation tracks genuine, *counterparty-diverse* activity and
  resists collusion farming.
- **L3 — reputation gate** *(the actual ride-time filter, no fresh burn)*: drivers and passengers
  filter the live feeds by a **user-set minimum reputation**, read from a cached score → **zero
  added latency**. Below threshold ⇒ hidden or flagged "unverified"; known pubkeys can be exempt.
- **L3′ — newcomer boost** *(optional, mempool)*: a user with no reputation can attach a fresh
  mempool burn to a single request for immediate visibility, optionally **anonymous**.

This stays **permissionless by construction**: burns only affect *visibility under each client's own
threshold* — anyone can still post, and **gating plus per-ride burn default off**. The trust-
minimising verifier (reconstruct the leaf/Merkle-sum root, fetch and re-derive the txid, bind the
`OP_RETURN` commitment to the burn output, SPV / multi-server cross-check, BIP340-verify the upvoter
signature) lives in the pure, host-tested `nairobi_core::burn` module, behind a `BurnService` trait
(`MockBurnService` for tests, real `NotaryBurnService` = notary HTTP + wallet + Electrum). The full
design is in
[`docs/superpowers/specs/2026-06-18-proof-of-burn-antisybil-design.md`](docs/superpowers/specs/2026-06-18-proof-of-burn-antisybil-design.md);
the protocol details are in [`docs/proof-of-burn-api.md`](docs/proof-of-burn-api.md).

## Status

- **Core logic — complete and tested.** The entire ride engine (identity, geocoding/routing,
  the escalating auction, deterministic first-taker-wins, the Nostr protocol, the relay transport,
  the full ride lifecycle, the modular wallet + LUD-16/M-Pesa payout logic, and the proof-of-burn
  anti-Sybil layer) lives in the `nairobi-core` crate and passes **136 unit tests**.
- **Proof of burn — core complete and host-tested.** The `burn` module (leaf/node hashing,
  Merkle-sum root, minimal Bitcoin tx/script parse + txid, the Part-B on-chain verifier, per-pubkey
  reputation) and the engine's bond → proof → reputation → gating lifecycle are exercised against a
  `MockBurnService`; the app wires the real `NotaryBurnService` (notary + Electrum + wallet) and a
  Settings action triggers the identity bond. Gating and per-ride burn are config-driven and
  **default off** (permissionless). The live notary/Electrum/Lightning path is not exercised here
  (offline, like the relay transport).
- **App + Android shell + build pipeline — building.** `./build.sh` compiles the Slint UI,
  cross-compiles for `aarch64-linux-android` (Skia + android-activity + nostr-sdk + Fedimint), and
  packages a valid, signed **`dist/nairobi-debug.apk`** (`io.nairobi.app`, minSdk 26). Following the
  proven [ntrack](https://github.com/f321x/ntrack) structure. The desktop build also runs (under a
  virtual display): the Home screen renders (above) and the app connects to live relays
  (`nos.lol`, `relay.damus.io`, `relay.primal.net`). *Full on-hardware behaviour and the live
  end-to-end ride flow remain to be exercised on a device.*

This is a **v1 / proof of concept**. Sybil resistance is now addressed by the proof-of-burn layer
above. Out of scope for now (by design): ratings and reputation UI, a pre-request "drivers nearby"
map, and key backup. See the design spec.

## Build

Core tests run on the host with Cargo; the APK builds in a rootless-friendly container (Docker or
Podman — no other host tooling needed).

```sh
# Core logic (fast, host)
cargo test -p nairobi-core
cargo clippy -p nairobi-core --all-targets -- -D warnings

# Android APK (containerised; builds the toolchain image on first run)
./build.sh                       # -> dist/nairobi-debug.apk
adb install -r dist/nairobi-debug.apk
```

See [`CLAUDE.md`](CLAUDE.md) for the architecture and developer notes, and
[`docs/superpowers/specs/`](docs/superpowers/specs/) for the full design.

## License

MIT
