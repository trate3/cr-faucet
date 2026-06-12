//! ID-based redemption poller. Reads two contract slots per tick:
//!   - `MiningPoolToken.nextRedemptionId()` — monotonically increasing counter
//!   - `MiningPoolToken.redemptions(id)` — the (user, amount, xmrAddress) for
//!     each id past our Redis cursor
//!
//! Replaces the older `eth_getLogs`-based poller. The log approach
//! required scanning every Sapphire block since the contract deploy
//! (Sapphire caps log queries at 100 blocks per call), which made
//! catch-up slow and resource-hostile. An id-based scan is O(redemptions)
//! and bounded by two cheap eth_call's per tick.
//!
//! Idempotent: `Store::enqueue_redemption` is keyed by `id`, so
//! re-fetching an id that was already enqueued is a no-op.

use alloy::eips::BlockId;
use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, RootProvider};
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};
use alloy::transports::http::{Client as HttpTransport, Http};
use anyhow::{Context, Result};
use pool_core::store::Store;
use std::time::Duration;
use tracing::{info, warn};

pub const CURSOR_NAME: &str = "redemption_events";

sol! {
    /// Mirror of the public storage + getter on MiningPoolToken.
    interface IMiningPoolToken {
        function nextRedemptionId() external view returns (uint256);
        function redemptions(uint256 id) external view returns (
            address user,
            uint256 amount,
            string memory xmrAddress
        );
    }
}

pub struct EventPoller {
    pub provider: RootProvider<Http<HttpTransport>>,
    pub store: Store,
    pub mining_pool_token: Address,
    /// First id we're allowed to look at. Usually 1; lets a fresh
    /// deployment skip historical redemptions if you've migrated.
    pub start_block: u64,
    /// Max ids to fetch in a single tick. Bounds the time spent in the
    /// loop when we're catching up from a long downtime. Reuses the
    /// old `events_chunk_size` config so existing pool.tomls Just Work.
    pub chunk_size: u64,
    /// Sleep between ticks when nothing new is found.
    pub poll_interval: Duration,
}

impl EventPoller {
    pub async fn run(&self) -> Result<()> {
        loop {
            let processed = self.tick().await.context("redemption poll tick")?;
            if processed == 0 {
                tokio::time::sleep(self.poll_interval).await;
            }
        }
    }

    pub async fn run_loop(&self) {
        loop {
            match self.tick().await {
                Ok(n) => {
                    if n == 0 {
                        tokio::time::sleep(self.poll_interval).await;
                    }
                }
                Err(e) => {
                    warn!(error=%e, "redemption poll tick failed; retrying after backoff");
                    tokio::time::sleep(self.poll_interval * 2).await;
                }
            }
        }
    }

    /// Pull at most `chunk_size` redemptions past our cursor. Returns the
    /// number of new ids enqueued.
    pub async fn tick(&self) -> Result<usize> {
        // Cursor is "last id we've already enqueued". Default to
        // `start_block - 1` so the first tick picks up id=start_block.
        let cursor = self
            .store
            .get_cursor(CURSOR_NAME)
            .await?
            .unwrap_or(self.start_block.saturating_sub(1));
        let next_id = self
            .read_next_redemption_id()
            .await
            .context("read nextRedemptionId")?;
        tracing::debug!(cursor, next_id, "redemption poller tick");
        if next_id <= cursor {
            return Ok(0);
        }
        let from = cursor + 1;
        let to = (from + self.chunk_size - 1).min(next_id);

        let mut enqueued = 0usize;
        for id in from..=to {
            let r = self
                .read_redemption(id)
                .await
                .with_context(|| format!("read redemption #{id}"))?;
            let atomic = u256_to_i64(r.amount)?;
            let inserted = self
                .store
                .enqueue_redemption(id, r.user, atomic, &r.xmr_address)
                .await?;
            if inserted {
                enqueued += 1;
            }
            // Advance the cursor per id so a transient failure mid-chunk
            // doesn't make us redo work on the next tick.
            self.store.set_cursor(CURSOR_NAME, id).await?;
        }
        if enqueued > 0 {
            info!(from, to, enqueued, "advanced redemption cursor");
        }
        Ok(enqueued)
    }

    async fn read_next_redemption_id(&self) -> Result<u64> {
        let call = IMiningPoolToken::nextRedemptionIdCall {};
        let req = TransactionRequest::default()
            .to(self.mining_pool_token)
            .input(call.abi_encode().into());
        let res = self
            .provider
            .call(&req)
            .block(BlockId::latest())
            .await?;
        let v = U256::abi_decode(&res, true)?;
        u256_to_u64(v)
    }

    async fn read_redemption(&self, id: u64) -> Result<RedemptionRow> {
        let call = IMiningPoolToken::redemptionsCall {
            id: U256::from(id),
        };
        let req = TransactionRequest::default()
            .to(self.mining_pool_token)
            .input(call.abi_encode().into());
        let res = self
            .provider
            .call(&req)
            .block(BlockId::latest())
            .await?;
        let decoded = IMiningPoolToken::redemptionsCall::abi_decode_returns(&res, true)?;
        Ok(RedemptionRow {
            user: decoded.user,
            amount: decoded.amount,
            xmr_address: decoded.xmrAddress,
        })
    }
}

struct RedemptionRow {
    user: Address,
    amount: U256,
    xmr_address: String,
}

fn u256_to_u64(v: U256) -> Result<u64> {
    let limbs = v.into_limbs();
    if limbs[1] | limbs[2] | limbs[3] != 0 {
        anyhow::bail!("u256 redemption id too large for u64");
    }
    Ok(limbs[0])
}

fn u256_to_i64(v: U256) -> Result<i64> {
    let limbs = v.into_limbs();
    if limbs[1] | limbs[2] | limbs[3] != 0 || limbs[0] > i64::MAX as u64 {
        anyhow::bail!("u256 amount too large for i64");
    }
    Ok(limbs[0] as i64)
}
