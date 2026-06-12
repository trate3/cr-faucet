//! PPS rate refresh loop.
//!
//! Inputs needed for the rate formula:
//!   - **Block reward** — fixed at the tail-emission constant (0.6 XMR per
//!     block since Monero's 2022 hard fork). Hardcoded; we don't ask the
//!     remote node, so it can't lie to us about it.
//!   - **Network difficulty** — sampled from a configurable pool of remote
//!     `monerod` RPCs. Each tick picks a random subset; we commit only when
//!     `quorum_size` of them agree on the same `(height, difficulty)`.
//!   - **Pool hashrate** — from in-process Metrics.
//!
//! Output: an atomic XMR per unit difficulty value, written to the shared
//! `RateCache` for the accountant to multiply by per-share difficulty.

use anyhow::{anyhow, Result};
use pool_core::cache::{FeeCache, RateCache};
use pool_core::metrics::Metrics;
use pool_core::pps::{compute, PpsInputs};
use pool_core::Config;
use rand::seq::SliceRandom;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Per-tick decay multiplier on each URL's accumulated failure score. Lower
/// = faster forgiveness; 0.5 means a single failure stops biasing sampling
/// after ~3–4 ticks of recovery.
const HEALTH_DECAY: f64 = 0.5;
/// Penalty added when a fetch fails (network / 5xx / JSON parse).
const HEALTH_PENALTY: f64 = 1.0;

/// Monero tail emission: a fixed 0.6 XMR per block since the August 2022
/// hard fork. Atomic units (12 decimals).
pub const TAIL_BLOCK_REWARD_ATOMIC: u64 = 600_000_000_000;

#[derive(Debug, Deserialize)]
struct GetInfoResult {
    difficulty: u64,
    height: u64,
}

#[derive(Debug, Deserialize)]
struct Rpc<T> {
    result: T,
}

/// Per-node observation. Returned by [`fetch_one`].
#[derive(Debug, Clone)]
pub struct NodeSample {
    pub url: String,
    pub height: u64,
    pub difficulty: u64,
}

pub async fn run_loop(
    cfg: Config,
    metrics: Arc<Metrics>,
    rate_cache: Arc<RateCache>,
    fee_cache: Arc<FeeCache>,
) {
    let client = cfg
        .tor
        .apply(reqwest::Client::builder().timeout(Duration::from_secs(8)))
        .build()
        .expect("reqwest client");
    let interval = Duration::from_secs(cfg.pps.refresh_secs as u64);
    // Per-URL recent-failure score. Decays each tick (HEALTH_DECAY) and gets
    // a HEALTH_PENALTY bump for every failed fetch. Sampling weight is
    // 1/(1+score), so a single failure roughly halves a node's pick odds for
    // a tick or two, then it rejoins the rotation. We deliberately do NOT
    // penalize values that fall outside the quorum (lying or just one block
    // ahead) — that would let one adversary dominate the trust set; the
    // random rotation across the full pool is the security property here.
    let mut health: HashMap<String, f64> = HashMap::new();
    loop {
        for v in health.values_mut() {
            *v *= HEALTH_DECAY;
        }
        match tick(&client, &cfg, &metrics, &rate_cache, &fee_cache, &mut health).await {
            Ok(rate) => info!(rate, "published pps rate"),
            Err(e) => warn!(error=%e, "rate tick failed"),
        }
        tokio::time::sleep(interval).await;
    }
}

async fn tick(
    client: &reqwest::Client,
    cfg: &Config,
    metrics: &Metrics,
    rate_cache: &RateCache,
    fee_cache: &FeeCache,
    health: &mut HashMap<String, f64>,
) -> Result<f64> {
    let urls = effective_pool(&cfg.pps.monerod_rpc, &cfg.pps.monerod_rpc_pool);
    if urls.is_empty() {
        return Err(anyhow!("no monerod RPC configured"));
    }
    let quorum = cfg.pps.quorum_size.max(1);
    let sample_size = if cfg.pps.sample_size > 0 {
        cfg.pps.sample_size
    } else {
        quorum + 1
    };

    let picks: Vec<String> = pick_weighted(&urls, health, sample_size.min(urls.len()));
    let outcomes = sample_nodes(client, &picks).await;
    for (url, result) in &outcomes {
        if let Err(e) = result {
            warn!(url=%url, error=%e, "monerod sample failed");
            *health.entry(url.clone()).or_insert(0.0) += HEALTH_PENALTY;
        }
    }
    let samples: Vec<NodeSample> = outcomes.into_iter().filter_map(|(_, r)| r.ok()).collect();
    let difficulty = decide_quorum(&samples, quorum)?;

    let hashrate = metrics.hashrate(Instant::now()).max(1.0);
    let bd = compute(PpsInputs {
        block_reward_atomic: TAIL_BLOCK_REWARD_ATOMIC as f64,
        network_difficulty: difficulty as f64,
        upstream_fee: cfg.pps.upstream_fee,
        pool_fee: fee_cache.get(),
        risk_buffer: cfg.pps.risk_buffer,
        operational_cost_atomic_per_second: cfg.pps.operational_cost_atomic_xmr_per_second as f64,
        pool_hashrate: hashrate,
    });
    rate_cache.set(bd.rate, chrono::Utc::now().timestamp());
    rate_cache.set_fee_rate(bd.fee_per_diff);
    Ok(bd.rate)
}

fn effective_pool(single: &str, pool: &[String]) -> Vec<String> {
    let mut out: Vec<String> = pool.iter().cloned().collect();
    if !single.is_empty() && !out.iter().any(|u| u == single) {
        out.push(single.to_string());
    }
    out
}

fn pick_random(urls: &[String], k: usize) -> Vec<String> {
    let mut rng = rand::thread_rng();
    urls.choose_multiple(&mut rng, k).cloned().collect()
}

/// Weighted-random sample without replacement, biased away from URLs with
/// a high recent-failure score. A node with zero score has full weight; a
/// node that just failed has weight `1/(1+HEALTH_PENALTY) = 0.5`. Falls
/// back to uniform if weights are degenerate (e.g. all zero, which they
/// won't be after the first failure but the rand API is finicky about it).
fn pick_weighted(urls: &[String], health: &HashMap<String, f64>, k: usize) -> Vec<String> {
    let mut rng = rand::thread_rng();
    match urls.choose_multiple_weighted(&mut rng, k, |u| {
        let score = health.get(u).copied().unwrap_or(0.0);
        1.0 / (1.0 + score)
    }) {
        Ok(it) => it.cloned().collect(),
        Err(_) => pick_random(urls, k),
    }
}

/// Fan out `get_info` over the URL list in parallel and return each URL
/// paired with its outcome. Failed fetches stay in the list as `Err` so the
/// caller can update per-node health state.
pub async fn sample_nodes(
    client: &reqwest::Client,
    urls: &[String],
) -> Vec<(String, Result<NodeSample>)> {
    let futures = urls.iter().cloned().map(|u| {
        let client = client.clone();
        async move {
            let result = fetch_one(&client, u.clone()).await;
            (u, result)
        }
    });
    futures::future::join_all(futures).await
}

/// Fan out `get_info` across the URL list, return the difficulty at the
/// latest height that has at least `quorum` matching reports. Heights within
/// one block of each other are NOT merged — we use the highest height with
/// quorum, on the assumption that honest nodes will all eventually agree.
pub async fn quorum_difficulty(
    client: &reqwest::Client,
    urls: &[String],
    quorum: usize,
) -> Result<u64> {
    let outcomes = sample_nodes(client, urls).await;
    let samples: Vec<NodeSample> = outcomes
        .into_iter()
        .filter_map(|(_, r)| match r {
            Ok(s) => Some(s),
            Err(e) => {
                warn!(error=%e, "monerod sample failed");
                None
            }
        })
        .collect();
    decide_quorum(&samples, quorum)
}

async fn fetch_one(client: &reqwest::Client, url: String) -> Result<NodeSample> {
    let resp: Rpc<GetInfoResult> = client
        .post(&url)
        .json(&serde_json::json!({"jsonrpc":"2.0","id":"0","method":"get_info"}))
        .send()
        .await?
        .json()
        .await?;
    Ok(NodeSample {
        url,
        height: resp.result.height,
        difficulty: resp.result.difficulty,
    })
}

/// Group samples by `(height, difficulty)`. Pick the bucket with the latest
/// height that meets quorum. If multiple difficulties share that height,
/// none of them is canonical → fail.
pub fn decide_quorum(samples: &[NodeSample], quorum: usize) -> Result<u64> {
    if samples.is_empty() {
        return Err(anyhow!("no monerod samples"));
    }
    let mut by_pair: HashMap<(u64, u64), usize> = HashMap::new();
    for s in samples {
        *by_pair.entry((s.height, s.difficulty)).or_insert(0) += 1;
    }
    // Group by height first; for each height, find the difficulty (if any)
    // that has quorum agreement. Among heights that have a quorum-difficulty,
    // pick the highest height.
    let mut by_height: HashMap<u64, Vec<((u64, u64), usize)>> = HashMap::new();
    for ((h, d), n) in by_pair {
        by_height.entry(h).or_default().push(((h, d), n));
    }
    let mut heights: Vec<u64> = by_height.keys().copied().collect();
    heights.sort_unstable_by(|a, b| b.cmp(a)); // descending

    for h in heights {
        let buckets = &by_height[&h];
        let mut quorum_diffs: Vec<u64> = buckets
            .iter()
            .filter(|(_, n)| *n >= quorum)
            .map(|((_, d), _)| *d)
            .collect();
        if quorum_diffs.len() == 1 {
            return Ok(quorum_diffs.remove(0));
        }
        if quorum_diffs.len() > 1 {
            return Err(anyhow!(
                "multiple difficulties meet quorum at height {h}: {quorum_diffs:?}"
            ));
        }
        // No quorum at this height; try the next-lower height.
    }
    Err(anyhow!(
        "no (height, difficulty) bucket reached quorum of {quorum} across {} samples",
        samples.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(url: &str, h: u64, d: u64) -> NodeSample {
        NodeSample {
            url: url.into(),
            height: h,
            difficulty: d,
        }
    }

    #[test]
    fn all_agree() {
        let v = vec![
            s("a", 100, 999),
            s("b", 100, 999),
            s("c", 100, 999),
        ];
        assert_eq!(decide_quorum(&v, 2).unwrap(), 999);
    }

    #[test]
    fn one_outlier_quorum_still_met() {
        let v = vec![
            s("a", 100, 999),
            s("b", 100, 999),
            s("c", 100, 12345), // outlier
        ];
        assert_eq!(decide_quorum(&v, 2).unwrap(), 999);
    }

    #[test]
    fn prefers_higher_height_when_quorum_met_there() {
        // c is one block ahead; b and c agree on the new difficulty; a still
        // sees the old block. Quorum=2 at height 101 → take that.
        let v = vec![
            s("a", 100, 100),
            s("b", 101, 200),
            s("c", 101, 200),
        ];
        assert_eq!(decide_quorum(&v, 2).unwrap(), 200);
    }

    #[test]
    fn ambiguous_at_same_height_errors() {
        // 2 nodes report difficulty 100 at height 5, 2 nodes report
        // difficulty 200 at the same height — that means honest nodes
        // can't disagree at the same height, so it's an attack signal.
        let v = vec![
            s("a", 5, 100),
            s("b", 5, 100),
            s("c", 5, 200),
            s("d", 5, 200),
        ];
        assert!(decide_quorum(&v, 2).is_err());
    }

    #[test]
    fn no_quorum_errors() {
        let v = vec![
            s("a", 100, 999),
            s("b", 101, 200),
            s("c", 102, 300),
        ];
        // Each height has only one reporter; quorum=2 isn't met anywhere.
        assert!(decide_quorum(&v, 2).is_err());
    }

    #[test]
    fn empty_errors() {
        assert!(decide_quorum(&[], 1).is_err());
    }

    #[test]
    fn weighted_sampler_biases_away_from_unhealthy() {
        // Three URLs, "bad" has a heavy penalty score, "good_a" / "good_b"
        // are healthy. Sample 2 of 3 many times — the bad one should be
        // picked notably less often than the healthy ones.
        let urls = vec!["good_a".to_string(), "good_b".to_string(), "bad".to_string()];
        let mut health = HashMap::new();
        health.insert("bad".to_string(), 10.0); // weight ~ 1/11 vs healthy 1
        let mut bad_picks = 0;
        let trials = 2000;
        for _ in 0..trials {
            let picks = pick_weighted(&urls, &health, 2);
            if picks.iter().any(|u| u == "bad") {
                bad_picks += 1;
            }
        }
        // With uniform sampling we'd expect "bad" in ~2/3 = 1333 of 2000.
        // With weight 1/11 it should be far below. Allow generous slack.
        assert!(
            bad_picks < 800,
            "unhealthy node was picked {bad_picks}/{trials} times — bias not working"
        );
    }

    #[test]
    fn weighted_sampler_uniform_when_all_healthy() {
        let urls = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let health = HashMap::new();
        // Just confirm we get 2 distinct picks from the 3-pool every time.
        for _ in 0..50 {
            let picks = pick_weighted(&urls, &health, 2);
            assert_eq!(picks.len(), 2);
            assert_ne!(picks[0], picks[1]);
        }
    }

    #[test]
    fn fallback_to_lower_height_if_top_lacks_quorum() {
        // Just one node is ahead; others agree on the older height.
        let v = vec![
            s("a", 100, 999),
            s("b", 100, 999),
            s("c", 101, 1234),
        ];
        assert_eq!(decide_quorum(&v, 2).unwrap(), 999);
    }
}
