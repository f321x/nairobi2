//! Hand-rolled geohash encoding (the standard base-32 algorithm; no external
//! crate). Ride requests carry several `g` tags at decreasing precision so a
//! driver can subscribe at whatever radius they want; this module produces
//! both a single hash and the prefix set.
//!
//! Precision reference (cell size): 4 ≈ 39×19 km, 5 ≈ 4.9×4.9 km,
//! 6 ≈ 1.2×0.6 km, 7 ≈ 153×153 m.

/// The geohash base-32 alphabet (no `a`, `i`, `l`, `o`).
const BASE32: &[u8; 32] = b"0123456789bcdefghjkmnpqrstuvwxyz";

/// Default proximity precisions emitted on a ride request (see module docs).
pub const MIN_PRECISION: usize = 4;
pub const MAX_PRECISION: usize = 7;

/// Encode a coordinate to a geohash of `precision` characters.
pub fn encode(lat: f64, lng: f64, precision: usize) -> String {
    let mut lat_range = (-90.0_f64, 90.0_f64);
    let mut lng_range = (-180.0_f64, 180.0_f64);
    let mut hash = String::with_capacity(precision);
    let mut bit = 0; // 0..=4 within the current 5-bit char
    let mut ch = 0usize; // accumulating 5-bit value
    let mut even = true; // even bit → longitude, odd → latitude

    while hash.len() < precision {
        if even {
            let mid = (lng_range.0 + lng_range.1) / 2.0;
            if lng >= mid {
                ch |= 1 << (4 - bit);
                lng_range.0 = mid;
            } else {
                lng_range.1 = mid;
            }
        } else {
            let mid = (lat_range.0 + lat_range.1) / 2.0;
            if lat >= mid {
                ch |= 1 << (4 - bit);
                lat_range.0 = mid;
            } else {
                lat_range.1 = mid;
            }
        }
        even = !even;

        if bit < 4 {
            bit += 1;
        } else {
            hash.push(BASE32[ch] as char);
            bit = 0;
            ch = 0;
        }
    }
    hash
}

/// Geohash prefixes from `min_p` to `max_p` inclusive (nested), for emitting as
/// multiple `g` tags. Returns the longest first is *not* guaranteed; order is
/// ascending precision.
pub fn prefixes(lat: f64, lng: f64, min_p: usize, max_p: usize) -> Vec<String> {
    let full = encode(lat, lng, max_p);
    (min_p..=max_p).map(|p| full[..p].to_string()).collect()
}

/// The default proximity prefix set ([`MIN_PRECISION`]..=[`MAX_PRECISION`]).
pub fn default_prefixes(lat: f64, lng: f64) -> Vec<String> {
    prefixes(lat, lng, MIN_PRECISION, MAX_PRECISION)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classic_reference_vectors() {
        // Well-known geohash test vectors.
        assert_eq!(encode(57.64911, 10.40744, 11), "u4pruydqqvj");
        assert_eq!(encode(42.6, -5.6, 5), "ezs42");
    }

    #[test]
    fn nairobi_encodes_stably() {
        let h = encode(-1.2921, 36.8219, 7);
        assert_eq!(h.len(), 7);
        // Deterministic.
        assert_eq!(h, encode(-1.2921, 36.8219, 7));
    }

    #[test]
    fn prefixes_are_nested_and_ascending() {
        let ps = prefixes(-1.2921, 36.8219, 4, 7);
        assert_eq!(ps.len(), 4);
        assert_eq!(ps[0].len(), 4);
        assert_eq!(ps[3].len(), 7);
        // Each shorter prefix is a prefix of the next longer one.
        for w in ps.windows(2) {
            assert!(w[1].starts_with(&w[0]), "{} not prefix of {}", w[0], w[1]);
        }
    }

    #[test]
    fn nearby_points_share_a_coarse_prefix() {
        // Two points ~1 km apart should share the precision-5 (~5 km) cell.
        let a = default_prefixes(-1.2921, 36.8219);
        let b = default_prefixes(-1.2980, 36.8260);
        assert_eq!(a[1], b[1], "precision-5 prefixes should match for nearby points");
    }
}
