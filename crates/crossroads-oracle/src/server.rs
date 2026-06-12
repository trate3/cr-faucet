//! The oracle's own self-contained HTTP server — multi-oracle + sign-only. Per
//! request it reads the NAMED permissionless oracle's committee config from chain
//! (verifying it trusts OUR registry, SSRF-filtering its RPCs), polls that
//! committee, and returns a signer-signed `BlockHashReport` bound to that oracle.
//! It NEVER posts on chain — the requester relays the report to the oracle's
//! `submitBlockHash` themselves.
//!
//! Rate limiting is PER TOR CIRCUIT (via the PROXY header from
//! `HiddenServiceExportCircuitID haproxy`): one hammerer only throttles itself; a
//! global ceiling backstops aggregate load; idle buckets are reaped.

use crate::{sign_report, BlockHashReport, OracleInfo, Settings, SignedReport};
use alloy::primitives::Address;
use alloy::providers::ProviderBuilder;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use hyper::body::Incoming;
use hyper::Request;
use hyper_util::rt::TokioIo;
use parking_lot::Mutex;
use serde::Deserialize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;
use tower::ServiceExt; // oneshot

sol! {
    #[sol(rpc)]
    interface IEvmBlockHashOracle {
        function registry() external view returns (address);
        function expectedSourceChainId() external view returns (uint256);
        function minConfirmations() external view returns (uint256);
        function mandateFinalized() external view returns (bool);
        function sourceRpcUrls() external view returns (string[] memory);
        function sourceRpcQuorum() external view returns (uint256);
    }
}

/// Per-Tor-circuit key (decoded from the PROXY header) inserted into request
/// extensions by [`serve`] and read by the rate limiter.
#[derive(Clone, Copy)]
struct Circuit(SocketAddr);

/// Token bucket: refills at `rate`/sec, burst capped at `rate`.
struct TokenBucket {
    tokens: f64,
    rate: f64,
    last: Instant,
}

impl TokenBucket {
    fn new(rate: f64, now: Instant) -> Self {
        Self { tokens: rate, rate, last: now }
    }
    fn refill(&mut self, now: Instant) {
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * self.rate).min(self.rate);
    }
    fn allow(&mut self, now: Instant) -> bool {
        self.refill(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
    fn is_full(&mut self, now: Instant) -> bool {
        self.refill(now);
        self.tokens >= self.rate
    }
}

/// Per-circuit rate limiter with a global backstop and lossless idle eviction.
struct CircuitLimiter {
    per_circuit_rate: f64,
    circuits: Mutex<HashMap<SocketAddr, TokenBucket>>,
    global: Mutex<TokenBucket>,
    calls: AtomicUsize,
}

const SWEEP_EVERY: usize = 256;

impl CircuitLimiter {
    fn new(per_circuit_rate: f64, global_rate: f64, now: Instant) -> Self {
        Self {
            per_circuit_rate,
            circuits: Mutex::new(HashMap::new()),
            global: Mutex::new(TokenBucket::new(global_rate, now)),
            calls: AtomicUsize::new(0),
        }
    }

    fn allow(&self, key: SocketAddr, now: Instant) -> bool {
        // Lossless idle eviction: drop buckets that have refilled to full (the
        // circuit went away). A full bucket == a fresh one, so this bounds memory
        // with no loss of state, and can't be gamed by reconnecting per request.
        if self.calls.fetch_add(1, Ordering::Relaxed) % SWEEP_EVERY == 0 {
            self.circuits.lock().retain(|_, b| !b.is_full(now));
        }
        // Per-circuit gate FIRST, so a throttled circuit never touches the global
        // ceiling (otherwise a hammerer could drain it and DoS everyone).
        {
            let mut m = self.circuits.lock();
            let b = m.entry(key).or_insert_with(|| TokenBucket::new(self.per_circuit_rate, now));
            if !b.allow(now) {
                return false;
            }
        }
        self.global.lock().allow(now)
    }
}

#[derive(Clone)]
pub struct OracleState {
    inner: Arc<Inner>,
}

struct Inner {
    settings: Settings,
    signer: PrivateKeySigner,
    signer_epoch: u64,
    sapphire_rpc: String,
    http: reqwest::Client,
    limiter: CircuitLimiter,
    cache: Mutex<HashMap<Address, Arc<OracleInfo>>>,
}

impl OracleState {
    pub fn new(settings: Settings, signer: PrivateKeySigner, signer_epoch: u64, sapphire_rpc: String) -> Self {
        let now = Instant::now();
        let per_circuit = settings.rate_limit_per_sec.max(1) as f64;
        let global = settings.global_rate_limit_per_sec.max(settings.rate_limit_per_sec).max(1) as f64;
        let inner = Inner {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("reqwest client"),
            limiter: CircuitLimiter::new(per_circuit, global, now),
            cache: Mutex::new(HashMap::new()),
            settings,
            signer,
            signer_epoch,
            sapphire_rpc,
        };
        Self { inner: Arc::new(inner) }
    }

    /// Read + cache a permissionless oracle's committee config. Refuses oracles
    /// that trust a different registry, and SSRF-filters their RPC URLs.
    async fn oracle_info(&self, oracle: Address) -> anyhow::Result<Arc<OracleInfo>> {
        if let Some(info) = self.inner.cache.lock().get(&oracle).cloned() {
            return Ok(info);
        }
        let provider = ProviderBuilder::new().on_http(self.inner.sapphire_rpc.parse()?);
        let c = IEvmBlockHashOracle::new(oracle, &provider);

        let registry: Address = c.registry().call().await?._0;
        if registry != self.inner.settings.registry {
            anyhow::bail!("oracle {oracle} trusts a different registry ({registry})");
        }
        let source_chain_id: u64 = c.expectedSourceChainId().call().await?._0.try_into().unwrap_or(0);
        let min_confirmations: u64 = c.minConfirmations().call().await?._0.try_into().unwrap_or(0);
        let mandate_finalized: bool = c.mandateFinalized().call().await?._0;
        let mut urls: Vec<String> = c.sourceRpcUrls().call().await?._0;
        let quorum: usize = c.sourceRpcQuorum().call().await?._0.try_into().unwrap_or(usize::MAX);
        if urls.len() > self.inner.settings.max_source_rpcs {
            urls.truncate(self.inner.settings.max_source_rpcs);
        }
        // SSRF: never fetch from a URL that resolves to a non-global IP. Keep only
        // the safe ones; require at least `quorum` to remain.
        let mut safe = Vec::with_capacity(urls.len());
        for u in urls {
            match crate::ssrf::assert_public_url(&u).await {
                Ok(()) => safe.push(u),
                Err(e) => tracing::warn!("oracle {oracle}: dropping RPC {u}: {e}"),
            }
        }
        if safe.len() < quorum {
            anyhow::bail!("oracle {oracle}: only {} SSRF-safe RPCs (< quorum {quorum})", safe.len());
        }
        let info = Arc::new(OracleInfo {
            source_chain_id,
            min_confirmations,
            mandate_finalized,
            rpc_urls: safe,
            quorum,
        });
        self.inner.cache.lock().insert(oracle, info.clone());
        Ok(info)
    }

    /// Read the named oracle's committee config, poll it, build + sign a report
    /// bound to that oracle. No chain writes.
    async fn sign_for(&self, oracle: Address, block_number: u64) -> anyhow::Result<SignedReport> {
        let i = &self.inner;
        let info = self.oracle_info(oracle).await?;
        let c = crate::source::fetch_confirmed(
            &i.http,
            &info.rpc_urls,
            info.quorum,
            block_number,
            info.min_confirmations,
            info.mandate_finalized,
        )
        .await?;

        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let report = BlockHashReport {
            source_chain_id: info.source_chain_id,
            block_number,
            block_hash: c.block_hash,
            required_confirmations: info.min_confirmations,
            observed_confirmations: c.observed_confirmations,
            quorum_tip: c.quorum_tip,
            observed_quorum: c.observed_quorum,
            require_finalized: info.mandate_finalized,
            finalized_block_number: c.finalized_block_number,
            expires_at: now + i.settings.report_ttl_secs,
            signer_epoch: i.signer_epoch,
        };
        // The digest binds the report to THIS oracle's address, so it can only be
        // submitted there (no cross-oracle replay).
        let sig = sign_report(&i.signer, i.settings.sapphire_chain_id, oracle, &report).await?;

        Ok(SignedReport {
            oracle: oracle.to_string(),
            source_chain_id: report.source_chain_id,
            block_number: report.block_number,
            block_hash: format!("0x{}", hex::encode(report.block_hash)),
            required_confirmations: report.required_confirmations,
            observed_confirmations: report.observed_confirmations,
            quorum_tip: report.quorum_tip,
            observed_quorum: report.observed_quorum,
            require_finalized: report.require_finalized,
            finalized_block_number: report.finalized_block_number,
            expires_at: report.expires_at,
            signer_epoch: report.signer_epoch,
            signer: format!("{:?}", i.signer.address()),
            signature: format!("0x{}", hex::encode(sig)),
        })
    }
}

#[derive(Deserialize)]
struct BlockQuery {
    oracle: Address,
    block_number: u64,
}

/// The oracle routes. Serve with [`serve`], which wires the per-circuit key.
pub fn router(state: OracleState) -> Router {
    Router::new()
        .route("/v1/blockhash", get(get_blockhash))
        .route("/healthz", get(healthz))
        .with_state(state)
}

/// Run the oracle's own server on `listener`. Each connection's PROXY header is
/// read first (the per-circuit key), then served by the router with that key in
/// extensions. axum 0.7 has no custom-Listener trait, so this is a hyper loop.
pub async fn serve(listener: TcpListener, state: OracleState) -> std::io::Result<()> {
    let app = router(state);
    loop {
        let (mut stream, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!("oracle accept error: {e}");
                continue;
            }
        };
        let circuit = crate::proxy::read_source(&mut stream).await.ok().flatten().unwrap_or(peer);
        let app = app.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let svc = hyper::service::service_fn(move |mut req: Request<Incoming>| {
                req.extensions_mut().insert(Circuit(circuit));
                app.clone().oneshot(req)
            });
            if let Err(e) = hyper::server::conn::http1::Builder::new().serve_connection(io, svc).await {
                tracing::debug!("oracle connection closed: {e}");
            }
        });
    }
}

async fn get_blockhash(
    State(st): State<OracleState>,
    Extension(Circuit(circuit)): Extension<Circuit>,
    q: Option<Query<BlockQuery>>,
) -> Response {
    if !st.inner.limiter.allow(circuit, Instant::now()) {
        return (StatusCode::TOO_MANY_REQUESTS, "rate limited").into_response();
    }
    let Query(q) = match q {
        Some(q) => q,
        None => return (StatusCode::BAD_REQUEST, "need ?oracle=0x..&block_number=N").into_response(),
    };
    match st.sign_for(q.oracle, q.block_number).await {
        Ok(report) => Json(report).into_response(),
        // Bad/unknown oracle, committee unavailable, quorum failure, too-new block.
        Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    }
}

async fn healthz(State(st): State<OracleState>) -> Response {
    Json(serde_json::json!({
        "ok": true,
        "registry": format!("{:?}", st.inner.settings.registry),
        "signer": format!("{:?}", st.inner.signer.address()),
        "signerEpoch": st.inner.signer_epoch,
        "cachedOracles": st.inner.cache.lock().len(),
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn one_circuit_throttle_does_not_starve_another() {
        let now = Instant::now();
        let lim = CircuitLimiter::new(2.0, 1000.0, now);
        let a = key("127.0.0.1:1");
        let b = key("127.0.0.2:2");
        assert!(lim.allow(a, now));
        assert!(lim.allow(a, now));
        assert!(!lim.allow(a, now));
        assert!(lim.allow(b, now));
        assert!(lim.allow(b, now));
    }

    #[test]
    fn global_ceiling_caps_aggregate() {
        let now = Instant::now();
        let lim = CircuitLimiter::new(1000.0, 2.0, now);
        let a = key("1.1.1.1:1");
        assert!(lim.allow(a, now));
        assert!(lim.allow(a, now));
        assert!(!lim.allow(a, now));
    }

    #[test]
    fn throttled_circuit_does_not_drain_global() {
        let now = Instant::now();
        let lim = CircuitLimiter::new(1.0, 5.0, now);
        let a = key("2.2.2.2:1");
        assert!(lim.allow(a, now));
        for _ in 0..10 {
            assert!(!lim.allow(a, now));
        }
        for k in ["3.3.3.3:1", "4.4.4.4:1", "5.5.5.5:1", "6.6.6.6:1"] {
            assert!(lim.allow(key(k), now), "global was wrongly drained by {k}");
        }
    }
}
