//! Fee → ROSE auto-swap.
//!
//! The pool's `[pps].pool_fee` cut is tracked per-share as it's earned: every
//! credited share accrues `pool_fee × raw_per_diff × difficulty` into the
//! `fee:accrued` counter (the part NOT credited to the miner, who is paid NET).
//! That accrual is the swappable fee. This task realizes it as native ROSE for
//! rent, autonomously:
//!
//!   1. mint fee-MPT for the un-minted accrual (`fee:accrued − FeeSwapper.claimed`,
//!      a voucher for the FeeSwapper signed by the enclave key — the same
//!      mechanism miners use, so no new mint authority). In production the mint is
//!      additionally capped by the wallet's real XMR surplus so it stays
//!      reserve-safe; the accrued fee is backed because miners only ever had a
//!      claim on the net. Then
//!   2. sell it for ROSE on the MPT/WROSE Uniswap pool via `FeeSwapper`, which
//!      forwards the proceeds straight to the rent reservoir.
//!
//! It fires only when **necessary** (the reservoir's ROSE balance is below
//! `rent_floor_wei`) and **profitable** (the live DEX quote clears the slippage
//! band; the on-chain `minOut` makes a thin/manipulated book a no-op, never a
//! loss), at a **randomized** cadence so the swap isn't front-runnable.
//!
//! All writes are LEGACY transactions — Sapphire supports only type-0 (see
//! `marker.rs`).

use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, RootProvider};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::Signer;
use alloy::sol;
use alloy::sol_types::{eip712_domain, SolCall, SolStruct};
use alloy::transports::http::{Client as HttpTransport, Http};
use anyhow::{bail, Context, Result};
use pool_core::cache::{effective_pool_fee, FeeCache};
use pool_core::config::FeeSwapConfig;
use pool_core::store::Store;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

sol! {
    struct Voucher {
        address user;
        uint256 cumulativeAmount;
        uint256 signedAt;
    }
    interface IMiningPoolToken {
        function claimed(address user) external view returns (uint256);
        function claim(address user, uint256 cumulativeAmount, uint256 signedAt, bytes sig) external returns (uint256);
    }
    interface IFeeSwapper {
        function reservoir() external view returns (address);
        function quoteRoseOut(uint256 mptIn) external view returns (uint256);
        function swapFeeToRose(uint256 mptIn, uint256 minOut, uint256 deadline) external returns (uint256);
    }
}

pub struct FeeSwapTask {
    pub read_provider: RootProvider<Http<HttpTransport>>,
    /// rofl-appd unix socket. The claim + swap are submitted through it as
    /// APP-ORIGIN evm.Calls, so the funded app account pays gas (no separately
    /// seeded operator EOA) and the swap satisfies FeeSwapper's
    /// roflEnsureAuthorizedOrigin gate. `signer` is used only to EIP-712-sign the
    /// claim voucher off-chain (no gas).
    pub appd_socket: String,
    pub signer: PrivateKeySigner,
    pub chain_id: u64,
    pub token: Address,
    pub fee_swapper: Address,
    pub reserve_ratio: f64,
    pub store: Store,
    pub cfg: FeeSwapConfig,
}

impl FeeSwapTask {
    pub async fn run_loop(&self) {
        info!(
            fee_swapper = %self.fee_swapper,
            "fee-swap task started"
        );
        loop {
            // Base cadence + a uniform random jitter so timing is unpredictable.
            let jitter = if self.cfg.jitter_secs > 0 {
                rand::random::<u64>() % (self.cfg.jitter_secs + 1)
            } else {
                0
            };
            tokio::time::sleep(Duration::from_secs(self.cfg.check_interval_secs + jitter)).await;
            match self.tick().await {
                Ok(Some(out)) => info!(
                    mpt_in = out.0,
                    rose_out = out.1,
                    "fee-swap: converted fee surplus to ROSE for rent"
                ),
                Ok(None) => {}
                Err(e) => warn!(error = %format!("{e:#}"), "fee-swap tick failed"),
            }
        }
    }

    /// One evaluation. Returns Some((mptIn, roseOut)) if a swap happened.
    pub async fn tick(&self) -> Result<Option<(u64, u128)>> {
        // --- necessary? reservoir rent low ---
        let reservoir = self.reservoir().await.context("read reservoir")?;
        let bal = self
            .read_provider
            .get_balance(reservoir)
            .await
            .context("reservoir balance")?;
        let floor = self
            .cfg
            .rent_floor_wei
            .trim()
            .parse::<U256>()
            .context("parse rent_floor_wei")?;
        if bal >= floor {
            return Ok(None); // rent topped up; nothing to do
        }

        // --- how much accrued fee remains un-minted? ---
        // The pool's `pool_fee` cut is accrued per-share into `fee:accrued`; the
        // FeeSwapper's on-chain cumulative `claimed` is what we've already minted
        // to it. The difference is the fee we've earned but not yet realized.
        let prev = self.claimed(self.fee_swapper).await.context("claimed")?;
        let accrued = self.store.fee_accrued().await? as u128;

        // --- reserve safety cap ---
        // In production the accrued fee is backed by real XMR surplus in the
        // wallet — the fee cut was never promised to miners (they're credited
        // NET), so minting it is non-dilutive up to that surplus. Cap the mint by
        // the wallet surplus so we never out-mint the backing. During test the
        // wallet snapshot reads 0 (stub upstream); pass no cap then so the
        // self-funding loop still runs end-to-end ("unbacked during test is fine").
        let wallet_cap = match self.store.treasury_snapshot().await? {
            Some(snap) if snap.monero_unlocked_atomic > 0 => Some(self.mintable_fee_atomic(
                snap.monero_unlocked_atomic,
                snap.mining_pool_token_total_supply,
                snap.pending_redemptions_atomic,
            )),
            _ => None,
        };

        let delta = swap_delta_atomic(
            accrued,
            u256_to_u128(prev),
            self.cfg.max_swap_atomic,
            wallet_cap,
        );
        if delta < self.cfg.min_swap_atomic {
            return Ok(None); // not enough fee accrued (or backed) to bother
        }

        // --- profitable? quote the DEX, set a slippage-bounded minOut ---
        let quote = self.quote_rose_out(delta).await.context("quote")?;
        if quote == 0 {
            warn!("fee-swap: DEX quote is 0 (no liquidity?); skipping");
            return Ok(None);
        }
        let min_out = quote * u128::from(10_000 - self.cfg.slippage_bps) / 10_000;

        // --- worth the gas? batch instead of trading dust. The swap is two txs
        //     (claim ~200k + swapFeeToRose ~400k); require the ROSE proceeds to
        //     clear that gas cost by `min_swap_gas_multiple`× or skip and let the
        //     surplus accumulate for a later, larger swap. ---
        let gas_price = self.read_provider.get_gas_price().await.context("eth_gasPrice")?;
        let swap_gas_cost = gas_price.saturating_mul(600_000);
        let gas_floor = swap_gas_cost.saturating_mul(self.cfg.min_swap_gas_multiple.max(1) as u128);
        if min_out < gas_floor {
            info!(
                rose_out = min_out,
                gas_cost = swap_gas_cost,
                "fee-swap: proceeds don't clear the swap gas yet — holding to batch"
            );
            return Ok(None);
        }

        // --- mint fee-MPT against the accrued fee (self-signed voucher) ---
        let new_cum = prev + U256::from(delta);
        let sig = self.sign_voucher(self.fee_swapper, new_cum).await?;
        self.send(
            self.token,
            IMiningPoolToken::claimCall {
                user: self.fee_swapper,
                cumulativeAmount: new_cum,
                signedAt: U256::from(now_secs()),
                sig: sig.into(),
            }
            .abi_encode(),
            200_000,
        )
        .await
        .context("claim fee-MPT")?;

        // --- sell it for ROSE → reservoir ---
        let deadline = U256::from(now_secs() + 300);
        self.send(
            self.fee_swapper,
            IFeeSwapper::swapFeeToRoseCall {
                mptIn: U256::from(delta),
                minOut: U256::from(min_out),
                deadline,
            }
            .abi_encode(),
            400_000,
        )
        .await
        .context("swapFeeToRose")?;

        Ok(Some((delta, min_out)))
    }

    fn mintable_fee_atomic(&self, unlocked: u128, supply: u128, pending: u128) -> u64 {
        mintable_fee(unlocked, supply, pending, self.reserve_ratio, self.cfg.max_swap_atomic)
    }

    fn domain(&self) -> alloy::sol_types::Eip712Domain {
        eip712_domain! {
            name: "MiningPoolToken",
            version: "1",
            chain_id: self.chain_id,
            verifying_contract: self.token,
        }
    }

    async fn sign_voucher(&self, user: Address, cum: U256) -> Result<Vec<u8>> {
        let v = Voucher {
            user,
            cumulativeAmount: cum,
            signedAt: U256::from(now_secs()),
        };
        let digest = v.eip712_signing_hash(&self.domain());
        let sig = self.signer.sign_hash(&digest).await.context("sign voucher")?;
        Ok(sig.as_bytes().to_vec())
    }

    async fn reservoir(&self) -> Result<Address> {
        let res = self
            .read_call(self.fee_swapper, IFeeSwapper::reservoirCall {}.abi_encode())
            .await?;
        Ok(IFeeSwapper::reservoirCall::abi_decode_returns(&res, true)?._0)
    }

    async fn quote_rose_out(&self, mpt_in: u64) -> Result<u128> {
        let res = self
            .read_call(
                self.fee_swapper,
                IFeeSwapper::quoteRoseOutCall { mptIn: U256::from(mpt_in) }.abi_encode(),
            )
            .await?;
        let v = IFeeSwapper::quoteRoseOutCall::abi_decode_returns(&res, true)?._0;
        Ok(u256_to_u128(v))
    }

    async fn claimed(&self, user: Address) -> Result<U256> {
        let res = self
            .read_call(self.token, IMiningPoolToken::claimedCall { user }.abi_encode())
            .await?;
        Ok(IMiningPoolToken::claimedCall::abi_decode_returns(&res, true)?._0)
    }

    async fn read_call(&self, to: Address, data: Vec<u8>) -> Result<alloy::primitives::Bytes> {
        let req = TransactionRequest::default().to(to).input(data.into());
        self.read_provider
            .call(&req)
            .block(alloy::eips::BlockId::latest())
            .await
            .context("eth_call")
    }

    /// Submit `data` to `to` as an APP-ORIGIN evm.Call via rofl-appd. The app
    /// account pays gas (no operator EOA float to babysit) and the call carries
    /// the ROFL origin that FeeSwapper.swapFeeToRose requires. appd encrypts the
    /// calldata (Sapphire confidential) and returns the call result; it errors on
    /// a non-2xx, which surfaces here.
    async fn send(&self, to: Address, data: Vec<u8>, gas_limit: u64) -> Result<()> {
        let resp =
            pool_core::appd::sign_submit_eth(&self.appd_socket, to.into_array(), &data, gas_limit)
                .await
                .context("appd sign-submit (app-origin)")?;
        // Defensive: appd returns JSON; if it carries an explicit error field,
        // treat it as a failure rather than a silent no-op.
        if resp.contains("\"error\"") {
            bail!("appd sign-submit reported an error: {resp}");
        }
        Ok(())
    }
}

/// Largest fee-MPT mintable now without breaching the redemption reserve.
/// Minting Δ raises outstanding by Δ, so we need
/// `unlocked ≥ (supply + pending + Δ) × ratio`  ⇒  `Δ ≤ surplus / ratio`,
/// where `surplus = unlocked − (supply + pending) × ratio`. Capped by `max_swap`.
/// Pure (no I/O) so the reserve-safety invariant is unit-testable.
fn mintable_fee(unlocked: u128, supply: u128, pending: u128, ratio: f64, max_swap: u64) -> u64 {
    let ratio = ratio.max(1.0);
    let required = (supply + pending) as f64 * ratio;
    let surplus = unlocked as f64 - required;
    if surplus <= 0.0 {
        return 0;
    }
    ((surplus / ratio).floor()).min(max_swap as f64).max(0.0) as u64
}

/// Un-minted accrued fee to realize this tick. The accrual `fee:accrued` is the
/// cumulative pool fee earned; `already_minted` is the FeeSwapper's on-chain
/// cumulative `claimed`. We mint the difference, bounded by `max_swap` (don't
/// drain it all at once) and — when `wallet_cap` is `Some` (a real, non-zero
/// wallet snapshot) — by the reserve-safe wallet surplus so we never out-mint the
/// backing. `wallet_cap == None` (stub/test wallet) skips that bound. Pure so the
/// monotonicity + cap invariants are unit-testable without RPC or Redis.
fn swap_delta_atomic(
    accrued: u128,
    already_minted: u128,
    max_swap: u64,
    wallet_cap: Option<u64>,
) -> u64 {
    if accrued <= already_minted {
        return 0; // nothing new accrued since the last swap
    }
    let mut delta = (accrued - already_minted).min(max_swap as u128);
    if let Some(cap) = wallet_cap {
        delta = delta.min(cap as u128);
    }
    delta as u64
}

/// Adaptive-fee controller: periodically reads the rent reservoir's native
/// balance and publishes an effective pool fee into the shared `FeeCache` that
/// pps-rate reads — raising the pool's cut as rent runs low, lowering it when
/// flush. Decoupled from pps-rate (which only reads the cache). With
/// `fee_min == fee_max` it's a no-op (constant fee), so enabling adaptive mode
/// without bounds is safe.
pub struct FeeController {
    pub read_provider: RootProvider<Http<HttpTransport>>,
    pub fee_swapper: Address,
    pub fee_cache: Arc<FeeCache>,
    pub critical_wei: u128,
    pub healthy_wei: u128,
    pub fee_min: f64,
    pub fee_max: f64,
    pub interval_secs: u64,
}

impl FeeController {
    pub async fn run_loop(&self) {
        info!(fee_swapper = %self.fee_swapper, "adaptive fee controller started");
        loop {
            match self.tick().await {
                Ok(fee) => info!(effective_fee = fee, "adaptive fee updated"),
                Err(e) => warn!(error = %format!("{e:#}"), "adaptive fee tick failed"),
            }
            tokio::time::sleep(Duration::from_secs(self.interval_secs.max(1))).await;
        }
    }

    pub async fn tick(&self) -> Result<f64> {
        let res = self
            .read_provider
            .call(
                &TransactionRequest::default()
                    .to(self.fee_swapper)
                    .input(IFeeSwapper::reservoirCall {}.abi_encode().into()),
            )
            .block(alloy::eips::BlockId::latest())
            .await
            .context("read reservoir")?;
        let reservoir = IFeeSwapper::reservoirCall::abi_decode_returns(&res, true)?._0;
        let bal = self
            .read_provider
            .get_balance(reservoir)
            .await
            .context("reservoir balance")?;
        let fee = effective_pool_fee(
            u256_to_u128(bal),
            self.critical_wei,
            self.healthy_wei,
            self.fee_min,
            self.fee_max,
        );
        self.fee_cache.set(fee);
        Ok(fee)
    }
}

fn now_secs() -> u64 {
    chrono::Utc::now().timestamp().max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::{mintable_fee, swap_delta_atomic};

    #[test]
    fn delta_is_accrued_minus_minted() {
        // 1000 accrued, 300 already minted → 700 to realize.
        assert_eq!(swap_delta_atomic(1000, 300, u64::MAX, None), 700);
    }

    #[test]
    fn delta_zero_when_nothing_new() {
        // fully caught up, or claimed somehow ahead → never negative/underflow.
        assert_eq!(swap_delta_atomic(500, 500, u64::MAX, None), 0);
        assert_eq!(swap_delta_atomic(500, 900, u64::MAX, None), 0);
    }

    #[test]
    fn delta_capped_by_max_swap() {
        // 10_000 un-minted but max_swap 2_000 → only 2_000 this tick (rest later).
        assert_eq!(swap_delta_atomic(10_000, 0, 2_000, None), 2_000);
    }

    #[test]
    fn delta_capped_by_wallet_surplus_in_production() {
        // 5_000 accrued, but the wallet only backs 1_200 of surplus → mint 1_200.
        assert_eq!(swap_delta_atomic(5_000, 0, u64::MAX, Some(1_200)), 1_200);
    }

    #[test]
    fn delta_skips_wallet_cap_when_none() {
        // test/stub wallet (None) → accrual drives, unbacked allowed.
        assert_eq!(swap_delta_atomic(5_000, 0, u64::MAX, None), 5_000);
    }

    #[test]
    fn delta_takes_the_tightest_bound() {
        // min(accrued-minted=4000, max_swap=3000, wallet_cap=1500) = 1500.
        assert_eq!(swap_delta_atomic(4_000, 0, 3_000, Some(1_500)), 1_500);
        // zero wallet surplus parks the swap even with accrual present.
        assert_eq!(swap_delta_atomic(4_000, 0, 3_000, Some(0)), 0);
    }

    #[test]
    fn no_surplus_mints_nothing() {
        // unlocked exactly covers reserve → nothing free.
        assert_eq!(mintable_fee(1050, 1000, 0, 1.05, u64::MAX), 0);
        // under-reserved → nothing (never dip into miner backing).
        assert_eq!(mintable_fee(900, 1000, 0, 1.05, u64::MAX), 0);
    }

    #[test]
    fn mints_surplus_over_reserve_and_stays_safe() {
        // unlocked 2100, supply 1000, ratio 1.05 → required 1050, surplus 1050,
        // Δ ≤ 1050/1.05 = 1000.
        let d = mintable_fee(2100, 1000, 0, 1.05, u64::MAX);
        assert_eq!(d, 1000);
        // After minting Δ, reserve still holds: unlocked ≥ (supply+Δ)×ratio.
        assert!(2100.0 >= (1000 + d) as f64 * 1.05);
    }

    #[test]
    fn pending_counts_against_reserve() {
        // pending redemptions raise the floor, shrinking mintable surplus.
        assert!(mintable_fee(2100, 1000, 500, 1.05, u64::MAX) < mintable_fee(2100, 1000, 0, 1.05, u64::MAX));
    }

    #[test]
    fn respects_max_cap() {
        assert_eq!(mintable_fee(1_000_000, 0, 0, 1.05, 5_000), 5_000);
    }
}

fn u256_to_u128(v: U256) -> u128 {
    let limbs = v.into_limbs();
    if limbs[2] | limbs[3] != 0 {
        return u128::MAX;
    }
    (limbs[0] as u128) | ((limbs[1] as u128) << 64)
}
