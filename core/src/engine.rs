//! The channel-driven ride engine — the single owner of all ride state.
//!
//! Like ntrack's engine, it is decoupled from UI and OS: it receives
//! [`EngineCmd`]s (UI actions, a 1 s [`EngineCmd::Tick`], GPS, and relay
//! [`PoolEvent`]s) and emits immutable [`UiEvent`] snapshots. It never holds a
//! UI handle, and the UI never reads engine state directly. The whole relay
//! layer is a generic [`Pool`], so every behaviour below is host-tested against
//! a [`MockPool`] with no network.
//!
//! Time is injected: the controller sends `Tick { now }` each second with the
//! real clock; tests send controlled timestamps for deterministic auctions.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use nostr_sdk::prelude::*;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::auction::{self, Auction};
use crate::burn::reputation::{BurnRecord, ReputationLedger};
use crate::burn::service::{BurnOutcome, BurnPurpose, BurnService, NoBurnService, NotarizeReq};
use crate::geo::LatLng;
use crate::keys;
use crate::matching::{self, Acceptance};
use crate::pool::{Pool, PoolEvent};
use crate::protocol::{self, Beacon, RideRequest, RideStatus};

/// NIP-40 expiry placed on each ride-request publish (refreshed on re-publish).
const REQUEST_EXPIRY_SECS: u64 = 90;
/// `since` lookback applied to subscriptions.
const SUBSCRIBE_WINDOW_SECS: u64 = 600;
/// Re-publish the request at least this often (to refresh the 90 s expiry even
/// once the rate has stopped climbing).
const REFRESH_SECS: u64 = 60;
/// Emit a location beacon to the counterpart every this many seconds while matched.
const BEACON_SECS: u64 = 5;
/// Buffer competing acceptances for this long before committing to the
/// deterministic winner, so the order relays deliver them in can't change who
/// wins (and an offline-then-reconnect passenger resolves identically).
const MATCH_WINDOW_SECS: u64 = 2;

// ---- Commands & snapshots --------------------------------------------------

/// How a driver sorts the offer list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    PickupDistance,
    Earnings,
    Rate,
    TripDistance,
}

/// Commands into the engine.
pub enum EngineCmd {
    // shared
    Tick { now: u64 },
    Location(LatLng),
    Pool(PoolEvent),
    SetRelays(Vec<String>),
    GoIdle,
    Shutdown,
    // passenger
    RequestRide {
        pickup: LatLng,
        dropoff: LatLng,
        distance_km: f64,
        currency: String,
        start_rate: u32,
        max_rate: u32,
    },
    CancelRequest,
    // driver
    GoOnline,
    SetSort(SortKey),
    TakeRide {
        request_id: String,
    },
    // shared, post-match
    SendDm(String),
    CompleteTrip,
    // proof-of-burn anti-sybil
    /// Publish an identity bond and burn `amount_sats` against it (L1).
    PublishBond {
        amount_sats: u64,
    },
    /// Set the minimum confirmed-burn reputation (sats) a counterparty must have
    /// to appear in the offer list. `0` disables gating (permissionless default).
    SetReputationThreshold(u64),
    /// A result from the [`BurnService`] (proof produced, or a third party's
    /// upvote verified). Forwarded by the controller like a [`PoolEvent`].
    Burn(BurnOutcome),
}

/// One chat message in a matched ride.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatMessage {
    pub from_me: bool,
    pub text: String,
    pub at: u64,
}

/// Passenger-side lifecycle phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PassengerPhase {
    Searching,
    Matched,
    Completed,
    Cancelled,
    Expired,
}

/// Immutable passenger snapshot for the UI.
#[derive(Clone, Debug)]
pub struct PassengerSnapshot {
    pub phase: PassengerPhase,
    pub current_rate: u32,
    pub fare_estimate: u32,
    pub elapsed_secs: u64,
    pub at_max: bool,
    /// Seconds until the rate steps up again while still escalating; `None`
    /// once the rate has reached its maximum (the UI hides the countdown then).
    pub secs_to_next_step: Option<u64>,
    pub currency: String,
    pub driver: Option<String>,
    pub driver_name: Option<String>,
    pub driver_location: Option<LatLng>,
    pub messages: Vec<ChatMessage>,
}

/// One nearby ride offer shown to a driver.
#[derive(Clone, Debug, PartialEq)]
pub struct Offer {
    pub request_id: String,
    pub passenger: String,
    pub passenger_name: String,
    pub pickup: LatLng,
    pub dropoff: LatLng,
    pub trip_distance_km: f64,
    /// Haversine driver→pickup; `f64::INFINITY` when the driver's GPS is unknown.
    pub pickup_distance_km: f64,
    pub rate: u32,
    pub earnings: u32,
    pub currency: String,
    /// True for the offer this driver has tapped TAKE on and is now awaiting the
    /// passenger's confirmation for. Lets the UI show "waiting…" on that card
    /// instead of an idle TAKE button (the offer stays listed until we win/lose).
    pub taken: bool,
}

/// Driver-side lifecycle phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriverPhase {
    Browsing,
    AwaitingConfirm,
    Trip,
    Lost,
    Completed,
}

/// Immutable driver snapshot for the UI.
#[derive(Clone, Debug)]
pub struct DriverSnapshot {
    pub phase: DriverPhase,
    pub offers: Vec<Offer>,
    pub trip: Option<Offer>,
    pub passenger_location: Option<LatLng>,
    pub messages: Vec<ChatMessage>,
}

/// A proof-of-burn notarization we initiated — surfaced to the UI so the user
/// can inspect the Bitcoin transaction (e.g. open it on a block explorer like
/// mempool.space).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notarization {
    /// Notarization transaction id, display hex (what `mempool.space/tx/<txid>`
    /// expects). Empty only for a not-yet-broadcast proof.
    pub txid: String,
    /// What the burn was for: `"Identity bond"`, `"Ride"`, or `"Boost"`.
    pub label: String,
    /// Sats burnt for this event (its leaf value).
    pub amount_sats: u64,
    /// Confirmed in a block (vs still in the mempool).
    pub confirmed: bool,
    /// Engine-clock unix seconds when we recorded it.
    pub at: u64,
}

/// What the engine renders to the UI.
#[derive(Clone, Debug)]
pub enum UiEvent {
    Idle,
    Passenger(PassengerSnapshot),
    Driver(DriverSnapshot),
    /// Ask the platform to start/stop GPS.
    NeedLocation(bool),
    Toast(String),
    /// Our proof-of-burn notarizations (most-recent first) plus our current
    /// total confirmed-burn reputation in sats — a transparency view the user
    /// can open on a block explorer.
    Notarizations {
        items: Vec<Notarization>,
        reputation_sats: u64,
    },
}

// ---- Internal state --------------------------------------------------------

struct PassengerState {
    t0: u64,
    last_publish: u64,
    auction: Auction,
    request: RideRequest,
    phase: PassengerPhase,
    published_ids: HashSet<String>,
    acceptances: Vec<Acceptance>,
    /// When set, the engine is collecting competing acceptances and will commit
    /// to the deterministic winner once `now` reaches this deadline.
    resolve_at: Option<u64>,
    driver: Option<PublicKey>,
    /// The winning acceptance, kept so we can build a ride-completion attestation
    /// (the per-ride burn target) on completion.
    winner_acc: Option<Acceptance>,
    driver_location: Option<LatLng>,
    last_beacon: u64,
    messages: Vec<ChatMessage>,
}

struct OfferEntry {
    event: Event,
    req: RideRequest,
}

struct DriverState {
    phase: DriverPhase,
    sort: SortKey,
    offers: BTreeMap<String, OfferEntry>, // key: passenger pubkey hex
    pending_take: Option<String>,         // passenger hex we accepted, awaiting confirm
    my_acceptance_id: Option<String>,     // our acceptance event id (completion ref)
    trip: Option<TripState>,
    last_beacon: u64,
    messages: Vec<ChatMessage>,
    passenger_location: Option<LatLng>,
}

struct TripState {
    passenger: PublicKey,
    offer: Offer,
}

enum Role {
    Idle,
    Passenger(PassengerState),
    Driver(DriverState),
}

/// The engine. Generic over the [`Pool`] so tests use a `MockPool`.
pub struct Engine<P: Pool> {
    keys: Keys,
    me: PublicKey,
    pool: Arc<P>,
    ui_tx: UnboundedSender<UiEvent>,
    now: u64,
    location: Option<LatLng>,
    role: Role,
    /// Proof-of-burn service (no-op [`NoBurnService`] unless built with
    /// [`Engine::with_burn`]).
    burn: Arc<dyn BurnService>,
    /// `true` only when a real burn backend is wired in — guards the bond /
    /// per-ride burn side effects so the default engine behaves exactly as before.
    burn_enabled: bool,
    /// Locally-computed, verified reputation per pubkey (hex).
    reputation: ReputationLedger,
    /// Minimum confirmed-burn sats to show a counterparty (`0` = gating off).
    rep_threshold: u64,
    /// Sats to burn on each completed ride (`0` = no per-ride burn).
    ride_burn_sats: u64,
    /// Notarizations we've initiated, most-recent first (surfaced to the UI).
    notarizations: Vec<Notarization>,
}

impl<P: Pool> Engine<P> {
    /// Create an idle engine with proof-of-burn disabled (the default path).
    pub fn new(keys: Keys, pool: Arc<P>, ui_tx: UnboundedSender<UiEvent>) -> Self {
        Self::build(keys, pool, ui_tx, Arc::new(NoBurnService), false, 0, 0)
    }

    /// Create an idle engine wired to a real [`BurnService`]. `rep_threshold`
    /// gates the offer list by confirmed reputation (sats); `ride_burn_sats` is
    /// the per-completed-ride burn (`0` to disable either).
    pub fn with_burn(
        keys: Keys,
        pool: Arc<P>,
        ui_tx: UnboundedSender<UiEvent>,
        burn: Arc<dyn BurnService>,
        rep_threshold: u64,
        ride_burn_sats: u64,
    ) -> Self {
        Self::build(keys, pool, ui_tx, burn, true, rep_threshold, ride_burn_sats)
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        keys: Keys,
        pool: Arc<P>,
        ui_tx: UnboundedSender<UiEvent>,
        burn: Arc<dyn BurnService>,
        burn_enabled: bool,
        rep_threshold: u64,
        ride_burn_sats: u64,
    ) -> Self {
        let me = keys.public_key();
        Self {
            keys,
            me,
            pool,
            ui_tx,
            now: 0,
            location: None,
            role: Role::Idle,
            burn,
            burn_enabled,
            reputation: ReputationLedger::new(),
            rep_threshold,
            ride_burn_sats,
            notarizations: Vec::new(),
        }
    }

    /// The locally-known confirmed-burn reputation (sats) for `pubkey_hex` — for
    /// the UI to show one's own or a counterparty's standing.
    pub fn reputation_sats(&self, pubkey_hex: &str) -> u64 {
        self.reputation.score_sats(pubkey_hex)
    }

    /// Run the engine loop until the command channel closes or `Shutdown`.
    pub async fn run(mut self, mut cmd_rx: UnboundedReceiver<EngineCmd>) {
        while let Some(cmd) = cmd_rx.recv().await {
            if matches!(cmd, EngineCmd::Shutdown) {
                break;
            }
            self.handle(cmd);
        }
    }

    /// Handle a single command. Public so tests can drive the engine directly.
    pub fn handle(&mut self, cmd: EngineCmd) {
        match cmd {
            EngineCmd::Tick { now } => {
                self.now = now;
                self.on_tick();
            }
            EngineCmd::Location(loc) => {
                self.location = Some(loc);
                self.on_location();
            }
            EngineCmd::Pool(ev) => self.on_pool(ev),
            EngineCmd::SetRelays(relays) => self.pool.set_relays(relays),
            EngineCmd::GoIdle => self.go_idle(),
            EngineCmd::Shutdown => {}
            EngineCmd::RequestRide {
                pickup,
                dropoff,
                distance_km,
                currency,
                start_rate,
                max_rate,
            } => self.request_ride(pickup, dropoff, distance_km, currency, start_rate, max_rate),
            EngineCmd::CancelRequest => self.cancel_request(),
            EngineCmd::GoOnline => self.go_online(),
            EngineCmd::SetSort(k) => self.set_sort(k),
            EngineCmd::TakeRide { request_id } => self.take_ride(&request_id),
            EngineCmd::SendDm(text) => self.send_dm(text),
            EngineCmd::CompleteTrip => self.complete_trip(),
            EngineCmd::PublishBond { amount_sats } => self.publish_bond(amount_sats),
            EngineCmd::SetReputationThreshold(t) => {
                self.rep_threshold = t;
                self.emit_current();
            }
            EngineCmd::Burn(outcome) => self.on_burn(outcome),
        }
    }

    fn emit(&self, ev: UiEvent) {
        let _ = self.ui_tx.send(ev);
    }

    // ---- Passenger ---------------------------------------------------------

    fn request_ride(
        &mut self,
        pickup: LatLng,
        dropoff: LatLng,
        distance_km: f64,
        currency: String,
        start_rate: u32,
        max_rate: u32,
    ) {
        let auction = Auction::new(start_rate, max_rate);
        let current_rate = auction.rate_at(0);
        let request = RideRequest {
            pickup,
            dropoff,
            distance_km,
            currency,
            start_rate,
            max_rate,
            current_rate,
            fare_estimate: auction::fare(current_rate, distance_km),
            status: RideStatus::Open,
            winner: None,
        };
        let mut p = PassengerState {
            t0: self.now,
            last_publish: self.now,
            auction,
            request,
            phase: PassengerPhase::Searching,
            published_ids: HashSet::new(),
            acceptances: Vec::new(),
            resolve_at: None,
            driver: None,
            winner_acc: None,
            driver_location: None,
            last_beacon: 0,
            messages: Vec::new(),
        };
        if let Some(id) = self.publish_request(&p.request) {
            p.published_ids.insert(id);
        }
        self.pool.subscribe(vec![
            protocol::acceptances_filter(&self.me, SUBSCRIBE_WINDOW_SECS),
            protocol::dm_filter(&self.me, SUBSCRIBE_WINDOW_SECS),
        ]);
        self.role = Role::Passenger(p);
        self.emit(UiEvent::NeedLocation(true));
        self.emit_passenger();
    }

    /// Build, sign and publish a ride request; returns its event id hex.
    fn publish_request(&self, req: &RideRequest) -> Option<String> {
        match protocol::build_ride_request(&self.keys, req, REQUEST_EXPIRY_SECS) {
            Ok(event) => {
                let id = event.id.to_hex();
                self.pool.publish(event);
                Some(id)
            }
            Err(e) => {
                log::warn!("build ride request: {e}");
                self.emit(UiEvent::Toast("could not post request".into()));
                None
            }
        }
    }

    fn cancel_request(&mut self) {
        let mut cancelled = None;
        if let Role::Passenger(p) = &mut self.role {
            p.request.status = RideStatus::Cancelled;
            p.phase = PassengerPhase::Cancelled;
            cancelled = Some(p.request.clone());
        }
        if let Some(req) = cancelled {
            self.publish_request(&req);
            self.emit_passenger();
            self.go_idle();
        }
    }

    /// Buffer a competing acceptance and arm the resolution window. The winner
    /// is chosen deterministically once the window elapses ([`Self::resolve_match`]),
    /// so the order relays deliver acceptances in can't change who wins.
    fn on_acceptance(&mut self, acc: Acceptance) {
        let now = self.now;
        if let Role::Passenger(p) = &mut self.role {
            if p.phase != PassengerPhase::Searching {
                return;
            }
            p.acceptances.push(acc);
            if p.resolve_at.is_none() {
                p.resolve_at = Some(now + MATCH_WINDOW_SECS);
            }
        } else {
            return;
        }
        self.emit_passenger();
    }

    /// Commit to the deterministic first-taker-wins winner over every acceptance
    /// collected this session, re-publish the request as Matched (telling the
    /// winner and the losers), and switch subscriptions to beacons + DMs.
    fn resolve_match(&mut self) {
        let now = self.now;
        let mut matched: Option<RideRequest> = None;
        if let Role::Passenger(p) = &mut self.role {
            let cands = matching::candidates(&p.acceptances, p.t0, &p.published_ids);
            let winning = matching::winner(&cands).cloned();
            match winning
                .as_ref()
                .and_then(|w| PublicKey::parse(&w.driver).ok().map(|pk| (pk, w.driver.clone())))
            {
                Some((driver_pk, driver_hex)) => {
                    p.phase = PassengerPhase::Matched;
                    p.driver = Some(driver_pk);
                    p.winner_acc = winning.clone();
                    p.request.status = RideStatus::Matched;
                    p.request.winner = Some(driver_hex);
                    p.last_beacon = now;
                    p.resolve_at = None;
                    matched = Some(p.request.clone());
                }
                None => p.resolve_at = None, // no valid winner; keep searching
            }
        }
        if let Some(req) = matched {
            self.publish_request(&req);
            self.pool.subscribe(vec![
                protocol::beacons_filter(&self.me, SUBSCRIBE_WINDOW_SECS),
                protocol::dm_filter(&self.me, SUBSCRIBE_WINDOW_SECS),
            ]);
            self.emit_passenger();
        }
    }

    // ---- Driver ------------------------------------------------------------

    fn go_online(&mut self) {
        self.role = Role::Driver(DriverState {
            phase: DriverPhase::Browsing,
            sort: SortKey::PickupDistance,
            offers: BTreeMap::new(),
            pending_take: None,
            my_acceptance_id: None,
            trip: None,
            last_beacon: 0,
            messages: Vec::new(),
            passenger_location: None,
        });
        // Subscribe to nearby requests; geohash scope set once we have GPS.
        self.resubscribe_driver();
        self.emit(UiEvent::NeedLocation(true));
        self.emit_driver();
    }

    fn resubscribe_driver(&self) {
        let geohashes = match self.location {
            Some(loc) => crate::geo::geohash::default_prefixes(loc.lat, loc.lng),
            None => Vec::new(),
        };
        let mut filters = vec![protocol::dm_filter(&self.me, SUBSCRIBE_WINDOW_SECS)];
        if !geohashes.is_empty() {
            filters.push(protocol::requests_filter(&geohashes, SUBSCRIBE_WINDOW_SECS));
        }
        // While in a trip, also receive the passenger's beacons.
        filters.push(protocol::beacons_filter(&self.me, SUBSCRIBE_WINDOW_SECS));
        // Proof-of-burn: discover reputation proofs self-published by the
        // passengers currently offering rides (to gate the list).
        if self.burn_enabled {
            if let Role::Driver(d) = &self.role {
                let authors: Vec<PublicKey> = d.offers.values().map(|e| e.event.pubkey).collect();
                if !authors.is_empty() {
                    filters.push(protocol::upvoting_filter(&authors, SUBSCRIBE_WINDOW_SECS));
                }
            }
        }
        self.pool.subscribe(filters);
    }

    fn set_sort(&mut self, sort: SortKey) {
        if let Role::Driver(d) = &mut self.role {
            d.sort = sort;
        }
        self.emit_driver();
    }

    /// React to an incoming ride request seen by a driver.
    fn on_request_event(&mut self, event: Event, req: RideRequest) {
        let now = self.now;
        let me_hex = self.me.to_hex();
        let passenger_hex = event.pubkey.to_hex();
        let mut won: Option<Offer> = None;
        let mut lost = false;
        let mut new_offer = false;

        if let Role::Driver(d) = &mut self.role {
            // A matched/cancelled/expired request leaves the open list…
            let still_open =
                req.status == RideStatus::Open && !protocol::is_expired(&event, now);

            // …but if it concerns a request we took, resolve win/lose first.
            if d.pending_take.as_deref() == Some(passenger_hex.as_str())
                && req.status == RideStatus::Matched
            {
                if req.winner.as_deref() == Some(me_hex.as_str()) {
                    let offer = build_offer(&event, &req, self.location, &passenger_hex);
                    d.phase = DriverPhase::Trip;
                    d.trip = Some(TripState {
                        passenger: event.pubkey,
                        offer: offer.clone(),
                    });
                    d.pending_take = None;
                    d.last_beacon = now;
                    won = Some(offer);
                } else {
                    d.pending_take = None;
                    d.phase = DriverPhase::Lost;
                    lost = true;
                }
                d.offers.remove(&passenger_hex);
            } else if still_open {
                new_offer = !d.offers.contains_key(&passenger_hex);
                d.offers.insert(
                    passenger_hex.clone(),
                    OfferEntry {
                        event: event.clone(),
                        req: req.clone(),
                    },
                );
            } else {
                d.offers.remove(&passenger_hex);
            }
        }

        if won.is_some() {
            self.resubscribe_driver();
            self.emit(UiEvent::NeedLocation(true));
        }
        // A newly-seen passenger: (re)subscribe to pick up their reputation
        // proofs so we can gate the offer.
        if new_offer && self.burn_enabled {
            self.resubscribe_driver();
        }
        if lost {
            self.emit(UiEvent::Toast("ride taken by another driver".into()));
        }
        self.emit_driver();
    }

    fn take_ride(&mut self, request_id: &str) {
        let keys = self.keys.clone();
        let mut accept_event: Option<Event> = None;
        if let Role::Driver(d) = &mut self.role {
            if let Some((passenger_hex, entry)) = d
                .offers
                .iter()
                .find(|(_, e)| e.event.id.to_hex() == request_id)
            {
                match protocol::build_acceptance(&keys, &entry.event) {
                    Ok(ev) => {
                        d.pending_take = Some(passenger_hex.clone());
                        d.my_acceptance_id = Some(ev.id.to_hex());
                        d.phase = DriverPhase::AwaitingConfirm;
                        accept_event = Some(ev);
                    }
                    Err(e) => log::warn!("build acceptance: {e}"),
                }
            }
        }
        if let Some(ev) = accept_event {
            self.pool.publish(ev);
            self.emit_driver();
        }
    }

    // ---- Shared (post-match) ----------------------------------------------

    fn send_dm(&mut self, text: String) {
        let now = self.now;
        let (recipient, msg) = match &mut self.role {
            Role::Passenger(p) => match p.driver {
                Some(drv) => {
                    p.messages.push(ChatMessage {
                        from_me: true,
                        text: text.clone(),
                        at: now,
                    });
                    (drv, text)
                }
                None => return,
            },
            Role::Driver(d) => match &d.trip {
                Some(t) => {
                    d.messages.push(ChatMessage {
                        from_me: true,
                        text: text.clone(),
                        at: now,
                    });
                    (t.passenger, text)
                }
                None => return,
            },
            Role::Idle => return,
        };
        self.pool.send_dm(recipient, msg);
        self.emit_current();
    }

    fn on_dm(&mut self, sender: PublicKey, message: String, at: u64) {
        let msg = ChatMessage {
            from_me: false,
            text: message,
            at,
        };
        match &mut self.role {
            Role::Passenger(p) if p.driver == Some(sender) => p.messages.push(msg),
            Role::Driver(d) if d.trip.as_ref().map(|t| t.passenger) == Some(sender) => {
                d.messages.push(msg)
            }
            _ => return,
        }
        self.emit_current();
    }

    fn on_beacon(&mut self, sender: PublicKey, beacon: Beacon) {
        match &mut self.role {
            Role::Passenger(p) if p.driver == Some(sender) => {
                p.driver_location = Some(beacon.coord);
            }
            Role::Driver(d) if d.trip.as_ref().map(|t| t.passenger) == Some(sender) => {
                d.passenger_location = Some(beacon.coord);
            }
            _ => return,
        }
        self.emit_current();
    }

    fn complete_trip(&mut self) {
        // Attest + burn for the completed ride before tearing down state (L2).
        self.maybe_burn_completion();
        match &mut self.role {
            Role::Passenger(p) => p.phase = PassengerPhase::Completed,
            Role::Driver(d) => {
                d.phase = DriverPhase::Completed;
                d.trip = None;
            }
            Role::Idle => {}
        }
        self.emit_current();
        self.go_idle();
    }

    fn go_idle(&mut self) {
        self.role = Role::Idle;
        self.pool.subscribe(vec![]);
        self.emit(UiEvent::NeedLocation(false));
        self.emit(UiEvent::Idle);
    }

    // ---- Proof-of-burn (anti-sybil) ---------------------------------------

    /// L1 — publish an immutable identity bond and burn `amount_sats` against it.
    fn publish_bond(&mut self, amount_sats: u64) {
        if !self.burn_enabled || amount_sats == 0 {
            return;
        }
        match protocol::build_identity_bond(&self.keys) {
            Ok(event) => {
                let event_id = protocol::event_id_bytes(&event);
                self.pool.publish(event);
                self.burn.notarize(NotarizeReq {
                    event_id,
                    value_sats: amount_sats,
                    purpose: BurnPurpose::Bond,
                    sign: true,
                    counterparty: None,
                });
                self.emit(UiEvent::Toast("bonding identity…".into()));
            }
            Err(e) => log::warn!("build identity bond: {e}"),
        }
    }

    /// L2 — on a completed ride, attest it and burn `ride_burn_sats` against the
    /// attestation, so both parties accrue reputation over time.
    fn maybe_burn_completion(&mut self) {
        if !self.burn_enabled || self.ride_burn_sats == 0 {
            return;
        }
        // Gather (request, acceptance, counterparty, fare, currency) from
        // whichever role we're in; skip if we lack the references.
        let completion = match &self.role {
            Role::Passenger(p) => match (p.driver, &p.winner_acc) {
                (Some(driver), Some(acc)) => Some((
                    acc.request_id.clone(),
                    acc.event_id.clone(),
                    driver,
                    p.request.fare_estimate,
                    p.request.currency.clone(),
                )),
                _ => None,
            },
            Role::Driver(d) => match (&d.trip, &d.my_acceptance_id) {
                (Some(t), Some(acc_id)) => Some((
                    t.offer.request_id.clone(),
                    acc_id.clone(),
                    t.passenger,
                    t.offer.earnings,
                    t.offer.currency.clone(),
                )),
                _ => None,
            },
            Role::Idle => None,
        };
        let Some((request_id, acceptance_id, counterparty, fare, currency)) = completion else {
            return;
        };
        match protocol::build_ride_completion(
            &self.keys,
            &request_id,
            &acceptance_id,
            &counterparty,
            fare,
            &currency,
        ) {
            Ok(event) => {
                let event_id = protocol::event_id_bytes(&event);
                self.pool.publish(event);
                self.burn.notarize(NotarizeReq {
                    event_id,
                    value_sats: self.ride_burn_sats,
                    purpose: BurnPurpose::Ride,
                    sign: true,
                    counterparty: Some(counterparty.to_hex()),
                });
            }
            Err(e) => log::warn!("build ride completion: {e}"),
        }
    }

    /// Handle a result pushed back by the [`BurnService`].
    fn on_burn(&mut self, outcome: BurnOutcome) {
        match outcome {
            BurnOutcome::Proven {
                purpose,
                proof,
                counterparty,
            } => {
                // Publish the proof (kind 30021) for others to discover, and
                // credit ourselves locally.
                match protocol::build_upvoting_event(&self.keys, &proof, Some(&self.me)) {
                    Ok(ev) => self.pool.publish(ev),
                    Err(e) => log::warn!("build upvoting event: {e}"),
                }
                self.reputation.record(
                    crate::burn::to_hex(&proof.leaf_hash()),
                    BurnRecord {
                        pubkey: self.me.to_hex(),
                        value_msat: proof.leaf_value_msat,
                        confirmed: proof.is_confirmed(),
                        counterparty,
                    },
                );
                let (toast_what, label) = match purpose {
                    BurnPurpose::Bond => ("bond", "Identity bond"),
                    BurnPurpose::Ride => ("ride", "Ride"),
                    BurnPurpose::Boost => ("boost", "Boost"),
                };
                self.emit(UiEvent::Toast(format!(
                    "{toast_what} reputation +{} sat",
                    proof.leaf_value_msat / 1000
                )));
                // Record the notarization tx for the transparency list.
                const MAX_NOTARIZATIONS: usize = 50;
                self.notarizations.insert(
                    0,
                    Notarization {
                        txid: proof.txid.clone(),
                        label: label.to_string(),
                        amount_sats: proof.leaf_value_msat / 1000,
                        confirmed: proof.is_confirmed(),
                        at: self.now,
                    },
                );
                self.notarizations.truncate(MAX_NOTARIZATIONS);
                self.emit(UiEvent::Notarizations {
                    items: self.notarizations.clone(),
                    reputation_sats: self.reputation.score_sats(&self.me.to_hex()),
                });
                self.emit_current();
            }
            BurnOutcome::Failed { reason, .. } => {
                self.emit(UiEvent::Toast(format!("burn failed: {reason}")));
            }
            BurnOutcome::Credited {
                pubkey,
                leaf_hash,
                value_msat,
                confirmed,
                counterparty,
            } => {
                let changed = self.reputation.record(
                    leaf_hash,
                    BurnRecord {
                        pubkey,
                        value_msat,
                        confirmed,
                        counterparty,
                    },
                );
                if changed {
                    self.emit_current(); // gating may now admit/hide an offer
                }
            }
            BurnOutcome::Rejected { reason } => log::debug!("upvote rejected: {reason}"),
        }
    }

    // ---- Tick / location / pool dispatch ----------------------------------

    fn on_tick(&mut self) {
        let now = self.now;
        // Read once up front so the role match below doesn't borrow
        // `self.location` while it holds `&mut self.role`.
        let has_location = self.location.is_some();
        let mut to_publish: Option<RideRequest> = None;
        let mut beacon_to: Option<PublicKey> = None;

        match &mut self.role {
            Role::Passenger(p) => match p.phase {
                PassengerPhase::Searching => {
                    // Freeze escalation while collecting acceptances (we're about
                    // to match); otherwise keep climbing the rate / refreshing.
                    if p.resolve_at.is_none() {
                        let elapsed = now.saturating_sub(p.t0);
                        if auction::is_expired(elapsed) {
                            p.phase = PassengerPhase::Expired;
                        } else {
                            let rate = p.auction.rate_at(elapsed);
                            let rate_changed = rate != p.request.current_rate;
                            let stale = now.saturating_sub(p.last_publish) >= REFRESH_SECS;
                            if rate_changed || stale {
                                p.request.current_rate = rate;
                                p.request.fare_estimate =
                                    auction::fare(rate, p.request.distance_km);
                                p.last_publish = now;
                                to_publish = Some(p.request.clone());
                            }
                        }
                    }
                }
                PassengerPhase::Matched
                    if has_location && now.saturating_sub(p.last_beacon) >= BEACON_SECS =>
                {
                    p.last_beacon = now;
                    beacon_to = p.driver;
                }
                _ => {}
            },
            Role::Driver(d) => {
                if d.phase == DriverPhase::Trip
                    && has_location
                    && now.saturating_sub(d.last_beacon) >= BEACON_SECS
                {
                    d.last_beacon = now;
                    beacon_to = d.trip.as_ref().map(|t| t.passenger);
                }
            }
            Role::Idle => {}
        }

        if let Some(req) = to_publish {
            if let Some(id) = self.publish_request(&req) {
                if let Role::Passenger(p) = &mut self.role {
                    p.published_ids.insert(id);
                }
            }
        }
        if let (Some(to), Some(loc)) = (beacon_to, self.location) {
            self.publish_beacon(to, loc);
        }
        // Commit a pending match once the collection window has elapsed.
        let resolve = matches!(
            &self.role,
            Role::Passenger(p)
                if p.phase == PassengerPhase::Searching
                    && p.resolve_at.is_some_and(|d| now >= d)
        );
        if resolve {
            self.resolve_match();
        }
        // While searching, refresh the passenger UI every tick (not only when
        // the rate steps) so the elapsed clock and the rate-step countdown
        // advance once a second. Other phases update on their own events (a
        // match, a beacon, a DM), so we avoid a needless per-second repaint of
        // the trip/chat screens (which carry a text input).
        if matches!(
            &self.role,
            Role::Passenger(p)
                if matches!(p.phase, PassengerPhase::Searching | PassengerPhase::Expired)
        ) {
            self.emit_passenger();
        }
    }

    fn publish_beacon(&self, to: PublicKey, loc: LatLng) {
        let beacon = Beacon {
            coord: loc,
            heading: None,
        };
        match protocol::build_beacon(&self.keys, &to, &beacon) {
            Ok(ev) => self.pool.publish(ev),
            Err(e) => log::warn!("build beacon: {e}"),
        }
    }

    fn on_location(&mut self) {
        // A driver's geohash scope depends on GPS; (re)subscribe when it arrives.
        if matches!(self.role, Role::Driver(_)) {
            self.resubscribe_driver();
            self.emit_driver();
        }
    }

    fn on_pool(&mut self, ev: PoolEvent) {
        match ev {
            PoolEvent::Incoming(event) => self.on_incoming_event(*event),
            PoolEvent::IncomingDm {
                sender,
                message,
                created_at,
            } => self.on_dm(sender, message, created_at),
            PoolEvent::Status { .. } | PoolEvent::PublishAck { .. } => {}
        }
    }

    fn on_incoming_event(&mut self, event: Event) {
        let kind = event.kind.as_u16();
        if kind == protocol::KIND_RIDE_ACCEPTANCE {
            if matches!(self.role, Role::Passenger(_)) {
                if let Ok(acc) = protocol::parse_acceptance(&event) {
                    self.on_acceptance(acc);
                }
            }
        } else if kind == protocol::KIND_RIDE_REQUEST {
            if matches!(self.role, Role::Driver(_)) {
                if let Ok(req) = protocol::parse_ride_request(&event) {
                    self.on_request_event(event, req);
                }
            }
        } else if kind == protocol::KIND_LOCATION_BEACON {
            let sender = event.pubkey;
            if let Ok(beacon) = protocol::parse_beacon(&self.keys, &event) {
                self.on_beacon(sender, beacon);
            }
        } else if kind == protocol::KIND_UPVOTING_EVENT && self.burn_enabled {
            // A counterparty's reputation proof — verify it on-chain (async),
            // crediting them when the service reports back.
            if let Ok(proof) = protocol::parse_upvoting_event(&event) {
                self.burn.verify_incoming(proof);
            }
        }
    }

    // ---- Snapshots ---------------------------------------------------------

    fn emit_current(&self) {
        match &self.role {
            Role::Passenger(_) => self.emit_passenger(),
            Role::Driver(_) => self.emit_driver(),
            Role::Idle => self.emit(UiEvent::Idle),
        }
    }

    fn emit_passenger(&self) {
        if let Role::Passenger(p) = &self.role {
            let elapsed = self.now.saturating_sub(p.t0);
            let driver_name = p.driver.as_ref().map(keys::derive_name);
            self.emit(UiEvent::Passenger(PassengerSnapshot {
                phase: p.phase,
                current_rate: p.request.current_rate,
                fare_estimate: p.request.fare_estimate,
                elapsed_secs: elapsed,
                at_max: auction::at_max(elapsed),
                secs_to_next_step: auction::secs_to_next_step(elapsed),
                currency: p.request.currency.clone(),
                driver: p.driver.map(|d| d.to_hex()),
                driver_name,
                driver_location: p.driver_location,
                messages: p.messages.clone(),
            }));
        }
    }

    fn emit_driver(&self) {
        if let Role::Driver(d) = &self.role {
            let mut offers: Vec<Offer> = d
                .offers
                .values()
                // L3 — hide passengers below the reputation bar (threshold 0 =
                // off, the permissionless default).
                .filter(|e| {
                    self.reputation
                        .meets(&e.event.pubkey.to_hex(), self.rep_threshold)
                })
                .map(|e| {
                    let mut o = build_offer(&e.event, &e.req, self.location, &e.event.pubkey.to_hex());
                    o.taken = d.pending_take.as_deref() == Some(o.passenger.as_str());
                    o
                })
                .collect();
            sort_offers(&mut offers, d.sort);
            self.emit(UiEvent::Driver(DriverSnapshot {
                phase: d.phase,
                offers,
                trip: d.trip.as_ref().map(|t| t.offer.clone()),
                passenger_location: d.passenger_location,
                messages: d.messages.clone(),
            }));
        }
    }
}

/// Build a UI [`Offer`] from a request event + payload, given the driver's GPS.
fn build_offer(event: &Event, req: &RideRequest, driver: Option<LatLng>, passenger_hex: &str) -> Offer {
    let pickup_distance_km = match driver {
        Some(loc) => loc.haversine_km(&req.pickup),
        None => f64::INFINITY,
    };
    Offer {
        request_id: event.id.to_hex(),
        passenger: passenger_hex.to_string(),
        passenger_name: keys::derive_name(&event.pubkey),
        pickup: req.pickup,
        dropoff: req.dropoff,
        trip_distance_km: req.distance_km,
        pickup_distance_km,
        rate: req.current_rate,
        earnings: req.fare_estimate,
        currency: req.currency.clone(),
        taken: false,
    }
}

/// Sort offers in place by the chosen key (best first).
fn sort_offers(offers: &mut [Offer], key: SortKey) {
    match key {
        SortKey::PickupDistance => offers.sort_by(|a, b| {
            a.pickup_distance_km
                .partial_cmp(&b.pickup_distance_km)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        SortKey::Earnings => offers.sort_by_key(|o| std::cmp::Reverse(o.earnings)),
        SortKey::Rate => offers.sort_by_key(|o| std::cmp::Reverse(o.rate)),
        SortKey::TripDistance => offers.sort_by(|a, b| {
            b.trip_distance_km
                .partial_cmp(&a.trip_distance_km)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::burn::service::{BurnOutcome, BurnPurpose, MockBurnService};
    use crate::pool::MockPool;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

    struct Harness {
        engine: Engine<MockPool>,
        pool: Arc<MockPool>,
        ui: UnboundedReceiver<UiEvent>,
    }

    fn harness() -> (Harness, Keys) {
        let keys = keys::generate();
        let pool = Arc::new(MockPool::new());
        let (tx, ui) = unbounded_channel();
        let engine = Engine::new(keys.clone(), pool.clone(), tx);
        (Harness { engine, pool, ui }, keys)
    }

    /// A harness wired to a [`MockBurnService`], returning the service (for
    /// request assertions) and its outcome receiver (to pump results back in).
    fn burn_harness(
        rep_threshold: u64,
        ride_burn_sats: u64,
    ) -> (Harness, Arc<MockBurnService>, UnboundedReceiver<BurnOutcome>, Keys) {
        let keys = keys::generate();
        let pool = Arc::new(MockPool::new());
        let (tx, ui) = unbounded_channel();
        let (btx, brx) = unbounded_channel();
        let burn = Arc::new(MockBurnService::new(btx));
        let engine = Engine::with_burn(
            keys.clone(),
            pool.clone(),
            tx,
            burn.clone(),
            rep_threshold,
            ride_burn_sats,
        );
        (Harness { engine, pool, ui }, burn, brx, keys)
    }

    /// Feed every queued burn outcome back into the engine (as the controller
    /// would).
    fn pump_burn(h: &mut Harness, brx: &mut UnboundedReceiver<BurnOutcome>) {
        while let Ok(o) = brx.try_recv() {
            h.engine.handle(EngineCmd::Burn(o));
        }
    }

    impl Harness {
        fn last_ui(&mut self) -> Option<UiEvent> {
            let mut last = None;
            while let Ok(ev) = self.ui.try_recv() {
                last = Some(ev);
            }
            last
        }
        fn last_passenger(&mut self) -> Option<PassengerSnapshot> {
            let mut last = None;
            while let Ok(ev) = self.ui.try_recv() {
                if let UiEvent::Passenger(p) = ev {
                    last = Some(p);
                }
            }
            last
        }
        fn last_driver(&mut self) -> Option<DriverSnapshot> {
            let mut last = None;
            while let Ok(ev) = self.ui.try_recv() {
                if let UiEvent::Driver(d) = ev {
                    last = Some(d);
                }
            }
            last
        }
    }

    fn request_cmd() -> EngineCmd {
        EngineCmd::RequestRide {
            pickup: LatLng::new(-1.2864, 36.8172),
            dropoff: LatLng::new(-1.3192, 36.9278),
            distance_km: 10.0,
            currency: "KES".into(),
            start_rate: 20,
            max_rate: 120,
        }
    }

    #[test]
    fn passenger_request_publishes_open_event_and_subscribes() {
        let (mut h, _k) = harness();
        h.engine.handle(EngineCmd::Tick { now: 1000 });
        h.engine.handle(request_cmd());

        let event = h.pool.last_published().expect("a request was published");
        let req = protocol::parse_ride_request(&event).unwrap();
        assert_eq!(req.status, RideStatus::Open);
        assert_eq!(req.current_rate, 20);
        assert_eq!(req.fare_estimate, 200);
        // Subscribed to acceptances + DMs.
        assert_eq!(h.pool.subscriptions().len(), 2);

        let snap = h.last_passenger().unwrap();
        assert_eq!(snap.phase, PassengerPhase::Searching);
        assert_eq!(snap.current_rate, 20);
    }

    #[test]
    fn rate_escalates_and_republishes_on_tick() {
        let (mut h, _k) = harness();
        h.engine.handle(EngineCmd::Tick { now: 1000 });
        h.engine.handle(request_cmd());
        let count_after_request = h.pool.published().len();

        // 30s later → one step up (span 100 / 10 = 10).
        h.engine.handle(EngineCmd::Tick { now: 1030 });
        let event = h.pool.last_published().unwrap();
        let req = protocol::parse_ride_request(&event).unwrap();
        assert_eq!(req.current_rate, 30);
        assert!(h.pool.published().len() > count_after_request);

        // At +5min the rate has reached max.
        h.engine.handle(EngineCmd::Tick { now: 1300 });
        let snap = h.last_passenger().unwrap();
        assert_eq!(snap.current_rate, 120);
        assert!(snap.at_max);
    }

    #[test]
    fn search_expires_after_max_lifetime() {
        let (mut h, _k) = harness();
        h.engine.handle(EngineCmd::Tick { now: 1000 });
        h.engine.handle(request_cmd());
        h.engine
            .handle(EngineCmd::Tick { now: 1000 + auction::MAX_LIFETIME_SECS });
        let snap = h.last_passenger().unwrap();
        assert_eq!(snap.phase, PassengerPhase::Expired);
    }

    #[test]
    fn first_acceptance_matches_and_republishes_as_matched() {
        let (mut h, _passenger) = harness();
        h.engine.handle(EngineCmd::Tick { now: 1000 });
        h.engine.handle(request_cmd());
        let request_event = h.pool.last_published().unwrap();

        let driver = keys::generate();
        let acc = protocol::build_acceptance(&driver, &request_event).unwrap();
        h.engine
            .handle(EngineCmd::Pool(PoolEvent::Incoming(Box::new(acc))));
        // Let the collection window elapse; a tick then commits the match.
        h.engine
            .handle(EngineCmd::Tick { now: 1000 + MATCH_WINDOW_SECS + 1 });

        // The re-published request is now Matched, naming the driver.
        let matched = h.pool.last_published().unwrap();
        let req = protocol::parse_ride_request(&matched).unwrap();
        assert_eq!(req.status, RideStatus::Matched);
        assert_eq!(req.winner, Some(driver.public_key().to_hex()));

        let snap = h.last_passenger().unwrap();
        assert_eq!(snap.phase, PassengerPhase::Matched);
        assert_eq!(snap.driver, Some(driver.public_key().to_hex()));
        assert!(snap.driver_name.is_some());
    }

    #[test]
    fn multiple_acceptances_pick_deterministic_first_winner() {
        let (mut h, _p) = harness();
        h.engine.handle(EngineCmd::Tick { now: 1000 });
        h.engine.handle(request_cmd());
        let request_event = h.pool.last_published().unwrap();

        // Two drivers accept; whichever has the earlier created_at / smaller id
        // wins. We build both and feed them in arbitrary order.
        let d1 = keys::generate();
        let d2 = keys::generate();
        let a1 = protocol::build_acceptance(&d1, &request_event).unwrap();
        let a2 = protocol::build_acceptance(&d2, &request_event).unwrap();
        let expected = matching::winner(&[
            protocol::parse_acceptance(&a1).unwrap(),
            protocol::parse_acceptance(&a2).unwrap(),
        ])
        .unwrap()
        .driver
        .clone();

        h.engine
            .handle(EngineCmd::Pool(PoolEvent::Incoming(Box::new(a2))));
        h.engine
            .handle(EngineCmd::Pool(PoolEvent::Incoming(Box::new(a1))));
        // Resolution happens after the window — over BOTH acceptances — so the
        // winner is the deterministic one regardless of arrival order.
        h.engine
            .handle(EngineCmd::Tick { now: 1000 + MATCH_WINDOW_SECS + 1 });

        let snap = h.last_passenger().unwrap();
        assert_eq!(snap.phase, PassengerPhase::Matched);
        assert_eq!(snap.driver, Some(expected));
    }

    #[test]
    fn driver_sees_open_offer_and_can_take_it() {
        // Driver side.
        let (mut h, _driver_keys) = harness();
        h.engine.handle(EngineCmd::Tick { now: 1000 });
        h.engine
            .handle(EngineCmd::Location(LatLng::new(-1.30, 36.82)));
        h.engine.handle(EngineCmd::GoOnline);

        // A passenger's request arrives.
        let passenger = keys::generate();
        let req = RideRequest {
            pickup: LatLng::new(-1.2864, 36.8172),
            dropoff: LatLng::new(-1.3192, 36.9278),
            distance_km: 10.0,
            currency: "KES".into(),
            start_rate: 30,
            max_rate: 100,
            current_rate: 30,
            fare_estimate: 300,
            status: RideStatus::Open,
            winner: None,
        };
        let event = protocol::build_ride_request(&passenger, &req, 90).unwrap();
        h.engine
            .handle(EngineCmd::Pool(PoolEvent::Incoming(Box::new(event.clone()))));

        let snap = h.last_driver().unwrap();
        assert_eq!(snap.offers.len(), 1);
        let offer = &snap.offers[0];
        assert_eq!(offer.earnings, 300);
        assert!(offer.pickup_distance_km.is_finite());

        // Take it → an acceptance is published referencing the request.
        let rid = offer.request_id.clone();
        h.engine.handle(EngineCmd::TakeRide { request_id: rid });
        let acc = h.pool.last_published().unwrap();
        let parsed = protocol::parse_acceptance(&acc).unwrap();
        assert_eq!(parsed.request_id, event.id.to_hex());

        let snap = h.last_driver().unwrap();
        assert_eq!(snap.phase, DriverPhase::AwaitingConfirm);
        // The taken offer is still listed (until we win/lose) but flagged so the
        // UI can show "waiting…" instead of a dead TAKE button.
        assert_eq!(snap.offers.len(), 1);
        assert!(snap.offers[0].taken);
    }

    #[test]
    fn driver_wins_when_matched_event_names_them() {
        let (mut h, driver_keys) = harness();
        h.engine.handle(EngineCmd::Tick { now: 1000 });
        h.engine
            .handle(EngineCmd::Location(LatLng::new(-1.30, 36.82)));
        h.engine.handle(EngineCmd::GoOnline);

        let passenger = keys::generate();
        let mut req = RideRequest {
            pickup: LatLng::new(-1.2864, 36.8172),
            dropoff: LatLng::new(-1.3192, 36.9278),
            distance_km: 10.0,
            currency: "KES".into(),
            start_rate: 30,
            max_rate: 100,
            current_rate: 30,
            fare_estimate: 300,
            status: RideStatus::Open,
            winner: None,
        };
        let open = protocol::build_ride_request(&passenger, &req, 90).unwrap();
        h.engine
            .handle(EngineCmd::Pool(PoolEvent::Incoming(Box::new(open.clone()))));
        let rid = h.last_driver().unwrap().offers[0].request_id.clone();
        h.engine.handle(EngineCmd::TakeRide { request_id: rid });

        // Passenger matched us.
        req.status = RideStatus::Matched;
        req.winner = Some(driver_keys.public_key().to_hex());
        let matched = protocol::build_ride_request(&passenger, &req, 90).unwrap();
        h.engine
            .handle(EngineCmd::Pool(PoolEvent::Incoming(Box::new(matched))));

        let snap = h.last_driver().unwrap();
        assert_eq!(snap.phase, DriverPhase::Trip);
        assert!(snap.trip.is_some());
        assert!(snap.offers.is_empty());
    }

    #[test]
    fn driver_loses_when_matched_event_names_someone_else() {
        let (mut h, _driver_keys) = harness();
        h.engine.handle(EngineCmd::Tick { now: 1000 });
        h.engine
            .handle(EngineCmd::Location(LatLng::new(-1.30, 36.82)));
        h.engine.handle(EngineCmd::GoOnline);

        let passenger = keys::generate();
        let other_driver = keys::generate();
        let mut req = RideRequest {
            pickup: LatLng::new(-1.2864, 36.8172),
            dropoff: LatLng::new(-1.3192, 36.9278),
            distance_km: 10.0,
            currency: "KES".into(),
            start_rate: 30,
            max_rate: 100,
            current_rate: 30,
            fare_estimate: 300,
            status: RideStatus::Open,
            winner: None,
        };
        let open = protocol::build_ride_request(&passenger, &req, 90).unwrap();
        h.engine
            .handle(EngineCmd::Pool(PoolEvent::Incoming(Box::new(open.clone()))));
        let rid = h.last_driver().unwrap().offers[0].request_id.clone();
        h.engine.handle(EngineCmd::TakeRide { request_id: rid });

        req.status = RideStatus::Matched;
        req.winner = Some(other_driver.public_key().to_hex());
        let matched = protocol::build_ride_request(&passenger, &req, 90).unwrap();
        h.engine
            .handle(EngineCmd::Pool(PoolEvent::Incoming(Box::new(matched))));

        let snap = h.last_driver().unwrap();
        assert_eq!(snap.phase, DriverPhase::Lost);
        assert!(snap.offers.is_empty());
    }

    #[test]
    fn matched_passenger_receives_driver_beacon() {
        let (mut h, _p) = harness();
        h.engine.handle(EngineCmd::Tick { now: 1000 });
        h.engine.handle(request_cmd());
        let request_event = h.pool.last_published().unwrap();
        let driver = keys::generate();
        let acc = protocol::build_acceptance(&driver, &request_event).unwrap();
        h.engine
            .handle(EngineCmd::Pool(PoolEvent::Incoming(Box::new(acc))));
        h.engine
            .handle(EngineCmd::Tick { now: 1000 + MATCH_WINDOW_SECS + 1 });

        // Driver's beacon to the passenger.
        let beacon = Beacon {
            coord: LatLng::new(-1.295, 36.83),
            heading: None,
        };
        // The passenger is the harness identity; encrypt to *them*.
        let passenger_pk = h.engine.me;
        let bev = protocol::build_beacon(&driver, &passenger_pk, &beacon).unwrap();
        h.engine
            .handle(EngineCmd::Pool(PoolEvent::Incoming(Box::new(bev))));

        let snap = h.last_passenger().unwrap();
        assert_eq!(snap.driver_location, Some(LatLng::new(-1.295, 36.83)));
    }

    #[test]
    fn cancel_publishes_cancelled_and_goes_idle() {
        let (mut h, _p) = harness();
        h.engine.handle(EngineCmd::Tick { now: 1000 });
        h.engine.handle(request_cmd());
        h.engine.handle(EngineCmd::CancelRequest);

        let last = h.pool.published().last().cloned().unwrap();
        let req = protocol::parse_ride_request(&last).unwrap();
        assert_eq!(req.status, RideStatus::Cancelled);
        assert!(matches!(h.last_ui(), Some(UiEvent::Idle)));
    }

    #[test]
    fn driver_sort_orders_offers() {
        let mut offers = vec![
            Offer {
                request_id: "a".into(),
                passenger: "pa".into(),
                passenger_name: "A".into(),
                pickup: LatLng::new(0.0, 0.0),
                dropoff: LatLng::new(0.0, 0.0),
                trip_distance_km: 5.0,
                pickup_distance_km: 3.0,
                rate: 50,
                earnings: 250,
                currency: "KES".into(),
                taken: false,
            },
            Offer {
                request_id: "b".into(),
                passenger: "pb".into(),
                passenger_name: "B".into(),
                pickup: LatLng::new(0.0, 0.0),
                dropoff: LatLng::new(0.0, 0.0),
                trip_distance_km: 12.0,
                pickup_distance_km: 1.0,
                rate: 40,
                earnings: 480,
                currency: "KES".into(),
                taken: false,
            },
        ];
        sort_offers(&mut offers, SortKey::PickupDistance);
        assert_eq!(offers[0].request_id, "b"); // closer pickup first
        sort_offers(&mut offers, SortKey::Earnings);
        assert_eq!(offers[0].request_id, "b"); // higher earnings first
        sort_offers(&mut offers, SortKey::Rate);
        assert_eq!(offers[0].request_id, "a"); // higher rate first
        sort_offers(&mut offers, SortKey::TripDistance);
        assert_eq!(offers[0].request_id, "b"); // longer trip first
    }

    // ---- proof-of-burn integration ----------------------------------------

    #[test]
    fn publish_bond_notarizes_then_publishes_proof_and_credits_self() {
        let (mut h, burn, mut brx, keys) = burn_harness(0, 0);
        h.engine.handle(EngineCmd::Tick { now: 1000 });
        h.engine.handle(EngineCmd::PublishBond { amount_sats: 500 });

        // An immutable identity-bond event is published…
        let bond = h.pool.last_published().unwrap();
        assert_eq!(bond.kind, Kind::Custom(protocol::KIND_IDENTITY_BOND));
        // …and the service is asked to burn against it.
        let reqs = burn.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].purpose, BurnPurpose::Bond);
        assert_eq!(reqs[0].value_sats, 500);
        assert!(reqs[0].sign);

        // The mock returns a proof; feed it back.
        pump_burn(&mut h, &mut brx);

        // We publish the proof as a kind-30021 upvoting event…
        let last = h.pool.last_published().unwrap();
        assert_eq!(last.kind, Kind::Custom(protocol::KIND_UPVOTING_EVENT));
        // …and credit ourselves locally.
        assert_eq!(h.engine.reputation_sats(&keys.public_key().to_hex()), 500);
    }

    #[test]
    fn proven_burn_surfaces_a_notarization_for_the_ui() {
        let (mut h, _burn, mut brx, _k) = burn_harness(0, 0);
        h.engine.handle(EngineCmd::Tick { now: 1000 });
        h.engine.handle(EngineCmd::PublishBond { amount_sats: 500 });
        pump_burn(&mut h, &mut brx);

        // The engine emits a Notarizations snapshot (list + reputation total).
        let mut snap = None;
        while let Ok(ev) = h.ui.try_recv() {
            if let UiEvent::Notarizations {
                items,
                reputation_sats,
            } = ev
            {
                snap = Some((items, reputation_sats));
            }
        }
        let (items, reputation_sats) = snap.expect("a Notarizations UiEvent");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "Identity bond");
        assert_eq!(items[0].amount_sats, 500);
        assert!(items[0].confirmed);
        assert_eq!(items[0].txid.len(), 64); // a 32-byte txid in display hex
        // The confirmed bond counts toward the surfaced reputation total.
        assert_eq!(reputation_sats, 500);
    }

    #[test]
    fn reputation_gate_hides_then_shows_a_passenger() {
        let (mut h, _burn, _brx, _k) = burn_harness(100, 0);
        h.engine.handle(EngineCmd::Tick { now: 1000 });
        h.engine.handle(EngineCmd::Location(LatLng::new(-1.30, 36.82)));
        h.engine.handle(EngineCmd::GoOnline);

        let passenger = keys::generate();
        let req = RideRequest {
            pickup: LatLng::new(-1.2864, 36.8172),
            dropoff: LatLng::new(-1.3192, 36.9278),
            distance_km: 10.0,
            currency: "KES".into(),
            start_rate: 30,
            max_rate: 100,
            current_rate: 30,
            fare_estimate: 300,
            status: RideStatus::Open,
            winner: None,
        };
        let event = protocol::build_ride_request(&passenger, &req, 90).unwrap();
        h.engine
            .handle(EngineCmd::Pool(PoolEvent::Incoming(Box::new(event))));

        // Below the 100-sat bar → hidden.
        assert!(h.last_driver().unwrap().offers.is_empty());

        // A verified upvote credits the passenger 200 sat (confirmed) → shown.
        h.engine.handle(EngineCmd::Burn(BurnOutcome::Credited {
            pubkey: passenger.public_key().to_hex(),
            leaf_hash: "leaf-a".into(),
            value_msat: 200_000,
            confirmed: true,
            counterparty: None,
        }));
        assert_eq!(h.last_driver().unwrap().offers.len(), 1);
    }

    #[test]
    fn unconfirmed_credit_does_not_pass_the_gate() {
        let (mut h, _burn, _brx, _k) = burn_harness(100, 0);
        h.engine.handle(EngineCmd::Tick { now: 1000 });
        h.engine.handle(EngineCmd::Location(LatLng::new(-1.30, 36.82)));
        h.engine.handle(EngineCmd::GoOnline);

        let passenger = keys::generate();
        let mut req = sample_open_request();
        req.fare_estimate = 300;
        let event = protocol::build_ride_request(&passenger, &req, 90).unwrap();
        h.engine
            .handle(EngineCmd::Pool(PoolEvent::Incoming(Box::new(event))));

        // A mempool-only (unconfirmed) burn does not count toward the gate.
        h.engine.handle(EngineCmd::Burn(BurnOutcome::Credited {
            pubkey: passenger.public_key().to_hex(),
            leaf_hash: "leaf-b".into(),
            value_msat: 500_000,
            confirmed: false,
            counterparty: None,
        }));
        assert!(h.last_driver().unwrap().offers.is_empty());
    }

    #[test]
    fn completing_a_ride_attests_and_burns_for_the_passenger() {
        let (mut h, burn, _brx, _k) = burn_harness(0, 10);
        h.engine.handle(EngineCmd::Tick { now: 1000 });
        h.engine.handle(request_cmd());
        let request_event = h.pool.last_published().unwrap();

        let driver = keys::generate();
        let acc = protocol::build_acceptance(&driver, &request_event).unwrap();
        h.engine
            .handle(EngineCmd::Pool(PoolEvent::Incoming(Box::new(acc))));
        h.engine
            .handle(EngineCmd::Tick { now: 1000 + MATCH_WINDOW_SECS + 1 });

        h.engine.handle(EngineCmd::CompleteTrip);

        // A ride-completion attestation was published…
        assert!(h
            .pool
            .published()
            .iter()
            .any(|e| e.kind == Kind::Custom(protocol::KIND_RIDE_COMPLETION)));
        // …and a per-ride burn requested, naming the driver as counterparty.
        let ride = burn
            .requests()
            .into_iter()
            .find(|r| r.purpose == BurnPurpose::Ride)
            .expect("a ride burn");
        assert_eq!(ride.value_sats, 10);
        assert_eq!(ride.counterparty, Some(driver.public_key().to_hex()));
    }

    fn sample_open_request() -> RideRequest {
        RideRequest {
            pickup: LatLng::new(-1.2864, 36.8172),
            dropoff: LatLng::new(-1.3192, 36.9278),
            distance_km: 10.0,
            currency: "KES".into(),
            start_rate: 30,
            max_rate: 100,
            current_rate: 30,
            fare_estimate: 300,
            status: RideStatus::Open,
            winner: None,
        }
    }
}
