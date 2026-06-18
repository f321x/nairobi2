//! The single error type for `nairobi-core`.

use thiserror::Error;

/// Errors produced anywhere in the core. Kept deliberately coarse: the UI only
/// ever needs to show a human-readable message, never branch on the variant.
#[derive(Debug, Error)]
pub enum Error {
    /// Persisted config is missing/unreadable/corrupt. We never silently wipe
    /// keys — the caller decides whether to regenerate.
    #[error("config error: {0}")]
    Config(String),

    /// Filesystem I/O failure (config read/write).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON (de)serialization failure (config, event payloads, API responses).
    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),

    /// A Nostr-layer failure (key parsing, event build/sign, relay client).
    #[error("nostr error: {0}")]
    Nostr(String),

    /// Geocoding / routing / geohash failure.
    #[error("geo error: {0}")]
    Geo(String),

    /// A wallet-layer failure (balance/receive/send, lightning-address
    /// resolution, or the underlying Fedimint/NWC backend).
    #[error("wallet error: {0}")]
    Wallet(String),
}

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;
