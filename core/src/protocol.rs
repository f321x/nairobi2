//! Build, sign, parse, and validate every Nostr event the app uses, plus the
//! subscription filters. The four event kinds:
//!
//! | Kind  | Class       | Meaning                                    |
//! |-------|-------------|--------------------------------------------|
//! | 11311 | replaceable | Ride Request (one active per passenger)    |
//! | 1313  | regular     | Ride Acceptance (a driver's claim, stored) |
//! | 21313 | ephemeral   | Location beacon (NIP-44 encrypted)         |
//!
//! Post-match chat uses NIP-17 private DMs via the `nostr-sdk` client (see
//! [`crate::pool`]), not a raw kind here.

use crate::error::{Error, Result};
use crate::geo::{geohash, LatLng};
use crate::matching::Acceptance;
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};

/// Replaceable: one active ride request per passenger (kind range 10000–19999).
pub const KIND_RIDE_REQUEST: u16 = 11311;
/// Regular/stored: a driver's claim on a request (range 1000–9999).
pub const KIND_RIDE_ACCEPTANCE: u16 = 1313;
/// Ephemeral: an encrypted live-location beacon (range 20000–29999).
pub const KIND_LOCATION_BEACON: u16 = 21313;
/// Regular/stored, **immutable**: the proof-of-burn identity bond. A stable
/// event id is the durable target the one-time bond burn references (a
/// replaceable event's id changes per version, orphaning the proof).
pub const KIND_IDENTITY_BOND: u16 = 13131;
/// Regular/stored, **immutable**: a ride-completion attestation — the target of
/// the per-ride (~1 %) burn. References the request, the acceptance, and the
/// counterparty (for reputation + diversity).
pub const KIND_RIDE_COMPLETION: u16 = 1314;
/// Addressable: the proof-of-burn "upvoting" event carrying a verifiable proof
/// (`docs/proof-of-burn-api.md` §7). Kept compatible with the notary's format.
pub const KIND_UPVOTING_EVENT: u16 = 30021;

/// Lifecycle status carried in the ride-request payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RideStatus {
    Open,
    Matched,
    Cancelled,
}

/// The JSON payload of a ride-request event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RideRequest {
    pub pickup: LatLng,
    pub dropoff: LatLng,
    pub distance_km: f64,
    pub currency: String,
    pub start_rate: u32,
    pub max_rate: u32,
    pub current_rate: u32,
    pub fare_estimate: u32,
    pub status: RideStatus,
    /// Winning driver pubkey (hex), set when `status == Matched`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub winner: Option<String>,
}

/// The (decrypted) payload of a location beacon.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Beacon {
    pub coord: LatLng,
    /// Optional heading in degrees (0 = north), if the device reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heading: Option<f64>,
}

// ---- Ride Request ---------------------------------------------------------

/// Build + sign a ride-request event. Emits one `g` tag per geohash precision
/// from the pickup (so drivers can filter by radius) and a NIP-40 `expiration`
/// `expiration_secs` in the future (refreshed on each 30 s re-publish). When
/// matched, adds a `p` tag naming the winning driver.
pub fn build_ride_request(keys: &Keys, req: &RideRequest, expiration_secs: u64) -> Result<Event> {
    let content = serde_json::to_string(req)?;
    let mut tags: Vec<Tag> = geohash::default_prefixes(req.pickup.lat, req.pickup.lng)
        .into_iter()
        .map(|g| Tag::custom(TagKind::custom("g"), [g]))
        .collect();
    tags.push(Tag::expiration(Timestamp::now() + expiration_secs));
    if let (RideStatus::Matched, Some(w)) = (req.status, req.winner.as_ref()) {
        let pk = PublicKey::parse(w).map_err(|e| Error::Nostr(format!("winner pubkey: {e}")))?;
        tags.push(Tag::public_key(pk));
    }
    EventBuilder::new(Kind::Custom(KIND_RIDE_REQUEST), content)
        .tags(tags)
        .sign_with_keys(keys)
        .map_err(|e| Error::Nostr(format!("sign ride request: {e}")))
}

/// Verify + parse a ride-request event into its payload.
pub fn parse_ride_request(event: &Event) -> Result<RideRequest> {
    require_kind(event, KIND_RIDE_REQUEST, "ride request")?;
    verify(event, "ride request")?;
    serde_json::from_str(&event.content).map_err(Into::into)
}

/// The geohash `g` tags on a ride request (used for proximity).
pub fn request_geohashes(event: &Event) -> Vec<String> {
    tag_values(event, "g")
}

// ---- Ride Acceptance ------------------------------------------------------

/// Build + sign a driver's acceptance referencing `request` (its `e` tag) and
/// p-tagging the passenger so they can subscribe to it.
pub fn build_acceptance(driver: &Keys, request: &Event) -> Result<Event> {
    EventBuilder::new(Kind::Custom(KIND_RIDE_ACCEPTANCE), "")
        .tags([Tag::event(request.id), Tag::public_key(request.pubkey)])
        .sign_with_keys(driver)
        .map_err(|e| Error::Nostr(format!("sign acceptance: {e}")))
}

/// Verify + reduce an acceptance event to the [`Acceptance`] arbitration record.
pub fn parse_acceptance(event: &Event) -> Result<Acceptance> {
    require_kind(event, KIND_RIDE_ACCEPTANCE, "acceptance")?;
    verify(event, "acceptance")?;
    let request_id = tag_value(event, "e")
        .ok_or_else(|| Error::Nostr("acceptance missing `e` tag".into()))?
        .to_string();
    Ok(Acceptance {
        event_id: event.id.to_hex(),
        created_at: event.created_at.as_secs(),
        driver: event.pubkey.to_hex(),
        request_id,
    })
}

// ---- Location beacon (NIP-44) ---------------------------------------------

/// Build + sign an encrypted location beacon addressed to `recipient`.
pub fn build_beacon(sender: &Keys, recipient: &PublicKey, beacon: &Beacon) -> Result<Event> {
    let plaintext = serde_json::to_string(beacon)?;
    let content = nip44::encrypt(sender.secret_key(), recipient, plaintext, nip44::Version::V2)
        .map_err(|e| Error::Nostr(format!("nip44 encrypt: {e}")))?;
    EventBuilder::new(Kind::Custom(KIND_LOCATION_BEACON), content)
        .tags([Tag::public_key(*recipient)])
        .sign_with_keys(sender)
        .map_err(|e| Error::Nostr(format!("sign beacon: {e}")))
}

/// Verify + decrypt a location beacon addressed to us.
pub fn parse_beacon(keys: &Keys, event: &Event) -> Result<Beacon> {
    require_kind(event, KIND_LOCATION_BEACON, "beacon")?;
    verify(event, "beacon")?;
    let plain = nip44::decrypt(keys.secret_key(), &event.pubkey, &event.content)
        .map_err(|e| Error::Nostr(format!("nip44 decrypt: {e}")))?;
    serde_json::from_str(&plain).map_err(Into::into)
}

// ---- Proof-of-burn events -------------------------------------------------

/// The raw 32 bytes of an event id (for notarizing it / leaf hashing).
pub fn event_id_bytes(event: &Event) -> [u8; 32] {
    event.id.to_bytes()
}

/// Build + sign an **identity bond** event — the stable, immutable target of a
/// one-time bond burn. Its event id (returned via [`Event::id`]) is what the
/// caller then notarizes. A `t` tag marks it for discovery.
pub fn build_identity_bond(keys: &Keys) -> Result<Event> {
    EventBuilder::new(Kind::Custom(KIND_IDENTITY_BOND), "")
        .tags([Tag::hashtag("nairobi-ride-bond")])
        .sign_with_keys(keys)
        .map_err(|e| Error::Nostr(format!("sign identity bond: {e}")))
}

/// Build + sign a **ride-completion attestation** — the immutable target of a
/// per-ride burn. References the request + acceptance events and p-tags the
/// counterparty so consumers can weight reputation by counterparty diversity.
pub fn build_ride_completion(
    keys: &Keys,
    request_id_hex: &str,
    acceptance_id_hex: &str,
    counterparty: &PublicKey,
    fare: u32,
    currency: &str,
) -> Result<Event> {
    let request_id =
        EventId::parse(request_id_hex).map_err(|e| Error::Nostr(format!("request id: {e}")))?;
    let acceptance_id = EventId::parse(acceptance_id_hex)
        .map_err(|e| Error::Nostr(format!("acceptance id: {e}")))?;
    EventBuilder::new(Kind::Custom(KIND_RIDE_COMPLETION), "")
        .tags([
            Tag::event(request_id),
            Tag::event(acceptance_id),
            Tag::public_key(*counterparty),
            Tag::custom(TagKind::custom("fare"), [fare.to_string(), currency.to_string()]),
        ])
        .sign_with_keys(keys)
        .map_err(|e| Error::Nostr(format!("sign ride completion: {e}")))
}

/// Build + sign a kind-30021 **upvoting event** carrying `proof`
/// (`docs/proof-of-burn-api.md` §7.1). `upvoted_author`, if given, is p-tagged.
pub fn build_upvoting_event(
    keys: &Keys,
    proof: &crate::burn::proof::BurnProof,
    upvoted_author: Option<&PublicKey>,
) -> Result<Event> {
    let n = proof.pack_n_tag();
    let mut tags: Vec<Tag> = vec![
        Tag::custom(TagKind::custom("e"), [crate::burn::to_hex(&proof.event_id)]),
        Tag::identifier(crate::burn::to_hex(&proof.leaf_hash())),
        Tag::custom(TagKind::custom("version"), [proof.version.to_string()]),
        Tag::custom(TagKind::custom("n"), n),
    ];
    if let (Some(pk), Some(sig)) = (proof.upvoter_pubkey, proof.upvoter_signature) {
        tags.push(Tag::custom(
            TagKind::custom("u"),
            [crate::burn::to_hex(&pk), crate::burn::to_hex(&sig)],
        ));
    }
    if let Some(author) = upvoted_author {
        tags.push(Tag::public_key(*author));
    }
    if let Some(chain) = &proof.chain {
        tags.push(Tag::custom(TagKind::custom("chain"), [chain.clone()]));
    }
    EventBuilder::new(Kind::Custom(KIND_UPVOTING_EVENT), "")
        .tags(tags)
        .sign_with_keys(keys)
        .map_err(|e| Error::Nostr(format!("sign upvoting event: {e}")))
}

/// Parse a kind-30021 upvoting event back into a [`crate::burn::proof::BurnProof`]
/// (`docs/proof-of-burn-api.md` §7.2). The caller verifies it on-chain.
pub fn parse_upvoting_event(event: &Event) -> Result<crate::burn::proof::BurnProof> {
    require_kind(event, KIND_UPVOTING_EVENT, "upvoting event")?;
    verify(event, "upvoting event")?;
    let event_id_hex =
        tag_value(event, "e").ok_or_else(|| Error::Nostr("upvote missing `e` tag".into()))?;
    let version: u32 = tag_value(event, "version").and_then(|v| v.parse().ok()).unwrap_or(0);
    let chain = tag_value(event, "chain").map(str::to_string);

    let n = tag_slice(event, "n").ok_or_else(|| Error::Nostr("upvote missing `n` tag".into()))?;
    if n.len() != 6 {
        return Err(Error::Nostr(format!("upvote `n` has {} values, want 6", n.len())));
    }
    let block_height: u64 = n[1].parse().map_err(|_| Error::Nostr("bad block_height".into()))?;
    let leaf_value: u64 = n[3].parse().map_err(|_| Error::Nostr("bad leaf_value".into()))?;
    let merkle_index: u64 = n[4].parse().map_err(|_| Error::Nostr("bad merkle_index".into()))?;

    let upvoter = tag_slice(event, "u").and_then(|u| match u {
        [pk, sig, ..] => Some((pk.as_str(), sig.as_str())),
        _ => None,
    });

    crate::burn::proof::proof_from_parts(
        version,
        chain,
        event_id_hex,
        &n[0],
        block_height,
        &n[2],
        leaf_value,
        merkle_index,
        &n[5],
        upvoter,
    )
}

// ---- Filters --------------------------------------------------------------

/// Subscription for nearby **open** ride requests at the given geohash prefixes
/// (the driver's area). Matched/expired are dropped client-side.
pub fn requests_filter(geohashes: &[String], since_secs_ago: u64) -> Filter {
    Filter::new()
        .kind(Kind::Custom(KIND_RIDE_REQUEST))
        .custom_tags(SingleLetterTag::lowercase(Alphabet::G), geohashes.to_vec())
        .since(Timestamp::now() - since_secs_ago)
}

/// Subscription for acceptances addressed to `passenger`.
pub fn acceptances_filter(passenger: &PublicKey, since_secs_ago: u64) -> Filter {
    Filter::new()
        .kind(Kind::Custom(KIND_RIDE_ACCEPTANCE))
        .pubkey(*passenger)
        .since(Timestamp::now() - since_secs_ago)
}

/// Subscription for location beacons addressed to `me`.
pub fn beacons_filter(me: &PublicKey, since_secs_ago: u64) -> Filter {
    Filter::new()
        .kind(Kind::Custom(KIND_LOCATION_BEACON))
        .pubkey(*me)
        .since(Timestamp::now() - since_secs_ago)
}

/// Subscription for NIP-17 gift-wrapped private DMs addressed to `me`
/// (kind 1059). The `nostr-sdk` client unwraps these into the inner message.
pub fn dm_filter(me: &PublicKey, since_secs_ago: u64) -> Filter {
    Filter::new()
        .kind(Kind::GiftWrap)
        .pubkey(*me)
        .since(Timestamp::now() - since_secs_ago)
}

/// Subscription for proof-of-burn upvoting events (kind 30021) self-published by
/// `authors` — how a client discovers a counterparty's reputation proofs.
pub fn upvoting_filter(authors: &[PublicKey], since_secs_ago: u64) -> Filter {
    Filter::new()
        .kind(Kind::Custom(KIND_UPVOTING_EVENT))
        .authors(authors.iter().copied())
        .since(Timestamp::now() - since_secs_ago)
}

// ---- NIP-40 expiry --------------------------------------------------------

/// The NIP-40 expiration timestamp (unix secs), if present.
pub fn expiration(event: &Event) -> Option<u64> {
    tag_value(event, "expiration").and_then(|v| v.parse().ok())
}

/// Whether `event` has expired as of `now` (unix secs). Clients enforce this
/// themselves rather than trusting relays to delete.
pub fn is_expired(event: &Event, now: u64) -> bool {
    expiration(event).is_some_and(|exp| now >= exp)
}

// ---- helpers --------------------------------------------------------------

fn require_kind(event: &Event, kind: u16, what: &str) -> Result<()> {
    if event.kind != Kind::Custom(kind) {
        return Err(Error::Nostr(format!(
            "not a {what}: kind {}",
            event.kind.as_u16()
        )));
    }
    Ok(())
}

fn verify(event: &Event, what: &str) -> Result<()> {
    event
        .verify()
        .map_err(|e| Error::Nostr(format!("{what} signature invalid: {e}")))
}

/// First value of the first tag named `name` (e.g. the value of an `e` tag).
fn tag_value<'a>(event: &'a Event, name: &str) -> Option<&'a str> {
    event.tags.iter().find_map(|t| {
        let parts = t.as_slice();
        (parts.first().map(String::as_str) == Some(name))
            .then(|| parts.get(1).map(String::as_str))
            .flatten()
    })
}

/// The values after the tag name of the first tag named `name` (e.g. the six
/// elements of an `n` proof tag).
fn tag_slice<'a>(event: &'a Event, name: &str) -> Option<&'a [String]> {
    event.tags.iter().find_map(|t| {
        let parts = t.as_slice();
        (parts.first().map(String::as_str) == Some(name)).then(|| &parts[1..])
    })
}

/// All values of every tag named `name` (e.g. all `g` geohashes).
fn tag_values(event: &Event, name: &str) -> Vec<String> {
    event
        .tags
        .iter()
        .filter_map(|t| {
            let parts = t.as_slice();
            if parts.first().map(String::as_str) == Some(name) {
                parts.get(1).cloned()
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::generate;

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
    fn ride_request_round_trips() {
        let keys = generate();
        let req = sample_request();
        let event = build_ride_request(&keys, &req, 90).unwrap();
        assert_eq!(event.kind, Kind::Custom(KIND_RIDE_REQUEST));
        let parsed = parse_ride_request(&event).unwrap();
        assert_eq!(parsed, req);
    }

    #[test]
    fn ride_request_carries_geohash_and_expiration_tags() {
        let keys = generate();
        let event = build_ride_request(&keys, &sample_request(), 90).unwrap();
        let ghs = request_geohashes(&event);
        assert!(ghs.len() >= 2, "expected several g tags, got {ghs:?}");
        // All are prefixes of the longest (nested).
        let longest = ghs.iter().max_by_key(|g| g.len()).unwrap();
        for g in &ghs {
            assert!(longest.starts_with(g.as_str()));
        }
        assert!(expiration(&event).is_some());
    }

    #[test]
    fn expiry_is_enforced_client_side() {
        let keys = generate();
        let event = build_ride_request(&keys, &sample_request(), 90).unwrap();
        let exp = expiration(&event).unwrap();
        assert!(!is_expired(&event, exp - 1));
        assert!(is_expired(&event, exp));
        assert!(is_expired(&event, exp + 100));
    }

    #[test]
    fn matched_request_tags_the_winner() {
        let keys = generate();
        let winner = generate().public_key();
        let mut req = sample_request();
        req.status = RideStatus::Matched;
        req.winner = Some(winner.to_hex());
        let event = build_ride_request(&keys, &req, 90).unwrap();
        // The winner is p-tagged.
        let p = tag_value(&event, "p").unwrap();
        assert_eq!(p, winner.to_hex());
        assert_eq!(parse_ride_request(&event).unwrap().winner, Some(winner.to_hex()));
    }

    #[test]
    fn parse_rejects_wrong_kind() {
        let keys = generate();
        let acc_event = build_acceptance(&keys, &build_ride_request(&keys, &sample_request(), 90).unwrap()).unwrap();
        assert!(parse_ride_request(&acc_event).is_err());
    }

    #[test]
    fn acceptance_references_request_and_passenger() {
        let passenger = generate();
        let driver = generate();
        let request = build_ride_request(&passenger, &sample_request(), 90).unwrap();
        let acc = build_acceptance(&driver, &request).unwrap();

        let parsed = parse_acceptance(&acc).unwrap();
        assert_eq!(parsed.request_id, request.id.to_hex());
        assert_eq!(parsed.driver, driver.public_key().to_hex());
        // Passenger is p-tagged so they can subscribe to it.
        assert_eq!(tag_value(&acc, "p").unwrap(), passenger.public_key().to_hex());
    }

    #[test]
    fn beacon_round_trips_for_recipient_only() {
        let driver = generate();
        let passenger = generate();
        let stranger = generate();
        let beacon = Beacon {
            coord: LatLng::new(-1.30, 36.85),
            heading: Some(42.0),
        };
        let event = build_beacon(&driver, &passenger.public_key(), &beacon).unwrap();

        // The intended recipient decrypts it.
        assert_eq!(parse_beacon(&passenger, &event).unwrap(), beacon);
        // A stranger cannot.
        assert!(parse_beacon(&stranger, &event).is_err());
    }

    #[test]
    fn filters_target_the_right_kinds() {
        let me = generate().public_key();
        let rf = requests_filter(&["u4pru".to_string()], 600);
        assert!(rf.kinds.as_ref().unwrap().contains(&Kind::Custom(KIND_RIDE_REQUEST)));
        let af = acceptances_filter(&me, 600);
        assert!(af.kinds.as_ref().unwrap().contains(&Kind::Custom(KIND_RIDE_ACCEPTANCE)));
        let bf = beacons_filter(&me, 600);
        assert!(bf.kinds.as_ref().unwrap().contains(&Kind::Custom(KIND_LOCATION_BEACON)));
    }

    #[test]
    fn identity_bond_is_immutable_and_has_a_stable_id() {
        let keys = generate();
        let bond = build_identity_bond(&keys).unwrap();
        assert_eq!(bond.kind, Kind::Custom(KIND_IDENTITY_BOND));
        assert_eq!(event_id_bytes(&bond), bond.id.to_bytes());
        // Verifies as a signed event.
        assert!(bond.verify().is_ok());
    }

    #[test]
    fn ride_completion_references_request_acceptance_and_counterparty() {
        let me = generate();
        let cp = generate();
        let req = build_ride_request(&me, &sample_request(), 90).unwrap();
        let acc = build_acceptance(&cp, &req).unwrap();
        let comp =
            build_ride_completion(&me, &req.id.to_hex(), &acc.id.to_hex(), &cp.public_key(), 370, "KES")
                .unwrap();
        assert_eq!(comp.kind, Kind::Custom(KIND_RIDE_COMPLETION));
        assert_eq!(tag_value(&comp, "p").unwrap(), cp.public_key().to_hex());
        let es = tag_values(&comp, "e");
        assert!(es.contains(&req.id.to_hex()) && es.contains(&acc.id.to_hex()));
    }

    #[test]
    fn upvoting_event_round_trips_a_signed_proof() {
        let keys = generate();
        let (mut proof, _root) = crate::burn::proof::testtree::mint_proof(
            [0x27u8; 32],
            42,
            [0x42u8; 32],
            Some([0xaau8; 32]),
            &[([1u8; 32], 8000), ([2u8; 32], 1000)],
            "ab".repeat(32).as_str(),
            900_000,
        );
        proof.upvoter_signature = Some([7u8; 64]); // dummy; parse doesn't verify
        let ev = build_upvoting_event(&keys, &proof, Some(&keys.public_key())).unwrap();
        assert_eq!(ev.kind, Kind::Custom(KIND_UPVOTING_EVENT));
        // The leaf hash is the `d` (addressable) identifier.
        assert_eq!(tag_value(&ev, "d").unwrap(), crate::burn::to_hex(&proof.leaf_hash()));
        assert_eq!(parse_upvoting_event(&ev).unwrap(), proof);
    }
}
