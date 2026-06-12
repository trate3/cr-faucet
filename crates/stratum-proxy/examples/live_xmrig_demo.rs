//! Live xmrig test harness. Runs a fake upstream + the real proxy with real
//! RandomX verification, prints every accepted share. Point xmrig at the
//! proxy's address and watch shares flow.
//!
//! Build with: `cargo run -p stratum-proxy --example live_xmrig_demo --features real`
//! Then run xmrig:
//!   xmrig -o 127.0.0.1:3333 -u 0x0000000000000000000000000000000000000abc -p test \
//!         --coin monero --no-color --donate-level 0 -t 1

use anyhow::Result;
use pool_core::config::{StratumConfig, UpstreamConfig};
use pool_core::metrics::Metrics;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use stratum_proxy::session::{run_listener, ProxyServices};
use stratum_proxy::{spawn_upstream, InMemorySink, JobStore};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tracing::info;

/// Fake upstream: accepts login, serves one fixed job at a very-low diff so
/// xmrig hits it quickly, drains submits silently. The blob is 76 zero bytes;
/// xmrig will faithfully nonce-hash whatever we give it.
async fn spawn_fake_upstream() -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?.to_string();
    tokio::spawn(async move {
        loop {
            let (sock, _) = match listener.accept().await {
                Ok(x) => x,
                Err(_) => continue,
            };
            tokio::spawn(async move {
                let (rd, mut wr) = sock.into_split();
                let mut rd = BufReader::new(rd);
                let mut line = String::new();
                // Read login.
                rd.read_line(&mut line).await.ok();
                let req: Value = serde_json::from_str(line.trim()).unwrap_or(json!({}));
                let req_id = req.get("id").cloned().unwrap_or(json!(1));
                let blob = vec![0u8; 76];
                // diff 1: every hash passes upstream target, every share forwarded.
                let job = json!({
                    "job_id": "upstream-1",
                    "blob": hex::encode(&blob),
                    "seed_hash": hex::encode([0xaa; 32]),
                    "target": hex::encode(0xFFFF_FFFFu32.to_le_bytes()),
                    "height": 1u64,
                });
                let resp = json!({
                    "id": req_id, "jsonrpc": "2.0",
                    "result": {"id": "fake-session", "job": job, "status": "OK"},
                });
                let mut s = serde_json::to_string(&resp).unwrap();
                s.push('\n');
                let _ = wr.write_all(s.as_bytes()).await;
                loop {
                    line.clear();
                    if rd.read_line(&mut line).await.unwrap_or(0) == 0 {
                        break;
                    }
                }
            });
        }
    });
    Ok(addr)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let upstream_addr = spawn_fake_upstream().await?;
    info!(upstream=%upstream_addr, "fake upstream up");

    let proxy_bind = "127.0.0.1:3333".to_owned();

    let stratum_cfg = StratumConfig {
        bind: proxy_bind.clone(),
        tls_cert: None,
        tls_key: None,
        // very low local diff so a single CPU finds shares almost immediately
        min_share_difficulty: 1000,
        target_seconds_per_share: 5,
        max_submits_per_second: 100,
        verification_warmup: 5,
        verification_sample_rate: 0.10,
        share_grace_secs: 1,
        idle_timeout_secs: 600,
        vardiff: Default::default(),
    };
    let upstream_cfg = UpstreamConfig {
        url: format!("tcp://{upstream_addr}"),
        user: "operator-xmr".into(),
        password: "x".into(),
        keepalive_secs: 60,
        socks5h_proxy: None,
        tls_pin_sha256: None,
        login_address_network: None,
        direct: false,
    };

    let jobs = JobStore::new();
    let (upstream, _u) = spawn_upstream(upstream_cfg, jobs.clone(), Arc::new(Metrics::new()));
    let sink = Arc::new(InMemorySink::default());

    // Build a verifier. With `--features real` this is the real RandomX VM;
    // without it, the StubVerifier (which accepts xmrig's submits without
    // actually validating — useful for quick smoke tests).
    #[cfg(feature = "real")]
    let verifier = Arc::new(randomx_verify::RandomXVerifier::new());
    #[cfg(not(feature = "real"))]
    let verifier = Arc::new(randomx_verify::StubVerifier);

    let services = Arc::new(ProxyServices {
        cfg: stratum_cfg,
        jobs: jobs.clone(),
        upstream,
        verifier,
        sink: sink.clone(),
        tls_acceptor: None,
    });

    tokio::spawn(async move {
        if let Err(e) = run_listener(services).await {
            tracing::error!(error=%e, "listener died");
        }
    });

    // Reporter loop.
    let mut last = 0usize;
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let g = sink.shares.lock();
        let n = g.len();
        if n > last {
            for s in &g[last..n] {
                info!(
                    miner=%s.miner.0,
                    diff=s.difficulty,
                    forwarded=s.forwarded_upstream,
                    "credited share"
                );
            }
            last = n;
        }
        info!(total_credited=n, "tick");
    }
}
