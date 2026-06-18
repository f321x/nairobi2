//! The wallet boundary — a modular Bitcoin / Lightning wallet API.
//!
//! Like the relay [`crate::pool::Pool`], the app talks to its wallet only
//! through the [`Wallet`] trait, so the backend swaps freely behind one
//! interface:
//!
//! * [`MockWallet`] — in-memory, deterministic, used by tests and the desktop
//!   simulator (no network, no funds).
//! * a **Fedimint** e-cash wallet on device (the `nairobi-wallet-fedimint`
//!   crate), funded over Lightning + on-chain Bitcoin.
//! * — later — a **Nostr Wallet Connect** (NIP-47) link to a remote wallet,
//!   which would be a drop-in third implementation of this same trait.
//!
//! Every method is **fire-and-forget** (mirrors [`crate::pool::Pool`]): callers
//! never await; effects surface asynchronously as [`WalletEvent`]s on a channel
//! the backend was constructed with. That keeps the wallet decoupled from the
//! UI and lets the engine/controller fold wallet results into the same
//! snapshot-and-render loop they already use for ride state.
//!
//! `pay_invoice` is the internal API the rest of the application uses to settle
//! Lightning invoices; [`pay_mpesa`] is the M-Pesa cash-out built on top of
//! [`Wallet::pay_address`] + the `bitcoin.co.ke` Lightning-address service.

pub mod lnaddress;

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

pub use lnaddress::LightningAddress;

/// A Bitcoin amount in **millisatoshis** — the Lightning-native unit.
/// `1 sat = 1000 msat`. On-chain figures are quoted in whole sats (a fraction
/// of a sat cannot be sent on-chain).
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize,
)]
pub struct Amount {
    msats: u64,
}

impl Amount {
    /// Zero.
    pub const ZERO: Amount = Amount { msats: 0 };

    /// From a raw millisatoshi count.
    pub const fn from_msats(msats: u64) -> Self {
        Self { msats }
    }

    /// From whole satoshis (saturating at `u64::MAX` msats).
    pub const fn from_sats(sats: u64) -> Self {
        Self {
            msats: sats.saturating_mul(1000),
        }
    }

    /// The raw millisatoshi count.
    pub const fn msats(self) -> u64 {
        self.msats
    }

    /// Whole satoshis, rounded **down** (the on-chain-spendable part).
    pub const fn sats(self) -> u64 {
        self.msats / 1000
    }

    /// `self + other`, saturating.
    pub const fn saturating_add(self, other: Amount) -> Amount {
        Amount {
            msats: self.msats.saturating_add(other.msats),
        }
    }

    /// `self - other`, saturating at zero.
    pub const fn saturating_sub(self, other: Amount) -> Amount {
        Amount {
            msats: self.msats.saturating_sub(other.msats),
        }
    }

    /// `self - other`, or `None` if it would go negative (a spend check).
    pub const fn checked_sub(self, other: Amount) -> Option<Amount> {
        match self.msats.checked_sub(other.msats) {
            Some(msats) => Some(Amount { msats }),
            None => None,
        }
    }
}

impl std::fmt::Display for Amount {
    /// Human form, in sats (the unit shown to users): `"1234 sat"`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} sat", self.sats())
    }
}

/// A BOLT-11 invoice that was created to **receive** funds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LightningInvoice {
    /// The `lnbc…` payment request string (what the payer scans/pastes).
    pub bolt11: String,
    /// The amount the invoice asks for.
    pub amount: Amount,
    /// The human description embedded in the invoice.
    pub description: String,
}

/// What a payment was *for* — tags [`WalletEvent`]s so the UI can phrase the
/// outcome ("M-Pesa payout sent" vs "Lightning payment sent").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaymentKind {
    /// A plain BOLT-11 Lightning invoice.
    Lightning,
    /// A LUD-16 Lightning address (`user@domain`).
    LightningAddress,
    /// An M-Pesa cash-out via `<phone>@bitcoin.co.ke`.
    Mpesa,
    /// An on-chain Bitcoin withdrawal.
    Onchain,
}

impl PaymentKind {
    /// A short, user-facing label.
    pub fn label(self) -> &'static str {
        match self {
            PaymentKind::Lightning => "Lightning payment",
            PaymentKind::LightningAddress => "Lightning address payment",
            PaymentKind::Mpesa => "M-Pesa payout",
            PaymentKind::Onchain => "on-chain withdrawal",
        }
    }
}

/// Effects the wallet pushes back toward the app (mirrors
/// [`crate::pool::PoolEvent`]). All asynchronous results of the fire-and-forget
/// [`Wallet`] calls arrive here.
#[derive(Clone, Debug, PartialEq)]
pub enum WalletEvent {
    /// The current spendable balance (answer to [`Wallet::refresh_balance`], or
    /// pushed spontaneously when it changes).
    Balance(Amount),
    /// A BOLT-11 invoice was created to receive over Lightning.
    InvoiceCreated(LightningInvoice),
    /// An on-chain deposit address was allocated.
    DepositAddress(String),
    /// A payment we initiated **succeeded**. `fees` is what the backend charged
    /// on top of `amount`.
    PaymentSucceeded {
        kind: PaymentKind,
        amount: Amount,
        fees: Amount,
    },
    /// A payment we initiated **failed**; `reason` is human-readable.
    PaymentFailed { kind: PaymentKind, reason: String },
    /// Funds arrived — an invoice we issued was paid, or a deposit confirmed.
    FundsReceived { amount: Amount },
    /// Backend connectivity changed (e.g. the federation connected/disconnected,
    /// or no wallet is configured yet).
    Status { connected: bool, detail: String },
}

/// The wallet abstraction the app depends on. **Fire-and-forget**: callers never
/// await; effects surface later as [`WalletEvent`]s on the channel the
/// implementation was built with.
///
/// Backends: [`MockWallet`] (here), `nairobi_wallet_fedimint::FedimintWallet`,
/// and a future NWC client — all interchangeable behind this trait.
pub trait Wallet: Send + Sync + 'static {
    /// Request the current balance → [`WalletEvent::Balance`].
    fn refresh_balance(&self);

    /// Create a BOLT-11 invoice for `amount` to **receive** over Lightning →
    /// [`WalletEvent::InvoiceCreated`].
    fn receive_lightning(&self, amount: Amount, description: String);

    /// Allocate an on-chain Bitcoin deposit address to **receive** →
    /// [`WalletEvent::DepositAddress`].
    fn receive_onchain(&self);

    /// **Pay a BOLT-11 invoice.** This is the internal API the rest of the app
    /// uses to settle Lightning invoices. `max_fee` caps the routing/gateway
    /// fee. Resolves to [`WalletEvent::PaymentSucceeded`] /
    /// [`WalletEvent::PaymentFailed`] with [`PaymentKind::Lightning`].
    fn pay_invoice(&self, bolt11: String, max_fee: Amount);

    /// Resolve a LUD-16 Lightning `address` to an invoice for `amount` and pay
    /// it. `kind` tags the resulting event (e.g. [`PaymentKind::Mpesa`] vs
    /// [`PaymentKind::LightningAddress`]).
    fn pay_address(&self, address: LightningAddress, amount: Amount, kind: PaymentKind);

    /// Withdraw `amount` on-chain to Bitcoin `address` →
    /// [`WalletEvent::PaymentSucceeded`] / [`PaymentFailed`] with
    /// [`PaymentKind::Onchain`].
    fn pay_onchain(&self, address: String, amount: Amount);
}

/// Pay an **M-Pesa** cash-out: build the `<phone>@bitcoin.co.ke` Lightning
/// address and pay it `amount`. bitcoin.co.ke converts the received sats to KES
/// at its rate and sends them to the phone number's M-Pesa account.
///
/// A thin, backend-agnostic helper over [`Wallet::pay_address`] so the M-Pesa
/// path is identical across every wallet implementation. Fails only if the
/// phone number can't be normalized into an address.
pub fn pay_mpesa(wallet: &dyn Wallet, phone: &str, amount: Amount) -> crate::Result<()> {
    let address = lnaddress::mpesa_address(phone)?;
    wallet.pay_address(address, amount, PaymentKind::Mpesa);
    Ok(())
}

// ---- MockWallet (tests + desktop simulator) -------------------------------

/// One payment the [`MockWallet`] was asked to make (for test assertions).
#[derive(Clone, Debug, PartialEq)]
pub struct MockPayment {
    pub kind: PaymentKind,
    /// The destination as a string: a BOLT-11 invoice, a Lightning address, or
    /// an on-chain address.
    pub destination: String,
    pub amount: Amount,
}

struct MockInner {
    balance: Amount,
    invoices: Vec<LightningInvoice>,
    payments: Vec<MockPayment>,
    deposits: u32,
}

/// In-memory [`Wallet`] for host tests and the desktop simulator: tracks a
/// balance, records every request, and emits believable [`WalletEvent`]s — with
/// no network and no real funds. Address-based and on-chain spends debit the
/// balance (so the M-Pesa flow is exercised end-to-end); a bare `pay_invoice`
/// succeeds without a debit since a mock can't read the invoice's amount.
pub struct MockWallet {
    tx: UnboundedSender<WalletEvent>,
    inner: Mutex<MockInner>,
}

impl MockWallet {
    /// An empty mock that emits on `tx`.
    pub fn new(tx: UnboundedSender<WalletEvent>) -> Self {
        Self::with_balance(tx, Amount::ZERO)
    }

    /// A mock pre-funded with `balance`.
    pub fn with_balance(tx: UnboundedSender<WalletEvent>, balance: Amount) -> Self {
        Self {
            tx,
            inner: Mutex::new(MockInner {
                balance,
                invoices: Vec::new(),
                payments: Vec::new(),
                deposits: 0,
            }),
        }
    }

    fn emit(&self, ev: WalletEvent) {
        let _ = self.tx.send(ev);
    }

    /// Current balance (for assertions / the sim).
    pub fn balance(&self) -> Amount {
        self.inner.lock().unwrap().balance
    }

    /// Every payment requested so far (in order).
    pub fn payments(&self) -> Vec<MockPayment> {
        self.inner.lock().unwrap().payments.clone()
    }

    /// Every invoice created so far.
    pub fn invoices(&self) -> Vec<LightningInvoice> {
        self.inner.lock().unwrap().invoices.clone()
    }

    /// Simulate incoming funds (a paid invoice / confirmed deposit): credit the
    /// balance and emit [`WalletEvent::FundsReceived`] + [`WalletEvent::Balance`].
    pub fn credit(&self, amount: Amount) {
        let balance = {
            let mut g = self.inner.lock().unwrap();
            g.balance = g.balance.saturating_add(amount);
            g.balance
        };
        self.emit(WalletEvent::FundsReceived { amount });
        self.emit(WalletEvent::Balance(balance));
    }

    /// Shared debit path for the address / on-chain spends: checks funds, debits
    /// and emits success (+ a fresh balance) or an insufficient-funds failure.
    fn spend(&self, kind: PaymentKind, destination: String, amount: Amount) {
        let result = {
            let mut g = self.inner.lock().unwrap();
            match g.balance.checked_sub(amount) {
                Some(new_balance) => {
                    g.balance = new_balance;
                    g.payments.push(MockPayment {
                        kind,
                        destination,
                        amount,
                    });
                    Ok(new_balance)
                }
                None => Err(()),
            }
        };
        match result {
            Ok(balance) => {
                self.emit(WalletEvent::PaymentSucceeded {
                    kind,
                    amount,
                    fees: Amount::ZERO,
                });
                self.emit(WalletEvent::Balance(balance));
            }
            Err(()) => self.emit(WalletEvent::PaymentFailed {
                kind,
                reason: "insufficient funds".into(),
            }),
        }
    }
}

impl Wallet for MockWallet {
    fn refresh_balance(&self) {
        self.emit(WalletEvent::Balance(self.balance()));
    }

    fn receive_lightning(&self, amount: Amount, description: String) {
        let invoice = LightningInvoice {
            // A clearly-fake but well-shaped invoice string.
            bolt11: format!("lnbcmock{}n1{}", amount.msats(), description.len()),
            amount,
            description,
        };
        self.inner.lock().unwrap().invoices.push(invoice.clone());
        self.emit(WalletEvent::InvoiceCreated(invoice));
    }

    fn receive_onchain(&self) {
        let n = {
            let mut g = self.inner.lock().unwrap();
            g.deposits += 1;
            g.deposits
        };
        self.emit(WalletEvent::DepositAddress(format!("bcrt1qmockdeposit{n:08}")));
    }

    fn pay_invoice(&self, bolt11: String, _max_fee: Amount) {
        // A mock can't read the invoice's amount, so it records the request and
        // reports success without debiting.
        self.inner.lock().unwrap().payments.push(MockPayment {
            kind: PaymentKind::Lightning,
            destination: bolt11,
            amount: Amount::ZERO,
        });
        self.emit(WalletEvent::PaymentSucceeded {
            kind: PaymentKind::Lightning,
            amount: Amount::ZERO,
            fees: Amount::ZERO,
        });
    }

    fn pay_address(&self, address: LightningAddress, amount: Amount, kind: PaymentKind) {
        self.spend(kind, address.to_string(), amount);
    }

    fn pay_onchain(&self, address: String, amount: Amount) {
        self.spend(PaymentKind::Onchain, address, amount);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

    fn drain(rx: &mut UnboundedReceiver<WalletEvent>) -> Vec<WalletEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    #[test]
    fn amount_units_and_arithmetic() {
        assert_eq!(Amount::from_sats(1).msats(), 1000);
        assert_eq!(Amount::from_msats(2500).sats(), 2); // floored
        assert_eq!(Amount::from_sats(3).to_string(), "3 sat");
        assert_eq!(
            Amount::from_sats(5).saturating_sub(Amount::from_sats(2)),
            Amount::from_sats(3)
        );
        assert_eq!(
            Amount::from_sats(2).saturating_sub(Amount::from_sats(5)),
            Amount::ZERO
        );
        assert_eq!(Amount::from_sats(2).checked_sub(Amount::from_sats(5)), None);
    }

    #[test]
    fn refresh_balance_emits_balance() {
        let (tx, mut rx) = unbounded_channel();
        let w = MockWallet::with_balance(tx, Amount::from_sats(100));
        w.refresh_balance();
        assert_eq!(drain(&mut rx), vec![WalletEvent::Balance(Amount::from_sats(100))]);
    }

    #[test]
    fn receive_lightning_creates_invoice() {
        let (tx, mut rx) = unbounded_channel();
        let w = MockWallet::new(tx);
        w.receive_lightning(Amount::from_sats(500), "ride".into());
        let evs = drain(&mut rx);
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            WalletEvent::InvoiceCreated(inv) => {
                assert_eq!(inv.amount, Amount::from_sats(500));
                assert_eq!(inv.description, "ride");
                assert!(inv.bolt11.starts_with("lnbc"));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(w.invoices().len(), 1);
    }

    #[test]
    fn receive_onchain_yields_address() {
        let (tx, mut rx) = unbounded_channel();
        let w = MockWallet::new(tx);
        w.receive_onchain();
        match &drain(&mut rx)[0] {
            WalletEvent::DepositAddress(a) => assert!(a.starts_with("bcrt1q")),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn pay_address_debits_and_succeeds() {
        let (tx, mut rx) = unbounded_channel();
        let w = MockWallet::with_balance(tx, Amount::from_sats(1000));
        let addr: LightningAddress = "alice@example.com".parse().unwrap();
        w.pay_address(addr, Amount::from_sats(300), PaymentKind::LightningAddress);

        assert_eq!(w.balance(), Amount::from_sats(700));
        let evs = drain(&mut rx);
        assert!(evs.contains(&WalletEvent::PaymentSucceeded {
            kind: PaymentKind::LightningAddress,
            amount: Amount::from_sats(300),
            fees: Amount::ZERO,
        }));
        assert!(evs.contains(&WalletEvent::Balance(Amount::from_sats(700))));
        assert_eq!(w.payments()[0].destination, "alice@example.com");
    }

    #[test]
    fn pay_mpesa_targets_bitcoin_co_ke_and_debits() {
        let (tx, mut rx) = unbounded_channel();
        let w = MockWallet::with_balance(tx, Amount::from_sats(2000));
        // 0712 345 678 → 254712345678@bitcoin.co.ke
        pay_mpesa(&w, "0712 345 678", Amount::from_sats(800)).unwrap();

        assert_eq!(w.balance(), Amount::from_sats(1200));
        let pay = &w.payments()[0];
        assert_eq!(pay.kind, PaymentKind::Mpesa);
        assert_eq!(pay.destination, "254712345678@bitcoin.co.ke");
        assert_eq!(pay.amount, Amount::from_sats(800));
        assert!(drain(&mut rx).iter().any(|e| matches!(
            e,
            WalletEvent::PaymentSucceeded { kind: PaymentKind::Mpesa, .. }
        )));
    }

    #[test]
    fn spend_over_balance_fails_without_debit() {
        let (tx, mut rx) = unbounded_channel();
        let w = MockWallet::with_balance(tx, Amount::from_sats(100));
        w.pay_onchain("bc1qexample".into(), Amount::from_sats(500));
        assert_eq!(w.balance(), Amount::from_sats(100)); // unchanged
        assert!(drain(&mut rx).iter().any(|e| matches!(
            e,
            WalletEvent::PaymentFailed { kind: PaymentKind::Onchain, .. }
        )));
        assert!(w.payments().is_empty());
    }

    #[test]
    fn credit_increases_balance_and_notifies() {
        let (tx, mut rx) = unbounded_channel();
        let w = MockWallet::new(tx);
        w.credit(Amount::from_sats(1500));
        assert_eq!(w.balance(), Amount::from_sats(1500));
        let evs = drain(&mut rx);
        assert!(evs.contains(&WalletEvent::FundsReceived {
            amount: Amount::from_sats(1500)
        }));
        assert!(evs.contains(&WalletEvent::Balance(Amount::from_sats(1500))));
    }
}
