//! Desktop development platform: a simulated GPS source and logged stand-ins
//! for the Android intents. Lets the full app (UI ↔ engine ↔ relays) run on a
//! workstation: `cargo run -p nairobi-app --features desktop`.
//!
//! Mirrors ntrack's `sim.rs`, walking a fix near Nairobi rather than Munich.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use nairobi_core::geo::LatLng;
use tokio::sync::mpsc;

use crate::platform::{Platform, PlatformEvent};

/// Nairobi CBD — the centre of the simulated walk.
const NAIROBI: LatLng = LatLng {
    lat: -1.2921,
    lng: 36.8219,
};

pub struct SimPlatform {
    tx: mpsc::UnboundedSender<PlatformEvent>,
    running: Arc<AtomicBool>,
    step: Arc<AtomicU64>,
}

impl SimPlatform {
    pub fn new(tx: mpsc::UnboundedSender<PlatformEvent>) -> Self {
        Self {
            tx,
            running: Arc::new(AtomicBool::new(false)),
            step: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl Platform for SimPlatform {
    fn has_location_permission(&self) -> bool {
        true
    }

    fn request_location_permission(&self) {
        let _ = self.tx.send(PlatformEvent::PermissionResult(true));
    }

    fn start_location(&self, _interval_ms: u64) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        log::info!("sim: location updates started");
        let tx = self.tx.clone();
        let running = self.running.clone();
        let step = self.step.clone();
        std::thread::spawn(move || {
            while running.load(Ordering::SeqCst) {
                let i = step.fetch_add(1, Ordering::SeqCst) as f64;
                // A slow drift in a small circle around Nairobi CBD, so the map
                // and the driver↔passenger distance change over time.
                let r = 0.002;
                let sample = LatLng::new(
                    NAIROBI.lat + r * (i / 20.0).sin(),
                    NAIROBI.lng + r * (i / 20.0).cos(),
                );
                if tx.send(PlatformEvent::Location(sample)).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_secs(2));
            }
            log::info!("sim: location updates stopped");
        });
    }

    fn stop_location(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    fn open_nav(&self, lat: f64, lng: f64, label: &str) {
        log::info!(
            "sim: open nav to {label}: https://www.openstreetmap.org/?mlat={lat}&mlon={lng}#map=16/{lat}/{lng}"
        );
    }

    fn notify(&self, title: &str, body: &str) {
        // The desktop build has no notification surface; log it loudly so the
        // match/arrival flows can still be exercised on a workstation.
        log::warn!("sim: 🔔 NOTIFICATION — {title}: {body}");
    }
}
