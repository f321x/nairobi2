//! Controller: bridges the Slint UI, the core [`Engine`] and the [`Platform`].
//!
//! Threading model (mirrors ntrack's `controller.rs`):
//! * Slint callbacks run on the UI thread and call [`Controller`] methods,
//!   which send [`EngineCmd`]s. They never mutate engine state directly.
//! * The engine runs inside a private tokio runtime; its [`UiEvent`]s are
//!   folded into a [`ViewState`] on a worker thread, then re-rendered onto the
//!   UI thread via `Weak::upgrade_in_event_loop`. [`Controller::render`] is
//!   idempotent and UI-thread-only.
//! * A 1 s UI timer drives `EngineCmd::Tick { now }` with the real clock (the
//!   auction escalates on ticks) and expires toasts.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nairobi_core::burn::electrum::ElectrumServer;
use nairobi_core::burn::notary::NotaryClient;
use nairobi_core::burn::service::{BurnService, NotaryBurnService};
use nairobi_core::config::{ConfigStore, DEFAULT_CURRENCY};
use nairobi_core::engine::{
    DriverPhase, DriverSnapshot, Engine, EngineCmd, Notarization, Offer, PassengerPhase,
    PassengerSnapshot, SortKey, UiEvent,
};
use nairobi_core::geo::routing;
use nairobi_core::geo::LatLng;
use nairobi_core::keys;
use nairobi_core::pool::{MockPool, Pool, SdkPool};
use nairobi_core::wallet::{Amount, MockWallet, Wallet, WalletEvent};
use nostr_sdk::prelude::{Event, Filter, Keys, PublicKey};
use slint::Weak;
use tokio::sync::mpsc;

use crate::map::{self, MapState};
use crate::platform::{Platform, PlatformEvent};
use crate::{ChatItem, MainWindow, NotarizationItem, OfferItem};

const TOAST_DURATION: Duration = Duration::from_secs(3);
/// Steps the rate steppers move by, per tap (whole currency units / km).
const RATE_STEP: u32 = 5;

/// A single concrete [`Pool`] the engine is monomorphized over: the real
/// `SdkPool` in production, falling back to a `MockPool` if relays can't be
/// reached. Keeping this an enum (rather than `Arc<dyn Pool>`) lets us pass an
/// `Arc<AppPool>` to `Engine::new`, which wants an `Arc<P: Pool>`.
enum AppPool {
    Sdk(Arc<SdkPool>),
    Mock(MockPool),
}

impl Pool for AppPool {
    fn publish(&self, event: Event) {
        match self {
            AppPool::Sdk(p) => p.publish(event),
            AppPool::Mock(p) => p.publish(event),
        }
    }
    fn subscribe(&self, filters: Vec<Filter>) {
        match self {
            AppPool::Sdk(p) => p.subscribe(filters),
            AppPool::Mock(p) => p.subscribe(filters),
        }
    }
    fn send_dm(&self, recipient: PublicKey, message: String) {
        match self {
            AppPool::Sdk(p) => p.send_dm(recipient, message),
            AppPool::Mock(p) => p.send_dm(recipient, message),
        }
    }
    fn set_relays(&self, relays: Vec<String>) {
        match self {
            AppPool::Sdk(p) => p.set_relays(relays),
            AppPool::Mock(p) => p.set_relays(relays),
        }
    }
}

/// Which top-level screen the UI shows. Index-aligned with the `screen` property
/// in `app.slint`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Home = 0,
    Request = 1,
    Waiting = 2,
    DriverList = 3,
    Trip = 4,
    Chat = 5,
    Settings = 6,
    Wallet = 7,
}

#[derive(Default)]
struct RateSetup {
    start: u32,
    max: u32,
}

/// The wallet screen's view state, folded from [`WalletEvent`]s.
#[derive(Default)]
struct WalletView {
    /// Whether a *real* wallet backend is connected (a Fedimint federation).
    /// `false` for the in-memory fallback — the wallet screen uses this to stay
    /// honest (it shows "configure a federation" guidance instead of a mock
    /// balance and fake send/receive controls).
    connected: bool,
    /// Latest spendable balance.
    balance: Amount,
    /// A one-line status / last-result message.
    status: String,
    /// The most recent Lightning invoice created to receive (shown to copy).
    invoice: String,
    /// The most recent on-chain deposit address (shown to copy).
    deposit_address: String,
    /// A send/receive is in flight.
    busy: bool,
}

struct ViewState {
    screen_i: i32,
    mode_passenger: bool, // true=passenger, false=driver (only meaningful when not Idle)
    in_chat: bool,        // Chat is an overlay over Trip; remember to return

    // pickup/dropoff the passenger is composing on the Request screen.
    pickup: Option<LatLng>,
    dropoff: Option<LatLng>,
    distance_km: Option<f64>,
    rate: RateSetup,
    currency: String,

    // latest snapshots
    passenger: Option<PassengerSnapshot>,
    driver: Option<DriverSnapshot>,

    // our own latest GPS fix
    location: Option<LatLng>,

    relays: Vec<String>,
    npub: String,
    /// The configured Fedimint federation invite (empty = none yet).
    federation_invite: String,
    /// Proof-of-burn notarizations we've initiated (most-recent first), shown in
    /// Settings with a tap-to-open-on-mempool.space action.
    notarizations: Vec<Notarization>,
    /// Our total confirmed-burn reputation (sats), shown above the list.
    reputation_sats: u64,

    /// Driver's current sort (the snapshot doesn't carry it; we mirror it from
    /// the SetSort we send so the toggle highlights correctly).
    sort: SortKey,

    toast: Option<(String, Instant)>,

    map: MapState,

    wallet: WalletView,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            screen_i: Screen::Home as i32,
            mode_passenger: true,
            in_chat: false,
            pickup: None,
            dropoff: None,
            distance_km: None,
            rate: RateSetup::default(),
            currency: String::new(),
            passenger: None,
            driver: None,
            location: None,
            relays: Vec::new(),
            npub: String::new(),
            federation_invite: String::new(),
            notarizations: Vec::new(),
            reputation_sats: 0,
            sort: SortKey::PickupDistance,
            toast: None,
            map: MapState::default(),
            wallet: WalletView::default(),
        }
    }
}

pub struct Controller {
    /// A *handle* to the engine's tokio runtime, never the runtime itself: the
    /// `Controller` is shared (`Arc`) and several of its tasks hold a clone, so
    /// owning the `Runtime` could drop it from a worker thread (a panic). The
    /// runtime is owned and torn down by [`crate::run_app`].
    rt: tokio::runtime::Handle,
    cmd_tx: mpsc::UnboundedSender<EngineCmd>,
    /// The modular wallet (Bitcoin / Lightning). A `MockWallet` by default; the
    /// real Fedimint backend (`nairobi-wallet-fedimint`) is swapped in behind the
    /// `fedimint` feature. The rest of the app pays Lightning invoices through
    /// this handle, and it can later be replaced by a Nostr Wallet Connect
    /// client without touching any caller.
    wallet: Arc<dyn Wallet>,
    /// Persists settings (the Fedimint federation invite) back to `config.json`.
    store: ConfigStore,
    platform: Arc<dyn Platform>,
    ui: Weak<MainWindow>,
    view: Arc<Mutex<ViewState>>,
}

impl Controller {
    pub fn new(
        rt: tokio::runtime::Handle,
        data_dir: PathBuf,
        platform: Arc<dyn Platform>,
        ui: Weak<MainWindow>,
    ) -> Arc<Self> {
        let (pool_tx, mut pool_rx) = mpsc::unbounded_channel();
        let (ui_tx, ui_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        // Load (or generate) the persisted identity + settings.
        let store = ConfigStore::new(&data_dir);
        let mut config = store.load().unwrap_or_default();
        let keys: Keys = match config.identity() {
            Ok(k) => {
                // Persist a freshly generated key so it survives a restart.
                if let Err(e) = store.save(&config) {
                    log::warn!("save config: {e}");
                }
                k
            }
            Err(e) => {
                log::error!("identity load failed ({e}); using an ephemeral key");
                keys::generate()
            }
        };
        let npub = keys::npub(&keys.public_key()).unwrap_or_default();
        let relays = config.relays.clone();
        let currency = if config.currency.is_empty() {
            DEFAULT_CURRENCY.to_string()
        } else {
            config.currency.clone()
        };

        // Build the relay pool on the runtime: prefer the real SdkPool (talks to
        // live relays); only fall back to the MockPool if connect fails so the
        // app still runs (e.g. offline desktop dev). The engine is generic over
        // `P: Pool` and takes an `Arc<P>`, so we keep it monomorphic by wrapping
        // the two implementations in a single [`AppPool`] enum rather than a
        // trait object (which would need `Arc<Arc<dyn Pool>>`).
        let pool: Arc<AppPool> = {
            let keys = keys.clone();
            let relays = relays.clone();
            let pool_tx = pool_tx.clone();
            rt.block_on(async move {
                match SdkPool::connect(keys, &relays, pool_tx).await {
                    Ok(p) => Arc::new(AppPool::Sdk(p)),
                    Err(e) => {
                        log::error!("SdkPool::connect failed ({e}); using MockPool");
                        Arc::new(AppPool::Mock(MockPool::new()))
                    }
                }
            })
        };

        // Build the modular wallet first (the proof-of-burn service pays notary
        // invoices with it). Mock by default (deterministic, no funds); the real
        // Fedimint backend is swapped in behind the `fedimint` feature. Its
        // `WalletEvent`s are folded into the view by a forwarder spawned below.
        let (wallet_tx, wallet_rx) = mpsc::unbounded_channel();
        let wallet = build_wallet(wallet_tx, data_dir, config.federation_invite.clone(), &rt);

        // Proof-of-burn anti-sybil service: the notary (paid over Lightning via
        // the wallet) + client-side Electrum verification. Results flow back as
        // `EngineCmd::Burn`. Gating + per-ride burn are config-driven and default
        // to off, so this is inert until the user opts in (permissionless).
        let (burn_tx, mut burn_rx) = mpsc::unbounded_channel();
        let electrum: Vec<ElectrumServer> = config
            .electrum_servers
            .iter()
            .map(|s| ElectrumServer::parse(s))
            .collect();
        let burn: Arc<dyn BurnService> = Arc::new(NotaryBurnService::new(
            keys.clone(),
            wallet.clone(),
            NotaryClient::public(),
            electrum,
            rt.clone(),
            burn_tx,
            Amount::from_sats(50),
        ));

        // Spawn the engine, wired to proof-of-burn.
        rt.spawn(
            Engine::with_burn(
                keys,
                pool,
                ui_tx,
                burn,
                config.reputation_threshold_sats,
                config.ride_burn_sats,
            )
            .run(cmd_rx),
        );

        // pool events → engine commands
        let cmd_tx2 = cmd_tx.clone();
        rt.spawn(async move {
            while let Some(ev) = pool_rx.recv().await {
                if cmd_tx2.send(EngineCmd::Pool(ev)).is_err() {
                    break;
                }
            }
        });

        // burn outcomes → engine commands
        let cmd_tx3 = cmd_tx.clone();
        rt.spawn(async move {
            while let Some(ev) = burn_rx.recv().await {
                if cmd_tx3.send(EngineCmd::Burn(ev)).is_err() {
                    break;
                }
            }
        });

        let view = ViewState {
            screen_i: Screen::Home as i32,
            mode_passenger: true,
            currency: currency.clone(),
            rate: RateSetup { start: 30, max: 120 },
            relays,
            npub,
            federation_invite: config.federation_invite.clone().unwrap_or_default(),
            ..Default::default()
        };

        let ctrl = Arc::new(Self {
            rt,
            cmd_tx,
            wallet,
            store,
            platform,
            ui,
            view: Arc::new(Mutex::new(view)),
        });
        ctrl.clone().spawn_ui_event_loop(ui_rx);
        ctrl.clone().spawn_wallet_event_loop(wallet_rx);
        // Pull an initial balance so the wallet screen shows it immediately.
        ctrl.wallet.refresh_balance();
        ctrl
    }

    /// Feed platform events (location fixes, permission results) in.
    pub fn spawn_platform_forwarder(
        self: &Arc<Self>,
        mut rx: mpsc::UnboundedReceiver<PlatformEvent>,
    ) {
        let ctrl = self.clone();
        self.rt.spawn(async move {
            while let Some(ev) = rx.recv().await {
                ctrl.on_platform_event(ev);
            }
        });
    }

    fn on_platform_event(self: &Arc<Self>, ev: PlatformEvent) {
        match ev {
            PlatformEvent::Location(loc) => {
                self.view.lock().unwrap().location = Some(loc);
                // If the passenger hasn't set a pickup yet, seed it from GPS.
                {
                    let mut v = self.view.lock().unwrap();
                    if v.pickup.is_none() && v.screen_i == Screen::Request as i32 {
                        v.pickup = Some(loc);
                        v.map.center_on(loc.lat, loc.lng, 15);
                    }
                }
                let _ = self.cmd_tx.send(EngineCmd::Location(loc));
                self.refresh_map();
                self.schedule_render();
            }
            PlatformEvent::PermissionResult(granted) => {
                if granted {
                    self.platform.start_location(5_000);
                } else {
                    self.toast("Location permission is needed to use the app");
                }
            }
            PlatformEvent::Back => self.on_back(),
        }
    }

    /// Handle the system back gesture: navigate to the previous in-app screen
    /// instead of letting Android close the app. Only when already at Home do we
    /// actually exit. The activity always lets us consume the event, so every
    /// branch here is responsible for either moving the user or exiting.
    fn on_back(self: &Arc<Self>) {
        let screen = self.view.lock().unwrap().screen_i;
        match screen {
            s if s == Screen::Chat as i32 => {
                // Chat is an overlay over the trip; back returns to the trip.
                {
                    let mut v = self.view.lock().unwrap();
                    v.in_chat = false;
                    v.screen_i = Screen::Trip as i32;
                }
                self.render_now();
            }
            s if s == Screen::Settings as i32 || s == Screen::Wallet as i32 => {
                // Reached from Home; back just returns there (role unchanged).
                self.view.lock().unwrap().screen_i = Screen::Home as i32;
                self.render_now();
            }
            s if s == Screen::Trip as i32 => {
                // Don't abandon an in-progress ride on a stray back; consume it
                // (the app stays put) — the trip ends via COMPLETE.
            }
            s if s == Screen::Request as i32
                || s == Screen::Waiting as i32
                || s == Screen::DriverList as i32 =>
            {
                // Leaving these returns Home and takes us out of the role
                // (cancels a search / goes offline) — same as the on-screen back.
                {
                    let mut v = self.view.lock().unwrap();
                    v.screen_i = Screen::Home as i32;
                    v.passenger = None;
                    v.driver = None;
                    v.in_chat = false;
                }
                let _ = self.cmd_tx.send(EngineCmd::GoIdle);
                self.platform.stop_location();
                self.render_now();
            }
            // Already at Home (or any unknown screen): back exits the app.
            _ => self.platform.exit_app(),
        }
    }

    // ---- UI-event fold ----------------------------------------------------

    fn spawn_ui_event_loop(self: Arc<Self>, mut ui_rx: mpsc::UnboundedReceiver<UiEvent>) {
        let ctrl = self.clone();
        self.rt.spawn(async move {
            while let Some(ev) = ui_rx.recv().await {
                ctrl.on_ui_event(ev);
            }
        });
    }

    fn on_ui_event(self: &Arc<Self>, ev: UiEvent) {
        match ev {
            UiEvent::Idle => {
                let mut v = self.view.lock().unwrap();
                v.passenger = None;
                v.driver = None;
                // A completed/cancelled ride returns to Home from any in-ride
                // screen (but leaves Home/Settings/Request alone).
                let in_ride = v.screen_i == Screen::Waiting as i32
                    || v.screen_i == Screen::Trip as i32
                    || v.screen_i == Screen::Chat as i32
                    || v.screen_i == Screen::DriverList as i32;
                if in_ride {
                    v.screen_i = Screen::Home as i32;
                    v.mode_passenger = true;
                    v.in_chat = false;
                }
            }
            UiEvent::Passenger(p) => {
                let mut v = self.view.lock().unwrap();
                let phase = p.phase;
                v.passenger = Some(p);
                v.mode_passenger = true;
                // Route the screen by phase (Chat is a sticky overlay).
                if !v.in_chat {
                    v.screen_i = match phase {
                        PassengerPhase::Searching => Screen::Waiting as i32,
                        PassengerPhase::Matched => Screen::Trip as i32,
                        _ => Screen::Home as i32,
                    };
                }
            }
            UiEvent::Driver(d) => {
                let mut v = self.view.lock().unwrap();
                let phase = d.phase;
                v.driver = Some(d);
                v.mode_passenger = false;
                if !v.in_chat {
                    v.screen_i = match phase {
                        DriverPhase::Browsing | DriverPhase::AwaitingConfirm | DriverPhase::Lost => {
                            Screen::DriverList as i32
                        }
                        DriverPhase::Trip => Screen::Trip as i32,
                        DriverPhase::Completed => Screen::Home as i32,
                    };
                }
            }
            UiEvent::NeedLocation(on) => {
                if on {
                    if self.platform.has_location_permission() {
                        self.platform.start_location(5_000);
                    } else {
                        self.platform.request_location_permission();
                    }
                } else {
                    self.platform.stop_location();
                }
                return; // no view-state change worth rendering by itself
            }
            UiEvent::Toast(msg) => {
                self.view.lock().unwrap().toast = Some((msg, Instant::now() + TOAST_DURATION));
            }
            UiEvent::Notarizations {
                items,
                reputation_sats,
            } => {
                let mut v = self.view.lock().unwrap();
                v.notarizations = items;
                v.reputation_sats = reputation_sats;
            }
        }
        self.refresh_map();
        self.schedule_render();
    }

    // ---- wallet -----------------------------------------------------------

    fn spawn_wallet_event_loop(
        self: Arc<Self>,
        mut wallet_rx: mpsc::UnboundedReceiver<WalletEvent>,
    ) {
        let ctrl = self.clone();
        self.rt.spawn(async move {
            while let Some(ev) = wallet_rx.recv().await {
                ctrl.on_wallet_event(ev);
            }
        });
    }

    /// Fold a [`WalletEvent`] into the wallet view, then re-render.
    fn on_wallet_event(self: &Arc<Self>, ev: WalletEvent) {
        // A completed payment or received funds changes the balance.
        let refresh = matches!(
            ev,
            WalletEvent::PaymentSucceeded { .. } | WalletEvent::FundsReceived { .. }
        );
        {
            let mut v = self.view.lock().unwrap();
            let w = &mut v.wallet;
            match ev {
                WalletEvent::Balance(a) => w.balance = a,
                WalletEvent::InvoiceCreated(inv) => {
                    w.busy = false;
                    w.invoice = inv.bolt11;
                    w.status = format!("⚡ invoice for {} ready — share it to get paid", inv.amount);
                }
                WalletEvent::DepositAddress(addr) => {
                    w.busy = false;
                    w.deposit_address = addr;
                    w.status = "🔗 on-chain deposit address ready".into();
                }
                WalletEvent::PaymentSucceeded { kind, fees, .. } => {
                    w.busy = false;
                    w.status = if fees.msats() > 0 {
                        format!("✓ {} sent (fee {})", kind.label(), fees)
                    } else {
                        format!("✓ {} sent", kind.label())
                    };
                }
                WalletEvent::PaymentFailed { kind, reason } => {
                    w.busy = false;
                    w.status = format!("✗ {} failed: {reason}", kind.label());
                }
                WalletEvent::FundsReceived { amount } => {
                    w.status = format!("✓ received {amount}");
                }
                WalletEvent::Status { connected, detail } => {
                    w.connected = connected;
                    w.status = detail;
                }
            }
        }
        if refresh {
            self.wallet.refresh_balance();
        }
        self.schedule_render();
    }

    /// Mark the wallet busy with a status line and render immediately.
    fn set_wallet_busy(self: &Arc<Self>, msg: &str) {
        {
            let mut v = self.view.lock().unwrap();
            v.wallet.busy = true;
            v.wallet.status = msg.to_string();
        }
        self.render_now();
    }

    /// Pay a BOLT-11 invoice (the app-facing Lightning-payment API).
    fn wallet_pay_invoice(self: &Arc<Self>, bolt11: String) {
        let bolt11 = bolt11.trim().to_string();
        if bolt11.is_empty() {
            self.toast("Paste a Lightning invoice first");
            return;
        }
        self.set_wallet_busy("Paying Lightning invoice…");
        // Advisory routing-fee cap; backends that don't support it ignore it.
        self.wallet.pay_invoice(bolt11, Amount::from_sats(50));
    }

    /// Cash out `sats` to a phone number's M-Pesa wallet via `<phone>@bitcoin.co.ke`.
    fn wallet_pay_mpesa(self: &Arc<Self>, phone: String, sats: u64) {
        self.set_wallet_busy("Sending M-Pesa payout…");
        if let Err(e) =
            nairobi_core::wallet::pay_mpesa(self.wallet.as_ref(), &phone, Amount::from_sats(sats))
        {
            self.view.lock().unwrap().wallet.busy = false;
            self.toast(&format!("M-Pesa: {e}"));
            self.render_now();
        }
    }

    /// Withdraw `sats` on-chain to a Bitcoin `address`.
    fn wallet_send_onchain(self: &Arc<Self>, address: String, sats: u64) {
        let address = address.trim().to_string();
        if address.is_empty() {
            self.toast("Enter a Bitcoin address");
            return;
        }
        self.set_wallet_busy("Sending on-chain…");
        self.wallet.pay_onchain(address, Amount::from_sats(sats));
    }

    fn toast(self: &Arc<Self>, msg: &str) {
        self.view.lock().unwrap().toast = Some((msg.to_string(), Instant::now() + TOAST_DURATION));
        self.schedule_render();
    }

    fn schedule_render(self: &Arc<Self>) {
        let ctrl = self.clone();
        let _ = self.ui.upgrade_in_event_loop(move |ui| ctrl.render(&ui));
    }

    /// Render synchronously when already on the UI thread; otherwise post.
    fn render_now(self: &Arc<Self>) {
        if let Some(ui) = self.ui.upgrade() {
            self.render(&ui);
        } else {
            self.schedule_render();
        }
    }

    // ---- map --------------------------------------------------------------

    /// Mark visible-but-missing tiles as loading, spawn their fetches.
    fn refresh_map(self: &Arc<Self>) {
        let to_fetch = self.view.lock().unwrap().map.missing_tiles();
        for id in to_fetch {
            self.spawn_tile_fetch(id);
        }
    }

    fn spawn_tile_fetch(self: &Arc<Self>, id: map::TileId) {
        let ctrl = self.clone();
        self.rt.spawn(async move {
            let slot = match map::fetch_tile(id).await {
                Some(buf) => map::TileSlot::Loaded(buf),
                None => map::TileSlot::Failed,
            };
            ctrl.view.lock().unwrap().map.insert(id, slot);
            ctrl.schedule_render();
        });
    }

    // ---- the 1 s tick -----------------------------------------------------

    /// Send a `Tick` with the real unix time (the auction escalates on ticks)
    /// and expire toasts. Called once a second by the UI timer.
    pub fn tick(self: &Arc<Self>) {
        let now = unix_now();
        let _ = self.cmd_tx.send(EngineCmd::Tick { now });
        // The live ride timers (rate countdown, elapsed clock) are repainted by
        // the per-tick passenger snapshot the engine emits, so we only need a
        // render here to expire a pending toast. Skipping the render otherwise
        // avoids re-pushing the window's properties every second, which on
        // Android resets the soft keyboard (IME) — e.g. snapping it back from
        // the digits page to letters — while the user is typing in a text field.
        let has_toast = self.view.lock().unwrap().toast.is_some();
        if has_toast {
            self.render_now();
        }
    }

    // ---- render -----------------------------------------------------------

    /// Render the whole view onto the UI. Idempotent; UI-thread-only.
    pub fn render(&self, ui: &MainWindow) {
        let mut view = self.view.lock().unwrap();

        if let Some((_, deadline)) = &view.toast {
            if Instant::now() >= *deadline {
                view.toast = None;
            }
        }
        ui.set_toast(view.toast.as_ref().map(|(m, _)| m.as_str()).unwrap_or("").into());

        ui.set_screen(view.screen_i);
        ui.set_currency(view.currency.clone().into());
        ui.set_npub(view.npub.clone().into());
        ui.set_relays(slint::ModelRc::new(slint::VecModel::from(
            view.relays
                .iter()
                .map(|r| r.clone().into())
                .collect::<Vec<slint::SharedString>>(),
        )));
        ui.set_notarizations(notarizations_model(&view.notarizations));
        ui.set_reputation(view.reputation_sats.to_string().into());

        // Copy the map view params out so the rest of render() can compute pin
        // offsets without holding a borrow of `view.map` across the UI setters.
        let (clat, clng, zoom) = (view.map.center_lat, view.map.center_lng, view.map.zoom);
        let offset = |c: LatLng| {
            let (dx, dy) =
                nairobi_core::geo::tiles::marker_offset(clat, clng, c.lat, c.lng, zoom);
            (dx as f32, dy as f32)
        };

        // ---- map image ----
        let map_w = view.map.vw.round().max(1.0) as u32;
        let map_h = view.map.vh.round().max(1.0) as u32;
        ui.set_map_image(view.map.render(map_w, map_h));

        // self dot
        match view.location {
            Some(c) => {
                let (dx, dy) = offset(c);
                ui.set_map_show_self(true);
                ui.set_map_self_dx(dx);
                ui.set_map_self_dy(dy);
            }
            None => ui.set_map_show_self(false),
        }

        // The pickup/dropoff pins and the "other" dot depend on the role.
        let mut pickup_coord = view.pickup;
        let mut dropoff_coord = view.dropoff;
        let mut other: Option<(LatLng, String)> = None;

        if view.mode_passenger {
            if let Some(p) = &view.passenger {
                if !p.currency.is_empty() {
                    ui.set_currency(p.currency.clone().into());
                }
                ui.set_waiting_rate(p.current_rate.to_string().into());
                ui.set_waiting_fare(p.fare_estimate.to_string().into());
                ui.set_waiting_elapsed(fmt_clock(p.elapsed_secs).into());
                ui.set_waiting_at_max(p.at_max);
                // Live countdown to the next rate increase (blank once at max).
                ui.set_waiting_countdown(
                    match p.secs_to_next_step {
                        Some(secs) if p.phase == PassengerPhase::Searching => {
                            format!("↑ rate rises in {secs}s")
                        }
                        _ => String::new(),
                    }
                    .into(),
                );
                ui.set_waiting_status(
                    match p.phase {
                        PassengerPhase::Searching if p.at_max => "At max rate — still searching",
                        PassengerPhase::Searching => "Searching for a driver…",
                        PassengerPhase::Matched => "Driver matched!",
                        PassengerPhase::Completed => "Trip complete",
                        PassengerPhase::Cancelled => "Cancelled",
                        PassengerPhase::Expired => "No driver found — try again",
                    }
                    .into(),
                );
                ui.set_trip_is_driver(false);
                let name = p.driver_name.clone().unwrap_or_else(|| "your driver".into());
                ui.set_trip_banner(format!("🚗 {name} is on the way").into());
                if let Some(loc) = p.driver_location {
                    other = Some((loc, name));
                }
                ui.set_messages(chat_model(&p.messages));
            }
        } else if let Some(d) = &view.driver {
            ui.set_sort_key(sort_to_i32(view.sort));
            ui.set_offers(offers_model(&d.offers));
            ui.set_driver_status(
                match d.phase {
                    DriverPhase::Browsing => "Looking for nearby ride requests…",
                    DriverPhase::AwaitingConfirm => "Waiting for the passenger to confirm…",
                    DriverPhase::Lost => "That ride was taken by another driver.",
                    DriverPhase::Trip => "On a trip.",
                    DriverPhase::Completed => "Trip complete.",
                }
                .into(),
            );
            ui.set_trip_is_driver(true);
            if let Some(t) = &d.trip {
                ui.set_trip_banner(format!("🧍 Pick up {} at the pin", t.passenger_name).into());
                // On a trip the pins show the passenger's requested pickup/dropoff.
                pickup_coord = Some(t.pickup);
                dropoff_coord = Some(t.dropoff);
            }
            if let Some(loc) = d.passenger_location {
                other = Some((loc, "passenger".to_string()));
            }
            ui.set_messages(chat_model(&d.messages));
        }

        // pickup pin
        match pickup_coord {
            Some(c) => {
                let (dx, dy) = offset(c);
                ui.set_map_show_pickup(true);
                ui.set_map_pickup_dx(dx);
                ui.set_map_pickup_dy(dy);
            }
            None => ui.set_map_show_pickup(false),
        }
        // dropoff pin
        match dropoff_coord {
            Some(c) => {
                let (dx, dy) = offset(c);
                ui.set_map_show_dropoff(true);
                ui.set_map_dropoff_dx(dx);
                ui.set_map_dropoff_dy(dy);
            }
            None => ui.set_map_show_dropoff(false),
        }
        // other dot
        match other {
            Some((c, label)) => {
                let (dx, dy) = offset(c);
                ui.set_map_show_other(true);
                ui.set_map_other_dx(dx);
                ui.set_map_other_dy(dy);
                ui.set_map_other_label(label.into());
            }
            None => ui.set_map_show_other(false),
        }

        // ---- request-screen rate setup + previews (local until RequestRide) --
        ui.set_start_rate(view.rate.start.to_string().into());
        ui.set_max_rate(view.rate.max.to_string().into());
        let dist = view.distance_km;
        ui.set_distance_preview(
            dist.map(|d| format!("{d:.1} km")).unwrap_or_else(|| "—".into()).into(),
        );
        ui.set_fare_preview(
            dist.map(|d| (view.rate.start as f64 * d).round() as u32)
                .map(|f| f.to_string())
                .unwrap_or_else(|| "—".into())
                .into(),
        );
        ui.set_can_request(view.pickup.is_some() && view.dropoff.is_some() && dist.is_some());

        // ---- wallet + federation ----
        let fed_configured = !view.federation_invite.is_empty();
        ui.set_wallet_connected(view.wallet.connected);
        ui.set_federation_configured(fed_configured);
        ui.set_wallet_balance(view.wallet.balance.sats().to_string().into());
        ui.set_wallet_status(view.wallet.status.clone().into());
        ui.set_wallet_invoice(view.wallet.invoice.clone().into());
        ui.set_wallet_deposit_address(view.wallet.deposit_address.clone().into());
        ui.set_wallet_busy(view.wallet.busy);
        let fed_status: slint::SharedString = if !fed_configured {
            "Not set — paste a federation invite".into()
        } else if view.wallet.connected {
            "Configured — wallet connected".into()
        } else {
            "Configured — connecting…".into()
        };
        ui.set_federation_status(fed_status);
    }

    // ---- UI callback wiring -----------------------------------------------

    pub fn attach(self: &Arc<Self>, ui: &MainWindow) {
        macro_rules! hook {
            ($setter:ident, |$ctrl:ident $(, $arg:ident : $ty:ty)*| $body:block) => {{
                let $ctrl = self.clone();
                ui.$setter(move |$($arg: $ty),*| $body);
            }};
        }

        // ---- home / navigation ----
        hook!(on_need_ride, |ctrl| {
            {
                let mut v = ctrl.view.lock().unwrap();
                v.mode_passenger = true;
                v.screen_i = Screen::Request as i32;
                v.pickup = v.location;
                v.dropoff = None;
                v.distance_km = None;
                if let Some(loc) = v.location {
                    v.map.center_on(loc.lat, loc.lng, 15);
                }
            }
            // We want a GPS fix for the pickup.
            if ctrl.platform.has_location_permission() {
                ctrl.platform.start_location(5_000);
            } else {
                ctrl.platform.request_location_permission();
            }
            ctrl.refresh_map();
            ctrl.render_now();
        });

        hook!(on_go_driving, |ctrl| {
            ctrl.view.lock().unwrap().mode_passenger = false;
            let _ = ctrl.cmd_tx.send(EngineCmd::GoOnline);
            ctrl.render_now();
        });

        hook!(on_back_home, |ctrl| {
            // Leaving a role tells the engine to go idle (cancels a search, takes
            // the driver offline).
            {
                let mut v = ctrl.view.lock().unwrap();
                v.screen_i = Screen::Home as i32;
                v.passenger = None;
                v.driver = None;
                v.in_chat = false;
            }
            let _ = ctrl.cmd_tx.send(EngineCmd::GoIdle);
            ctrl.platform.stop_location();
            ctrl.render_now();
        });

        hook!(on_open_settings, |ctrl| {
            ctrl.view.lock().unwrap().screen_i = Screen::Settings as i32;
            ctrl.render_now();
        });

        // ---- map gestures ----
        // Slint passes `length` *callback arguments* as `f32` logical pixels
        // (whereas `length` *properties* are set with `slint::LogicalLength`).
        hook!(on_map_tapped, |ctrl, x: f32, y: f32| {
            // On the Request screen a tap sets the drop-off.
            {
                let mut v = ctrl.view.lock().unwrap();
                if v.screen_i != Screen::Request as i32 {
                    return;
                }
                let (lat, lng) = v.map.tap_to_latlng(x as f64, y as f64);
                v.dropoff = Some(LatLng::new(lat, lng));
            }
            ctrl.recompute_route();
            ctrl.render_now();
        });
        hook!(on_map_panned, |ctrl, dx: f32, dy: f32| {
            ctrl.view.lock().unwrap().map.pan(dx as f64, dy as f64);
            ctrl.refresh_map();
            ctrl.render_now();
        });
        hook!(on_map_viewport, |ctrl, w: f32, h: f32| {
            let changed = ctrl.view.lock().unwrap().map.set_viewport(w as f64, h as f64);
            if changed {
                ctrl.refresh_map();
                ctrl.render_now();
            }
        });
        hook!(on_map_zoom_in, |ctrl| {
            ctrl.view.lock().unwrap().map.zoom_by(1);
            ctrl.refresh_map();
            ctrl.render_now();
        });
        hook!(on_map_zoom_out, |ctrl| {
            ctrl.view.lock().unwrap().map.zoom_by(-1);
            ctrl.refresh_map();
            ctrl.render_now();
        });

        // ---- request: search + rate setup ----
        hook!(on_search, |ctrl, query: slint::SharedString| {
            ctrl.search(query.to_string());
        });
        hook!(on_set_pickup_here, |ctrl| {
            ctrl.place_at_center(true);
        });
        hook!(on_set_dropoff_here, |ctrl| {
            ctrl.place_at_center(false);
        });
        hook!(on_start_rate_inc, |ctrl| {
            ctrl.adjust_rate(true, RATE_STEP as i64);
        });
        hook!(on_start_rate_dec, |ctrl| {
            ctrl.adjust_rate(true, -(RATE_STEP as i64));
        });
        hook!(on_max_rate_inc, |ctrl| {
            ctrl.adjust_rate(false, RATE_STEP as i64);
        });
        hook!(on_max_rate_dec, |ctrl| {
            ctrl.adjust_rate(false, -(RATE_STEP as i64));
        });

        hook!(on_request_ride, |ctrl| {
            ctrl.request_ride();
        });
        hook!(on_cancel_request, |ctrl| {
            let _ = ctrl.cmd_tx.send(EngineCmd::CancelRequest);
        });

        // ---- driver ----
        hook!(on_set_sort, |ctrl, key: i32| {
            let sort = i32_to_sort(key);
            ctrl.view.lock().unwrap().sort = sort;
            let _ = ctrl.cmd_tx.send(EngineCmd::SetSort(sort));
            ctrl.render_now();
        });
        hook!(on_take_ride, |ctrl, request_id: slint::SharedString| {
            let _ = ctrl.cmd_tx.send(EngineCmd::TakeRide {
                request_id: request_id.to_string(),
            });
        });

        // ---- trip / chat ----
        hook!(on_open_chat, |ctrl| {
            let mut v = ctrl.view.lock().unwrap();
            v.in_chat = true;
            v.screen_i = Screen::Chat as i32;
            drop(v);
            ctrl.render_now();
        });
        hook!(on_close_chat, |ctrl| {
            let mut v = ctrl.view.lock().unwrap();
            v.in_chat = false;
            v.screen_i = Screen::Trip as i32;
            drop(v);
            ctrl.render_now();
        });
        hook!(on_navigate, |ctrl| {
            ctrl.navigate();
        });
        hook!(on_complete, |ctrl| {
            let _ = ctrl.cmd_tx.send(EngineCmd::CompleteTrip);
        });
        hook!(on_send_dm, |ctrl, text: slint::SharedString| {
            let t = text.trim().to_string();
            if !t.is_empty() {
                let _ = ctrl.cmd_tx.send(EngineCmd::SendDm(t));
            }
        });

        // ---- settings ----
        hook!(on_add_relay, |ctrl, url: slint::SharedString| {
            let u = url.trim().to_string();
            if u.is_empty() {
                return;
            }
            {
                let mut v = ctrl.view.lock().unwrap();
                if !v.relays.contains(&u) {
                    v.relays.push(u);
                }
            }
            ctrl.push_relays();
            ctrl.render_now();
        });
        hook!(on_remove_relay, |ctrl, url: slint::SharedString| {
            let u = url.to_string();
            ctrl.view.lock().unwrap().relays.retain(|r| r != &u);
            ctrl.push_relays();
            ctrl.render_now();
        });
        hook!(on_set_federation, |ctrl, invite: slint::SharedString| {
            ctrl.set_federation(invite.trim().to_string());
        });
        hook!(on_open_notarization, |ctrl, txid: slint::SharedString| {
            let txid = txid.trim();
            if !txid.is_empty() {
                ctrl.platform
                    .open_url(&format!("https://mempool.space/tx/{txid}"));
            }
        });

        // ---- wallet ----
        hook!(on_open_wallet, |ctrl| {
            ctrl.view.lock().unwrap().screen_i = Screen::Wallet as i32;
            ctrl.wallet.refresh_balance();
            ctrl.render_now();
        });
        hook!(on_wallet_refresh, |ctrl| {
            ctrl.wallet.refresh_balance();
        });
        hook!(on_wallet_receive_lightning, |ctrl, amount: slint::SharedString| {
            match parse_sats(&amount) {
                Some(s) => {
                    ctrl.set_wallet_busy("Creating Lightning invoice…");
                    ctrl.wallet
                        .receive_lightning(Amount::from_sats(s), "nairobi top-up".into());
                }
                None => ctrl.toast("Enter an amount in sat"),
            }
        });
        hook!(on_wallet_receive_onchain, |ctrl| {
            ctrl.set_wallet_busy("Getting a deposit address…");
            ctrl.wallet.receive_onchain();
        });
        hook!(on_wallet_copy, |ctrl, text: slint::SharedString| {
            if text.is_empty() {
                return;
            }
            ctrl.platform.copy_to_clipboard(&text);
            ctrl.toast("Copied to clipboard");
        });
        hook!(on_wallet_pay_invoice, |ctrl, bolt11: slint::SharedString| {
            ctrl.wallet_pay_invoice(bolt11.to_string());
        });
        hook!(on_wallet_pay_mpesa, |ctrl, phone: slint::SharedString, amount: slint::SharedString| {
            match parse_sats(&amount) {
                Some(s) => ctrl.wallet_pay_mpesa(phone.to_string(), s),
                None => ctrl.toast("Enter an amount in sat"),
            }
        });
        hook!(on_wallet_send_onchain, |ctrl, address: slint::SharedString, amount: slint::SharedString| {
            match parse_sats(&amount) {
                Some(s) => ctrl.wallet_send_onchain(address.to_string(), s),
                None => ctrl.toast("Enter an amount in sat"),
            }
        });
    }

    // ---- actions ----------------------------------------------------------

    /// Adjust the start or max rate by `delta`, keeping `max >= start >= step`.
    fn adjust_rate(self: &Arc<Self>, is_start: bool, delta: i64) {
        {
            let mut v = self.view.lock().unwrap();
            if is_start {
                let n = (v.rate.start as i64 + delta).max(RATE_STEP as i64) as u32;
                v.rate.start = n;
                if v.rate.max < v.rate.start {
                    v.rate.max = v.rate.start;
                }
            } else {
                let n = (v.rate.max as i64 + delta).max(RATE_STEP as i64) as u32;
                v.rate.max = n.max(v.rate.start);
            }
        }
        self.render_now();
    }

    /// Place the pickup (or drop-off) at the current map centre — the spot under
    /// the fixed crosshair on the Request screen. Lets the user pinpoint a point
    /// precisely by panning rather than tapping a small target.
    fn place_at_center(self: &Arc<Self>, is_pickup: bool) {
        {
            let mut v = self.view.lock().unwrap();
            let coord = LatLng::new(v.map.center_lat, v.map.center_lng);
            if is_pickup {
                v.pickup = Some(coord);
            } else {
                v.dropoff = Some(coord);
            }
        }
        self.recompute_route();
        self.render_now();
    }

    /// Geocode `query` off-thread; on the first hit, set it as the drop-off and
    /// recompute the route.
    fn search(self: &Arc<Self>, query: String) {
        if query.trim().is_empty() {
            return;
        }
        let ctrl = self.clone();
        self.rt.spawn(async move {
            match routing::geocode(&query).await {
                Ok(places) if !places.is_empty() => {
                    let coord = places[0].coord;
                    {
                        let mut v = ctrl.view.lock().unwrap();
                        v.dropoff = Some(coord);
                        // Center between pickup and dropoff for a useful frame.
                        if let Some(pickup) = v.pickup {
                            let clat = (pickup.lat + coord.lat) / 2.0;
                            let clng = (pickup.lng + coord.lng) / 2.0;
                            v.map.center_on(clat, clng, 13);
                        } else {
                            v.map.center_on(coord.lat, coord.lng, 14);
                        }
                    }
                    ctrl.recompute_route();
                    ctrl.refresh_map();
                    ctrl.schedule_render();
                }
                Ok(_) => ctrl.toast("No place found for that search"),
                Err(e) => {
                    log::warn!("geocode failed: {e}");
                    ctrl.toast("Search failed — check your connection");
                }
            }
        });
    }

    /// Recompute the driving distance (OSRM, with a haversine fallback) between
    /// the current pickup and drop-off, off-thread.
    fn recompute_route(self: &Arc<Self>) {
        let (pickup, dropoff) = {
            let v = self.view.lock().unwrap();
            (v.pickup, v.dropoff)
        };
        let (Some(from), Some(to)) = (pickup, dropoff) else {
            return;
        };
        let ctrl = self.clone();
        self.rt.spawn(async move {
            let info = routing::route(from, to).await;
            ctrl.view.lock().unwrap().distance_km = Some(info.distance_km);
            ctrl.schedule_render();
        });
    }

    /// Post the ride request to the engine with the composed pickup/drop-off,
    /// route distance and rate setup.
    fn request_ride(self: &Arc<Self>) {
        let (pickup, dropoff, distance_km, currency, start, max) = {
            let v = self.view.lock().unwrap();
            (
                v.pickup,
                v.dropoff,
                v.distance_km,
                v.currency.clone(),
                v.rate.start,
                v.rate.max,
            )
        };
        let (Some(pickup), Some(dropoff)) = (pickup, dropoff) else {
            self.toast("Set a pickup and a drop-off first");
            return;
        };
        let distance_km = match distance_km {
            Some(d) => d,
            // No route yet: fall back to a haversine estimate so we never block.
            None => pickup.haversine_km(&dropoff) * routing::ROAD_FACTOR,
        };
        let _ = self.cmd_tx.send(EngineCmd::RequestRide {
            pickup,
            dropoff,
            distance_km,
            currency,
            start_rate: start,
            max_rate: max,
        });
    }

    /// The driver NAVIGATE hand-off: launch external navigation to the pickup
    /// (or, once the passenger is picked up, the drop-off).
    fn navigate(self: &Arc<Self>) {
        let target = {
            let v = self.view.lock().unwrap();
            v.driver
                .as_ref()
                .and_then(|d| d.trip.as_ref())
                .map(|t| (t.pickup, t.passenger_name.clone()))
        };
        if let Some((coord, label)) = target {
            self.platform.open_nav(coord.lat, coord.lng, &format!("Pickup: {label}"));
        }
    }

    /// Push the current relay list to the engine (it reconnects).
    fn push_relays(self: &Arc<Self>) {
        let relays = self.view.lock().unwrap().relays.clone();
        let _ = self.cmd_tx.send(EngineCmd::SetRelays(relays));
    }

    /// Persist the Fedimint federation invite to `config.json`. The wallet binds
    /// to a federation at startup, so this takes effect on the next launch.
    fn set_federation(self: &Arc<Self>, invite: String) {
        let mut config = self.store.load().unwrap_or_default();
        config.federation_invite = if invite.is_empty() { None } else { Some(invite.clone()) };
        // Persist the in-session relay edits at the same time (the only other
        // mutable setting), so saving here never rolls them back.
        config.relays = self.view.lock().unwrap().relays.clone();
        match self.store.save(&config) {
            Ok(()) => {
                self.view.lock().unwrap().federation_invite = invite;
                self.toast("Federation saved — restart the app to connect your wallet");
            }
            Err(e) => self.toast(&format!("Could not save federation: {e}")),
        }
        self.render_now();
    }

    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(EngineCmd::Shutdown);
        std::thread::sleep(Duration::from_millis(200));
    }
}

// ---- helpers --------------------------------------------------------------

/// Build the wallet backend: the real Fedimint wallet when the `fedimint`
/// feature is enabled and a federation invite is configured, otherwise a
/// deterministic in-memory [`MockWallet`] (used by the desktop simulator and as
/// a safe fallback). Returns a trait object so the backend is swappable — a
/// future `nairobi-wallet-nwc` would slot in here identically.
fn build_wallet(
    wallet_tx: mpsc::UnboundedSender<WalletEvent>,
    data_dir: PathBuf,
    federation_invite: Option<String>,
    rt: &tokio::runtime::Handle,
) -> Arc<dyn Wallet> {
    #[cfg(feature = "fedimint")]
    if let Some(invite) = federation_invite.clone() {
        match nairobi_wallet_fedimint::FedimintWallet::connect_blocking(
            rt.clone(),
            data_dir.clone(),
            invite,
            wallet_tx.clone(),
        ) {
            Ok(w) => {
                log::info!("fedimint wallet connected");
                return Arc::new(w);
            }
            Err(e) => log::error!("fedimint wallet init failed ({e}); using mock wallet"),
        }
    }

    // No real wallet backend: either no federation is configured, the connection
    // failed, or this build was compiled without the `fedimint` feature. Report
    // `connected: false` with an honest message so the wallet screen guides the
    // user to configure a federation instead of showing a fake balance. The
    // in-memory fallback keeps the rest of the app from panicking on a missing
    // wallet; its (zero) balance is never shown while disconnected.
    let _ = (&data_dir, rt);
    let detail = match federation_invite.as_deref() {
        Some(invite) if !invite.is_empty() => {
            "Could not connect to your Fedimint federation. Check the invite in Settings and restart."
        }
        _ => "No wallet yet. Add a Fedimint federation in Settings to fund a Bitcoin / Lightning wallet.",
    };
    let _ = wallet_tx.send(WalletEvent::Status {
        connected: false,
        detail: detail.into(),
    });
    Arc::new(MockWallet::new(wallet_tx))
}

/// Parse a positive whole-sat amount from a text field (`None` if blank/invalid).
fn parse_sats(s: &str) -> Option<u64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    t.parse::<u64>().ok().filter(|n| *n > 0)
}

/// Current unix time in whole seconds.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// "M:SS" elapsed/clock.
fn fmt_clock(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

fn sort_to_i32(k: SortKey) -> i32 {
    match k {
        SortKey::PickupDistance => 0,
        SortKey::Earnings => 1,
        SortKey::Rate => 2,
        SortKey::TripDistance => 3,
    }
}

fn i32_to_sort(i: i32) -> SortKey {
    match i {
        1 => SortKey::Earnings,
        2 => SortKey::Rate,
        3 => SortKey::TripDistance,
        _ => SortKey::PickupDistance,
    }
}

/// "1.2 km away" / "<100 m away" — a driver→pickup distance.
fn fmt_pickup_distance(km: f64) -> String {
    if !km.is_finite() {
        return "distance unknown".into();
    }
    if km < 0.1 {
        "<100 m away".into()
    } else if km < 1.0 {
        format!("{:.0} m away", km * 1000.0)
    } else {
        format!("{km:.1} km away")
    }
}

fn offers_model(offers: &[Offer]) -> slint::ModelRc<OfferItem> {
    let items: Vec<OfferItem> = offers
        .iter()
        .map(|o| OfferItem {
            request_id: o.request_id.clone().into(),
            passenger_name: o.passenger_name.clone().into(),
            pickup_distance: fmt_pickup_distance(o.pickup_distance_km).into(),
            trip_distance: format!("{:.0} km trip", o.trip_distance_km).into(),
            rate: format!("{} {}/km", o.rate, o.currency).into(),
            earnings: o.earnings.to_string().into(),
            currency: o.currency.clone().into(),
        })
        .collect();
    slint::ModelRc::new(slint::VecModel::from(items))
}

fn notarizations_model(items: &[Notarization]) -> slint::ModelRc<NotarizationItem> {
    let rows: Vec<NotarizationItem> = items
        .iter()
        .map(|n| NotarizationItem {
            txid: n.txid.clone().into(),
            txid_short: short_txid(&n.txid).into(),
            label: n.label.clone().into(),
            amount: n.amount_sats.to_string().into(),
            confirmed: n.confirmed,
        })
        .collect();
    slint::ModelRc::new(slint::VecModel::from(rows))
}

/// Abbreviate a txid for display: `"a1b2c3d4…7e8f9a0b"` (or the whole thing when
/// it's already short).
fn short_txid(txid: &str) -> String {
    if txid.len() <= 18 {
        txid.to_string()
    } else {
        format!("{}…{}", &txid[..8], &txid[txid.len() - 8..])
    }
}

fn chat_model(messages: &[nairobi_core::engine::ChatMessage]) -> slint::ModelRc<ChatItem> {
    let items: Vec<ChatItem> = messages
        .iter()
        .map(|m| ChatItem {
            from_me: m.from_me,
            text: m.text.clone().into(),
        })
        .collect();
    slint::ModelRc::new(slint::VecModel::from(items))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_formats() {
        assert_eq!(fmt_clock(0), "0:00");
        assert_eq!(fmt_clock(5), "0:05");
        assert_eq!(fmt_clock(83), "1:23");
        assert_eq!(fmt_clock(600), "10:00");
    }

    #[test]
    fn txid_is_abbreviated_for_display() {
        let txid = "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b";
        assert_eq!(short_txid(txid), "4a5e1e4b…fdeda33b");
        // Short strings pass through unchanged.
        assert_eq!(short_txid("deadbeef"), "deadbeef");
    }

    #[test]
    fn sort_round_trips() {
        for k in [
            SortKey::PickupDistance,
            SortKey::Earnings,
            SortKey::Rate,
            SortKey::TripDistance,
        ] {
            assert_eq!(i32_to_sort(sort_to_i32(k)), k);
        }
    }

    #[test]
    fn pickup_distance_buckets() {
        assert_eq!(fmt_pickup_distance(f64::INFINITY), "distance unknown");
        assert_eq!(fmt_pickup_distance(0.05), "<100 m away");
        assert_eq!(fmt_pickup_distance(0.4), "400 m away");
        assert_eq!(fmt_pickup_distance(2.5), "2.5 km away");
    }
}
