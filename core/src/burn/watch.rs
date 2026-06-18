//! Persistence of *our own* proof-of-burn watches — the durable record behind
//! the Settings reputation balance.
//!
//! A burn is slow: after the Lightning invoice is paid the notary batches the
//! request, RBF-replaces the transaction repeatedly (the txid keeps changing),
//! and only confirms a block (often much) later. The in-memory engine ledger
//! would forget all of that across an app restart, so the balance would reset
//! and a burn that confirmed while the app was closed would never be credited.
//!
//! [`BurnStore`] persists one [`PersistedBurn`] per burn (keyed by the notary
//! `rhash`, which is 1:1 with the burn and stable across RBF). On startup the
//! controller (a) seeds the engine so the balance is correct immediately and
//! offline, and (b) re-polls every still-unconfirmed burn by `rhash` until it
//! confirms. The **leaf hash** is the stable identity of the burn across RBF;
//! the `txid` is refreshed as the notary replaces the transaction.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::service::BurnPurpose;
use crate::error::Result;

/// One of our burns, persisted so the balance survives restarts and an
/// unconfirmed burn can be watched to completion across launches.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedBurn {
    /// The notary invoice payment hash — the handle for `get_proof`, 1:1 with
    /// this burn and stable across RBF replacements.
    pub rhash: String,
    /// The notarized Nostr event id (hex) — for diagnostics / fail messages.
    pub event_id: String,
    /// What the burn was for (drives the label + confirmation policy).
    pub purpose: BurnPurpose,
    /// Ride attestations carry the counterparty pubkey (hex) for diversity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counterparty: Option<String>,
    /// Stable leaf hash (hex) — the burn's identity across RBF; the dedup key in
    /// the reputation ledger. Empty until the first proof is verified.
    #[serde(default)]
    pub leaf: String,
    /// Latest verified notarization txid (display hex). Refreshed on each RBF.
    #[serde(default)]
    pub txid: String,
    /// This burn's share, in sats.
    #[serde(default)]
    pub value_sats: u64,
    /// Confirmed in a block (vs still mempool). Confirmed burns are kept for the
    /// balance but no longer watched.
    #[serde(default)]
    pub confirmed: bool,
}

/// Reads/writes the set of [`PersistedBurn`]s at `<dir>/burns.json`. Cheap and
/// rarely written (only when a burn starts, refreshes its txid, or confirms), so
/// it just rewrites the whole file atomically under a lock — many watch tasks
/// upsert concurrently.
pub struct BurnStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl BurnStore {
    /// Point the store at `<dir>/burns.json`.
    pub fn new(dir: &Path) -> Self {
        Self {
            path: dir.join("burns.json"),
            lock: Mutex::new(()),
        }
    }

    /// The full path to the burns file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load all persisted burns (empty when the file is missing; a corrupt file
    /// is surfaced as an error so we never silently drop the balance).
    pub fn load(&self) -> Result<Vec<PersistedBurn>> {
        let _g = self.lock.lock().unwrap();
        Self::load_locked(&self.path)
    }

    fn load_locked(path: &Path) -> Result<Vec<PersistedBurn>> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    /// Insert or replace the burn with this `rhash`, then persist atomically.
    pub fn upsert(&self, burn: PersistedBurn) -> Result<()> {
        let _g = self.lock.lock().unwrap();
        let mut all = Self::load_locked(&self.path)?;
        match all.iter_mut().find(|b| b.rhash == burn.rhash) {
            Some(existing) => *existing = burn,
            None => all.push(burn),
        }
        self.save_locked(&all)
    }

    fn save_locked(&self, all: &[PersistedBurn]) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(all)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_dir() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("nairobi-burns-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn burn(rhash: &str, confirmed: bool) -> PersistedBurn {
        PersistedBurn {
            rhash: rhash.into(),
            event_id: "ab".repeat(32),
            purpose: BurnPurpose::Bond,
            counterparty: None,
            leaf: "cd".repeat(32),
            txid: "ef".repeat(32),
            value_sats: 500,
            confirmed,
        }
    }

    #[test]
    fn load_missing_is_empty() {
        let store = BurnStore::new(&temp_dir());
        assert!(store.load().unwrap().is_empty());
    }

    #[test]
    fn upsert_inserts_then_replaces_by_rhash() {
        let store = BurnStore::new(&temp_dir());
        store.upsert(burn("r1", false)).unwrap();
        store.upsert(burn("r2", false)).unwrap();
        assert_eq!(store.load().unwrap().len(), 2);

        // Same rhash, now confirmed — replaces in place (no duplicate).
        store.upsert(burn("r1", true)).unwrap();
        let all = store.load().unwrap();
        assert_eq!(all.len(), 2);
        let r1 = all.iter().find(|b| b.rhash == "r1").unwrap();
        assert!(r1.confirmed);
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = temp_dir();
        BurnStore::new(&dir).upsert(burn("r1", true)).unwrap();
        // A fresh store over the same dir sees the persisted burn.
        let reloaded = BurnStore::new(&dir).load().unwrap();
        assert_eq!(reloaded, vec![burn("r1", true)]);
    }
}
