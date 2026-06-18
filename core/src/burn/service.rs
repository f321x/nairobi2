//! The proof-of-burn service seam — mirrors [`crate::pool::Pool`] and
//! [`crate::wallet::Wallet`]: the engine talks to it **fire-and-forget** and
//! results surface asynchronously as [`BurnOutcome`]s on a channel.
//!
//! - [`MockBurnService`] — deterministic, no network, no Lightning; lets the
//!   whole bond → proof → reputation → gating lifecycle be host-tested.
//! - [`NotaryBurnService`] — the real one: notary HTTP (Part A) + the
//!   [`Wallet`] to pay the invoice + Electrum verification (Part B). Not
//!   host-testable offline.
//!
//! Keeping the engine behind this trait preserves its purity: it never touches
//! HTTP, Lightning, or Bitcoin directly.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use nostr_sdk::prelude::*;
use nostr_sdk::secp256k1::Message;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

use super::electrum::{ElectrumClient, ElectrumServer};
use super::notary::NotaryClient;
use super::proof::{leaf_hash, BurnProof};
use super::verify::{verify_proof_against_tx, VerifiedBurn};
use super::watch::{BurnStore, PersistedBurn};
use super::{from_hex_array, proof::B32, to_hex};
use crate::wallet::{Amount, Wallet};

/// Why a burn is being made — routes the result in the engine and decides the
/// confirmation policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BurnPurpose {
    /// One-time identity bond (durable reputation).
    Bond,
    /// Per-ride attestation (~1 % of fare; durable, with a counterparty).
    Ride,
    /// Opt-in newcomer/priority boost (mempool acceptance is enough).
    Boost,
}

impl BurnPurpose {
    /// Human label for the transparency list / toasts (shared by the engine and
    /// the persisted-burn seed so they never drift).
    pub fn label(self) -> &'static str {
        match self {
            BurnPurpose::Bond => "Identity bond",
            BurnPurpose::Ride => "Ride",
            BurnPurpose::Boost => "Boost",
        }
    }
}

/// A request to notarize one of *our own* events.
#[derive(Clone, Debug)]
pub struct NotarizeReq {
    /// The Nostr event id to burn for (bond / completion / live request).
    pub event_id: B32,
    /// Satoshis to burn.
    pub value_sats: u64,
    pub purpose: BurnPurpose,
    /// Attach our x-only key + BIP340 leaf signature (personal reputation).
    /// `false` makes the upvote anonymous (funds the event, not the identity).
    pub sign: bool,
    /// For [`BurnPurpose::Ride`]: the counterparty pubkey (hex), for diversity.
    pub counterparty: Option<String>,
}

/// Effects the burn service pushes back to the engine.
#[derive(Clone, Debug, PartialEq)]
pub enum BurnOutcome {
    /// Our own burn produced (and we verified) a proof — publish the kind-30021
    /// upvoting event and credit ourselves. The proof is boxed (it dwarfs the
    /// other variants).
    Proven {
        purpose: BurnPurpose,
        proof: Box<BurnProof>,
        counterparty: Option<String>,
    },
    /// Our own burn failed (notary/payment/verification).
    Failed {
        purpose: BurnPurpose,
        event_id: B32,
        reason: String,
    },
    /// A third party's upvote verified on-chain — credit `pubkey`.
    Credited {
        pubkey: String,
        leaf_hash: String,
        value_msat: u64,
        confirmed: bool,
        counterparty: Option<String>,
    },
    /// A third party's upvote failed verification (ignored, logged).
    Rejected { reason: String },
}

/// The seam the engine depends on. Fire-and-forget; results arrive on the
/// channel the implementation was built with.
pub trait BurnService: Send + Sync + 'static {
    /// Notarize one of our own events (produce a proof we can publish).
    fn notarize(&self, req: NotarizeReq);
    /// Verify a third party's already-parsed proof on-chain and, if it carries a
    /// valid signed upvoter, credit them.
    fn verify_incoming(&self, proof: BurnProof);
    /// Resume watching a previously-persisted, still-unconfirmed burn (by its
    /// notary `rhash`) — no new payment; re-poll + re-verify until it confirms.
    /// Called on startup for each unconfirmed [`PersistedBurn`].
    fn resume(&self, burn: PersistedBurn);
}

/// A no-op service for an engine constructed without proof-of-burn (keeps the
/// default path identical to before this feature).
pub struct NoBurnService;

impl BurnService for NoBurnService {
    fn notarize(&self, _req: NotarizeReq) {}
    fn verify_incoming(&self, _proof: BurnProof) {}
    fn resume(&self, _burn: PersistedBurn) {}
}

// ---- MockBurnService (tests + desktop simulator) --------------------------

/// In-memory [`BurnService`]: synthesizes deterministic proofs and credits with
/// no network or Lightning. Records requests for assertions.
pub struct MockBurnService {
    tx: UnboundedSender<BurnOutcome>,
    inner: Mutex<MockInner>,
}

#[derive(Default)]
struct MockInner {
    requests: Vec<NotarizeReq>,
    verified: Vec<BurnProof>,
    resumed: Vec<PersistedBurn>,
    fail: bool,
}

impl MockBurnService {
    /// A mock that emits on `tx` and always succeeds.
    pub fn new(tx: UnboundedSender<BurnOutcome>) -> Self {
        Self {
            tx,
            inner: Mutex::new(MockInner::default()),
        }
    }

    /// Make every subsequent [`BurnService::notarize`] fail (to test the error
    /// path).
    pub fn set_failing(&self, fail: bool) {
        self.inner.lock().unwrap().fail = fail;
    }

    /// Every notarize request recorded so far.
    pub fn requests(&self) -> Vec<NotarizeReq> {
        self.inner.lock().unwrap().requests.clone()
    }

    /// Every burn a [`BurnService::resume`] was called with so far.
    pub fn resumed(&self) -> Vec<PersistedBurn> {
        self.inner.lock().unwrap().resumed.clone()
    }

    fn emit(&self, ev: BurnOutcome) {
        let _ = self.tx.send(ev);
    }
}

impl BurnService for MockBurnService {
    fn notarize(&self, req: NotarizeReq) {
        let fail = {
            let mut g = self.inner.lock().unwrap();
            g.requests.push(req.clone());
            g.fail
        };
        if fail {
            self.emit(BurnOutcome::Failed {
                purpose: req.purpose,
                event_id: req.event_id,
                reason: "mock failure".into(),
            });
            return;
        }
        // A deterministic, self-consistent (but not on-chain) proof. The engine
        // trusts the service's outcome, so the mock need not anchor it.
        let nonce = {
            let mut n = [0u8; 32];
            n[..8].copy_from_slice(&req.value_sats.to_be_bytes());
            n
        };
        let proof = BurnProof {
            version: super::PROOF_VERSION,
            chain: None,
            event_id: req.event_id,
            leaf_value_msat: req.value_sats * 1000,
            nonce,
            merkle_hashes: Vec::new(),
            merkle_index: 0,
            txid: to_hex(&[0u8; 32]),
            // Bonds/rides land confirmed; a Boost is provisional (mempool).
            block_height: if req.purpose == BurnPurpose::Boost { 0 } else { 1 },
            upvoter_pubkey: None,
            upvoter_signature: None,
        };
        self.emit(BurnOutcome::Proven {
            purpose: req.purpose,
            proof: Box::new(proof),
            counterparty: req.counterparty,
        });
    }

    fn verify_incoming(&self, proof: BurnProof) {
        self.inner.lock().unwrap().verified.push(proof.clone());
        match proof.upvoter_pubkey {
            Some(pk) => self.emit(BurnOutcome::Credited {
                pubkey: to_hex(&pk),
                leaf_hash: to_hex(&proof.leaf_hash()),
                value_msat: proof.leaf_value_msat,
                confirmed: proof.is_confirmed(),
                counterparty: None,
            }),
            None => self.emit(BurnOutcome::Rejected {
                reason: "anonymous upvote credits no identity".into(),
            }),
        }
    }

    fn resume(&self, burn: PersistedBurn) {
        self.inner.lock().unwrap().resumed.push(burn.clone());
        // Simulate the resumed burn confirming on re-poll (re-emits a Proven the
        // engine folds in, deduped by leaf hash).
        let proof = BurnProof {
            version: super::PROOF_VERSION,
            chain: None,
            event_id: from_hex_array::<32>(&burn.event_id).unwrap_or([0u8; 32]),
            leaf_value_msat: burn.value_sats * 1000,
            nonce: [0u8; 32],
            merkle_hashes: Vec::new(),
            merkle_index: 0,
            txid: burn.txid.clone(),
            block_height: 1,
            upvoter_pubkey: None,
            upvoter_signature: None,
        };
        self.emit(BurnOutcome::Proven {
            purpose: burn.purpose,
            proof: Box::new(proof),
            counterparty: burn.counterparty,
        });
    }
}

// ---- NotaryBurnService (real: notary + wallet + Electrum) ------------------

/// Poll the notary fast while waiting for the first proof so the balance moves
/// within seconds of paying…
const POLL_FAST: Duration = Duration::from_secs(5);
/// …then back off once a proof is surfaced and we're only waiting for the next
/// RBF / the confirmation (saves battery and notary/Electrum load).
const POLL_SLOW: Duration = Duration::from_secs(30);
/// Give up a *fresh* burn only if no verifiable proof appears at all within this
/// window (payment not seen / notary down). Once a proof is surfaced this no
/// longer applies — the burn is persisted and watched to confirmation instead.
const FETCH_DEADLINE: Duration = Duration::from_secs(5 * 60);
/// Watch one burn for its confirmation, per launch, for up to this long (a
/// notary batch RBFs well past a single block). A burn still unconfirmed when
/// this elapses stays persisted and resumes on the next launch.
const CONFIRM_DEADLINE: Duration = Duration::from_secs(60 * 60);

/// The watch's decision for a freshly fetched proof. Pure, so the network
/// loop's policy is unit-tested even though the loop itself does live I/O.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WatchDecision {
    /// (Re-)emit a `Proven` outcome. True on the first proof and whenever the
    /// txid changes (RBF) or it confirms, so the ledger always reflects the
    /// current proof. The engine de-noises — it only toasts / re-publishes on a
    /// genuine state change, not a bare txid refresh.
    emit: bool,
    /// The watch is finished (confirmed, or a boost that only needs mempool).
    done: bool,
}

/// Decide whether to (re-)emit and whether to stop, given the proof's
/// `confirmed` flag, whether its `txid` differs from the one last emitted (RBF),
/// and whether this is a boost.
///
/// For one of *our own* burns we surface the notary's proof directly — the
/// mempool one as provisional (pending), the confirmed one as durable. We do
/// **not** gate this on a client-side Electrum check: a just-broadcast /
/// RBF-churning notary batch tx is routinely unfetchable by exact txid, and
/// port-50002 Electrum servers are frequently unreachable on mobile, so gating
/// on them is exactly what kept a paid bond stuck at a 0 balance. Trust here is
/// liveness-only (the notary actually burns the funds), matching the design's
/// trust model — and any *counterparty* still independently verifies our
/// published kind-30021 proof on-chain (see [`NotaryBurnService::verify_incoming`],
/// the actual anti-Sybil boundary). Confirmed-only still gates *durable*
/// reputation in the engine's ledger; the mempool proof only shows as pending.
fn watch_decision(confirmed: bool, txid_changed: bool, is_boost: bool) -> WatchDecision {
    WatchDecision {
        emit: txid_changed || confirmed,
        done: confirmed || is_boost,
    }
}

/// The real service: requests a burn from the notary, pays the invoice with the
/// app [`Wallet`], polls for the proof, and verifies it against Electrum. The
/// resulting burns are persisted ([`BurnStore`]) so the balance survives a
/// restart and unconfirmed burns can be [resumed](BurnService::resume).
pub struct NotaryBurnService {
    keys: Keys,
    wallet: Arc<dyn Wallet>,
    notary: NotaryClient,
    electrum: Vec<ElectrumServer>,
    burns: Arc<BurnStore>,
    handle: tokio::runtime::Handle,
    tx: UnboundedSender<BurnOutcome>,
    max_fee: Amount,
}

impl NotaryBurnService {
    /// Construct the real service. `electrum` is a cross-check set of servers;
    /// `max_fee` caps the Lightning routing fee when paying the notary invoice.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        keys: Keys,
        wallet: Arc<dyn Wallet>,
        notary: NotaryClient,
        electrum: Vec<ElectrumServer>,
        burns: Arc<BurnStore>,
        handle: tokio::runtime::Handle,
        tx: UnboundedSender<BurnOutcome>,
        max_fee: Amount,
    ) -> Self {
        Self {
            keys,
            wallet,
            notary,
            electrum,
            burns,
            handle,
            tx,
            max_fee,
        }
    }

    /// Fetch the notarization tx from any Electrum server and verify the proof's
    /// on-chain binding (Part B). Tries servers in order until one answers.
    async fn fetch_and_verify(
        electrum: &[ElectrumServer],
        proof: &BurnProof,
    ) -> crate::Result<VerifiedBurn> {
        let mut last = crate::Error::Burn("no electrum servers configured".into());
        for server in electrum {
            let client = ElectrumClient::new(server.clone());
            match client.get_transaction(&proof.txid).await {
                Ok(raw) => match verify_proof_against_tx(proof, &raw, None) {
                    Ok(v) => return Ok(v),
                    Err(e) => last = e,
                },
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// Watch one burn (by notary `rhash`) to confirmation: poll `get_proof` and
    /// (re-)emit a `Proven` whenever the notary first returns a proof, its txid
    /// changes (RBF), or it confirms — persisting the latest state so the balance
    /// survives a restart. The mempool proof is surfaced as provisional the
    /// moment the notary returns it (no Electrum gate — see [`watch_decision`]),
    /// so a paid bond shows a pending balance within seconds; the confirmed proof
    /// upgrades it to durable. `last_txid` is `None` for a fresh burn (drives the
    /// fast-poll + fetch deadline) and the persisted txid when resuming.
    /// `report_failures` is `true` for a fresh burn (the user is waiting on the
    /// payment they just made, so a timeout is surfaced as a toast) and `false`
    /// for a background resume on startup (stop silently — an old watch that
    /// can't make progress shouldn't toast a failure out of the blue; the balance
    /// it already seeded stays as-is).
    #[allow(clippy::too_many_arguments)]
    async fn run_watch(
        notary: NotaryClient,
        burns: Arc<BurnStore>,
        tx: UnboundedSender<BurnOutcome>,
        rhash: String,
        event_id: B32,
        purpose: BurnPurpose,
        counterparty: Option<String>,
        mut last_txid: Option<String>,
        report_failures: bool,
    ) {
        let event_id_hex = to_hex(&event_id);
        let fail = |reason: String| BurnOutcome::Failed {
            purpose,
            event_id,
            reason,
        };
        let start = tokio::time::Instant::now();
        loop {
            let surfaced = last_txid.is_some();
            let elapsed = start.elapsed();
            if elapsed >= CONFIRM_DEADLINE || (!surfaced && elapsed >= FETCH_DEADLINE) {
                // A fresh burn we never surfaced timed out (payment/notary
                // problem); a surfaced-but-unconfirmed one stays persisted and
                // resumes later. A background resume stops silently.
                if !surfaced && report_failures {
                    let _ = tx.send(fail("timed out waiting for proof".into()));
                }
                return;
            }
            tokio::time::sleep(if surfaced { POLL_SLOW } else { POLL_FAST }).await;

            let p = match notary.get_proof(&rhash).await {
                Ok(Some(p)) => p,
                // Still waiting for the payment to settle / the batch to build.
                Ok(None) => continue,
                Err(e) => {
                    // A transient notary/transport hiccup must never kill a burn
                    // the user already paid for — retry until a deadline fires.
                    // (`get_proof` only errors on a non-"waiting" notary error or
                    // a network blip; both are worth re-polling, not bailing on.)
                    log::debug!("get_proof({rhash}): {e}");
                    continue;
                }
            };
            let confirmed = p.is_confirmed();
            let txid_changed = last_txid.as_deref() != Some(p.txid.as_str());
            let decision = watch_decision(confirmed, txid_changed, purpose == BurnPurpose::Boost);
            if decision.emit {
                // Persist the latest state (leaf is stable across RBF; txid is
                // refreshed) so the balance survives a restart and an unconfirmed
                // burn resumes by rhash next launch.
                let _ = burns.upsert(PersistedBurn {
                    rhash: rhash.clone(),
                    event_id: event_id_hex.clone(),
                    purpose,
                    counterparty: counterparty.clone(),
                    leaf: to_hex(&p.leaf_hash()),
                    txid: p.txid.clone(),
                    value_sats: p.leaf_value_msat / 1000,
                    confirmed,
                });
                last_txid = Some(p.txid.clone());
                let _ = tx.send(BurnOutcome::Proven {
                    purpose,
                    proof: Box::new(p),
                    counterparty: counterparty.clone(),
                });
            }
            if decision.done {
                return;
            }
        }
    }
}

/// 32 cryptographically-random bytes (a fresh secp key's secret), for a leaf
/// nonce — no extra RNG dependency.
fn random_32() -> [u8; 32] {
    crate::keys::generate().secret_key().to_secret_bytes()
}

impl BurnService for NotaryBurnService {
    fn notarize(&self, req: NotarizeReq) {
        let keys = self.keys.clone();
        let wallet = self.wallet.clone();
        let notary = self.notary.clone();
        let burns = self.burns.clone();
        let tx = self.tx.clone();
        let max_fee = self.max_fee;

        self.handle.spawn(async move {
            let nonce = random_32();
            let event_id_hex = to_hex(&req.event_id);
            let nonce_hex = to_hex(&nonce);

            // Optionally sign the leaf with our Nostr identity key (BIP340).
            let pk_hex = keys.public_key().to_hex();
            let sig_hex;
            let upvoter = if req.sign {
                let leaf_h = leaf_hash(
                    &req.event_id,
                    req.value_sats * 1000,
                    &nonce,
                    Some(&keys.public_key().to_bytes()),
                );
                let sig = keys.sign_schnorr(&Message::from_digest(leaf_h));
                sig_hex = to_hex(sig.as_ref());
                Some((pk_hex.as_str(), sig_hex.as_str()))
            } else {
                None
            };

            // Part A: request the burn, pay the invoice.
            let added = match notary
                .add_request(&event_id_hex, req.value_sats, &nonce_hex, upvoter)
                .await
            {
                Ok(a) => a,
                Err(e) => {
                    let _ = tx.send(BurnOutcome::Failed {
                        purpose: req.purpose,
                        event_id: req.event_id,
                        reason: format!("add_request: {e}"),
                    });
                    return;
                }
            };
            wallet.pay_invoice(added.invoice.clone(), max_fee);

            // Persist the watch *before* the first proof so a crash between
            // paying and the proof landing doesn't orphan the payment — it
            // resumes by rhash next launch. (No leaf/txid yet → not seeded.)
            let _ = burns.upsert(PersistedBurn {
                rhash: added.rhash.clone(),
                event_id: event_id_hex,
                purpose: req.purpose,
                counterparty: req.counterparty.clone(),
                leaf: String::new(),
                txid: String::new(),
                value_sats: req.value_sats,
                confirmed: false,
            });

            // Watch to confirmation: get_proof → emit. The notary first returns a
            // mempool proof (block_height == 0, within seconds) which we surface
            // as pending, RBF-replaces the txid repeatedly (we refresh it), then
            // confirms a block much later (we upgrade it to durable); the watch
            // surfaces each state (deduped by the stable leaf hash) and persists
            // it so the balance survives a restart.
            Self::run_watch(
                notary,
                burns,
                tx,
                added.rhash,
                req.event_id,
                req.purpose,
                req.counterparty,
                None,
                true, // fresh burn — surface a timeout to the waiting user
            )
            .await;
        });
    }

    fn resume(&self, burn: PersistedBurn) {
        // Already settled — the balance was seeded from disk; nothing to watch.
        if burn.confirmed {
            return;
        }
        let notary = self.notary.clone();
        let burns = self.burns.clone();
        let tx = self.tx.clone();
        self.handle.spawn(async move {
            let event_id = from_hex_array::<32>(&burn.event_id).unwrap_or([0u8; 32]);
            // Seed `last_txid` with the persisted one so we only re-emit on an
            // RBF txid change or the confirmation — the amount was already seeded.
            let last_txid = (!burn.txid.is_empty()).then(|| burn.txid.clone());
            Self::run_watch(
                notary,
                burns,
                tx,
                burn.rhash,
                event_id,
                burn.purpose,
                burn.counterparty,
                last_txid,
                false, // background resume — stop silently, don't toast failures
            )
            .await;
        });
    }

    fn verify_incoming(&self, proof: BurnProof) {
        let electrum = self.electrum.clone();
        let tx = self.tx.clone();
        self.handle.spawn(async move {
            match Self::fetch_and_verify(&electrum, &proof).await {
                Ok(v) if v.upvoter_verified => {
                    if let Some(pk) = proof.upvoter_pubkey {
                        let _ = tx.send(BurnOutcome::Credited {
                            pubkey: to_hex(&pk),
                            leaf_hash: to_hex(&proof.leaf_hash()),
                            value_msat: v.leaf_value_msat,
                            confirmed: v.confirmations > 0 || proof.is_confirmed(),
                            counterparty: None,
                        });
                    }
                }
                Ok(_) => {
                    let _ = tx.send(BurnOutcome::Rejected {
                        reason: "upvote is anonymous or unsigned".into(),
                    });
                }
                Err(e) => {
                    let _ = tx.send(BurnOutcome::Rejected {
                        reason: format!("verify: {e}"),
                    });
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

    fn drain(rx: &mut UnboundedReceiver<BurnOutcome>) -> Vec<BurnOutcome> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    #[test]
    fn watch_decision_emits_on_first_then_rbf_then_confirmation() {
        // First verified (mempool) proof: txid is "new" vs nothing emitted →
        // emit, keep watching.
        assert_eq!(
            watch_decision(false, true, false),
            WatchDecision { emit: true, done: false }
        );
        // Same mempool txid on a later poll → nothing changed, stay quiet.
        assert_eq!(
            watch_decision(false, false, false),
            WatchDecision { emit: false, done: false }
        );
        // RBF replaced the txid (still mempool) → re-emit to refresh it, keep
        // watching. The amount/balance is unchanged (deduped by leaf hash).
        assert_eq!(
            watch_decision(false, true, false),
            WatchDecision { emit: true, done: false }
        );
        // Confirmed → emit (upgrade pending→durable) and finish, even if the
        // txid didn't change between the last mempool poll and the block.
        assert_eq!(
            watch_decision(true, false, false),
            WatchDecision { emit: true, done: true }
        );
        // A boost is satisfied by mempool acceptance — finish at the first proof.
        assert_eq!(
            watch_decision(false, true, true),
            WatchDecision { emit: true, done: true }
        );
    }

    #[test]
    fn mock_notarize_emits_a_proven_outcome() {
        let (tx, mut rx) = unbounded_channel();
        let svc = MockBurnService::new(tx);
        svc.notarize(NotarizeReq {
            event_id: [7u8; 32],
            value_sats: 500,
            purpose: BurnPurpose::Bond,
            sign: true,
            counterparty: None,
        });
        let evs = drain(&mut rx);
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            BurnOutcome::Proven { purpose, proof, .. } => {
                assert_eq!(*purpose, BurnPurpose::Bond);
                assert_eq!(proof.leaf_value_msat, 500_000);
                assert!(proof.is_confirmed());
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(svc.requests().len(), 1);
    }

    #[test]
    fn mock_boost_is_unconfirmed() {
        let (tx, mut rx) = unbounded_channel();
        let svc = MockBurnService::new(tx);
        svc.notarize(NotarizeReq {
            event_id: [1u8; 32],
            value_sats: 10,
            purpose: BurnPurpose::Boost,
            sign: false,
            counterparty: None,
        });
        match &drain(&mut rx)[0] {
            BurnOutcome::Proven { proof, .. } => assert!(!proof.is_confirmed()),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn mock_can_be_made_to_fail() {
        let (tx, mut rx) = unbounded_channel();
        let svc = MockBurnService::new(tx);
        svc.set_failing(true);
        svc.notarize(NotarizeReq {
            event_id: [2u8; 32],
            value_sats: 100,
            purpose: BurnPurpose::Ride,
            sign: true,
            counterparty: Some("bob".into()),
        });
        assert!(matches!(drain(&mut rx)[0], BurnOutcome::Failed { .. }));
    }

    #[test]
    fn mock_verify_incoming_credits_signed_upvotes() {
        let (tx, mut rx) = unbounded_channel();
        let svc = MockBurnService::new(tx);
        let mut proof = BurnProof {
            version: 0,
            chain: None,
            event_id: [9u8; 32],
            leaf_value_msat: 700_000,
            nonce: [3u8; 32],
            merkle_hashes: Vec::new(),
            merkle_index: 0,
            txid: to_hex(&[0u8; 32]),
            block_height: 5,
            upvoter_pubkey: Some([0xabu8; 32]),
            upvoter_signature: Some([0u8; 64]),
        };
        svc.verify_incoming(proof.clone());
        match &drain(&mut rx)[0] {
            BurnOutcome::Credited { pubkey, value_msat, confirmed, .. } => {
                assert_eq!(pubkey, &to_hex(&[0xabu8; 32]));
                assert_eq!(*value_msat, 700_000);
                assert!(confirmed);
            }
            other => panic!("unexpected {other:?}"),
        }
        // Anonymous proof → rejected.
        proof.upvoter_pubkey = None;
        svc.verify_incoming(proof);
        assert!(matches!(drain(&mut rx)[0], BurnOutcome::Rejected { .. }));
    }
}
