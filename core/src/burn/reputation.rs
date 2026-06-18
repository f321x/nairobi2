//! Per-pubkey accrual of verified proofs-of-burn — the durable, Sybil-resistant
//! reputation a client gates ride requests/acceptances on.
//!
//! Pure and serde-persistable. The ledger does **not** verify anything or
//! resolve Nostr events; the caller (the [`crate::engine`] / [`super::service`])
//! verifies a proof on-chain, resolves which pubkey it credits, and records the
//! result here. The ledger's job is the accounting the spec calls for:
//!
//! - **dedup by leaf hash** — replays of the same upvote share a leaf hash and
//!   collapse to one (so an attacker can't inflate by re-publishing);
//! - **confirmed-only score** — only burns confirmed in a block count toward
//!   durable reputation (mempool ones are provisional);
//! - **counterparty diversity** — distinct counterparties in completion
//!   attestations, the anti-collusion signal that raw sat-sum lacks.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// One recorded burn, keyed in the ledger by its leaf hash (hex).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BurnRecord {
    /// Pubkey (hex) this burn credits — the self-signed upvoter / event author.
    pub pubkey: String,
    /// Burnt share, in msat.
    pub value_msat: u64,
    /// Confirmed in a block (vs mempool-only).
    pub confirmed: bool,
    /// For per-ride attestations: the counterparty pubkey (hex), enabling
    /// diversity weighting. `None` for identity bonds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counterparty: Option<String>,
}

/// A client's local view of who has burnt how much. Keyed by leaf hash so
/// duplicates collapse automatically.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ReputationLedger {
    /// leaf-hash hex → record.
    by_leaf: HashMap<String, BurnRecord>,
}

impl ReputationLedger {
    /// An empty ledger.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a verified burn under `leaf_hash_hex`. Idempotent by leaf hash:
    /// a repeat is ignored, except that an **unconfirmed → confirmed** upgrade
    /// is applied (the same leaf later lands in a block). Returns `true` if the
    /// ledger changed.
    pub fn record(&mut self, leaf_hash_hex: String, record: BurnRecord) -> bool {
        match self.by_leaf.get(&leaf_hash_hex) {
            Some(existing) if existing.confirmed || !record.confirmed => false,
            _ => {
                self.by_leaf.insert(leaf_hash_hex, record);
                true
            }
        }
    }

    /// Total **confirmed** burnt sats credited to `pubkey_hex` (deduped by leaf
    /// hash). This is the score a spam threshold is applied to.
    pub fn score_sats(&self, pubkey_hex: &str) -> u64 {
        self.by_leaf
            .values()
            .filter(|r| r.confirmed && r.pubkey == pubkey_hex)
            .map(|r| r.value_msat / 1000)
            .sum()
    }

    /// Confirmed burnt sats including provisional mempool burns (used for the
    /// opt-in newcomer/priority path, capped by the caller).
    pub fn provisional_sats(&self, pubkey_hex: &str) -> u64 {
        self.by_leaf
            .values()
            .filter(|r| r.pubkey == pubkey_hex)
            .map(|r| r.value_msat / 1000)
            .sum()
    }

    /// Number of **distinct** counterparties in this pubkey's confirmed ride
    /// attestations — the anti-collusion diversity signal.
    pub fn diversity(&self, pubkey_hex: &str) -> usize {
        let mut seen = std::collections::HashSet::new();
        for r in self.by_leaf.values() {
            if r.confirmed && r.pubkey == pubkey_hex {
                if let Some(cp) = &r.counterparty {
                    seen.insert(cp.as_str());
                }
            }
        }
        seen.len()
    }

    /// Does `pubkey` meet a `threshold_sats` confirmed-burn bar? A threshold of
    /// `0` admits everyone (gating disabled), preserving permissionlessness.
    pub fn meets(&self, pubkey_hex: &str, threshold_sats: u64) -> bool {
        threshold_sats == 0 || self.score_sats(pubkey_hex) >= threshold_sats
    }

    /// Number of distinct leaves recorded (for diagnostics/tests).
    pub fn len(&self) -> usize {
        self.by_leaf.len()
    }

    /// Whether the ledger is empty.
    pub fn is_empty(&self) -> bool {
        self.by_leaf.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(pubkey: &str, sats: u64, confirmed: bool, cp: Option<&str>) -> BurnRecord {
        BurnRecord {
            pubkey: pubkey.into(),
            value_msat: sats * 1000,
            confirmed,
            counterparty: cp.map(str::to_string),
        }
    }

    #[test]
    fn sums_confirmed_burns_per_pubkey() {
        let mut l = ReputationLedger::new();
        l.record("leaf1".into(), rec("alice", 500, true, None));
        l.record("leaf2".into(), rec("alice", 300, true, Some("bob")));
        l.record("leaf3".into(), rec("bob", 1000, true, None));
        assert_eq!(l.score_sats("alice"), 800);
        assert_eq!(l.score_sats("bob"), 1000);
        assert_eq!(l.score_sats("carol"), 0);
    }

    #[test]
    fn dedups_replayed_leaves() {
        let mut l = ReputationLedger::new();
        assert!(l.record("leaf1".into(), rec("alice", 500, true, None)));
        // Same leaf hash replayed (e.g. re-published under a new pubkey) — must
        // not inflate.
        assert!(!l.record("leaf1".into(), rec("attacker", 500, true, None)));
        assert_eq!(l.score_sats("alice"), 500);
        assert_eq!(l.score_sats("attacker"), 0);
        assert_eq!(l.len(), 1);
    }

    #[test]
    fn unconfirmed_does_not_count_until_upgraded() {
        let mut l = ReputationLedger::new();
        l.record("leaf1".into(), rec("alice", 500, false, None));
        assert_eq!(l.score_sats("alice"), 0); // mempool only
        assert_eq!(l.provisional_sats("alice"), 500);
        // It confirms later → upgrade applies.
        assert!(l.record("leaf1".into(), rec("alice", 500, true, None)));
        assert_eq!(l.score_sats("alice"), 500);
        // A confirmed record is never downgraded by a stale unconfirmed replay.
        assert!(!l.record("leaf1".into(), rec("alice", 500, false, None)));
        assert_eq!(l.score_sats("alice"), 500);
    }

    #[test]
    fn diversity_counts_distinct_counterparties() {
        let mut l = ReputationLedger::new();
        l.record("a".into(), rec("driver", 100, true, Some("p1")));
        l.record("b".into(), rec("driver", 100, true, Some("p2")));
        l.record("c".into(), rec("driver", 100, true, Some("p1"))); // repeat cp
        l.record("d".into(), rec("driver", 100, true, None)); // a bond, no cp
        assert_eq!(l.diversity("driver"), 2);
    }

    #[test]
    fn threshold_zero_admits_everyone() {
        let l = ReputationLedger::new();
        assert!(l.meets("nobody", 0));
        assert!(!l.meets("nobody", 1));
    }

    #[test]
    fn ledger_serde_round_trips() {
        let mut l = ReputationLedger::new();
        l.record("leaf1".into(), rec("alice", 500, true, Some("bob")));
        let json = serde_json::to_string(&l).unwrap();
        let back: ReputationLedger = serde_json::from_str(&json).unwrap();
        assert_eq!(back.score_sats("alice"), 500);
    }
}
