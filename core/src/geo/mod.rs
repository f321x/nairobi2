//! Geo layer: geohash proximity tags, distance math, and OSM-based
//! geocoding/routing — all built on a minimal rustls/ring HTTPS GET (no
//! `reqwest`/`hyper`, no second crypto stack for the Android build).
//!
//! Pure pieces (geohash, haversine, response parsing, the fallback) are
//! host-tested. The actual network calls degrade gracefully: routing falls
//! back to a haversine estimate so a fare can always be shown.

pub mod geohash;
pub mod http;
pub mod routing;

use serde::{Deserialize, Serialize};

/// A WGS84 coordinate in decimal degrees.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LatLng {
    pub lat: f64,
    pub lng: f64,
}

impl LatLng {
    /// Construct a coordinate.
    pub fn new(lat: f64, lng: f64) -> Self {
        Self { lat, lng }
    }

    /// Great-circle distance to `other` in kilometers (haversine).
    pub fn haversine_km(&self, other: &LatLng) -> f64 {
        const R: f64 = 6371.0088; // mean Earth radius, km
        let lat1 = self.lat.to_radians();
        let lat2 = other.lat.to_radians();
        let dlat = (other.lat - self.lat).to_radians();
        let dlng = (other.lng - self.lng).to_radians();
        let a =
            (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlng / 2.0).sin().powi(2);
        2.0 * R * a.sqrt().clamp(-1.0, 1.0).asin()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn haversine_one_degree_lng_at_equator() {
        // 1° of longitude at the equator ≈ 111.32 km.
        let d = LatLng::new(0.0, 0.0).haversine_km(&LatLng::new(0.0, 1.0));
        assert!((d - 111.32).abs() < 0.5, "got {d}");
    }

    #[test]
    fn haversine_zero_for_same_point() {
        let p = LatLng::new(-1.2921, 36.8219); // Nairobi
        assert!(p.haversine_km(&p).abs() < 1e-9);
    }

    #[test]
    fn haversine_known_city_pair() {
        // Nairobi CBD → Jomo Kenyatta International Airport ≈ 15 km straight line.
        let cbd = LatLng::new(-1.2864, 36.8172);
        let jkia = LatLng::new(-1.3192, 36.9278);
        let d = cbd.haversine_km(&jkia);
        assert!((12.0..18.0).contains(&d), "got {d}");
    }
}
