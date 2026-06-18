//! The no-dependency stub compiled when the crate is built **without** the
//! `fedimint` feature. It satisfies the [`Wallet`] trait and exposes the same
//! `connect_blocking` constructor as the real backend, but cannot connect — so a
//! default workspace build (and `cargo test --workspace`) stays light and
//! ring-only. The app only depends on this crate behind its own `fedimint`
//! feature, so in practice the stub is compiled but never instantiated.

use std::path::PathBuf;

use tokio::runtime::Handle;
use tokio::sync::mpsc::UnboundedSender;

use nairobi_core::wallet::{Amount, LightningAddress, PaymentKind, Wallet, WalletEvent};

/// Stand-in for the real Fedimint wallet when the `fedimint` feature is off.
pub struct FedimintWallet;

impl FedimintWallet {
    /// Always fails: the working backend needs `--features fedimint`.
    pub fn connect_blocking(
        _rt: Handle,
        _data_dir: PathBuf,
        _invite: String,
        _tx: UnboundedSender<WalletEvent>,
    ) -> Result<Self, String> {
        Err("nairobi-wallet-fedimint was built without the `fedimint` feature".into())
    }
}

impl Wallet for FedimintWallet {
    fn refresh_balance(&self) {}
    fn receive_lightning(&self, _amount: Amount, _description: String) {}
    fn receive_onchain(&self) {}
    fn pay_invoice(&self, _bolt11: String, _max_fee: Amount) {}
    fn pay_address(&self, _address: LightningAddress, _amount: Amount, _kind: PaymentKind) {}
    fn pay_onchain(&self, _address: String, _amount: Amount) {}
}
