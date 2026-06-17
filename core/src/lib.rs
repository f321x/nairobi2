//! # nairobi-core
//!
//! The UI-free, OS-free core of **nairobi2** — a permissionless, fully
//! Nostr-native ridesharing app. Everything here runs on any host and is
//! unit-testable off-device (the discipline borrowed from the `ntrack`
//! reference project).
//!
//! ## Shape
//!
//! - [`keys`] / [`config`] — auto-generated Nostr identity + persisted settings.
//! - [`geo`] — hand-rolled geohash, distance math, and the Nominatim/OSRM
//!   clients (with a haversine fallback) over a minimal rustls/ring HTTPS GET.
//! - [`auction`] — the pure escalating-rate schedule and fare math.
//! - [`matching`] — deterministic, server-free first-taker-wins resolution.
//! - [`protocol`] — build / sign / parse / validate every Nostr event kind.
//! - [`pool`] — the [`pool::Pool`] transport trait, a test `MockPool`, and the
//!   real `nostr-sdk`-backed implementation.
//! - [`engine`] — the single channel-driven task that owns all ride state and
//!   talks to the UI and OS only over channels.
//!
//! No module here ever touches a UI toolkit or an OS API directly.

pub mod auction;
pub mod config;
pub mod error;
pub mod geo;
pub mod keys;
pub mod matching;
pub mod pool;
pub mod protocol;

pub use error::{Error, Result};

#[cfg(test)]
mod tests {
    #[test]
    fn crate_smoke() {
        // Sanity: the workspace builds and the test harness runs offline.
        assert_eq!(2 + 2, 4);
    }
}
