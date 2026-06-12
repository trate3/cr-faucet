//! Full production pipeline against real services:
//!  - real Redis-backed accountant + Store
//!  - real pps-rate loop polling a real monerod (regtest is fine)
//!  - real RandomX verifier (`--features real`)
//!  - fake stratum upstream (we don't want to actually mine on a public pool
//!    just to validate the credit pipeline)
//!
//! Env:
//!   POOL_REDIS_URL    e.g. redis://127.0.0.1:6379
//!   POOL_MONEROD_RPC  e.g. http://127.0.0.1:18081/json_rpc
//!   POOL_BIND         default 127.0.0.1:3333
//!
//! Then point xmrig at POOL_BIND and watch the `bal:earned` HASH in Redis.

use anyhow::Result;
use async_trait::async_trait;
use pool_core::cache::RateCache;
use pool_core::config::{
    Config, L2Config, MoneroConfig, OperatorApiConfig, PpsConfig, RandomxConfig, RedisConfig,
    StratumConfig, UpstreamConfig,
};
use pool_core::metrics::Metrics;
use pool_core::store::Store;
use pool_core::ShareAccepted;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use stratum_proxy::session::{run_listener, ProxyServices};
use stratum_proxy::{spawn_upstream, JobStore, ShareSink};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
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
                rd.read_line(&mut line).await.ok();
                let req: Value = serde_json::from_str(line.trim()).unwrap_or(json!({}));
                let req_id = req.get("id").cloned().unwrap_or(json!(1));
                let blob = vec![0u8; 76];
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

    let redis_url = std::env::var("POOL_REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    let monerod_rpc =
        std::env::var("POOL_MONEROD_RPC").unwrap_or_else(|_| "http://127.0.0.1:18081/json_rpc".into());
    let bind = std::env::var("POOL_BIND").unwrap_or_else(|_| "127.0.0.1:3333".into());

    let upstream_addr = spawn_fake_upstream().await?;
    info!(upstream=%upstream_addr, "fake upstream up");

    let cfg = Config {
        stratum: StratumConfig {
            bind: bind.clone(),
            tls_cert: None,
            tls_key: None,
            min_share_difficulty: 1000,
            target_seconds_per_share: 5,
            max_submits_per_second: 100,
            verification_warmup: 5,
            verification_sample_rate: 0.10,
        share_grace_secs: 1,
        idle_timeout_secs: 600,
        vardiff: Default::default(),
        },
        upstream: UpstreamConfig {
            url: format!("tcp://{upstream_addr}"),
            user: "operator-xmr".into(),
            password: "x".into(),
            keepalive_secs: 60,
        socks5h_proxy: None,
        tls_pin_sha256: None,
        login_address_network: None,
        direct: false,
        },
        pps: PpsConfig {
            pool_fee: 0.01,
            fee_mode: Default::default(),
            fee_min: None,
            fee_max: None,
            risk_buffer: 0.05,
            upstream_fee: 0.006,
            operational_cost_atomic_xmr_per_second: 0, // disable op-cost so the demo is easy to read
            monerod_rpc: monerod_rpc.clone(),
            monerod_rpc_pool: Vec::new(),
            quorum_size: 1, // demo: only one local node, so quorum is trivially 1
            sample_size: 0, // 0 → default = quorum_size + 1
            refresh_secs: 5, // tighter than prod default of 60s so the demo is snappier
        },
        redis: RedisConfig { url: redis_url.clone() },
        l2: L2Config {
            rpc_ws: "http://localhost".into(),
            chain_id: 1,
            mining_pool_token_address: "0x0000000000000000000000000000000000000000".into(),
            signer_key_path: "/tmp/none".into(),
            rpc_http: None,
            events_from_block: 0,
            events_chunk_size: 5000,
            events_poll_secs: 5,
        },
        monero: MoneroConfig {
            wallet_rpc: "http://localhost".into(),
            wallet_rpc_user: None,
            wallet_rpc_pass: None,
            min_reserve_ratio: 1.0,
            per_tx_cap_atomic: 1_000_000_000_000,
            per_day_cap_atomic: 10_000_000_000_000,
            max_payout_premium_bp: u32::MAX,
            confirm_wait_secs: 0,
            network: pool_core::config::MoneroNetwork::Testnet,
            wallet_filename: "pool".into(),
            wallet_password: "".into(),
            restore_height_lookback: 720,
        },
        operator_api: OperatorApiConfig {
            bind: "127.0.0.1:0".into(),
        },
        randomx: RandomxConfig::default(),
        tor: Default::default(),
        hashvault: Default::default(),
        single_active: Default::default(),
        fee_swap: Default::default(),
        reveal_wallet_address_once: false,
        reveal_wallet_pubkey: None,
        self_fund: Default::default(),
        endpoint_registry: Default::default(),
        oracle: Default::default(),
    };

    let store = Store::connect(&cfg.redis.url).await?;
    info!(redis=%cfg.redis.url, "redis connected");
    let rate = Arc::new(RateCache::new());
    let fee_cache = Arc::new(pool_core::cache::FeeCache::new(cfg.pps.pool_fee));
    let metrics = Arc::new(Metrics::new());

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
    let (upstream, _u) = spawn_upstream(cfg.upstream.clone(), jobs.clone(), metrics.clone());

    #[cfg(feature = "real")]
    let verifier = Arc::new(randomx_verify::RandomXVerifier::new_light());
    #[cfg(not(feature = "real"))]
    let verifier = Arc::new(randomx_verify::StubVerifier);

    let sink = Arc::new(RedisSink {
        store: store.clone(),
        rate: rate.clone(),
        metrics: metrics.clone(),
    });

    let services = Arc::new(ProxyServices {
        cfg: cfg.stratum.clone(),
        jobs,
        upstream,
        verifier,
        sink,
        tls_acceptor: None,
    });

    tokio::spawn(async move {
        if let Err(e) = run_listener(services).await {
            tracing::error!(error=%e, "listener died");
        }
    });

    // Reporter: every 5s print rate + active miners + total credited.
    let mut conn = store.conn();
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let rate_value = rate.get();
        let hashrate = metrics.hashrate(std::time::Instant::now());
        let active = metrics.active_miners();
        let earned: Vec<(String, i64)> = match redis::AsyncCommands::hgetall::<_, Vec<(String, i64)>>(
            &mut conn,
            "bal:earned",
        )
        .await
        {
            Ok(v) => v,
            Err(_) => Vec::new(),
        };
        let total_earned: i64 = earned.iter().map(|(_, v)| *v).sum();
        info!(
            rate_atomic_per_diff = rate_value,
            pool_hashrate = hashrate,
            active_miners = active,
            total_atomic_xmr_credited = total_earned,
            "tick"
        );
        for (miner, atomic) in &earned {
            info!(miner=%miner, atomic_xmr=%atomic, "bal:earned");
        }
    }
}
