//! Live probe: connect the real production `upstream::spawn` client to
//! HashVault using TLS + pinned cert (+ optional SOCKS5h) and wait for a
//! job to land in the JobStore.
//!
//! Env:
//!   HV_SOCKS5H            optional, e.g. "socks5h://127.0.0.1:9050"
//!   HV_FINGERPRINT_SHA256 optional; when set, pin to this cert. When unset,
//!                         encrypt-only (same as xmrig default).
//!   HV_USER               your Monero wallet address (defaults to a benign
//!                         dummy that HashVault accepts for connectivity
//!                         testing but won't pay)
//! Exits 0 on first job received within 30s, non-zero otherwise.

use anyhow::Result;
use pool_core::config::UpstreamConfig;
use pool_core::metrics::Metrics;
use std::sync::Arc;
use std::time::Duration;
use stratum_proxy::{spawn_upstream, JobStore};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cfg = UpstreamConfig {
        url: "stratum+ssl://pool.hashvault.pro:443".into(),
        user: std::env::var("HV_USER").unwrap_or_else(|_| {
            "44AFFq5kSiGBoZ4NMDwYtN18obc8AemS33DBLWs3H7otXft3XjrpDtQGv7SqSsaBYBb98uNbr2VBBEt7f2wfn3RVGQBEP3A".into()
        }),
        password: "tiny-pool-probe".into(),
        keepalive_secs: 60,
        socks5h_proxy: std::env::var("HV_SOCKS5H").ok(),
        tls_pin_sha256: std::env::var("HV_FINGERPRINT_SHA256").ok(),
        login_address_network: None,
        direct: false,
    };
    let jobs = JobStore::new();
    let (_client, _h) = spawn_upstream(cfg, jobs.clone(), Arc::new(Metrics::new()));

    for _ in 0..60 {
        if let Some(j) = jobs.current() {
            println!(
                "OK: received job_id={} height={:?} target={} blob_len={}",
                j.job_id,
                j.height,
                j.upstream_target_hex,
                j.blob_hex.len()
            );
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    anyhow::bail!("no job within 30s; check logs above")
}
