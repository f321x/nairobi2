# nairobi2 — Proof-of-Burn Anti-Sybil (Design Research)

**Date:** 2026-06-18
**Status:** Approved; **core implemented** — the `nairobi-core::burn` module (proof/tx/verify/
reputation + the `BurnService` seam), the engine's bond→proof→reputation→gating lifecycle, and the
app's `NotaryBurnService` wiring, all host-tested behind config that defaults gating + per-ride burn
**off** (permissionless). Remaining: a Settings UI action to trigger the identity bond, ledger
persistence across restarts, and exercising the live notary/Electrum/Lightning path on a device.
Maintainer decisions are collected in §15.
**Depends on:** [`2026-06-17-nairobi2-ridesharing-design.md`](2026-06-17-nairobi2-ridesharing-design.md) (the v1 design; sybil resistance is its #1 deferred item, §12).
**External refs:** *The Price of Attention: Attaching Bitcoin Fees to Nostr Events*, T. Voegtlin, 2025; reference implementation [`spesmilo/notary`](https://github.com/spesmilo/notary); public notary `https://notary.electrum.org`; protocol notes in `docs/proof-of-burn-api.md` (the uploaded API documentation).

---

## 1. Goal

Make publishing the Nostr events that nairobi2 relies on **cost real money**, so that flooding the geohash-scoped request/acceptance feeds (Sybil spam) becomes linearly expensive, while honest users pay little and build durable, portable reputation. We do this with **proof-of-burn**: a publicly verifiable Bitcoin commitment that a number of satoshis was irreversibly sacrificed to the miners, attached to a Nostr event. Burns are produced by the **notary** (`notary.electrum.org`), paid via the app's forthcoming **Lightning wallet**, and **verified client-side against Electrum servers** — no backend, no trusted third party for *validity* (only for liveness/actually-burning). This keeps the project's "no backend, ever" invariant intact: proof-of-burn is a **client-side filter**, never a gatekeeper.

This document does **not** write code. It picks an architecture, justifies it against the constraints, and maps it onto the existing crates so implementation can start from a settled plan.

---

## 2. Threat model — what Sybil attacks actually look like here

| # | Vector | Mechanism today | Impact |
|---|--------|-----------------|--------|
| T1 | **Request flooding** | One pubkey = one *replaceable* request (kind 11311), but N throwaway pubkeys = N requests in a geohash. Identity is free (`Keys::generate`). | Drivers' offer lists drown in fake requests; price-discovery skewed; wasted attention. |
| T2 | **Fake acceptances / no-shows** | Any pubkey can publish an Acceptance (kind 1313) and win first-taker. | Passengers matched to ghosts; denial-of-service on real drivers. |
| T3 | **Reputation farming** | If/when reputation exists, colluding identities fake completed rides to mint standing. | Defeats any naive reputation system. |
| T4 | **Beacon / chat spam** | Beacons (21313) and DMs (1059) are post-match and encrypted to the counterpart. | Low — only a matched counterparty can target you. Out of scope. |

Proof-of-burn directly addresses **T1, T2, T3**. T4 needs no burn (already gated by the match). The whole design therefore targets the two *public, pre-match* event classes — **ride requests and acceptances** — plus the reputation that gates them.

---

## 3. Constraints that shape the design

These are non-negotiable and rule out the obvious "burn every event" approach.

1. **Latency.** A burn requires: notary `add_request` → pay a BOLT11 invoice → notary broadcasts/RBFs a Bitcoin tx → proof available. *Confirmed* proofs take ≥1 block (~10 min). *Mempool* proofs (`block_height == 0`) are available in seconds but are weaker (you trust the notary not to RBF-replace the burn with a self-pay). **A ride request re-publishes every 30 s and beacons fire every 5 s** — neither can wait on a burn. Burns must live **off the ride's critical path**.
2. **Fee schedule punishes tiny burns.** Notary fee is `x` for `x≤8` sat (100 % overhead), `x/2` for `≤32`, `x/4` for `≤256`, `x/8` above. Plus Lightning routing and the P2WSH dust floor. **Many micro-burns are economically absurd**; few larger burns are efficient.
3. **Engine purity.** `engine.rs` is the single state owner, channel-driven, with **time injected** via `Tick{now}`. It must stay host-testable with no network. Anything doing HTTP/LN/Electrum I/O must sit *behind a trait* (like `Pool`) and feed results back as `EngineCmd`s — never block the engine.
4. **Permissionless invariant.** No central party may decide who can ride. Proof-of-burn must remain *opt-in economics that affect visibility*, with each client choosing its own thresholds. There is no allow-list and no gate.
5. **The wallet is the only spend rail.** Burns are paid by the in-progress Lightning wallet (BOLT11 pay API). No burn = no wallet dependency met; the design must degrade gracefully when the wallet/notary is unavailable.

---

## 4. Design options considered

| Option | What it is | Verdict |
|--------|-----------|---------|
| **A. Per-message burn** | Notarize *every* event (each 30 s request re-publish, each beacon, each chat). | **Reject.** Violates C1 (latency) and C2 (cost) catastrophically. A single ride would need ~10 request burns + dozens of beacon burns. Nonsensical. |
| **B. One identity bond** | Burn once against a stable identity event; that *is* your baseline reputation; top up occasionally. | **Keep — Layer 1.** One-time, confirmed proof fine (not latency-sensitive). Makes each Sybil identity cost real sats. This is the user's *"drivers burn an initial amount for initial reputation."* |
| **C. Per-ride burn (~1 %)** | After a completed ride, each party burns ~1 % of the fare against a ride-completion attestation. | **Keep — Layer 2.** Off critical path (post-ride), confirmed proofs fine. Reputation accrues with real usage. This is the user's *"spend ~1 % of the drive value… both build reputation over time."* |
| **D. Per-request mempool burn** | Attach a fresh *mempool* burn to each live ride request for immediate gating. | **Only as opt-in (Layer 3′).** Mempool proofs are weak (C1) and one-burn-per-request is poor UX. Useful **only** as a *newcomer/priority boost* before reputation exists; never mandatory. |

The losing idea in all of this is "burn the message." The winning idea is **burn the identity and the rides, then gate the messages on the accumulated reputation** — the cost is amortised off the critical path, and ride-time filtering reads a cached score with zero added latency.

---

## 5. Recommended architecture — layered reputation, not per-message

Four layers. The **MVP is L1 + L3** (bond + gate); L2 and L3′ are enhancements.

### L1 — Identity bond *(one-time, confirmed)*
- On opting in, a user burns a configurable amount (e.g. a few hundred sats) against a **stable, immutable "identity bond" event** they author and self-sign as the burn's `upvoter` (see §6 for binding).
- Result: a baseline `reputation(pubkey)` ≥ the burned amount, verifiable by anyone. N Sybil identities now cost `N × bond + fees`.
- Confirmed proof only; produced lazily in the background — the user can ride immediately, reputation lands minutes later.

### L2 — Proof-of-ride *(per completed ride, confirmed)*
- On `CompleteTrip`, each party may burn ~1 % of the fare against a **ride-completion attestation** that references the real request id, acceptance id, and counterparty pubkey.
- Accrues reputation proportional to genuine activity; honest users barely notice the cost; reputation stays **fresh** (supports decay, §6).
- The attestation's references let consumers weight reputation by **counterparty diversity** (§6) — the real anti-collusion lever against T3.

### L3 — Reputation gate *(no fresh burn — the actual ride-time filter)*
- Drivers/passengers filter the live request/acceptance feeds by a **user-set minimum reputation**. The expensive part already happened (L1/L2); ride-time is a cache lookup → **zero added latency**, satisfying C1/C3.
- Below threshold ⇒ hidden or visually flagged ("unverified"). Followed/known pubkeys may be exempted (whitepaper §2 pattern).

### L3′ — Newcomer / priority boost *(optional, mempool, opt-in)*
- A user with no reputation (or wanting priority) can attach a **fresh mempool burn** to a single live request to be visible immediately. Capped and UI-flagged as provisional (whitepaper §7). Can be **anonymous** (no `upvoter` key) to preserve privacy at the cost of not building *personal* reputation.

**How a counterparty learns your reputation:** a request/acceptance carries a compact pointer (the author's bond event id and/or the relevant kind-30021 coordinates). On first contact with a new pubkey, the client fetches those kind-30021 upvoting events, verifies one or two against Electrum, and **caches the score keyed by pubkey** (persisted). Subsequent events from that pubkey are filtered from cache. First-contact verification is background work, off the `TakeRide` path.

---

## 6. Reputation accounting

- **Binding a burn to a pubkey (two requirements, both needed for *personal* reputation):**
  1. the burned Nostr event is **authored by** that pubkey, **and**
  2. the proof's `upvoter_pubkey` equals that pubkey and its **BIP340 signature over `leaf_hash` verifies**.
  Requirement 2 is the strong one: anyone can burn for someone else's `event_id`, but only the key-holder can produce the upvoter signature. **The Nostr identity key *is* a secp256k1 x-only/BIP340 key — the same key signs the leaf.** No second keypair.
- **Score:** `reputation(pubkey) = Σ verified, confirmed, deduped leaf_value` over that pubkey's bond + completion events.
- **Dedup by leaf hash** (the `d` tag of the kind-30021 event) — replays of a published upvote share a leaf hash and must collapse to one (whitepaper §6, API §6.4).
- **Confirmed-only for durable reputation** (`block_height > 0`, SPV-validated). Mempool proofs grant only provisional L3′ visibility.
- **Counterparty diversity (anti-collusion, T3):** weight a driver's reputation by the number of *distinct, bonded* counterparties in their completion attestations, not raw sat-sum. A farmer must then run many bonded identities **and** stage many distinct rides — cost compounds and the pattern (same pair, implausible timing/geography) becomes detectable.
- **Decay (optional):** window or exponentially decay old burns so standing reflects *ongoing* participation and a one-time bond doesn't confer permanent rank. Naturally rewards L2.

> **Honest economics.** Pure sat-sum reputation costs a Sybil the same whether bought via L1 or farmed via L2 — both convert "free identity" into "linear monetary cost," which is the win. L2's *extra* value is UX (pay-as-you-go vs upfront), recency, and — with diversity weighting — genuine collusion resistance. Present both; don't oversell L2 as cheaper-to-defend than it is.

---

## 7. Nostr event-model additions

Extends the v1 model (§4 of the v1 spec); no Nostr protocol change.

| Event | Kind | Storage | Purpose |
|-------|------|---------|---------|
| **Identity bond** | regular, immutable (e.g. `13131`) | stored | Stable target the L1 burn references; one per top-up. `id` is permanent so proofs keep resolving. A replaceable "profile" may *point* at the latest, but the **burn target must be immutable** (a replaceable event's id changes every version, orphaning the proof). |
| **Ride-completion attestation** | regular, immutable (e.g. `1314`) | stored | L2 burn target. Tags: `["e", request_id]`, `["e", acceptance_id]`, `["p", counterparty_pubkey]`, fare. Naturally permanent. |
| **Upvoting event (proof carrier)** | `30021` (addressable) | replaceable | The proof itself, per the notary spec. Tags `e`(burned event), `d`(leaf hash), `n`(packed proof), `u`(upvoter pk+sig), `p`, `chain`. We **consume** these to compute reputation, and **publish/replicate** our own. |

**Request/acceptance additions:** an optional `["pob", <bond_event_id>]` (or kind-30021 coordinate) pointer so a counterparty can locate reputation, plus — for L3′ — the packed mempool proof inline. Absence of a pointer ⇒ "unverified," handled by the consumer's threshold policy.

---

## 8. Verification path (client-side, against Electrum) — the trust-minimising half

Implements Part B of the API doc. Per-proof:

1. **Reconstruct `leaf_hash`** = `SHA256("Leaf:" ‖ event_id(32) ‖ value_msat(8 BE) ‖ nonce(32) ‖ upvoter_pk-or-zeros(32))`. ⚠️ **Order is `nonce` then `pubkey`** — follow the *code/API doc*; the whitepaper §3 prints `pubkey ‖ nonce`, which is the academic ordering and **wrong for the running notary**. One byte off breaks every hash.
2. **Reconstruct root** from `leaf_value` + `merkle_hashes` + `merkle_index` via the Merkle-sum recurrence (`node_hash` with `"Node:"` prefix, msat values, 8-byte BE); assert `root_msat % 1000 == 0`.
3. **Fetch tx** `blockchain.transaction.get(txid)`; recompute txid (double-SHA256 of the witness-stripped serialization) and require it matches — guards a lying server.
4. **Parse tx:** find the single `OP_RETURN` with 36-byte payload `0x0021 ‖ root(32) ‖ csv_delay(2 BE)`; rebuild the P2WSH burn `scriptPubKey` from `csv_delay` (CScriptNum-encode the push — `144` → `0x90 0x00`); locate the matching output.
5. **Bind:** `op_return_root == computed_root` **and** `burn_output.value (sat) == root_value (sat)`. This is the crux — the notary can't inflate one event's share without inflating the on-chain burn equally.
6. **Inclusion (pick trust level, §8.1).** Confirmed: `get_merkle` + `block.header`, check the branch against the header's merkle root and the header against your chain.
7. **Upvoter sig:** if `upvoter_pubkey` present, BIP340-verify over `leaf_hash`.

### 8.1 Trust levels (phone-pragmatic)
- **Full SPV** — validate headers (from a baked-in **checkpoint** updated per release, not genesis IBD) and check `get_merkle` against them. Trustless up to SPV. *Target for durable reputation, hardening phase.*
- **Multi-server cross-check** — query *K* independent Electrum servers; accept on agreement (tx, merkle root, recent header). Cheap, defeats a single lying/withholding server. **Recommended v1 default**, since integrity ultimately rests on PoW/merkle, not on any one server.
- **Mempool** — server-trusted, provisional, capped (L3′ only).

### 8.2 Transport gotchas
- **Electrum is JSON-RPC over a raw TLS socket** (newline-delimited), *not* HTTP — ports 50002 (SSL) / 50001 (TCP). Reuse the existing **tokio-rustls/ring** stack for the socket; the framing is trivial (`{"id","method","params"}\n`).
- **Many Electrum servers self-sign** — `webpki-roots` validation will reject them. Integrity comes from SPV/merkle, not TLS, so use **TOFU / pinned certs** (or accept-any) for Electrum, and lean on multi-server cross-check. (A hosted REST explorer like `mempool.emzy.de`/`blockstream.info` over the existing HTTPS-GET path is a viable *fallback* for clients that won't open raw sockets, but the user's requirement is Electrum, so Electrum is primary.)

---

## 9. Notary + wallet integration (Part A) and engine wiring

**Producing a proof:** `nonce ← random32` → (optional) sign `leaf_hash` with the Nostr key → `POST add_request {event_id, value_sats, nonce, [upvoter_pubkey, upvoter_signature]}` → `{invoice, rhash}` → **wallet pays `invoice`** → poll `get_proof {rhash}` (or WebSocket) until a proof returns → **verify locally (§8)** → publish/cache the kind-30021 event. `add_request` is an HTTPS **POST** — `geo::http` is GET-only today and must gain a small POST+JSON path (same TLS stack).

**The wallet seam (the one external dependency):**
```text
trait Wallet { async fn pay_bolt11(&self, invoice: &str) -> Result<Preimage>; }
```
The forthcoming wallet implements this; tests use a mock that returns a canned preimage. The notary-client task calls it; the **engine never touches Lightning directly**.

**Keeping the engine pure (mirror the `Pool` pattern exactly):**
```text
trait BurnService {                  // real impl = notary HTTP + Wallet + Electrum verify
    fn notarize(&self, req: NotarizeReq);   // event_id, amount, optional upvoter sig
}
enum BurnEvent { ProofReady{event_id, leaf_value, confirmed, upvoting_event}, Failed{event_id, reason} }
```
- `notarize(...)` is fire-and-forget (like `Pool::publish`); a background task does HTTP/LN/Electrum and feeds results back as a new `EngineCmd::Burn(BurnEvent)` (like `PoolEvent`).
- A **`MockBurnService`** keeps the whole bond → proof → reputation → gating lifecycle host-testable with **no network and no Lightning**, exactly as `MockPool` does for relays. New engine inputs: `EngineCmd::PublishBond{amount}`, `EngineCmd::Burn(BurnEvent)`; reputation cache + thresholds in state; filter incoming requests/acceptances by reputation in the existing `on_request_event` / `on_acceptance` paths; surface reputation in `Passenger/DriverSnapshot` for the UI.

---

## 10. Where it lives in the code

```
core/src/
├─ burn/
│  ├─ proof.rs       leaf/node hash, compute_root, kind-30021 (de)serialize     [pure, host-tested]
│  ├─ tx.rs          minimal BTC tx parse, txid(SHA256d), CScriptNum, P2WSH, OP_RETURN  [pure]
│  ├─ electrum.rs    JSON-RPC over tokio-rustls; get / get_merkle / block.header; multi-server
│  ├─ verify.rs      Part B algorithm → VerifiedBurn{leaf_value, confirmations}   [pure given tx+merkle]
│  ├─ notary.rs      Part A client (add_request POST, get_proof, leaf signing)
│  ├─ service.rs     BurnService trait + MockBurnService + real (notary+Wallet+verify)
│  └─ reputation.rs  per-pubkey accrual, dedup-by-leaf, diversity/decay, persistence  [pure]
├─ geo/http.rs       + small POST/JSON path (same ring TLS stack)
├─ protocol.rs       + bond (13131) & completion (1314) build/parse; kind-30021 parse; `pob` tag
├─ engine.rs         + PublishBond / Burn(BurnEvent) cmds; reputation cache; feed-filtering; snapshots
└─ config.rs         + persisted reputation cache, thresholds, bond state
```
The split keeps `proof`/`tx`/`verify`/`reputation` **pure and exhaustively host-testable** (the project's discipline), with I/O confined to `electrum.rs`/`notary.rs`/`service.rs` behind the trait.

---

## 11. Dependencies — near-zero new crypto

Everything cryptographic is **already in the locked tree** (transitively via `nostr-sdk` and `tokio-rustls`), so no new TLS/crypto stack is introduced — honouring the "one TLS stack, ring only" rule:

| Need | Source already present |
|------|------------------------|
| SHA-256 / SHA-256d (leaf, node, txid) | `bitcoin_hashes 0.14` (re-exported as `nostr::hashes`) — or `ring::digest` |
| BIP340 Schnorr sign/verify (raw 32-byte msg) | `secp256k1 0.29` (re-exported as `nostr::secp256k1`) |
| TLS sockets (Electrum + notary) | `tokio-rustls`/`ring` (already the relay/HTTP stack) |

New **code** (not deps): HTTP POST, Electrum JSON-RPC framing, a minimal Bitcoin tx/script parser, Merkle-sum math, reputation accounting — all small and hand-rolled, in the spirit of the existing hand-rolled geohash/HTTP. (We add `bitcoin_hashes`/`secp256k1` as *direct* deps pinned to the already-resolved versions; no new crates are pulled.)

---

## 12. Permissionlessness & privacy

- **No gatekeeper.** Burns affect *visibility under a consumer's own threshold*, nothing more. Anyone can still post; clients choose what to show. This is exactly the whitepaper model and preserves "no backend, ever."
- **Notary trust is liveness-only.** The notary is trusted to *actually burn* and stay up — never for validity (we verify on-chain). A cheating notary produces no valid proof, which the client detects (the burn simply doesn't land) and routes around. Mitigations: use the reputable `notary.electrum.org`; keep the notary a **swappable interface**; retain the LN preimage as proof-of-payment.
- **Privacy trade-off (call it out).** Today, uninstall ⇒ fresh free identity. A bond ties money (an LN payment + an on-chain batch the notary sees) to a persistent identity, reducing deniability — especially L1. Mitigations: keep bonds **optional**; allow **anonymous** L3′ boosts (no `upvoter` key) that fund an *event* without building *personal* reputation; document the trade-off in onboarding.

---

## 13. Failure modes

| Failure | Behaviour |
|---------|-----------|
| Wallet/notary down, payment fails | Burn is background + retried; user rides anyway at current reputation. Never blocks the ride loop. |
| Proof never confirms / RBF changes txid | Re-fetch (kind-30021 is replaceable); treat as provisional until `block_height > 0`; durable reputation waits for confirmation. |
| Electrum server lies/withholds | Multi-server cross-check; SPV/merkle is the real integrity check; self-signed certs handled by TOFU. |
| Notary pockets the burn | No valid proof ⇒ detectable; swap notary; reputation just doesn't accrue. |
| Cost to honest users | Bond optional + small; 1 % paid in sats via the wallet; numerals-for-money UX; "ride without a bond (less visible)" path keeps it permissionless. |

---

## 14. Phasing (build order — value-first, risk-first)

0. **Verification core** — `proof.rs` + `tx.rs` + `verify.rs` + `electrum.rs`, tested against captured real proofs from `notary.electrum.org` / `example.sh` vectors. Pure, host-testable, no wallet needed. *Highest value, lowest risk — do first; it's also the trust-minimising heart.*
1. **Notary client + wallet seam** (Part A) — end-to-end produce-a-proof once the wallet's `pay_bolt11` exists. `BurnService` + `MockBurnService`.
2. **L1 bond + reputation + L3 gate** — the actual anti-Sybil payoff (T1/T2). Engine filtering + persisted cache + UI.
3. **L2 per-ride burn + completion attestations + diversity weighting** (T3).
4. **L3′ mempool boost, decay, threshold tuning, full-SPV checkpoint headers** (hardening).

After Phase 0–2 the app has working Sybil resistance; 3–4 deepen it.

---

## 15. Decisions for the maintainer

These are product calls, not technical blockers. My recommendation in **bold**.

1. **Bond: mandatory or optional?** *Recommend* **optional-but-encouraged** (preserves permissionlessness; unbonded users are simply low-visibility). A market can effectively require it by raising thresholds.
2. **Who burns per ride, and how much?** *Recommend* **both parties, ~1 % each, opt-in**, each building their own reputation. Tunable; 0 % is allowed.
3. **Default reputation threshold & whether drivers vs passengers differ.** *Recommend* shipping a **low non-zero default** with a simple UI stepper, and a "show unverified (flagged)" toggle rather than a hard hide.
4. **Reputation = raw sat-sum, or diversity-weighted?** *Recommend* **start sat-sum (Phase 2), add diversity weighting in Phase 3** when completion attestations exist.
5. **Verification trust level for v1.** *Recommend* **multi-server cross-check**, with full-SPV checkpoint headers deferred to Phase 4.
6. **Privacy default.** *Recommend* bonds **off by default**, with a clear opt-in explaining the money↔identity linkage, and anonymous L3′ available.

---

## 16. Worked example (illustrative, not final numbers)

- Driver bonds **500 sat** once (invoice ≈ 500 + 125 fee = 625 sat). Baseline reputation 500.
- Each completed ride (fare ≈ 30 000 sat-equivalent): burns **~300 sat** (1 %), invoice ≈ 300 + 75. After 10 rides: +3 000 reputation across 10 distinct counterparties.
- A Sybil wanting 50 fake "drivers" each above a 500-sat threshold spends **≥ 25 000 sat + fees + 50 Lightning payments**, and to fake *ride history* must additionally stage diverse, plausibly-timed, mutually-bonded rides — cost compounds and patterns become detectable.
- Honest cost is a rounding error on a real fare; Sybil cost is linear and visible. That is the entire point.
