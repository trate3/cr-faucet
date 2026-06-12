//! Operator + miner read API. Reads in-process Metrics + persistent state
//! from `pool_core::store::Store` (Redis).

use axum::{
    extract::{Path, State},
    routing::get,
    Json, Router,
};
use pool_core::cache::RateCache;
use pool_core::metrics::{Metrics, UpstreamHealth};
use pool_core::store::Store;
use serde::Serialize;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub metrics: Arc<Metrics>,
    pub rate: Arc<RateCache>,
    /// Latest HashVault stats pull. `None` until the first successful
    /// fetch; `Some` once we've ever reached the API.
    pub upstream_stats: Arc<parking_lot::RwLock<Option<hashvault_client::Stats>>>,
    pub upstream_stats_as_of_unix: Arc<std::sync::atomic::AtomicI64>,
    /// The pool's Tor v3 onion hostname (e.g. `abc…xyz.onion`), read once at
    /// startup from the hidden-service `hostname` file. `None` when no hidden
    /// service is configured (no ROFL KMS / `TOR_HS_ENABLED=false`). Served by
    /// `/onion` so miners can discover the censorship-resistant endpoint —
    /// nothing else publishes it.
    pub onion: Option<String>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/miner/:addr", get(get_miner))
        .route("/rate", get(get_rate))
        .route("/pool", get(get_pool))
        .route("/treasury", get(get_treasury))
        .route("/upstream_stats", get(get_upstream_stats))
        .route("/onion", get(get_onion))
        .with_state(state)
}

#[derive(Serialize)]
struct OnionView {
    /// The pool's Tor v3 onion hostname, or null when no hidden service is
    /// configured. Stable across restarts/redeploys (KMS-derived).
    onion: Option<String>,
    /// Ready-to-use downstream stratum endpoint over Tor (virtual port 3333,
    /// per deploy/torrc), or null. Point xmrig at `stratum+tcp://<this>`.
    stratum: Option<String>,
    /// This read API reachable over Tor (HTTP on virtual port 80), or null.
    api: Option<String>,
}

/// Advertise the pool's onion address + the endpoints reachable over it.
/// This is the only place the onion URL is published — it is never written
/// on-chain or to any other artifact.
async fn get_onion(State(s): State<AppState>) -> Json<OnionView> {
    let onion = s.onion.clone();
    Json(OnionView {
        // Mirrors the `HiddenServicePort` map in deploy/torrc: 3333 → stratum,
        // 80 → this API.
        stratum: onion.as_ref().map(|o| format!("{o}:3333")),
        api: onion.as_ref().map(|o| format!("http://{o}")),
        onion,
    })
}

async fn get_upstream_stats(State(s): State<AppState>) -> Json<UpstreamStatsView> {
    Json(UpstreamStatsView {
        stats: s.upstream_stats.read().clone(),
        as_of_unix: s.upstream_stats_as_of_unix.load(std::sync::atomic::Ordering::Relaxed),
    })
}

#[derive(Serialize)]
struct UpstreamStatsView {
    /// Last successful pull from HashVault (or None if we've never
    /// reached the API). Wallet address is intentionally NOT echoed
    /// here — the threshold endpoint takes no auth other than the
    /// address itself, so leaking it would let anyone redirect or
    /// freeze our payouts.
    stats: Option<hashvault_client::Stats>,
    as_of_unix: i64,
}

#[derive(Serialize)]
struct MinerView {
    miner: String,
    cumulative_owed_atomic: i64,
    last_voucher_cumulative: i64,
    shares: u64,
    work: u64,
    last_share_ms: i64,
}

#[derive(Serialize)]
struct RateView {
    atomic_xmr_per_diff: f64,
    set_at_unix: i64,
}

#[derive(Serialize)]
struct PoolView {
    hashrate: f64,
    total_work: u64,
    active_miners: usize,
    upstream: UpstreamView,
}

#[derive(Serialize)]
struct UpstreamView {
    /// True iff the pool currently has a logged-in stratum session with
    /// the upstream Monero pool. While `false`, shares submitted by miners
    /// are still verified + credited locally, but no actual XMR is being
    /// mined.
    connected: bool,
    /// Unix-seconds of the last connect/disconnect transition. Lets a
    /// miner dashboard show "disconnected for N minutes" without polling
    /// twice.
    last_change_unix: i64,
    /// Failed reconnect attempts since the last healthy upstream session.
    /// Climbs while disconnected, resets after a session stays healthy for
    /// long enough.
    consecutive_failures: u32,
    /// Lifetime count of shares the upstream pool rejected after we
    /// forwarded them. A sudden rise usually means the operator is banned
    /// or shipping stale shares.
    submit_rejects_total: u64,
    /// Lifetime count of shares the upstream pool accepted. Pair with
    /// `submit_rejects_total` to compute a reject ratio.
    submit_accepts_total: u64,
}

impl From<UpstreamHealth> for UpstreamView {
    fn from(h: UpstreamHealth) -> Self {
        Self {
            connected: h.connected,
            // Stored as unix-ms internally; downgrade to seconds for the
            // public view since per-ms precision isn't useful here.
            last_change_unix: h.last_change_unix_ms / 1000,
            consecutive_failures: h.consecutive_failures,
            submit_rejects_total: h.submit_rejects_total,
            submit_accepts_total: h.submit_accepts_total,
        }
    }
}

async fn get_miner(
    State(s): State<AppState>,
    Path(addr): Path<String>,
) -> Result<Json<MinerView>, axum::http::StatusCode> {
    let parsed = alloy::primitives::Address::from_str(&addr)
        .map_err(|_| axum::http::StatusCode::BAD_REQUEST)?;
    let bs = s
        .store
        .balance_state(parsed)
        .await
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    let snap = s.metrics.miner_snapshot(&parsed.into_array());
    Ok(Json(MinerView {
        miner: format!("{:#x}", parsed),
        cumulative_owed_atomic: bs.earned,
        last_voucher_cumulative: bs.last_voucher_cumulative,
        shares: snap.map(|x| x.shares).unwrap_or(0),
        work: snap.map(|x| x.work).unwrap_or(0),
        last_share_ms: snap.map(|x| x.last_share_ms).unwrap_or(0),
    }))
}

async fn get_rate(State(s): State<AppState>) -> Json<RateView> {
    Json(RateView {
        atomic_xmr_per_diff: s.rate.get(),
        set_at_unix: s.rate.set_at_unix(),
    })
}

async fn get_pool(State(s): State<AppState>) -> Json<PoolView> {
    Json(PoolView {
        hashrate: s.metrics.hashrate(Instant::now()),
        total_work: s.metrics.total_work(),
        active_miners: s.metrics.active_miners(),
        upstream: s.metrics.upstream_health().into(),
    })
}

#[derive(Serialize)]
struct TreasuryView {
    monero_balance_atomic: String,
    monero_unlocked_atomic: String,
    pending_redemptions_mining_pool_token: String,
    pending_redemptions_count: u64,
    mining_pool_token_total_supply: String,
    /// Atomic XMR you'd receive per 1 MiningPoolToken base unit if you burned right
    /// now: `balance / (totalSupply + pending)`. **This is the rate that
    /// will be used by the consumer.** Users should multiply their intended
    /// burn amount by this rate to see what they'll get in XMR (minus the
    /// Monero tx fee, which the recipient pays).
    per_mining_pool_token_base_unit_atomic_xmr: Option<f64>,
    /// Same rate, presented as "atomic XMR per whole mining-pool token"
    /// (12 decimals). Easier to read when token amounts are denominated in
    /// whole units.
    per_token_atomic_xmr: Option<f64>,
    /// Unix seconds the snapshot was last refreshed (currently every ~10s).
    /// Null: there is no snapshot yet.
    as_of_unix: Option<i64>,
}

async fn get_treasury(
    State(s): State<AppState>,
) -> Result<Json<TreasuryView>, axum::http::StatusCode> {
    let snap = s
        .store
        .treasury_snapshot()
        .await
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    let view = match snap {
        Some(s) => {
            let denom = s.mining_pool_token_total_supply.saturating_add(s.pending_redemptions_atomic);
            let per_base_unit = if denom > 0 {
                Some(s.monero_balance_atomic as f64 / denom as f64)
            } else {
                None
            };
            let per_whole_token = per_base_unit.map(|r| r * 1e12);
            TreasuryView {
                monero_balance_atomic: s.monero_balance_atomic.to_string(),
                monero_unlocked_atomic: s.monero_unlocked_atomic.to_string(),
                pending_redemptions_mining_pool_token: s.pending_redemptions_atomic.to_string(),
                pending_redemptions_count: s.pending_redemptions_count,
                mining_pool_token_total_supply: s.mining_pool_token_total_supply.to_string(),
                per_mining_pool_token_base_unit_atomic_xmr: per_base_unit,
                per_token_atomic_xmr: per_whole_token,
                as_of_unix: Some(s.as_of_unix),
            }
        }
        None => TreasuryView {
            monero_balance_atomic: "0".into(),
            monero_unlocked_atomic: "0".into(),
            pending_redemptions_mining_pool_token: "0".into(),
            pending_redemptions_count: 0,
            mining_pool_token_total_supply: "0".into(),
            per_mining_pool_token_base_unit_atomic_xmr: None,
            per_token_atomic_xmr: None,
            as_of_unix: None,
        },
    };
    Ok(Json(view))
}
