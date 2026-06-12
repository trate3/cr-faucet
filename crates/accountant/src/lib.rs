//! Accountant: per-share crediting against Redis. One round-trip per share
//! (HINCRBY), rate read from in-process cache, metrics updated in memory.
//!
//! Durability is configured at the Redis level (AOF=everysec is the
//! recommended TEE setting; ≤1s of credits lost on hard crash). The contract's
//! `claimed[user]` map is the only solvency-critical state, and lives on the
//! L2 — not here.

use anyhow::Result;
use pool_core::cache::RateCache;
use pool_core::metrics::Metrics;
use pool_core::store::Store;
use pool_core::ShareAccepted;

#[inline]
pub async fn credit(
    store: &Store,
    rate: &RateCache,
    metrics: &Metrics,
    share: &ShareAccepted,
) -> Result<i64> {
    let rate_value = rate.get();
    let credit = (rate_value * share.difficulty as f64) as i64;
    if credit < 0 {
        return Ok(0);
    }
    let addr_bytes: [u8; 20] = share.miner.0.into_array();
    metrics.record_share(&addr_bytes, share.difficulty, std::time::Instant::now());
    if credit == 0 {
        return Ok(0);
    }
    let _new_total = store.add_earned(share.miner.0, credit).await?;

    // Accrue the pool's fee cut for this share — the `pool_fee` portion NOT
    // credited to the miner. Tracked separately so the fee-swap can self-mint
    // `fee_accrued − claimed` MPT → ROSE to pay for the pool's own rent. The
    // `risk_buffer` portion is deliberately NOT accrued: it stays as the reserve
    // cushion backing redemptions, never swept. Minting against this is
    // non-dilutive — miners only ever had a claim on the net (already credited).
    let fee = (rate.fee_rate() * share.difficulty as f64) as i64;
    if fee > 0 {
        store.add_fee_accrued(fee).await?;
    }
    Ok(credit)
}
