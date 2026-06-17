//! In-app OSM "slippy" map for the ride screens.
//!
//! Ported from ntrack's `map.rs`, but **all** projection / tile math is
//! delegated to [`nairobi_core::geo::tiles`] and tiles are fetched with
//! [`nairobi_core::geo::http::https_get`] (the same rustls/ring HTTPS stack the
//! relay layer uses — no new crypto/cross-language dependency). PNG tiles are
//! decoded with the `image` crate.
//!
//! [`MapState`] holds the view (centre, zoom, viewport) and a tile cache. Tile
//! fetches happen off-thread and update the cache; [`MapState::render`] runs on
//! the UI thread and composes the cached tiles into a single
//! [`slint::Image`]. Pickup/dropoff pins and self/other dots are placed by the
//! `.slint` overlay using offsets computed via [`nairobi_core::geo::tiles`]
//! (see [`MapState::offset`]); there is intentionally **no route polyline**.

use std::collections::{HashMap, VecDeque};

use nairobi_core::geo::http;
use nairobi_core::geo::tiles::{self, Placement, TileId, TILE_SIZE};
use slint::{Rgba8Pixel, SharedPixelBuffer};

/// OSM raster tile host.
const TILE_HOST: &str = "tile.openstreetmap.org";
/// Overscan (px) around the viewport when choosing tiles to fetch, so a short
/// pan reveals already-loaded imagery instead of blank space.
pub const TILE_MARGIN: f64 = 128.0;
/// Cap on cached tiles (≈ a few screenfuls). Oldest-inserted are evicted.
const CACHE_CAP: usize = 256;

/// Cache slot for one tile.
pub enum TileSlot {
    /// A fetch is in flight.
    Loading,
    /// Decoded RGBA pixels, ready to blit at render time.
    Loaded(SharedPixelBuffer<Rgba8Pixel>),
    /// Fetch or decode failed; not retried until the cache is cleared.
    Failed,
}

/// View + tile cache backing a map. Lives inside the controller's view state
/// (behind its mutex); fetch tasks update the cache, [`MapState::render`] reads
/// it on the UI thread.
pub struct MapState {
    pub center_lat: f64,
    pub center_lng: f64,
    pub zoom: u32,
    /// Last-reported viewport size (px); seeded with a phone-ish default so we
    /// fetch a sensible set even before the UI reports its real geometry.
    pub vw: f64,
    pub vh: f64,
    tiles: HashMap<TileId, TileSlot>,
    /// Insertion order, for FIFO eviction past [`CACHE_CAP`].
    order: VecDeque<TileId>,
}

impl Default for MapState {
    fn default() -> Self {
        Self {
            // Centre on Nairobi at a street-ish zoom until the controller frames
            // the actual ride.
            center_lat: -1.2921,
            center_lng: 36.8219,
            zoom: 14,
            vw: 400.0,
            vh: 600.0,
            tiles: HashMap::new(),
            order: VecDeque::new(),
        }
    }
}

impl MapState {
    pub fn contains(&self, id: &TileId) -> bool {
        self.tiles.contains_key(id)
    }

    /// Insert or replace a tile, evicting the oldest entries past the cap.
    pub fn insert(&mut self, id: TileId, slot: TileSlot) {
        if self.tiles.insert(id, slot).is_none() {
            self.order.push_back(id);
        }
        while self.order.len() > CACHE_CAP {
            if let Some(old) = self.order.pop_front() {
                self.tiles.remove(&old);
            }
        }
    }

    /// Center the view on (`lat`,`lng`) at `zoom`.
    pub fn center_on(&mut self, lat: f64, lng: f64, zoom: u32) {
        self.center_lat = lat;
        self.center_lng = lng;
        self.zoom = zoom.clamp(tiles::MIN_ZOOM, tiles::MAX_ZOOM);
    }

    /// Set the viewport size; returns `true` when it actually changed (so the
    /// caller can avoid a needless refetch).
    pub fn set_viewport(&mut self, w: f64, h: f64) -> bool {
        if w <= 0.0 || h <= 0.0 {
            return false;
        }
        if (self.vw - w).abs() < 0.5 && (self.vh - h).abs() < 0.5 {
            return false;
        }
        self.vw = w;
        self.vh = h;
        true
    }

    /// Apply a content drag of (`dx`,`dy`) px to the centre.
    pub fn pan(&mut self, dx: f64, dy: f64) {
        let (lat, lng) = tiles::pan(self.center_lat, self.center_lng, self.zoom, dx, dy);
        self.center_lat = lat;
        self.center_lng = lng;
    }

    /// Step the zoom by `delta`, clamped to the allowed range.
    pub fn zoom_by(&mut self, delta: i32) {
        let z = (self.zoom as i32 + delta).clamp(tiles::MIN_ZOOM as i32, tiles::MAX_ZOOM as i32);
        self.zoom = z as u32;
    }

    /// The lat/lng under a viewport pixel measured from the centre (used to drop
    /// a dropoff pin where the user taps). The overlay reports a tap relative to
    /// the viewport centre.
    pub fn tap_to_latlng(&self, px: f64, py: f64) -> (f64, f64) {
        tiles::tap_to_latlng(self.center_lat, self.center_lng, self.zoom, px, py)
    }

    /// Offset (px) of a point from the viewport centre, for placing a pin/dot in
    /// the `.slint` overlay.
    pub fn offset(&self, lat: f64, lng: f64) -> (f64, f64) {
        tiles::marker_offset(self.center_lat, self.center_lng, lat, lng, self.zoom)
    }

    /// Tiles currently missing from the cache for the visible viewport, marked
    /// `Loading` so they aren't refetched. The caller spawns a fetch per id.
    pub fn missing_tiles(&mut self) -> Vec<TileId> {
        let placements = tiles::visible_tiles(
            self.center_lat,
            self.center_lng,
            self.zoom,
            self.vw,
            self.vh,
            TILE_MARGIN,
        );
        let mut fetch = Vec::new();
        for p in &placements {
            if !self.contains(&p.id) {
                self.insert(p.id, TileSlot::Loading);
                fetch.push(p.id);
            }
        }
        fetch
    }

    /// Compose the cached tiles into one image sized `width`×`height` (logical
    /// px). UI-thread-only and idempotent — missing tiles are simply skipped
    /// (they fill in on the next render once their fetch lands).
    pub fn render(&self, width: u32, height: u32) -> slint::Image {
        let (w, h) = (width.max(1), height.max(1));
        let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(w, h);
        let stride = w as usize;
        // Backing colour for not-yet-loaded tiles (OSM land grey).
        {
            let px = buf.make_mut_slice();
            for p in px.iter_mut() {
                *p = Rgba8Pixel {
                    r: 0x20,
                    g: 0x23,
                    b: 0x2c,
                    a: 0xff,
                };
            }
        }

        let cx = w as f64 / 2.0;
        let cy = h as f64 / 2.0;
        let placements = tiles::visible_tiles(
            self.center_lat,
            self.center_lng,
            self.zoom,
            w as f64,
            h as f64,
            0.0,
        );
        let dst = buf.make_mut_slice();
        for Placement { id, dx, dy } in placements {
            let Some(TileSlot::Loaded(tile)) = self.tiles.get(&id) else {
                continue;
            };
            blit(dst, stride, w, h, tile, cx + dx, cy + dy);
        }
        slint::Image::from_rgba8(buf)
    }
}

/// Blit `tile` into `dst` (a `w`×`h` RGBA buffer with row `stride`) with its
/// top-left corner at the (possibly negative, possibly fractional) destination
/// pixel (`tx`,`ty`), clipping to the buffer bounds. Nearest-neighbour: the OSM
/// tiles are already at the target zoom, so source and destination are 1:1.
fn blit(
    dst: &mut [Rgba8Pixel],
    stride: usize,
    w: u32,
    h: u32,
    tile: &SharedPixelBuffer<Rgba8Pixel>,
    tx: f64,
    ty: f64,
) {
    let tw = tile.width() as i64;
    let th = tile.height() as i64;
    let src = tile.as_slice();
    let ox = tx.round() as i64;
    let oy = ty.round() as i64;
    let x0 = ox.max(0);
    let y0 = oy.max(0);
    let x1 = (ox + tw).min(w as i64);
    let y1 = (oy + th).min(h as i64);
    for dy in y0..y1 {
        let sy = (dy - oy) as usize;
        for dx in x0..x1 {
            let sx = (dx - ox) as usize;
            let s = src[sy * tile.width() as usize + sx];
            dst[dy as usize * stride + dx as usize] = s;
        }
    }
}

/// Fetch and decode one OSM tile via the core HTTPS GET. `None` on any
/// network/HTTP/decode error.
pub async fn fetch_tile(id: TileId) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let path = tiles::tile_path(id);
    let bytes = http::https_get(TILE_HOST, &path, "image/png").await.ok()?;
    decode_png(&bytes)
}

fn decode_png(bytes: &[u8]) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(w, h);
    buf.make_mut_bytes().copy_from_slice(img.as_raw());
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_evicts_oldest_past_cap() {
        let mut m = MapState::default();
        for i in 0..(CACHE_CAP as u32 + 10) {
            m.insert(TileId { z: 14, x: i, y: 0 }, TileSlot::Failed);
        }
        assert!(m.tiles.len() <= CACHE_CAP);
        assert!(!m.contains(&TileId { z: 14, x: 0, y: 0 }));
        assert!(m.contains(&TileId {
            z: 14,
            x: CACHE_CAP as u32 + 9,
            y: 0
        }));
    }

    #[test]
    fn viewport_change_is_detected() {
        let mut m = MapState::default();
        assert!(m.set_viewport(500.0, 900.0));
        assert!(!m.set_viewport(500.0, 900.0)); // unchanged
        assert!(!m.set_viewport(0.0, 0.0)); // ignored
    }

    #[test]
    fn missing_tiles_marks_loading() {
        let mut m = MapState::default();
        m.set_viewport(512.0, 512.0);
        let first = m.missing_tiles();
        assert!(!first.is_empty());
        // Already marked Loading → not returned again.
        let second = m.missing_tiles();
        assert!(second.is_empty());
    }

    #[test]
    fn render_produces_image_of_requested_size() {
        let m = MapState::default();
        let img = m.render(320, 480);
        assert_eq!(img.size().width, 320);
        assert_eq!(img.size().height, 480);
    }

    #[test]
    fn zoom_clamps_to_range() {
        let mut m = MapState::default();
        m.center_on(-1.29, 36.82, 999);
        assert_eq!(m.zoom, tiles::MAX_ZOOM);
        m.zoom_by(-100);
        assert_eq!(m.zoom, tiles::MIN_ZOOM);
    }
}
