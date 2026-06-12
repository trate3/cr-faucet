//! Live pipeline: the REAL proxy with HashVault as its upstream, serving jobs to
//! a downstream miner. Verifies xmrig receives HashVault's mainnet jobs through
//! our proxy. Point xmrig at 127.0.0.1:3334.
//!   cargo run -q -p stratum-proxy --example hashvault_pipeline
//!   xmrig -o 127.0.0.1:3334 -u 0xMINER -p test --coin monero --no-huge-pages

use anyhow::Result;
use pool_core::config::{StratumConfig, UpstreamConfig};
use pool_core::metrics::Metrics;
use std::sync::Arc;
use std::time::Duration;
use stratum_proxy::session::{run_listener, ProxyServices};
use stratum_proxy::{spawn_upstream, InMemorySink, JobStore};
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // Real production upstream client -> HashVault over TLS.
    let upstream_cfg = UpstreamConfig {
        url: "stratum+ssl://pool.hashvault.pro:443".into(),
        user: "44AFFq5kSiGBoZ4NMDwYtN18obc8AemS33DBLWs3H7otXft3XjrpDtQGv7SqSsaBYBb98uNbr2VBBEt7f2wfn3RVGQBEP3A".into(),
        password: "tiny-pool-test".into(),
        keepalive_secs: 60,
        socks5h_proxy: None, // direct egress (HashVault is clearnet)
        tls_pin_sha256: None,
        login_address_network: None,
        direct: false,
    };
    let jobs = JobStore::new();
    let (upstream, _u) = spawn_upstream(upstream_cfg, jobs.clone(), Arc::new(Metrics::new()));

    let stratum_cfg = StratumConfig {
        bind: "127.0.0.1:3334".into(),
        tls_cert: None,
        tls_key: None,
        min_share_difficulty: 1000, // low so a CPU finds shares fast
        target_seconds_per_share: 10,
        max_submits_per_second: 100,
        verification_warmup: 5,
        verification_sample_rate: 0.10,
        share_grace_secs: 20,
        idle_timeout_secs: 600,
        vardiff: Default::default(),
    };
    // Real RandomX under --features real so xmrig's valid shares actually verify;
    // StubVerifier otherwise (job-flow check only). HV_RANDOMX_MODE=light|full
    // (default full) mirrors the pool's [randomx].mode config setting.
    #[cfg(feature = "real")]
    let verifier = {
        let mode = std::env::var("HV_RANDOMX_MODE").unwrap_or_else(|_| "full".into());
        info!(%mode, "RandomX verifier mode");
        if mode == "light" {
            Arc::new(randomx_verify::RandomXVerifier::new_light())
        } else {
            Arc::new(randomx_verify::RandomXVerifier::new_full())
        }
    };
    #[cfg(not(feature = "real"))]
    let verifier = Arc::new(randomx_verify::StubVerifier);
    let services = Arc::new(ProxyServices {
        cfg: stratum_cfg,
        jobs: jobs.clone(),
        upstream,
        verifier,
        sink: Arc::new(InMemorySink::default()),
        tls_acceptor: None,
    });
    tokio::spawn(async move {
        if let Err(e) = run_listener(services).await {
            tracing::error!(error=%e, "listener died");
        }
    });

    // Report the HashVault job the proxy is serving.
    let mut last = String::new();
    loop {
        tokio::time::sleep(Duration::from_secs(3)).await;
        if let Some(j) = jobs.current() {
            if j.job_id != last {
                info!(job_id=%j.job_id, height=?j.height, upstream_diff=j.upstream_diff, "serving HashVault job to miners");
                last = j.job_id;
            }
        }
    }
}
