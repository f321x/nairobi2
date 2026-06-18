//! A **Fedimint** e-cash backend for the modular [`nairobi_core::wallet::Wallet`]
//! trait.
//!
//! [`FedimintWallet`] joins a Fedimint federation (from an invite code) and
//! implements the same fire-and-forget `Wallet` interface the rest of the app
//! already uses, so it drops in beside the `MockWallet` (tests / desktop) and a
//! future Nostr-Wallet-Connect backend with no caller changes. It can be funded
//! over **Lightning** (issue an invoice) and **on-chain Bitcoin** (a peg-in
//! deposit address), send funds (pay an invoice, withdraw on-chain), and — via
//! [`nairobi_core::wallet::pay_mpesa`] / [`Wallet::pay_address`] — cash out to
//! M-Pesa through `bitcoin.co.ke`.
//!
//! ## Feature flag
//!
//! The real client SDK is a large, C/QUIC-heavy dependency tree, so it sits
//! behind the **`fedimint`** cargo feature (off by default). Without the
//! feature this crate compiles a tiny [`FedimintWallet`] *stub* (so the workspace
//! stays light and green); with `--features fedimint` it compiles the working
//! backend against `fedimint-client` 0.11.
//!
//! ## Building the real backend (incl. Android)
//!
//! The Fedimint crates set `--cfg tokio_unstable` in their build metadata, so
//! the consumer must pass the same flag — this crate's `.cargo/config.toml`
//! does that for standalone builds; an Android (`cargo-ndk`) build must export
//! `RUSTFLAGS="--cfg tokio_unstable"`. The database is the pure-Rust
//! `fedimint-cursed-redb` (no RocksDB/C++), chosen so the aarch64 cross-build
//! gains no extra native dependency beyond the `ring`/secp256k1 the app already
//! links.

#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(not(feature = "fedimint"))]
mod stub;
#[cfg(not(feature = "fedimint"))]
pub use stub::FedimintWallet;

#[cfg(feature = "fedimint")]
mod backend;
#[cfg(feature = "fedimint")]
pub use backend::FedimintWallet;
