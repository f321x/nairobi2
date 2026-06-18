//! nairobi app: Slint UI + platform glue around `nairobi-core`.
//!
//! Entry points:
//! * Android: [`android_main`] (cdylib, loaded by `MainActivity`)
//! * Desktop dev build: `src/main.rs` (`--features desktop`)
//!
//! Mirrors ntrack's `lib.rs`, minus the headless/boot-resume path (this app has
//! no background-service mode).

slint::include_modules!();

pub mod controller;
pub mod glue;
pub mod map;
pub mod platform;
pub mod sim;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use slint::ComponentHandle;
use tokio::sync::mpsc;

use crate::controller::Controller;
use crate::platform::{Platform, PlatformEvent};

/// Create the window, controller and timers, then run the event loop until the
/// window closes. Shared by the Android and desktop entry points.
pub fn run_app(
    data_dir: PathBuf,
    platform: Arc<dyn Platform>,
    platform_rx: mpsc::UnboundedReceiver<PlatformEvent>,
) {
    // With the Fedimint backend compiled in, two rustls crypto providers are
    // linked: `ring` (what nostr-sdk and fedimint-core are wired to) and
    // `aws-lc-rs` (pulled by fedimint-connectors' iroh/quinn QUIC transport).
    // rustls 0.23 has no implicit default when more than one provider is present
    // and panics on any plain `ClientConfig::builder()`. Pin the process-wide
    // default to `ring`; iroh/quinn pick aws-lc-rs explicitly, so they are
    // unaffected. Must run before any TLS handshake (relays or the federation).
    #[cfg(feature = "fedimint")]
    {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    // Own the engine's tokio runtime here rather than inside the (shared, Arc'd)
    // Controller: several of the Controller's tasks hold a clone of it, so if it
    // owned the runtime the last clone could drop it from a worker thread — a
    // panic ("Cannot drop a runtime ... from within an asynchronous context")
    // that crashes the app when the Android Activity is recreated. Owning it
    // here means the runtime is only ever torn down from this (non-worker)
    // thread, after `ui.run()` returns.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");

    let ui = MainWindow::new().expect("failed to create main window");
    let controller = Controller::new(rt.handle().clone(), data_dir, platform, ui.as_weak());
    controller.attach(&ui);
    controller.spawn_platform_forwarder(platform_rx);

    // Drive the engine's 1 s tick (the auction escalates on ticks) and refresh
    // relative timestamps / expire toasts. Cheap and keeps the auction live.
    let tick_timer = slint::Timer::default();
    {
        let ctrl = controller.clone();
        tick_timer.start(slint::TimerMode::Repeated, Duration::from_secs(1), move || {
            ctrl.tick();
        });
    }

    ui.run().expect("event loop failed");
    controller.shutdown();
    // Tear the runtime down from this (non-worker) thread so a task's
    // Arc<Controller> clone can never trigger a drop-from-async panic.
    rt.shutdown_background();
}

#[cfg(all(target_os = "android", not(feature = "android")))]
compile_error!(
    "Android builds need the android-activity backend: pass `--no-default-features --features android` (scripts/build-apk.sh does this)"
);

/// Android entry point, invoked by the android-activity glue after
/// `MainActivity` loads this library.
#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: slint::android::AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("nairobi"),
    );
    std::panic::set_hook(Box::new(|info| {
        log::error!("panic: {info}");
    }));
    log::info!("nairobi starting");

    slint::android::init(app.clone()).expect("slint android init failed");

    let data_dir = app
        .internal_data_path()
        .unwrap_or_else(|| PathBuf::from("/data/local/tmp/nairobi"));

    let (tx, rx) = mpsc::unbounded_channel();
    let platform = match glue::AndroidPlatform::new(tx) {
        Ok(p) => p,
        Err(e) => {
            log::error!("failed to initialize android platform: {e}");
            return;
        }
    };
    run_app(data_dir, Arc::new(platform), rx);
}
