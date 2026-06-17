# nairobi2 — Permissionless Nostr Ridesharing (v1 Design)

**Date:** 2026-06-17
**Status:** Approved design — ready for implementation planning
**Reference project:** `../ntrack` (Rust + Slint Android app; build pipeline, channel-driven engine, OSM map, JNI platform glue)

---

## 1. Overview

nairobi2 is a **permissionless ridesharing app for Android**, written in **Rust + [Slint](https://slint.dev)**, built entirely on the **Nostr protocol**. Passengers post an escalating-price ride request; drivers browse a sorted list of nearby requests and take one; the first taker wins automatically. Payment is **cash / peer-to-peer** in person. The UI resembles Uber's, adapted for a permissionless, server-free architecture, and is designed to be usable by **non-literate users**.

### Goals
- A thin, working end-to-end loop: **post → browse/sort → take → meet (live map + chat + nav handoff) → complete**.
- **Fully Nostr-native.** No aggregator, no hosted backend — now or ever. All coordination is signed Nostr events over public relays.
- **Pure-Rust** wherever possible; reuse ntrack's proven Android + Slint + OSM-map patterns.
- **Usable by non-literate people:** icon-first, numerals for money, pictogram chat, large tap targets.

### Non-goals (v1)
- **Sybil resistance / reputation / ratings** — explicitly out of scope (to be addressed later).
- **Pre-request "cars nearby" map** — would require driver presence beacons; deferred.
- **Boot-resume** of an in-progress ride (ntrack's reboot machinery) — a ride is short-lived; dropped for v1.
- **Seed-phrase backup ceremony** — identity is auto-generated and stored locally; optional key export only.
- Any non-cash payment rail.

---

## 2. Locked decisions & constraints

| Decision | Value |
|---|---|
| Stack | Rust + Slint + `nostr-sdk`, Android-first |
| Architecture | "A" — mirror ntrack's `core/` + `app/` + `android/` split with a channel-driven Engine |
| Backend | **None, ever.** Scale only via geohash-scoped relay subscriptions + replaceable events + NIP-40 expiry + client-side sorting |
| Nostr library | `nostr-sdk` (high-level), **not** a hand-rolled relay pool (ntrack's fallback exists only as last resort) |
| TLS backend | Must pin to **rustls/ring** (not `aws_lc_rs`) for the Android aarch64 cross-build — early de-risking spike |
| Payment | Cash / p2p only |
| Distance/geocode | Nominatim (geocode) + OSRM / `routing.openstreetmap.de` (route distance); haversine×~1.3 fallback |
| Map | Ported from ntrack's pure-Rust OSM slippy map; **tap-to-pick** + **driver dot**; **no route polyline** |
| Post-match coordination | Live location dots + driver **Navigate** handoff + **NIP-17 encrypted chat** |
| First-taker-wins | Deterministic: lowest `created_at`, tie-break lowest event id |

---

## 3. Architecture & crate layout

Three parts, mirroring ntrack's separation of concerns:

```
nairobi2/
├─ core/   nairobi-core   (no UI, no OS — fully unit-tested on host)
│   ├─ engine.rs      single async task; owns all state; channels only
│   ├─ auction.rs     escalating-rate timer + fare math (pure)
│   ├─ matching.rs    first-taker-wins resolution (pure)
│   ├─ protocol.rs    build/sign/parse/validate Nostr events
│   ├─ pool.rs        `Pool` trait over nostr-sdk Client (+ MockPool for tests)
│   ├─ geo.rs         Nominatim + OSRM clients; geohash; distance/fare
│   ├─ keys.rs        identity (auto-generated, redacting SecretString)
│   └─ config.rs      persisted identity + settings
├─ app/    nairobi-app   (Slint UI + glue; cdylib for Android, binary for desktop)
│   ├─ ui/            .slint screens (Home, Request, Waiting, DriverList, Trip, Chat, Settings)
│   ├─ controller.rs  wires Slint ⇄ Engine; renders UiEvent snapshots on the UI thread
│   ├─ map.rs         ported OSM slippy map (tap-to-pick + dots)
│   ├─ platform.rs    Platform trait (location, nav-handoff, notify, …)
│   ├─ glue.rs        AndroidPlatform (JNI); compiles on all targets
│   └─ sim.rs         desktop simulator (synthetic GPS)
└─ android/  + build.sh + docker/   (ported & renamed from ntrack)
```

### Engine boundary (the core design)
UI and OS talk to the Engine **only over channels**. State crosses the boundary **only as immutable snapshots**; the Engine never holds a UI handle, and the UI never reads engine state directly.

- **In — `EngineCmd`:** `RequestRide`, `UpdateRate` (tick-driven), `CancelRequest`, `TakeRide`, `SendDm`, `CompleteTrip`, `Location`, `LocationUnavailable`, `Tick`, `Shutdown`, plus config mutations.
- **Out — `UiEvent` (snapshots + side-effect requests):** `RequestSnapshot`, `OfferList`, `TripSnapshot`, `Messages`, `NeedLocation(bool)`, `Notify`, `Toast`.

The Engine is generic over a `Pool` trait so the entire relay layer swaps to a `MockPool` in tests. The tricky logic (30 s escalation, replaceable re-publish, first-wins arbitration, dedup) lives here in pure, host-testable code — exactly where ntrack puts its share/alert state machine.

### Threading & UI bridge (ntrack pattern)
A `Controller` owns a private tokio runtime, spawns the Engine plus forwarder tasks (pool→engine, engine→ui, platform→engine), folds `UiEvent`s into an `Arc<Mutex<ViewState>>`, and renders on the UI thread via `Weak::upgrade_in_event_loop`. Slint callbacks run on the UI thread and route through `EngineCmd`s.

### Platform abstraction (ntrack pattern)
`Platform` trait covers what Slint doesn't: location updates, runtime permission, **open-in-external-nav** (the driver Navigate handoff), notifications, clipboard. Two impls: `glue.rs` (`AndroidPlatform`, JNI — compiles on every target, constructs only on Android) and `sim.rs` (desktop simulator).

---

## 4. Nostr event model

All events are signed; nothing exists server-side beyond standard relays.

| # | Event | Kind (app-defined; finalize in impl) | Storage | Purpose |
|---|-------|------|---------|---------|
| 1 | **Ride Request** | replaceable `11311` (one active per passenger) | replaceable | The "offer" drivers browse; re-published every 30 s with the new rate |
| 2 | **Ride Acceptance** | regular `1313` | stored | A driver claims a request; stored so the passenger resolves the winner even after a brief disconnect |
| 3 | **Location beacon** | ephemeral `21313`, NIP-44 to counterpart | not stored | The moving driver/passenger dot; high-frequency + privacy → no stored trail |
| 4 | **Chat** | NIP-17 DM (kind 14 / 1059 gift-wrap) | stored (gift-wrapped) | Post-match encrypted messaging |

### Ride Request structure
- **`content` (JSON):** `{pickup:{lat,lon}, dropoff:{lat,lon}, distance_km, currency, start_rate, max_rate, current_rate, fare_estimate, status}` where `status ∈ {open, matched, cancelled}`.
- **Tags:** multiple `["g", <geohash prefix>]` at lengths ~4–7 (so a driver can filter at their chosen radius); `["expiration", <now+90s>]` (NIP-40); after a match `["p", <winner_pubkey>]`.
- **Re-publish:** every 30 s with updated `current_rate` and refreshed expiration. Being replaceable, the higher-`created_at` event supersedes the prior one on every relay. If the passenger stops (cancel / app closed), it expires within ~90 s.

### Ride Acceptance structure
- **Tags:** `["e", <request_event_id>]`, `["p", <passenger_pubkey>]`. `content` may carry an optional driver hint (kept minimal — permissionless).
- Regular/stored kind so a momentarily-offline passenger still resolves deterministically on reconnect.

---

## 5. Auction / escalation (pure logic — `auction.rs`)

- **30 s steps**, **5 min window** → **10 steps**; `step = (max_rate − start_rate) / 10` (rounded to the currency unit).
- `rate(t) = min(max_rate, start_rate + floor((t − t0)/30s) · step)`; holds at `max_rate` after 5 min.
- Each tick: Engine recomputes the rate → **re-publishes the replaceable Ride Request** → emits a snapshot. Passenger sees the rising rate; each driver's list updates as the replaced event propagates.
- **Fare estimate** = `rate × distance_km`. The driver list's "total earnings" is the same value.
- If nothing takes it at `max`, the request keeps refreshing at `max`; an **overall stop after 15 min total** (configurable) shows a "try again?" prompt.

---

## 6. First-taker-wins (deterministic, server-free — `matching.rs`)

1. Drivers publish **Acceptance** events; several may race.
2. The passenger collects acceptances for its active request from its subscription.
3. **Winner = lowest `created_at`; tie-break = lexicographically smallest event id.** Deterministic — every client computes the same winner without a server.
4. The passenger re-publishes the request as `status:matched` + winner `p`-tag → the winner transitions to the Trip screen; losers see "taken" and the request drops off their lists.
5. Because Acceptance is *stored*, a passenger offline at the moment of acceptance resolves the same winner on reconnect.

**Clock-skew caveat:** `created_at` ordering is best-effort "first"; the id tie-break guarantees *determinism* even if not perfectly real-time-first. Acceptable for v1 since fairness/sybil are out of scope.

---

## 7. Driver list & scaling (no server)

Each driver subscribes `kinds:[11311], #g:[<their-area geohash prefixes>], since:<recent>` across the relay set; geohash precision bounds the volume to their locality. Client-side, the driver app:
- drops expired / non-`open` events (never trusting relays to delete);
- computes **distance-to-pickup** via OSRM from the driver's current GPS;
- sorts by **distance-to-pickup / total earnings / rate per km / trip distance** (icon-toggled).

Replaceable events + NIP-40 expiry keep relay-side state bounded. This scales to reasonable per-locality sizes with zero aggregation.

---

## 8. UI screens & principles

**Cross-cutting principle:** icon-first, color-coded, large tap targets, **numerals for money** (recognized even by non-readers), **pictogram chat**, haptic/sound cues. Text is always secondary to an icon, never the only signal.

1. **Onboarding** — auto-generate the Nostr key silently; request **location** permission (+ notifications). No seed-phrase ceremony.
2. **Home** — two big pictogram buttons: 🙋 *"I need a ride"* (passenger) vs 🚗 *"I'm driving"* (driver). Same identity does both.
3. **Passenger → Request** — live map; GPS pin = pickup (draggable); **tap to set dropoff**; optional search box. **Rate setup:** start-rate + max-rate **per km** via large **+/− steppers**; live distance + fare-at-start/fare-at-max preview. Big **REQUEST** button.
4. **Passenger → Waiting** — map + **rate ticking upward** (animated) + countdown + "searching"; **Cancel**. On match → Trip.
5. **Driver → Offer list** — request cards (pickup distance · trip distance · rate/km · **total earnings**) + icon **sort** toggle. Tap → detail → big **TAKE**. On take → publishes Acceptance; won → Trip, lost → "taken" toast.
6. **Trip** (shared) — map with both **dots** (driver moving) + pickup/dropoff pins, **no route line**. Driver gets the big **NAVIGATE** handoff (external app; pickup → then dropoff). Both get **Chat**, a status banner, **Cancel**, and **Done** at the end (cash settled in person; no ratings).
7. **Chat** — NIP-17 DMs with a row of **pictogram quick-replies** (📍 here · ⏱ 2 min · ❓ where · 👍 ok · 📞 call) plus optional free text.
8. **Settings** (tiny) — relay list, currency, optional key export.

### Identity / relays / currency
- **Identity:** auto-generated key persisted in app-private storage (mirrors ntrack config; secret wrapped in a redacting `SecretString`). No backup ceremony → uninstall = new identity (acceptable for v1; optional key export in Settings).
- **Relays:** hardcoded reliable default set (e.g. `relay.damus.io`, `nos.lol`, `relay.primal.net`, `relay.nostr.band`), editable in Settings; `nostr-sdk` manages the pool.
- **Currency:** integer amounts in a **configurable** currency (default **KES**), shown as large numerals + short code.

---

## 9. Error handling / resilience

- **Relays:** publish to all, succeed if ≥1 ACKs; `nostr-sdk` auto-reconnects subscriptions. All relays down → "offline, retrying" banner; the 30 s re-publish ticks keep retrying.
- **Nominatim/OSRM** (strict usage policies): descriptive `User-Agent`, **debounce** geocoding (on submit, not per keystroke), cache results. **Fallback:** OSRM failure → haversine × ~1.3 road-factor (flagged "approx"); `routing.openstreetmap.de` configurable as an alternate endpoint.
- **GPS denied/unavailable:** fall back to **manual pin placement** for pickup; driver list works on last-known location with degraded distance sort.
- **Match-race edges:** passenger offline during acceptances → resolves on reconnect (stored acceptances). **Winner goes dark** (no beacon within ~N s) → passenger gets a re-request path. Cross-relay disagreement → clients union events; `status:matched` + winner `p`-tag is authoritative.
- **Expiry:** clients also drop expired / non-`open` events locally.
- **Lifecycle/Android:** a **foreground service** keeps live location alive while matched + a high-priority match notification. **No boot-resume.** Unreadable config → regenerate identity with a warning.

---

## 10. Testing strategy (the ntrack discipline — pure logic off-device)

- **Host unit tests:** `auction.rs` (rate schedule, clamp, boundaries, rounding); `matching.rs` (first-wins ordering, id tie-break, offline-then-resolve, stale-version ignores); `geo.rs` (geohash, haversine, fare math, Nominatim/OSRM parsing against JSON fixtures, fallback); `protocol.rs` (build/sign/parse/validate every kind, tag round-trips, expiry filtering).
- **Engine tests against `MockPool`** (no network) — the bulk: request → escalate → accept → match → trip → complete; cancellation; multi-acceptance race; reconnect resolution.
- **Local mock-relay integration smoke** (ntrack ships a `mock_relay` example) to validate the `nostr-sdk` wire round-trip.
- **Desktop simulator** (`sim.rs`, synthetic GPS) to drive two instances (passenger + driver) through the full loop by hand.
- **`clippy -D warnings`** gate (CI parity with ntrack).
- **Early de-risking spike (first plan task):** confirm `nostr-sdk` builds for Android `aarch64` pinned to **rustls/ring**; if intractable, fall back to the hand-rolled pool.

---

## 11. Build pipeline (Docker + bash, rootless/SELinux)

- Port ntrack's `build.sh` + `docker/Dockerfile` + `android/` Gradle shell, renamed; subcommands `apk | release | keystore | test | image | shell | clean`.
- Builder image: Ubuntu 24.04 base, JDK 17, Android SDK 34, NDK r27, Gradle 8.11, Rust + Android targets + `cargo-ndk`; `minSdk 26`.
- **Rootless/SELinux from day one:** keep `:z` volume labels and podman support (`DOCKER=podman`); chown outputs back to the invoking UID (`CHOWN_UID/GID`) so nothing is root-owned. `cargo-ndk` builds release `.so` into `jniLibs/<abi>/` before Gradle packages them.

---

## 12. Risks & deferred items

**Risks**
- `nostr-sdk` TLS pulling `aws_lc_rs` and breaking the Android cross-build → de-risk first; hand-rolled-pool fallback exists.
- Nominatim/OSRM public-endpoint rate limits → debounce + cache + haversine fallback.
- Clock skew affecting "first" ordering → determinism guaranteed by id tie-break; fairness out of scope for v1.

**Deferred (post-v1)**
- Sybil resistance, reputation, ratings.
- Pre-request "drivers nearby" map (needs presence beacons).
- Boot-resume of an in-progress ride.
- Seed-phrase backup ceremony / multi-device identity.
- Voice notes in chat.

---

## 13. Glossary

- **Passenger** — posts a ride request and pays (the user's "taker" in the original brief).
- **Driver** — browses requests and takes one; provides the ride.
- **Ride Request** — the replaceable, escalating-price Nostr event passengers post.
- **Acceptance** — a driver's stored event claiming a request.
- **Beacon** — an ephemeral, NIP-44-encrypted location update (the map dot).
