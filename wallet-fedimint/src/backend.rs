//! The working Fedimint backend (compiled with `--features fedimint`).
//!
//! Joins a federation from an invite code and implements
//! [`nairobi_core::wallet::Wallet`] over `fedimint-client` 0.11. Every trait
//! method is fire-and-forget: it spawns the async Fedimint operation on the
//! shared Tokio runtime and reports the outcome back as a [`WalletEvent`], so
//! the wallet stays decoupled from the UI exactly like the relay [`Pool`].
//!
//! The seed (a BIP-39 mnemonic) and the e-cash database (`fedimint-cursed-redb`,
//! pure Rust) are persisted under the app's data directory.

use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use futures::StreamExt;
use tokio::runtime::Handle;
use tokio::sync::mpsc::UnboundedSender;

use nairobi_core::wallet::{
    lnaddress, Amount, LightningAddress, LightningInvoice, PaymentKind, Wallet, WalletEvent,
};

use fedimint_bip39::{Bip39RootSecretStrategy, Mnemonic};
use fedimint_client::module::ClientModuleInstance;
use fedimint_client::secret::RootSecretStrategy;
use fedimint_client::{Client, ClientHandle, RootSecret};
use fedimint_connectors::ConnectorRegistry;
use fedimint_core::core::OperationId;
use fedimint_core::db::IRawDatabaseExt;
use fedimint_core::invite_code::InviteCode;
use fedimint_core::Amount as FmAmount;
use fedimint_ln_client::{
    LightningClientInit, LightningClientModule, LnPayState, LnReceiveState,
};
use fedimint_mint_client::MintClientInit;
use fedimint_wallet_client::{WalletClientInit, WalletClientModule, WithdrawState};

/// A Fedimint e-cash wallet behind the [`Wallet`] trait.
pub struct FedimintWallet {
    rt: Handle,
    client: Arc<ClientHandle>,
    tx: UnboundedSender<WalletEvent>,
}

impl FedimintWallet {
    /// Join (or re-open) the federation named by `invite` and return a ready
    /// wallet. Blocks the calling thread on `rt` while connecting (mirrors how
    /// the app brings up the relay `SdkPool`).
    pub fn connect_blocking(
        rt: Handle,
        data_dir: PathBuf,
        invite: String,
        tx: UnboundedSender<WalletEvent>,
    ) -> Result<Self> {
        let client = connect_client(&rt, data_dir, invite)?;
        let _ = tx.send(WalletEvent::Status {
            connected: true,
            detail: "Fedimint wallet connected".into(),
        });
        let wallet = Self { rt, client, tx };
        wallet.refresh_balance();
        Ok(wallet)
    }

    /// Spawn `fut` on the runtime; nothing to await — results are sent on `tx`.
    fn spawn<F>(&self, fut: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.rt.spawn(fut);
    }
}

impl Wallet for FedimintWallet {
    fn refresh_balance(&self) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        self.spawn(async move {
            emit_balance(&client, &tx).await;
        });
    }

    fn receive_lightning(&self, amount: Amount, description: String) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        self.spawn(async move {
            match create_invoice(&client, amount, &description).await {
                Ok((bolt11, op)) => {
                    let _ = tx.send(WalletEvent::InvoiceCreated(LightningInvoice {
                        bolt11,
                        amount,
                        description,
                    }));
                    // Watch for the incoming payment so the balance updates itself.
                    watch_ln_receive(client, op, amount, tx).await;
                }
                Err(e) => {
                    let _ = tx.send(WalletEvent::PaymentFailed {
                        kind: PaymentKind::Lightning,
                        reason: format!("invoice: {e}"),
                    });
                }
            }
        });
    }

    fn receive_onchain(&self) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        self.spawn(async move {
            match deposit_address(&client).await {
                Ok(address) => {
                    let _ = tx.send(WalletEvent::DepositAddress(address));
                }
                Err(e) => {
                    let _ = tx.send(WalletEvent::Status {
                        connected: true,
                        detail: format!("deposit address: {e}"),
                    });
                }
            }
        });
    }

    fn pay_invoice(&self, bolt11: String, max_fee: Amount) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        self.spawn(async move {
            finish_payment(&client, &tx, PaymentKind::Lightning, Amount::ZERO, || {
                pay_bolt11(&client, &bolt11, max_fee)
            })
            .await;
        });
    }

    fn pay_address(&self, address: LightningAddress, amount: Amount, kind: PaymentKind) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        self.spawn(async move {
            // Resolve the LUD-16 address to a BOLT-11 invoice, then pay it.
            match lnaddress::resolve(&address, amount).await {
                Ok(bolt11) => {
                    finish_payment(&client, &tx, kind, amount, || {
                        pay_bolt11(&client, &bolt11, Amount::ZERO)
                    })
                    .await;
                }
                Err(e) => {
                    let _ = tx.send(WalletEvent::PaymentFailed {
                        kind,
                        reason: e.to_string(),
                    });
                }
            }
        });
    }

    fn pay_onchain(&self, address: String, amount: Amount) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        self.spawn(async move {
            finish_payment(&client, &tx, PaymentKind::Onchain, amount, || {
                withdraw_onchain(&client, &address, amount)
            })
            .await;
        });
    }
}

// ---- connect --------------------------------------------------------------

/// Join the federation on first run (creating the seed + DB), or re-open it on
/// subsequent runs.
fn connect_client(rt: &Handle, data_dir: PathBuf, invite: String) -> Result<Arc<ClientHandle>> {
    rt.block_on(async move {
        fs::create_dir_all(&data_dir).ok();
        let invite_code =
            InviteCode::from_str(invite.trim()).context("parse federation invite code")?;

        let mnemonic = load_or_create_mnemonic(&data_dir)?;
        let root_secret = RootSecret::StandardDoubleDerive(
            Bip39RootSecretStrategy::<12>::to_root_secret(&mnemonic),
        );

        // Pure-Rust hybrid memory/redb database — no RocksDB/C++ dependency.
        let db = fedimint_cursed_redb::MemAndRedb::new(data_dir.join("fedimint.redb"))
            .await
            .context("open wallet database")?
            .into_database();

        let connectors = ConnectorRegistry::build_from_client_defaults().bind().await?;

        let mut builder = Client::builder().await?;
        builder.with_module(LightningClientInit::default());
        builder.with_module(MintClientInit);
        builder.with_module(WalletClientInit::default());

        // A marker distinguishes "already joined" (re-open) from the first join.
        let joined = data_dir.join("fedimint.joined");
        let client = if joined.exists() {
            builder
                .open(connectors, db, root_secret)
                .await
                .context("re-open federation client")?
        } else {
            let client = builder
                .preview(connectors, &invite_code)
                .await?
                .join(db, root_secret)
                .await
                .context("join federation")?;
            fs::write(&joined, b"1").ok();
            client
        };
        Ok(Arc::new(client))
    })
}

/// Load the persisted BIP-39 mnemonic, or generate + persist a fresh one.
fn load_or_create_mnemonic(data_dir: &Path) -> Result<Mnemonic> {
    let path = data_dir.join("fedimint.seed");
    if let Ok(words) = fs::read_to_string(&path) {
        return Mnemonic::parse(words.trim()).context("parse stored mnemonic");
    }
    let mnemonic = Mnemonic::generate(12).context("generate mnemonic")?;
    fs::write(&path, mnemonic.to_string()).context("persist mnemonic")?;
    Ok(mnemonic)
}

// ---- operations -----------------------------------------------------------

fn ln(client: &ClientHandle) -> Result<ClientModuleInstance<'_, LightningClientModule>> {
    client
        .get_first_module::<LightningClientModule>()
        .context("federation has no Lightning module")
}

fn onchain(client: &ClientHandle) -> Result<ClientModuleInstance<'_, WalletClientModule>> {
    client
        .get_first_module::<WalletClientModule>()
        .context("federation has no on-chain wallet module")
}

/// Read the spendable Bitcoin e-cash balance.
async fn read_balance(client: &ClientHandle) -> Result<FmAmount> {
    client.get_balance_for_btc().await
}

async fn emit_balance(client: &ClientHandle, tx: &UnboundedSender<WalletEvent>) {
    match read_balance(client).await {
        Ok(amt) => {
            let _ = tx.send(WalletEvent::Balance(Amount::from_msats(amt.msats)));
        }
        Err(e) => log::warn!("read balance: {e}"),
    }
}

/// Create a BOLT-11 invoice for `amount`, returning `(invoice, operation id)`.
async fn create_invoice(
    client: &ClientHandle,
    amount: Amount,
    description: &str,
) -> Result<(String, OperationId)> {
    let ln = ln(client)?;
    // Make sure we know a gateway to receive over Lightning.
    let _ = ln.update_gateway_cache().await;
    let gateway = ln.get_gateway(None, false).await.ok().flatten();
    let desc = lightning_invoice::Bolt11InvoiceDescription::Direct(
        lightning_invoice::Description::new(description.to_string())
            .context("invoice description")?,
    );
    let (op, invoice, _preimage) = ln
        .create_bolt11_invoice(FmAmount::from_msats(amount.msats()), desc, None, (), gateway)
        .await?;
    Ok((invoice.to_string(), op))
}

/// Wait for an issued invoice to be paid, then announce the funds + new balance.
async fn watch_ln_receive(
    client: Arc<ClientHandle>,
    op: OperationId,
    amount: Amount,
    tx: UnboundedSender<WalletEvent>,
) {
    let Ok(ln) = ln(&client) else { return };
    let Ok(updates) = ln.subscribe_ln_receive(op).await else {
        return;
    };
    let mut stream = updates.into_stream();
    while let Some(state) = stream.next().await {
        match state {
            LnReceiveState::Claimed => {
                let _ = tx.send(WalletEvent::FundsReceived { amount });
                emit_balance(&client, &tx).await;
                break;
            }
            LnReceiveState::Canceled { .. } => break,
            _ => {}
        }
    }
}

/// Allocate an on-chain peg-in deposit address.
async fn deposit_address(client: &ClientHandle) -> Result<String> {
    let wallet = onchain(client)?;
    let (_op, address, _tweak) = wallet.safe_allocate_deposit_address(()).await?;
    Ok(address.to_string())
}

/// Pay a BOLT-11 invoice; returns the fee paid on success.
async fn pay_bolt11(client: &ClientHandle, bolt11: &str, max_fee: Amount) -> Result<Amount> {
    let invoice = lightning_invoice::Bolt11Invoice::from_str(bolt11.trim())
        .map_err(|e| anyhow!("invalid invoice: {e}"))?;
    let ln = ln(client)?;
    let _ = ln.update_gateway_cache().await;
    let payment = ln.pay_bolt11_invoice(None, invoice, ()).await?;
    let fee = Amount::from_msats(payment.fee.msats);
    if max_fee.msats() > 0 && fee.msats() > max_fee.msats() {
        log::warn!("lightning fee {fee} exceeds cap {max_fee}");
    }
    let op = payment.payment_type.operation_id();
    let updates = ln.subscribe_ln_pay(op).await?;
    let mut stream = updates.into_stream();
    while let Some(state) = stream.next().await {
        match state {
            LnPayState::Success { .. } => return Ok(fee),
            LnPayState::Refunded { .. } => return Err(anyhow!("payment refunded")),
            LnPayState::Canceled => return Err(anyhow!("payment canceled")),
            _ => {}
        }
    }
    Err(anyhow!("payment ended without a final state"))
}

/// Withdraw `amount` on-chain to `address`; returns the network fee on success.
async fn withdraw_onchain(client: &ClientHandle, address: &str, amount: Amount) -> Result<Amount> {
    let address = bitcoin::Address::from_str(address.trim())
        .map_err(|e| anyhow!("invalid address: {e}"))?
        .assume_checked();
    let wallet = onchain(client)?;
    let sats = bitcoin::Amount::from_sat(amount.sats());
    let fees = wallet.get_withdraw_fees(&address, sats).await?;
    let fee = Amount::from_sats(fees.amount().to_sat());
    let op = wallet.withdraw(&address, sats, fees, ()).await?;
    let updates = wallet.subscribe_withdraw_updates(op).await?;
    let mut stream = updates.into_stream();
    while let Some(state) = stream.next().await {
        match state {
            WithdrawState::Succeeded(_txid) => return Ok(fee),
            WithdrawState::Failed(e) => return Err(anyhow!(e)),
            _ => {}
        }
    }
    Err(anyhow!("withdrawal ended without a final state"))
}

/// Run a payment future and translate its result into the success/failure
/// [`WalletEvent`]s, refreshing the balance after a success.
async fn finish_payment<'a, F, Fut>(
    client: &'a ClientHandle,
    tx: &'a UnboundedSender<WalletEvent>,
    kind: PaymentKind,
    amount: Amount,
    pay: F,
) where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Amount>> + 'a,
{
    match pay().await {
        Ok(fees) => {
            let _ = tx.send(WalletEvent::PaymentSucceeded { kind, amount, fees });
            emit_balance(client, tx).await;
        }
        Err(e) => {
            let _ = tx.send(WalletEvent::PaymentFailed {
                kind,
                reason: e.to_string(),
            });
        }
    }
}
