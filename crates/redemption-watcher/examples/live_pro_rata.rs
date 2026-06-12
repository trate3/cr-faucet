//! Live pro-rata redemption demo against real monero-wallet-rpc.
//!
//! Mirrors the user's worked example:
//!   wallet = 0.6 XMR, totalSupply = 4 MiningPoolToken, pending = 2 MiningPoolToken,
//!   next redeem = 1.0 MiningPoolToken → payout = 0.1 XMR.
//!
//! Env:
//!   SOURCE_WALLET_RPC      e.g. http://127.0.0.1:18083/json_rpc
//!   RECIPIENT_WALLET_RPC   e.g. http://127.0.0.1:18084/json_rpc
//!   MONEROD_RPC            e.g. http://127.0.0.1:18081/json_rpc
//!   REDIS_URL              e.g. redis://127.0.0.1:6379
//!   WALLET_TARGET_ATOMIC   default 600_000_000_000 (0.6 XMR)
//!   TOTAL_SUPPLY_TOKENS    default 4 (MiningPoolToken whole tokens, 12 decimals)
//!   PENDING_TOKENS         default 2
//!   BURN_TOKENS            default 1   (the redeem to be processed)
//!
//! Assumes monerod is running with --regtest --offline --fixed-difficulty 1
//! and the source wallet is already funded enough to make `transfer` work
//! (i.e. mine 200+ blocks to it first; the live_real_payout example sets
//! this up).

use anyhow::{Context, Result};
use pool_core::config::MoneroConfig;
use pool_core::store::{Store, TreasurySnapshot};
use redemption_watcher::payouts::Payouts;
use serde_json::{json, Value};

const DECIMALS: u128 = 1_000_000_000_000;

async fn rpc(url: &str, method: &str, params: Value) -> Result<Value> {
    let client = reqwest::Client::new();
    let body = json!({"jsonrpc":"2.0","id":"0","method":method,"params":params});
    let r: Value = client.post(url).json(&body).send().await?.json().await?;
    if let Some(err) = r.get("error") {
        anyhow::bail!("rpc {method} error: {err}");
    }
    Ok(r["result"].clone())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let src = std::env::var("SOURCE_WALLET_RPC")?;
    let dst = std::env::var("RECIPIENT_WALLET_RPC")?;
    let monerod = std::env::var("MONEROD_RPC")?;
    let redis_url = std::env::var("REDIS_URL")?;
    let target_balance: u128 = std::env::var("WALLET_TARGET_ATOMIC")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(600_000_000_000); // 0.6 XMR
    let total_supply_tokens: u128 = std::env::var("TOTAL_SUPPLY_TOKENS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4);
    let pending_tokens: u128 = std::env::var("PENDING_TOKENS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    let burn_tokens: u128 = std::env::var("BURN_TOKENS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);

    let dest_addr = {
        let r = rpc(&dst, "get_address", json!({"account_index":0})).await?;
        r["address"].as_str().unwrap().to_string()
    };
    let src_addr = {
        let r = rpc(&src, "get_address", json!({"account_index":0})).await?;
        r["address"].as_str().unwrap().to_string()
    };
    eprintln!("source address: {src_addr}");
    eprintln!("dest   address: {dest_addr}");

    // Reset to a clean Redis state.
    let store = Store::connect(&redis_url).await?;
    {
        let mut c = store.conn();
        let _: () = redis::cmd("FLUSHDB").query_async(&mut c).await?;
    }

    // Enqueue the burn-to-process. We only enqueue one; the snapshot's
    // pending value declares "what the rest of the queue notionally is".
    let evm_user: alloy::primitives::Address = "0x000000000000000000000000000000000000abc0".parse()?;
    let burn_base_units = burn_tokens * DECIMALS;
    store
        .enqueue_redemption(42, evm_user, burn_base_units as i64, &dest_addr)
        .await?;

    // Seed the snapshot to mirror the worked example exactly. In prod the
    // refresher does this from live RPCs.
    let snap = TreasurySnapshot {
        monero_balance_atomic: target_balance,
        monero_unlocked_atomic: target_balance,
        // The snapshot's pending value is what gets read at refresh time.
        // The drain handler reads pending LIVE from Store; only the math
        // for totalSupply + pending uses the live value. So this snapshot
        // value is shown to users in /treasury but the math uses live.
        pending_redemptions_atomic: pending_tokens * DECIMALS,
        pending_redemptions_count: pending_tokens as u64,
        // Our enqueue above already incremented the live pending counter
        // by burn_base_units. The snapshot's totalSupply represents
        // "tokens still in circulation, NOT yet burned". So we set it to
        // (total_supply_tokens) and the live pending is now
        // (burn_base_units). For the example to land on 0.1 XMR with
        // wallet=0.6 and 4-active+2-pending, we want denom = 6T. The live
        // pending is only burn_base_units = 1T, so totalSupply must be
        // 5T to make denom = 6T. Set accordingly.
        mining_pool_token_total_supply: (total_supply_tokens + pending_tokens - burn_tokens) * DECIMALS,
        as_of_unix: chrono::Utc::now().timestamp(),
    };
    store.set_treasury_snapshot(&snap).await?;

    let expected_payout =
        burn_base_units * snap.monero_balance_atomic / (snap.mining_pool_token_total_supply + burn_base_units);
    eprintln!(
        "expected: burn={burn_tokens}T × wallet={target_balance} / (totalSupply={} + pending_live={burn_base_units}) = {expected_payout} atomic XMR",
        snap.mining_pool_token_total_supply
    );

    let before = rpc(&dst, "get_balance", json!({"account_index":0})).await?;
    let before_unl = before["unlocked_balance"].as_u64().unwrap_or(0);
    eprintln!("recipient before: unlocked={before_unl}");

    let payouts = Payouts::new(
        store.clone(),
        MoneroConfig {
            wallet_rpc: src.clone(),
            wallet_rpc_user: None,
            wallet_rpc_pass: None,
            min_reserve_ratio: 1.0,
            per_tx_cap_atomic: u64::MAX,
            per_day_cap_atomic: u64::MAX,
            max_payout_premium_bp: u32::MAX,
            confirm_wait_secs: 0,
            network: pool_core::config::MoneroNetwork::Testnet,
            wallet_filename: "pool".into(),
            wallet_password: "".into(),
            restore_height_lookback: 720,
        },
    )
    .await?;
    let n = payouts.drain_once().await?;
    eprintln!("payouts: processed={n}");

    rpc(
        &monerod,
        "generateblocks",
        json!({"amount_of_blocks": 12, "wallet_address": src_addr}),
    )
    .await
    .context("mine confirmations")?;
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    rpc(&dst, "refresh", json!({})).await?;
    let after = rpc(&dst, "get_balance", json!({"account_index":0})).await?;
    let after_unl = after["unlocked_balance"].as_u64().unwrap_or(0);
    let delta = after_unl - before_unl;
    eprintln!("recipient after:  unlocked={after_unl} delta={delta}");
    eprintln!(
        "expected_payout={} delta={} (delta = payout - fee, so delta < payout by ~Monero fee)",
        expected_payout, delta
    );
    assert!(
        (delta as u128) < expected_payout && (delta as u128) > expected_payout * 95 / 100,
        "delta {} should be within ~5% of expected {} (less fee)",
        delta,
        expected_payout
    );
    Ok(())
}
