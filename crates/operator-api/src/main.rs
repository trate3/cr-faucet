use anyhow::Result;
use operator_api::{router, AppState};
use pool_core::cache::RateCache;
use pool_core::metrics::Metrics;
use pool_core::store::Store;
use pool_core::Config;
use std::env;
use std::sync::Arc;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cfg = Config::load(env::var("POOL_CONFIG").unwrap_or_else(|_| "pool.toml".into()))?;
    let store = Store::connect(&cfg.redis.url).await?;
    let metrics = Arc::new(Metrics::new());
    let rate = Arc::new(RateCache::new());
    let onion = std::fs::read_to_string(
        std::path::Path::new(&cfg.tor.hidden_service_dir).join("hostname"),
    )
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty());
    let app = router(AppState {
        store,
        metrics,
        rate,
        upstream_stats: Arc::new(parking_lot::RwLock::new(None)),
        upstream_stats_as_of_unix: Arc::new(std::sync::atomic::AtomicI64::new(0)),
        onion,
    });
    info!(bind=%cfg.operator_api.bind, "operator-api listening");
    let listener = tokio::net::TcpListener::bind(&cfg.operator_api.bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
