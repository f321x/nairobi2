//! OpenStreetMap geocoding (Nominatim) and driving-route distance (OSRM).
//!
//! URL building and response parsing are pure and host-tested. The network
//! calls are thin wrappers over [`super::http`]. Routing degrades gracefully:
//! on any failure it returns a haversine × road-factor estimate flagged
//! `approximate`, so the app can always show a distance and fare.

use crate::error::{Error, Result};
use crate::geo::http::https_get_str;
use crate::geo::LatLng;
use serde::Deserialize;

/// Public Nominatim geocoder host.
pub const NOMINATIM_HOST: &str = "nominatim.openstreetmap.org";
/// Public OSRM routing host (demo server). `routing.openstreetmap.de` is an
/// alternate; configurable later.
pub const OSRM_HOST: &str = "router.project-osrm.org";

/// Straight-line → road-distance inflation used when OSRM is unavailable.
pub const ROAD_FACTOR: f64 = 1.3;
/// Rough urban average speed (km/h) for the fallback ETA.
const FALLBACK_SPEED_KMH: f64 = 30.0;

/// A geocoding search result.
#[derive(Clone, Debug, PartialEq)]
pub struct Place {
    pub coord: LatLng,
    pub label: String,
}

/// Distance + duration of a driving route.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RouteInfo {
    pub distance_km: f64,
    pub duration_min: f64,
    /// `true` when this is the haversine fallback rather than a real OSRM route.
    pub approximate: bool,
}

// ---- Nominatim ------------------------------------------------------------

#[derive(Deserialize)]
struct NominatimItem {
    lat: String,
    lon: String,
    display_name: String,
}

/// Build the Nominatim search path for a free-text query (`format=jsonv2`).
pub fn nominatim_path(query: &str) -> String {
    format!("/search?q={}&format=jsonv2&limit=5", urlencode(query))
}

/// Parse a Nominatim `jsonv2` response body into places.
pub fn parse_nominatim(body: &str) -> Result<Vec<Place>> {
    let items: Vec<NominatimItem> = serde_json::from_str(body)?;
    items
        .into_iter()
        .map(|i| {
            let lat = i
                .lat
                .parse::<f64>()
                .map_err(|e| Error::Geo(format!("bad lat {:?}: {e}", i.lat)))?;
            let lng = i
                .lon
                .parse::<f64>()
                .map_err(|e| Error::Geo(format!("bad lon {:?}: {e}", i.lon)))?;
            Ok(Place {
                coord: LatLng::new(lat, lng),
                label: i.display_name,
            })
        })
        .collect()
}

/// Geocode a free-text query via Nominatim. Network/parse errors → `Err`.
pub async fn geocode(query: &str) -> Result<Vec<Place>> {
    let body = https_get_str(NOMINATIM_HOST, &nominatim_path(query)).await?;
    parse_nominatim(&body)
}

// ---- OSRM -----------------------------------------------------------------

#[derive(Deserialize)]
struct OsrmResponse {
    code: String,
    #[serde(default)]
    routes: Vec<OsrmRoute>,
}

#[derive(Deserialize)]
struct OsrmRoute {
    distance: f64, // meters
    duration: f64, // seconds
}

/// Build the OSRM driving-route path (note OSRM expects `lng,lat` order).
pub fn osrm_path(from: LatLng, to: LatLng) -> String {
    format!(
        "/route/v1/driving/{},{};{},{}?overview=false",
        from.lng, from.lat, to.lng, to.lat
    )
}

/// Parse an OSRM `/route` response into a real (`approximate = false`) route.
pub fn parse_osrm(body: &str) -> Result<RouteInfo> {
    let resp: OsrmResponse = serde_json::from_str(body)?;
    if resp.code != "Ok" {
        return Err(Error::Geo(format!("OSRM code: {}", resp.code)));
    }
    let r = resp
        .routes
        .first()
        .ok_or_else(|| Error::Geo("OSRM returned no routes".into()))?;
    Ok(RouteInfo {
        distance_km: r.distance / 1000.0,
        duration_min: r.duration / 60.0,
        approximate: false,
    })
}

/// Haversine × [`ROAD_FACTOR`] estimate, used when OSRM is unavailable.
pub fn fallback_distance(from: LatLng, to: LatLng) -> RouteInfo {
    let km = from.haversine_km(&to) * ROAD_FACTOR;
    RouteInfo {
        distance_km: km,
        duration_min: km / FALLBACK_SPEED_KMH * 60.0,
        approximate: true,
    }
}

/// Driving route distance via OSRM, falling back to a haversine estimate on any
/// network/HTTP/parse failure (so a fare can always be computed).
pub async fn route(from: LatLng, to: LatLng) -> RouteInfo {
    match https_get_str(OSRM_HOST, &osrm_path(from, to)).await {
        Ok(body) => match parse_osrm(&body) {
            Ok(info) => info,
            Err(e) => {
                log::warn!("OSRM parse failed ({e}); using haversine fallback");
                fallback_distance(from, to)
            }
        },
        Err(e) => {
            log::warn!("OSRM request failed ({e}); using haversine fallback");
            fallback_distance(from, to)
        }
    }
}

// ---- helpers --------------------------------------------------------------

/// Percent-encode a URL query component (RFC 3986 unreserved set kept literal).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nominatim_path_encodes_query() {
        assert_eq!(
            nominatim_path("Nairobi CBD"),
            "/search?q=Nairobi%20CBD&format=jsonv2&limit=5"
        );
        assert_eq!(
            nominatim_path("café & bar"),
            "/search?q=caf%C3%A9%20%26%20bar&format=jsonv2&limit=5"
        );
    }

    #[test]
    fn parses_nominatim_response() {
        let body = r#"[
            {"lat":"-1.2864","lon":"36.8172","display_name":"Nairobi CBD, Kenya"},
            {"lat":"-1.3192","lon":"36.9278","display_name":"JKIA, Nairobi, Kenya"}
        ]"#;
        let places = parse_nominatim(body).unwrap();
        assert_eq!(places.len(), 2);
        assert_eq!(places[0].coord, LatLng::new(-1.2864, 36.8172));
        assert_eq!(places[1].label, "JKIA, Nairobi, Kenya");
    }

    #[test]
    fn empty_nominatim_response_is_empty_vec() {
        assert!(parse_nominatim("[]").unwrap().is_empty());
    }

    #[test]
    fn osrm_path_uses_lng_lat_order() {
        let p = osrm_path(LatLng::new(-1.2864, 36.8172), LatLng::new(-1.3192, 36.9278));
        assert_eq!(
            p,
            "/route/v1/driving/36.8172,-1.2864;36.9278,-1.3192?overview=false"
        );
    }

    #[test]
    fn parses_osrm_ok_response() {
        let body = r#"{"code":"Ok","routes":[{"distance":18500.0,"duration":1500.0}]}"#;
        let info = parse_osrm(body).unwrap();
        assert!((info.distance_km - 18.5).abs() < 1e-9);
        assert!((info.duration_min - 25.0).abs() < 1e-9);
        assert!(!info.approximate);
    }

    #[test]
    fn osrm_error_code_is_err() {
        let body = r#"{"code":"NoRoute","routes":[]}"#;
        assert!(parse_osrm(body).is_err());
    }

    #[test]
    fn osrm_no_routes_is_err() {
        let body = r#"{"code":"Ok","routes":[]}"#;
        assert!(parse_osrm(body).is_err());
    }

    #[test]
    fn fallback_is_haversine_times_road_factor_and_approximate() {
        let from = LatLng::new(-1.2864, 36.8172);
        let to = LatLng::new(-1.3192, 36.9278);
        let info = fallback_distance(from, to);
        let expected = from.haversine_km(&to) * ROAD_FACTOR;
        assert!((info.distance_km - expected).abs() < 1e-9);
        assert!(info.approximate);
        assert!(info.duration_min > 0.0);
    }
}
