//! Nostr identity for nairobi2.
//!
//! A keypair is auto-generated on first launch and persisted (see [`crate::config`]).
//! There is no seed-phrase ceremony in v1 — the secret lives in app-private
//! storage, wrapped in [`SecretString`] so it never leaks into logs.
//!
//! Because the network is permissionless and pseudonymous, we also derive a
//! stable, memorable **display name** ("Adjective Animal") and a stable
//! **color** from the public key. A non-literate user can recognize a
//! counterpart by name + color without reading a pubkey.

use crate::error::{Error, Result};
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};

/// A string that never reveals its contents via `Debug`/`Display`. Secret keys
/// are always kept inside this type.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretString(String);

impl SecretString {
    /// Wrap a sensitive string.
    pub fn new(s: String) -> Self {
        Self(s)
    }

    /// Borrow the underlying value. Call sites should keep the exposed `&str`
    /// as short-lived as possible and never log it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretString(<redacted>)")
    }
}

impl std::fmt::Display for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Generate a fresh random identity.
pub fn generate() -> Keys {
    Keys::generate()
}

/// Parse a secret key from `nsec1…` bech32 or 64-char hex into a full keypair.
pub fn parse_secret(input: &str) -> Result<Keys> {
    Keys::parse(input).map_err(|e| Error::Nostr(format!("invalid secret key: {e}")))
}

/// The `nsec1…` bech32 form of the secret key, wrapped so it never leaks.
pub fn nsec(keys: &Keys) -> Result<SecretString> {
    keys.secret_key()
        .to_bech32()
        .map(SecretString::new)
        .map_err(|e| Error::Nostr(format!("nsec encode: {e}")))
}

/// The `npub1…` bech32 form of a public key (safe to display/share).
pub fn npub(pk: &PublicKey) -> Result<String> {
    pk.to_bech32()
        .map_err(|e| Error::Nostr(format!("npub encode: {e}")))
}

const ADJECTIVES: &[&str] = &[
    "Swift", "Bright", "Calm", "Bold", "Kind", "Clever", "Brave", "Lucky", "Quiet", "Sunny",
    "Eager", "Gentle", "Happy", "Jolly", "Keen", "Lively", "Merry", "Noble", "Proud", "Rapid",
    "Shiny", "Tidy", "Witty", "Zesty", "Mighty", "Nimble", "Quick", "Royal", "Steady", "Warm",
    "Cool", "Fair",
];

const ANIMALS: &[&str] = &[
    "Lion", "Zebra", "Eagle", "Gazelle", "Rhino", "Cheetah", "Falcon", "Giraffe", "Hippo", "Impala",
    "Jackal", "Leopard", "Mongoose", "Buffalo", "Oryx", "Panther", "Crane", "Hawk", "Heron", "Ibis",
    "Kudu", "Lynx", "Meerkat", "Otter", "Puma", "Robin", "Stork", "Tiger", "Vulture", "Wolf",
    "Antelope", "Bee",
];

/// A short, stable, human-recognizable label derived from the public key. Used
/// to identify a counterpart visually without reading the pubkey.
pub fn derive_name(pk: &PublicKey) -> String {
    let b = pk.to_bytes();
    let adj = ADJECTIVES[b[0] as usize % ADJECTIVES.len()];
    let animal = ANIMALS[b[1] as usize % ANIMALS.len()];
    format!("{adj} {animal}")
}

/// A stable, always-visible RGB color derived from the public key. Each channel
/// is kept in `64..=255` so the color is legible on any background.
pub fn display_color(pk: &PublicKey) -> (u8, u8, u8) {
    let b = pk.to_bytes();
    let chan = |x: u8| 64u8.saturating_add(x % 192);
    (chan(b[2]), chan(b[3]), chan(b[4]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_string_is_redacted() {
        let s = SecretString::new("nsec1supersecret".to_string());
        assert_eq!(format!("{s:?}"), "SecretString(<redacted>)");
        assert_eq!(format!("{s}"), "<redacted>");
        // The value is still retrievable for use.
        assert_eq!(s.expose(), "nsec1supersecret");
    }

    #[test]
    fn npub_is_bech32() {
        let keys = generate();
        let npub = npub(&keys.public_key()).unwrap();
        assert!(npub.starts_with("npub1"), "got {npub}");
    }

    #[test]
    fn nsec_round_trips_to_same_pubkey() {
        let keys = generate();
        let nsec = nsec(&keys).unwrap();
        assert!(nsec.expose().starts_with("nsec1"));
        let reparsed = parse_secret(nsec.expose()).unwrap();
        assert_eq!(reparsed.public_key(), keys.public_key());
    }

    #[test]
    fn parse_secret_rejects_garbage() {
        assert!(parse_secret("not-a-key").is_err());
    }

    #[test]
    fn derived_name_is_deterministic_and_in_range() {
        let keys = generate();
        let pk = keys.public_key();
        let n1 = derive_name(&pk);
        let n2 = derive_name(&pk);
        assert_eq!(n1, n2);
        let (adj, animal) = n1.split_once(' ').unwrap();
        assert!(ADJECTIVES.contains(&adj));
        assert!(ANIMALS.contains(&animal));
    }

    #[test]
    fn display_color_channels_are_visible() {
        let keys = generate();
        let (r, g, b) = display_color(&keys.public_key());
        for c in [r, g, b] {
            assert!(c >= 64, "channel {c} too dark");
        }
    }
}
