//! rend-server entry point.

use rend_server::Config;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rend_server=info,tower_http=info".into()),
        )
        .init();

    let config = Config {
        listen: std::env::var("REND_LISTEN")
            .unwrap_or_else(|_| "0.0.0.0:8080".to_string()),
        data_dir: std::env::var("REND_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/data")),
        // Soft cap on per-tx fuel — generous enough for typical
        // contracts, low enough to bound a stuck tx's wall-clock.
        // Override via env for batch-style imports etc.
        fuel: std::env::var("REND_FUEL")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(2_000_000),
    };

    rend_server::serve(config).await
}
