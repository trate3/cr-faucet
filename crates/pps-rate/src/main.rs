//! Standalone PPS rate publisher; usually run as a task inside the proxy
//! binary instead.

use anyhow::Result;
use pool_core::cache::{FeeCache, RateCache};
use pool_core::metrics::Metrics;
use pool_core::Config;
use std::env;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cfg = Config::load(env::var("POOL_CONFIG").unwrap_or_else(|_| "pool.toml".into()))?;
    let metrics = Arc::new(Metrics::new());
    let rate_cache = Arc::new(RateCache::new());
    // Standalone: fixed fee at the configured pool_fee (no adaptive controller).
    let fee_cache = Arc::new(FeeCache::new(cfg.pps.pool_fee));
    pps_rate::run_loop(cfg, metrics, rate_cache, fee_cache).await;
    Ok(())
}
