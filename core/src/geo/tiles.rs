//! Pure Web Mercator projection + OSM tile math, host-tested. The Slint
//! rendering and PNG decoding live in the app crate; everything here is
//! UI-free so the geometry is testable off-device. Ported from ntrack's map.

/// Side length of an OSM tile, in (logical) pixels.
pub const TILE_SIZE: f64 = 256.0;
/// Allowed zoom range: `MIN` ≈ a continent, `MAX` ≈ street level.
pub const MIN_ZOOM: u32 = 2;
pub const MAX_ZOOM: u32 = 18;
/// Web Mercator is only defined up to this latitude.
const MAX_LAT: f64 = 85.051_128_779_806_59;

/// An OSM tile coordinate (`z/x/y`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TileId {
    pub z: u32,
    pub x: u32,
    pub y: u32,
}

/// A tile placed in the viewport: `dx`/`dy` is its top-left corner's offset
/// (px) from the viewport centre.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub id: TileId,
    pub dx: f64,
    pub dy: f64,
}

/// World-pixel extent at zoom `z` (`256 · 2^z`).
fn world_size(z: u32) -> f64 {
    TILE_SIZE * f64::from(1u32 << z)
}

/// Project lat/lng (degrees) to world-pixel coordinates at zoom `z`.
pub fn project(lat: f64, lng: f64, z: u32) -> (f64, f64) {
    let lat = lat.clamp(-MAX_LAT, MAX_LAT);
    let s = world_size(z);
    let x = (lng + 180.0) / 360.0 * s;
    let sin = lat.to_radians().sin();
    let y = (0.5 - ((1.0 + sin) / (1.0 - sin)).ln() / (4.0 * std::f64::consts::PI)) * s;
    (x, y)
}

/// Inverse of [`project`].
pub fn unproject(x: f64, y: f64, z: u32) -> (f64, f64) {
    let s = world_size(z);
    let lng = x / s * 360.0 - 180.0;
    let n = std::f64::consts::PI * (1.0 - 2.0 * y / s);
    let lat = n.sinh().atan().to_degrees();
    (lat, lng)
}

/// Tiles covering a `vw`×`vh` viewport centred at (`lat`,`lng`) at zoom `z`,
/// plus a `margin` (px) overscan. Out-of-range rows are skipped; columns wrap.
pub fn visible_tiles(lat: f64, lng: f64, z: u32, vw: f64, vh: f64, margin: f64) -> Vec<Placement> {
    let (cx, cy) = project(lat, lng, z);
    let half_w = vw / 2.0 + margin;
    let half_h = vh / 2.0 + margin;
    let min_tx = ((cx - half_w) / TILE_SIZE).floor() as i64;
    let max_tx = ((cx + half_w) / TILE_SIZE).floor() as i64;
    let min_ty = ((cy - half_h) / TILE_SIZE).floor() as i64;
    let max_ty = ((cy + half_h) / TILE_SIZE).floor() as i64;
    let n = 1i64 << z;
    let mut out = Vec::new();
    for ty in min_ty..=max_ty {
        if ty < 0 || ty >= n {
            continue; // no vertical wrap
        }
        for tx in min_tx..=max_tx {
            let id = TileId {
                z,
                x: tx.rem_euclid(n) as u32, // horizontal wrap
                y: ty as u32,
            };
            out.push(Placement {
                id,
                dx: tx as f64 * TILE_SIZE - cx,
                dy: ty as f64 * TILE_SIZE - cy,
            });
        }
    }
    out
}

/// Offset (px) of a point from the viewport centre at zoom `z` (for placing a
/// pickup pin / driver dot on the map).
pub fn marker_offset(center_lat: f64, center_lng: f64, lat: f64, lng: f64, z: u32) -> (f64, f64) {
    let (cx, cy) = project(center_lat, center_lng, z);
    let (px, py) = project(lat, lng, z);
    (px - cx, py - cy)
}

/// New centre after dragging the map content by (`dx`,`dy`) px.
pub fn pan(center_lat: f64, center_lng: f64, z: u32, dx: f64, dy: f64) -> (f64, f64) {
    let (cx, cy) = project(center_lat, center_lng, z);
    unproject(cx - dx, cy - dy, z)
}

/// Lat/lng under a tap at viewport pixel (`px`,`py`) measured from the centre
/// (used to drop a dropoff pin where the user taps).
pub fn tap_to_latlng(center_lat: f64, center_lng: f64, z: u32, px: f64, py: f64) -> (f64, f64) {
    let (cx, cy) = project(center_lat, center_lng, z);
    unproject(cx + px, cy + py, z)
}

/// Centre + zoom framing all `points` (lat,lng) inside a `vw`×`vh` viewport.
/// Empty → a world view; a single point → a sensible street-level zoom.
pub fn fit(points: &[(f64, f64)], vw: f64, vh: f64) -> (f64, f64, u32) {
    match points {
        [] => return (20.0, 0.0, MIN_ZOOM),
        [(lat, lng)] => return (*lat, *lng, 14),
        _ => {}
    }
    let mut min_lat = 90.0f64;
    let mut max_lat = -90.0f64;
    let mut min_lng = 180.0f64;
    let mut max_lng = -180.0f64;
    for &(lat, lng) in points {
        min_lat = min_lat.min(lat);
        max_lat = max_lat.max(lat);
        min_lng = min_lng.min(lng);
        max_lng = max_lng.max(lng);
    }
    let center_lat = (min_lat + max_lat) / 2.0;
    let center_lng = (min_lng + max_lng) / 2.0;
    let budget_w = (vw * 0.8).max(64.0);
    let budget_h = (vh * 0.8).max(64.0);
    let mut zoom = MIN_ZOOM;
    for z in (MIN_ZOOM..=MAX_ZOOM).rev() {
        let (x0, y0) = project(max_lat, min_lng, z); // top-left
        let (x1, y1) = project(min_lat, max_lng, z); // bottom-right
        if (x1 - x0).abs() <= budget_w && (y1 - y0).abs() <= budget_h {
            zoom = z;
            break;
        }
    }
    (center_lat, center_lng, zoom)
}

/// Path for an OSM raster tile: `/z/x/y.png`.
pub fn tile_path(id: TileId) -> String {
    format!("/{}/{}/{}.png", id.z, id.x, id.y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_unproject_round_trips() {
        for &(lat, lng) in &[(0.0, 0.0), (-1.2921, 36.8219), (48.2, 16.37), (-33.87, 151.21)] {
            for z in [2, 8, 14, 18] {
                let (x, y) = project(lat, lng, z);
                let (lat2, lng2) = unproject(x, y, z);
                assert!((lat - lat2).abs() < 1e-6, "lat {lat}->{lat2} @z{z}");
                assert!((lng - lng2).abs() < 1e-6, "lng {lng}->{lng2} @z{z}");
            }
        }
    }

    #[test]
    fn visible_tiles_cover_viewport() {
        let tiles = visible_tiles(-1.2921, 36.8219, 14, 512.0, 512.0, 64.0);
        assert!(!tiles.is_empty());
        // All tiles are within the zoom's tile grid.
        let n = 1u32 << 14;
        for p in &tiles {
            assert!(p.id.x < n && p.id.y < n);
        }
    }

    #[test]
    fn pan_then_unpan_returns_to_start() {
        let (lat, lng, z) = (-1.2921, 36.8219, 15);
        let (lat2, lng2) = pan(lat, lng, z, 50.0, -30.0);
        let (lat3, lng3) = pan(lat2, lng2, z, -50.0, 30.0);
        assert!((lat - lat3).abs() < 1e-6 && (lng - lng3).abs() < 1e-6);
    }

    #[test]
    fn tap_maps_center_to_itself() {
        let (lat, lng, z) = (-1.2921, 36.8219, 15);
        let (lat2, lng2) = tap_to_latlng(lat, lng, z, 0.0, 0.0);
        assert!((lat - lat2).abs() < 1e-9 && (lng - lng2).abs() < 1e-9);
    }

    #[test]
    fn fit_handles_empty_single_and_pair() {
        assert_eq!(fit(&[], 512.0, 512.0).2, MIN_ZOOM);
        assert_eq!(fit(&[(-1.29, 36.82)], 512.0, 512.0).2, 14);
        let (clat, clng, z) = fit(&[(-1.2864, 36.8172), (-1.3192, 36.9278)], 512.0, 512.0);
        assert!((clat - (-1.3028)).abs() < 0.01);
        assert!((clng - 36.8725).abs() < 0.01);
        assert!((MIN_ZOOM..=MAX_ZOOM).contains(&z));
    }

    #[test]
    fn tile_path_format() {
        assert_eq!(tile_path(TileId { z: 14, x: 9876, y: 5432 }), "/14/9876/5432.png");
    }
}
