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
use tokio::sync::mpsc::UnboundedSender;

use super::electrum::{ElectrumClient, ElectrumServer};
use super::notary::NotaryClient;
use super::proof::{leaf_hash, BurnProof};
use super::verify::{verify_proof_against_tx, VerifiedBurn};
use super::{proof::B32, to_hex};
use crate::wallet::{Amount, Wallet};

/// Why a burn is being made — routes the result in the engine and decides the
/// confirmation policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BurnPurpose {
    /// One-time identity bond (durable reputation).
    Bond,
    /// Per-ride attestation (~1 % of fare; durable, with a counterparty).
    Ride,
    /// Opt-in newcomer/priority boost (mempool acceptance is enough).
    Boost,
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
}

/// A no-op service for an engine constructed without proof-of-burn (keeps the
/// default path identical to before this feature).
pub struct NoBurnService;

impl BurnService for NoBurnService {
    fn notarize(&self, _req: NotarizeReq) {}
    fn verify_incoming(&self, _proof: BurnProof) {}
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
}

// ---- NotaryBurnService (real: notary + wallet + Electrum) ------------------

/// Poll the notary fast while waiting for the first (mempool) proof so the user
/// gets feedback within seconds of paying…
const POLL_FAST: Duration = Duration::from_secs(5);
/// …then back off once the provisional proof is in hand and we're only waiting
/// for a block (saves battery / notary load during the long confirmation wait).
const POLL_SLOW: Duration = Duration::from_secs(30);
/// Give up the whole burn only if *no* proof appears at all within this window
/// (payment not seen / notary down). The mempool proof normally lands within a
/// minute of the invoice settling; this is a generous backstop.
const FETCH_DEADLINE: Duration = Duration::from_secs(5 * 60);
/// Keep watching for the *confirmation* (to upgrade pending → durable) for up to
/// this long. A notary batch RBFs for a while before it lands in a block, so the
/// ~10 min of a single block is not enough; the provisional proof is already
/// surfaced as pending throughout. (Resuming the watch across app restarts is a
/// deferred durability item — see the design spec's status note.)
const CONFIRM_DEADLINE: Duration = Duration::from_secs(60 * 60);

/// What the watch does with a freshly fetched proof. Pure, so the network
/// loop's policy is unit-tested even though the loop itself does live I/O.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProofStep {
    /// Confirmed on-chain — verify it against Electrum (Part B) before crediting
    /// durable reputation, then finish.
    VerifyAndFinish,
    /// The first mempool proof — surface it as provisional straight away
    /// (server-trusted, no Electrum gate; design §8.1) so pending reputation and
    /// a notarization entry show right after paying.
    SurfaceProvisional,
    /// A mempool proof we've already surfaced — keep waiting for a block.
    AwaitConfirmation,
}

/// Decide what to do with a proof given whether it's confirmed and whether we've
/// already surfaced the provisional one. A mempool proof is *not* gated on our
/// own Electrum check (it's unreliable for a just-broadcast / RBF-replaced tx,
/// and per §8.1 mempool acceptance is server-trusted and provisional anyway);
/// only a confirmed proof is verified before it counts toward durable reputation.
fn proof_step(confirmed: bool, announced_provisional: bool) -> ProofStep {
    match (confirmed, announced_provisional) {
        (true, _) => ProofStep::VerifyAndFinish,
        (false, false) => ProofStep::SurfaceProvisional,
        (false, true) => ProofStep::AwaitConfirmation,
    }
}

/// The real service: requests a burn from the notary, pays the invoice with the
/// app [`Wallet`], polls for the proof, and verifies it against Electrum.
pub struct NotaryBurnService {
    keys: Keys,
    wallet: Arc<dyn Wallet>,
    notary: NotaryClient,
    electrum: Vec<ElectrumServer>,
    handle: tokio::runtime::Handle,
    tx: UnboundedSender<BurnOutcome>,
    max_fee: Amount,
}

impl NotaryBurnService {
    /// Construct the real service. `electrum` is a cross-check set of servers;
    /// `max_fee` caps the Lightning routing fee when paying the notary invoice.
    pub fn new(
        keys: Keys,
        wallet: Arc<dyn Wallet>,
        notary: NotaryClient,
        electrum: Vec<ElectrumServer>,
        handle: tokio::runtime::Handle,
        tx: UnboundedSender<BurnOutcome>,
        max_fee: Amount,
    ) -> Self {
        Self {
            keys,
            wallet,
            notary,
            electrum,
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
        let electrum = self.electrum.clone();
        let tx = self.tx.clone();
        let max_fee = self.max_fee;

        self.handle.spawn(async move {
            let fail = |reason: String| BurnOutcome::Failed {
                purpose: req.purpose,
                event_id: req.event_id,
                reason,
            };

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

            // Part A: request the burn, pay the invoice, poll for the proof.
            let added = match notary
                .add_request(&event_id_hex, req.value_sats, &nonce_hex, upvoter)
                .await
            {
                Ok(a) => a,
                Err(e) => {
                    let _ = tx.send(fail(format!("add_request: {e}")));
                    return;
                }
            };
            wallet.pay_invoice(added.invoice.clone(), max_fee);

            // Poll for the proof. The notary returns a **mempool** proof
            // (`block_height == 0`) within seconds, then the **confirmed** one
            // (the RBF'd batch lands in a block) much later. We surface the
            // mempool proof as soon as the notary returns it — provisional,
            // server-trusted (design §8.1), so the user sees pending reputation +
            // a notarization entry right after paying. We do *not* block that on
            // our own Electrum check, which is unreliable for a just-broadcast /
            // RBF-replaced tx; full client-side verification gates only the
            // confirmed proof that grants durable reputation. The engine upgrades
            // pending → durable in place (deduped by leaf hash) when the confirmed
            // proof arrives. A Boost is satisfied by the mempool proof and stops.
            let start = tokio::time::Instant::now();
            let mut announced_provisional = false;
            loop {
                let elapsed = start.elapsed();
                if elapsed >= CONFIRM_DEADLINE
                    || (!announced_provisional && elapsed >= FETCH_DEADLINE)
                {
                    // Give up *only* if nothing was ever surfaced; once the
                    // provisional proof is pending in the UI a quiet stop is
                    // correct (the confirmation may simply outlast this watch).
                    if !announced_provisional {
                        let _ = tx.send(fail("timed out waiting for proof".into()));
                    }
                    return;
                }
                tokio::time::sleep(if announced_provisional { POLL_SLOW } else { POLL_FAST }).await;

                let p = match notary.get_proof(&added.rhash).await {
                    Ok(Some(p)) => p,
                    Ok(None) => continue,
                    Err(e) => {
                        // A transient notary hiccup must not nuke an already-
                        // pending burn; only fail before anything is surfaced.
                        log::debug!("get_proof: {e}");
                        if announced_provisional {
                            continue;
                        }
                        let _ = tx.send(fail(format!("get_proof: {e}")));
                        return;
                    }
                };

                match proof_step(p.is_confirmed(), announced_provisional) {
                    ProofStep::VerifyAndFinish => {
                        // Part B: verify on-chain before crediting durable rep. A
                        // just-confirmed tx may not be indexed by every Electrum
                        // server yet, so a miss is "not ready" — keep polling.
                        match Self::fetch_and_verify(&electrum, &p).await {
                            Ok(_) => {
                                let _ = tx.send(BurnOutcome::Proven {
                                    purpose: req.purpose,
                                    proof: Box::new(p),
                                    counterparty: req.counterparty.clone(),
                                });
                                return;
                            }
                            Err(e) => {
                                log::debug!("confirmed burn proof not yet verifiable: {e}");
                                continue;
                            }
                        }
                    }
                    ProofStep::SurfaceProvisional => {
                        announced_provisional = true;
                        let _ = tx.send(BurnOutcome::Proven {
                            purpose: req.purpose,
                            proof: Box::new(p),
                            counterparty: req.counterparty.clone(),
                        });
                        if req.purpose == BurnPurpose::Boost {
                            return; // mempool acceptance is enough for a boost
                        }
                    }
                    ProofStep::AwaitConfirmation => continue,
                }
            }
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
    fn proof_step_surfaces_mempool_then_awaits_confirmation() {
        // A mempool proof we haven't shown yet is surfaced provisionally — note
        // this branch is reached *without* an Electrum check, so a fresh /
        // RBF-churning tx still gives the user immediate feedback.
        assert_eq!(proof_step(false, false), ProofStep::SurfaceProvisional);
        // Once surfaced, further mempool re-fetches just keep waiting (no double
        // surface) until a block lands.
        assert_eq!(proof_step(false, true), ProofStep::AwaitConfirmation);
        // A confirmed proof is verified on-chain before it credits durable
        // reputation, whether or not we surfaced the provisional one first.
        assert_eq!(proof_step(true, false), ProofStep::VerifyAndFinish);
        assert_eq!(proof_step(true, true), ProofStep::VerifyAndFinish);
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
