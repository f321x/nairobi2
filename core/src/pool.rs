//! The relay transport boundary.
//!
//! The engine talks to relays only through the [`Pool`] trait, so the entire
//! network layer swaps to a [`MockPool`] in tests (the discipline ntrack uses
//! for its relay pool). All methods are fire-and-forget; results and incoming
//! events arrive back asynchronously as [`PoolEvent`]s on a channel.
//!
//! [`SdkPool`] is the real implementation over the `nostr-sdk` client — built
//! offline here and compile-checked; it needs a live network and relays to
//! exercise at runtime.

use std::sync::{Arc, Mutex};

use nostr_sdk::prelude::*;
use tokio::sync::mpsc::UnboundedSender;

/// Events the relay layer pushes back toward the engine.
#[derive(Debug, Clone)]
pub enum PoolEvent {
    /// A relay connection came up or went down.
    Status { url: String, connected: bool },
    /// A (non-DM) event arrived on a subscription.
    Incoming(Box<Event>),
    /// A NIP-17 private DM arrived and was unwrapped.
    IncomingDm {
        sender: PublicKey,
        message: String,
        created_at: u64,
    },
    /// A publish was accepted by at least one relay (`accepted`) or by none.
    PublishAck { event_id: EventId, accepted: bool },
}

/// The transport abstraction the engine depends on. Fire-and-forget: callers
/// never await; effects surface later as [`PoolEvent`]s.
pub trait Pool: Send + Sync + 'static {
    /// Publish a signed event to all relays.
    fn publish(&self, event: Event);
    /// Replace the active subscription set with `filters`.
    fn subscribe(&self, filters: Vec<Filter>);
    /// Send a NIP-17 encrypted direct message to `recipient`.
    fn send_dm(&self, recipient: PublicKey, message: String);
    /// Update the relay URL list (adds new relays and connects).
    fn set_relays(&self, relays: Vec<String>);
}

// ---- MockPool (tests) -----------------------------------------------------

/// In-memory [`Pool`] for host tests: records everything, sends nothing. Tests
/// inject incoming events by feeding `EngineCmd::Pool(PoolEvent::…)` directly.
#[derive(Default)]
pub struct MockPool {
    published: Mutex<Vec<Event>>,
    subscriptions: Mutex<Vec<Filter>>,
    dms: Mutex<Vec<(PublicKey, String)>>,
    relays: Mutex<Vec<String>>,
}

impl MockPool {
    /// A fresh, empty mock.
    pub fn new() -> Self {
        Self::default()
    }
    /// Every event published so far (in order).
    pub fn published(&self) -> Vec<Event> {
        self.published.lock().unwrap().clone()
    }
    /// The most recently published event, if any.
    pub fn last_published(&self) -> Option<Event> {
        self.published.lock().unwrap().last().cloned()
    }
    /// The current subscription filter set.
    pub fn subscriptions(&self) -> Vec<Filter> {
        self.subscriptions.lock().unwrap().clone()
    }
    /// Every DM sent so far as `(recipient, message)`.
    pub fn dms(&self) -> Vec<(PublicKey, String)> {
        self.dms.lock().unwrap().clone()
    }
    /// The current relay list.
    pub fn relays(&self) -> Vec<String> {
        self.relays.lock().unwrap().clone()
    }
}

impl Pool for MockPool {
    fn publish(&self, event: Event) {
        self.published.lock().unwrap().push(event);
    }
    fn subscribe(&self, filters: Vec<Filter>) {
        *self.subscriptions.lock().unwrap() = filters;
    }
    fn send_dm(&self, recipient: PublicKey, message: String) {
        self.dms.lock().unwrap().push((recipient, message));
    }
    fn set_relays(&self, relays: Vec<String>) {
        *self.relays.lock().unwrap() = relays;
    }
}

// ---- SdkPool (real, nostr-sdk) --------------------------------------------

/// The real [`Pool`] backed by a `nostr-sdk` [`Client`]. Spawns a background
/// task that forwards incoming relay events (unwrapping NIP-17 DMs) as
/// [`PoolEvent`]s.
pub struct SdkPool {
    client: Client,
    handle: tokio::runtime::Handle,
    pool_tx: UnboundedSender<PoolEvent>,
}

impl SdkPool {
    /// Connect to `relays` and start forwarding notifications. Must be called
    /// from within a Tokio runtime.
    pub async fn connect(
        keys: Keys,
        relays: &[String],
        pool_tx: UnboundedSender<PoolEvent>,
    ) -> crate::Result<Arc<Self>> {
        let client = Client::new(keys);
        for url in relays {
            if let Err(e) = client.add_relay(url.as_str()).await {
                log::warn!("add_relay {url}: {e}");
            }
        }
        client.connect().await;

        let pool = Arc::new(Self {
            client,
            handle: tokio::runtime::Handle::current(),
            pool_tx,
        });
        let bg = pool.clone();
        pool.handle.spawn(async move { bg.forward_notifications().await });
        Ok(pool)
    }

    /// Forward relay events to the engine until the notification channel closes.
    async fn forward_notifications(self: Arc<Self>) {
        let mut notifications = self.client.notifications();
        loop {
            match notifications.recv().await {
                Ok(RelayPoolNotification::Event { event, .. }) => {
                    if event.kind == Kind::GiftWrap {
                        // NIP-17 DM: unwrap (async) and forward the rumor.
                        if let Ok(UnwrappedGift { sender, rumor }) =
                            self.client.unwrap_gift_wrap(&event).await
                        {
                            if rumor.kind == Kind::PrivateDirectMessage {
                                let _ = self.pool_tx.send(PoolEvent::IncomingDm {
                                    sender,
                                    message: rumor.content,
                                    created_at: rumor.created_at.as_secs(),
                                });
                            }
                        }
                    } else {
                        let _ = self.pool_tx.send(PoolEvent::Incoming(event));
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            }
        }
    }
}

impl Pool for SdkPool {
    fn publish(&self, event: Event) {
        let client = self.client.clone();
        let tx = self.pool_tx.clone();
        self.handle.spawn(async move {
            match client.send_event(&event).await {
                Ok(output) => {
                    let _ = tx.send(PoolEvent::PublishAck {
                        event_id: output.val,
                        accepted: !output.success.is_empty(),
                    });
                }
                Err(e) => log::warn!("publish failed: {e}"),
            }
        });
    }

    fn subscribe(&self, filters: Vec<Filter>) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            client.unsubscribe_all().await;
            for f in filters {
                if let Err(e) = client.subscribe(f, None).await {
                    log::warn!("subscribe failed: {e}");
                }
            }
        });
    }

    fn send_dm(&self, recipient: PublicKey, message: String) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            if let Err(e) = client.send_private_msg(recipient, message, []).await {
                log::warn!("send_dm failed: {e}");
            }
        });
    }

    fn set_relays(&self, relays: Vec<String>) {
        let client = self.client.clone();
        self.handle.spawn(async move {
            for url in relays {
                let _ = client.add_relay(url.as_str()).await;
            }
            client.connect().await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::generate;
    use crate::protocol::{self, RideRequest, RideStatus};
    use crate::geo::LatLng;

    fn sample_request() -> RideRequest {
        RideRequest {
            pickup: LatLng::new(-1.2864, 36.8172),
            dropoff: LatLng::new(-1.3192, 36.9278),
            distance_km: 18.5,
            currency: "KES".to_string(),
            start_rate: 20,
            max_rate: 120,
            current_rate: 20,
            fare_estimate: 370,
            status: RideStatus::Open,
            winner: None,
        }
    }

    #[test]
    fn mock_records_published_events() {
        let keys = generate();
        let pool = MockPool::new();
        let event = protocol::build_ride_request(&keys, &sample_request(), 90).unwrap();
        pool.publish(event.clone());
        assert_eq!(pool.published().len(), 1);
        assert_eq!(pool.last_published().unwrap().id, event.id);
    }

    #[test]
    fn mock_replaces_subscriptions() {
        let me = generate().public_key();
        let pool = MockPool::new();
        pool.subscribe(vec![protocol::acceptances_filter(&me, 600)]);
        assert_eq!(pool.subscriptions().len(), 1);
        pool.subscribe(vec![
            protocol::requests_filter(&["u4pru".into()], 600),
            protocol::beacons_filter(&me, 600),
        ]);
        assert_eq!(pool.subscriptions().len(), 2);
    }

    #[test]
    fn mock_records_dms_and_relays() {
        let bob = generate().public_key();
        let pool = MockPool::new();
        pool.send_dm(bob, "i'm here".to_string());
        pool.set_relays(vec!["wss://relay.example".to_string()]);
        assert_eq!(pool.dms(), vec![(bob, "i'm here".to_string())]);
        assert_eq!(pool.relays(), vec!["wss://relay.example".to_string()]);
    }
}
