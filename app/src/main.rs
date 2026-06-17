//! Desktop development entry point with a simulated location source.
//! Build with: `cargo run -p nairobi-app --features desktop`

use std::path::PathBuf;
use std::sync::Arc;

use nairobi_app::sim::SimPlatform;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let data_dir = std::env::var_os("NAIROBI_DATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
                .map(|base| base.join("nairobi"))
                .unwrap_or_else(|| PathBuf::from("./nairobi-data"))
        });
    log::info!("data dir: {}", data_dir.display());

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let platform = SimPlatform::new(tx);
    nairobi_app::run_app(data_dir, Arc::new(platform), rx);
}
