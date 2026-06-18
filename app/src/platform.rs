//! Platform abstraction: everything the app needs from the OS that is not
//! covered by the UI toolkit. Android implements this with JNI calls into the
//! Java `LocationBridge` (see [`crate::glue`]); the desktop dev build uses a
//! simulator ([`crate::sim`]).
//!
//! Mirrors ntrack's `platform.rs`, trimmed to the rideshare surface: location
//! + permission, the driver NAVIGATE hand-off (`open_nav`), and notifications.

use nairobi_core::geo::LatLng;

/// Events flowing from the platform into the controller.
#[derive(Debug, Clone)]
pub enum PlatformEvent {
    /// A fresh GPS fix.
    Location(LatLng),
    /// Result of a permission request triggered by
    /// [`Platform::request_location_permission`].
    PermissionResult(bool),
    /// The system back gesture / button was pressed. The controller maps it to
    /// the previous in-app screen, and only exits the app when already at Home.
    Back,
}

/// The OS surface the app needs beyond the UI toolkit. A single trait with two
/// implementations selected at the entry point: [`crate::glue::AndroidPlatform`]
/// (JNI) and [`crate::sim::SimPlatform`] (desktop simulator).
pub trait Platform: Send + Sync + 'static {
    /// Whether location permission is already granted.
    fn has_location_permission(&self) -> bool;
    /// Ask the OS for location permission. The outcome arrives asynchronously as
    /// [`PlatformEvent::PermissionResult`].
    fn request_location_permission(&self);
    /// Start platform location updates at the given cadence (and on Android the
    /// foreground service that keeps them alive in the background).
    fn start_location(&self, interval_ms: u64);
    /// Stop platform location updates.
    fn stop_location(&self);
    /// The driver NAVIGATE hand-off: launch external turn-by-turn navigation to
    /// (`lat`,`lng`) labelled `label` (Android `ACTION_VIEW` on a `geo:` URI).
    /// Fire-and-forget.
    fn open_nav(&self, lat: f64, lng: f64, label: &str);
    /// Open a web `url` in the system browser (Android `ACTION_VIEW` on the
    /// `https:` URI) — used to inspect a proof-of-burn notarization transaction
    /// on a block explorer. Fire-and-forget.
    fn open_url(&self, url: &str);
    /// Copy `text` to the system clipboard — used so a Lightning invoice / on-chain
    /// deposit address can be pasted into another wallet to fund this one. Fire-and-forget.
    fn copy_to_clipboard(&self, text: &str);
    /// Raise a notification the user should see even when the app is backgrounded
    /// (a match found, a driver arriving). Fire-and-forget.
    fn notify(&self, title: &str, body: &str);
    /// Close the app (finish the Android activity). Called by the controller when
    /// the back gesture is pressed while already on the Home screen, so back from
    /// the root exits as the user expects. Fire-and-forget; a no-op off Android.
    fn exit_app(&self);
}
