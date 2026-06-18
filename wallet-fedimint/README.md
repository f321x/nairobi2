# nairobi-wallet-fedimint

A **Fedimint** e-cash backend for the [`nairobi_core::wallet::Wallet`] trait — the real
Bitcoin/Lightning wallet behind nairobi2's modular wallet API.

`FedimintWallet` joins a Fedimint federation (from an invite code) and implements the same
fire-and-forget `Wallet` interface the app already uses for the `MockWallet`, so it drops in with no
caller changes. It supports:

- **Funding:** issue a BOLT-11 Lightning invoice, or allocate an on-chain peg-in deposit address.
- **Sending:** pay a Lightning invoice, withdraw on-chain.
- **M-Pesa cash-out:** `Wallet::pay_address` + `nairobi_core::wallet::pay_mpesa` resolve
  `<phone>@bitcoin.co.ke` and pay it over Lightning (the service converts sats → KES → M-Pesa).
- **Balance:** the spendable e-cash balance.

Built against `fedimint-client` **0.11**. The seed (a BIP-39 mnemonic) and the database
(`fedimint-cursed-redb`, pure Rust) are persisted under the app's data directory.

## Feature flag (off by default)

The Fedimint client SDK is a large dependency tree, so **all of it is optional**, behind the
`fedimint` feature. Without the feature this crate is a tiny stub (it still implements `Wallet`, but
every call reports the backend is not built), which keeps the main nairobi2 workspace light and
ring-only. Build the real backend with:

```sh
RUSTFLAGS="--cfg tokio_unstable" cargo check -p nairobi-wallet-fedimint --features fedimint
```

In the app, enable it via the app's own feature: `cargo … -p nairobi-app --features fedimint`
(also requires the `tokio_unstable` rustflag).

## Caveats for the Android / release build

- **`--cfg tokio_unstable` is mandatory** — the Fedimint crates set it in their build metadata; the
  cargo-ndk invocation must export `RUSTFLAGS="--cfg tokio_unstable"`.
- **Database:** uses `fedimint-cursed-redb` (pure Rust), *not* RocksDB, so the aarch64 cross-build
  gains no C++ dependency.
- **⚠️ TLS / ring-only:** nairobi2's rule is one TLS stack, `ring` only. With this feature on,
  `fedimint-connectors`' **iroh** QUIC transport pulls **`aws-lc-rs`** (via `quinn`). Before
  shipping a Fedimint-enabled APK, audit/disable the iroh transport or force its rustls
  `CryptoProvider` back to `ring`. With the feature **off**, none of this is compiled and the
  default APK is unaffected.

[`nairobi_core::wallet::Wallet`]: ../core/src/wallet/mod.rs
