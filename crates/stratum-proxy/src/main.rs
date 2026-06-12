use anyhow::Result;
use async_trait::async_trait;
use pool_core::cache::{FeeCache, RateCache};
use pool_core::metrics::Metrics;
use pool_core::store::Store;
use pool_core::{Config, ShareAccepted};
use std::env;
use std::sync::Arc;
use stratum_proxy::session::{run_listener, ProxyServices};
use stratum_proxy::{spawn_upstream, JobStore, ShareSink};
use tracing::{info, warn};

struct RedisSink {
    store: Store,
    rate: Arc<RateCache>,
    metrics: Arc<Metrics>,
}

#[async_trait]
impl ShareSink for RedisSink {
    async fn credit(&self, share: ShareAccepted) -> anyhow::Result<i64> {
        accountant::credit(&self.store, &self.rate, &self.metrics, &share).await
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cfg_path = env::var("POOL_CONFIG").unwrap_or_else(|_| "pool.toml".into());
    let cfg = Config::load(&cfg_path)?;
    let store = Store::connect(&cfg.redis.url).await?;
    let rate = Arc::new(RateCache::new());
    let fee_cache = Arc::new(FeeCache::new(cfg.pps.pool_fee));
    let metrics = Arc::new(Metrics::new());

    // Start the PPS rate refresh loop. Without this the rate stays at 0 and
    // every credited share is worth 0 atomic XMR, so balances never move.
    {
        let cfg = cfg.clone();
        let metrics = metrics.clone();
        let rate = rate.clone();
        let fee_cache = fee_cache.clone();
        tokio::spawn(async move {
            pps_rate::run_loop(cfg, metrics, rate, fee_cache).await;
            warn!("pps-rate loop exited");
        });
    }

    let jobs = JobStore::new();
    let (upstream, _upstream_handle) =
        spawn_upstream(cfg.upstream.clone(), jobs.clone(), metrics.clone());

    #[cfg(feature = "real")]
    let verifier = {
        use pool_core::config::RandomxMode;
        match cfg.randomx.mode {
            RandomxMode::Light => {
                info!("initializing RandomX verifier: LIGHT (cache-only, ~256 MB)");
                Arc::new(randomx_verify::RandomXVerifier::new_light())
            }
            RandomxMode::Full => {
                info!("initializing RandomX verifier: FULL (dataset, ~2 GB)");
                Arc::new(randomx_verify::RandomXVerifier::new_full())
            }
        }
    };
    #[cfg(not(feature = "real"))]
    let verifier = {
        // randomx-verify built without `real`; mode setting ignored.
        let _ = &cfg.randomx;
        Arc::new(randomx_verify::StubVerifier)
    };
    let sink = Arc::new(RedisSink {
        store,
        rate,
        metrics,
    });

    let services = Arc::new(ProxyServices {
        cfg: cfg.stratum.clone(),
        jobs,
        upstream,
        verifier,
        sink,
        tls_acceptor: None,
    });
    info!("stratum proxy starting");
    run_listener(services).await
}
