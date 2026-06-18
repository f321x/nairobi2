//! Persisted identity + settings.
//!
//! Stored as `config.json` in a caller-provided data directory (Android
//! app-private storage on device; an `$XDG`-style dir in the desktop
//! simulator). Writes are atomic (temp file + rename). A corrupt file is an
//! **error**, never a silent wipe — losing the key means losing the identity,
//! so the caller decides whether to regenerate.

use crate::error::{Error, Result};
use crate::keys::{self, SecretString};
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Default relay set. Editable in Settings; chosen for reliability and
/// permissive usage. The app is fully relay-driven (no aggregator), so this is
/// the only piece of "infrastructure" configuration.
pub const DEFAULT_RELAYS: &[&str] = &[
    "wss://relay.damus.io",
    "wss://nos.lol",
    "wss://relay.primal.net",
    "wss://relay.nostr.band",
];

/// Default display currency. Plain integer amounts are shown as large numerals
/// next to this short code (numerals are recognizable to non-literate users).
pub const DEFAULT_CURRENCY: &str = "KES";

/// Everything we persist between launches.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// The user's Nostr secret key as `nsec1…`. `None` until first generated.
    pub secret: Option<SecretString>,
    /// Relays to publish to / subscribe from.
    pub relays: Vec<String>,
    /// Display currency code (e.g. `KES`).
    pub currency: String,
    /// Fedimint federation invite code the wallet joins on first use. `None`
    /// until the user pastes one in Settings; the mock/desktop wallet ignores it.
    pub federation_invite: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            secret: None,
            relays: DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect(),
            currency: DEFAULT_CURRENCY.to_string(),
            federation_invite: None,
        }
    }
}

impl Config {
    /// Return the identity keypair, generating a fresh one on first use and
    /// storing its `nsec` back into `self.secret`. The caller is responsible
    /// for persisting via [`ConfigStore::save`] afterwards so the freshly
    /// generated key survives a restart.
    pub fn identity(&mut self) -> Result<Keys> {
        match &self.secret {
            Some(s) => keys::parse_secret(s.expose()),
            None => {
                let k = keys::generate();
                self.secret = Some(keys::nsec(&k)?);
                Ok(k)
            }
        }
    }
}

/// Reads/writes [`Config`] at a fixed `config.json` path.
pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    /// Point the store at `<dir>/config.json`.
    pub fn new(dir: &Path) -> Self {
        Self {
            path: dir.join("config.json"),
        }
    }

    /// The full path to the config file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load the config, falling back to defaults when the file is missing. A
    /// present-but-unparseable file is a hard error (we never silently discard
    /// a stored key).
    pub fn load(&self) -> Result<Config> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
                Error::Config(format!("corrupt config at {}: {e}", self.path.display()))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Atomically persist the config (write to a temp file, then rename).
    pub fn save(&self, config: &Config) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(config)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A unique temp directory for one test, without pulling in the `tempfile`
    /// crate. Cleaned up by the OS / left under the system temp dir.
    fn temp_dir() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("nairobi-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn load_missing_returns_defaults() {
        let store = ConfigStore::new(&temp_dir());
        let cfg = store.load().unwrap();
        assert!(cfg.secret.is_none());
        assert_eq!(cfg.currency, DEFAULT_CURRENCY);
        assert_eq!(cfg.relays.len(), DEFAULT_RELAYS.len());
    }

    #[test]
    fn save_then_load_round_trips() {
        let store = ConfigStore::new(&temp_dir());
        let cfg = Config {
            currency: "USD".to_string(),
            relays: vec!["wss://example.relay".to_string()],
            ..Default::default()
        };
        store.save(&cfg).unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.currency, "USD");
        assert_eq!(loaded.relays, vec!["wss://example.relay".to_string()]);
    }

    #[test]
    fn corrupt_file_is_an_error_not_a_wipe() {
        let dir = temp_dir();
        let store = ConfigStore::new(&dir);
        std::fs::write(store.path(), b"{ this is not json").unwrap();
        let err = store.load().unwrap_err();
        assert!(matches!(err, Error::Config(_)), "got {err:?}");
    }

    #[test]
    fn identity_generates_then_is_stable_across_reload() {
        let store = ConfigStore::new(&temp_dir());
        let mut cfg = store.load().unwrap();
        assert!(cfg.secret.is_none());

        let keys1 = cfg.identity().unwrap();
        assert!(cfg.secret.is_some(), "identity() must populate the secret");
        store.save(&cfg).unwrap();

        // A second call on the same config returns the same identity.
        let keys1b = cfg.identity().unwrap();
        assert_eq!(keys1.public_key(), keys1b.public_key());

        // And it survives a reload from disk.
        let mut reloaded = store.load().unwrap();
        let keys2 = reloaded.identity().unwrap();
        assert_eq!(keys1.public_key(), keys2.public_key());
    }
}
