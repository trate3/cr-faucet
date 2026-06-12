//! Pro-rata payout: payout = burned × wallet / (totalSupply + pending).
//!
//! The example you specified: 0.6 XMR wallet, 4 active + 2 pending = 6
//! denominator, 1.0 next-redeem → 0.1 XMR payout.
//!
//! Implementation note: amounts are in 12-decimal base units throughout
//! (MiningPoolToken uses 12 decimals, matching XMR's atomic units). So:
//!   - 4 tokens  active = 4 × 10^12 base units
//!   - 2 tokens  pending = 2 × 10^12 base units
//!   - 0.6 XMR  wallet = 6 × 10^11 atomic
//!   - 1.0 token burned = 1 × 10^12 base units
//!   - expected payout = 0.1 XMR = 10^11 atomic
//!
//! Run serially: `cargo test --test pro_rata_payout -- --test-threads=1`.

use axum::{extract::State, routing::post, Json, Router};
use pool_core::config::MoneroConfig;
use pool_core::store::{Store, TreasurySnapshot};
use redemption_watcher::payouts::Payouts;
use serde_json::{json, Value};
use std::sync::Arc;

const DECIMALS: u128 = 1_000_000_000_000;

#[derive(Clone, Default)]
struct WalletState {
    calls: Arc<parking_lot::Mutex<Vec<Value>>>,
}

async fn fake_wallet_rpc() -> (WalletState, u16) {
    let state = WalletState::default();
    let s2 = state.clone();
    let app = Router::new()
        .route(
            "/json_rpc",
            post(|State(s): State<WalletState>, Json(body): Json<Value>| async move {
                s.calls.lock().push(body.clone());
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": body["id"].clone(),
                    "result": { "tx_hash": "fake-pro-rata-tx", "fee": 0, "amount": 0 }
                }))
            }),
        )
        .with_state(s2);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (state, port)
}

#[tokio::test]
async fn matches_users_worked_example() {
    let Some(redis_url) = std::env::var("ANVIL_TEST_REDIS_URL").ok() else { return };
    let store = Store::connect(&redis_url).await.unwrap();
    {
        let mut c = store.conn();
        let _: () = redis::cmd("FLUSHDB").query_async(&mut c).await.unwrap();
    }

    // Pre-state: 4 active, 2 pending (already-burned not-yet-paid), 0.6 XMR wallet.
    let user: alloy::primitives::Address = "0x000000000000000000000000000000000000abcd".parse().unwrap();
    // Enqueue 2 pending: one of them is the "1.0 next-redeem". Make one of
    // size 1.0 (= 10^12) and one of size 1.0 to total 2 tokens pending.
    store.enqueue_redemption(1, user, 1 * DECIMALS as i64, "44addr").await.unwrap();
    store.enqueue_redemption(2, user, 1 * DECIMALS as i64, "44addr").await.unwrap();

    // Active supply = 4 whole mining-pool tokens.
    let snap = TreasurySnapshot {
        monero_balance_atomic: 6 * 10u128.pow(11),    // 0.6 XMR
        monero_unlocked_atomic: 6 * 10u128.pow(11),
        pending_redemptions_atomic: 2 * DECIMALS,     // 2 mining-pool tokens
        pending_redemptions_count: 2,
        mining_pool_token_total_supply: 4 * DECIMALS,        // 4 mining-pool tokens active
        as_of_unix: chrono::Utc::now().timestamp(),
    };
    store.set_treasury_snapshot(&snap).await.unwrap();

    // Run the consumer.
    let (wallet, port) = fake_wallet_rpc().await;
    let payouts = Payouts::new(
        store.clone(),
        MoneroConfig {
            wallet_rpc: format!("http://127.0.0.1:{port}/json_rpc"),
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
    .await
    .unwrap();
    let _ = payouts.drain_once().await.unwrap();

    let calls = wallet.calls.lock().clone();
    let transfer_calls: Vec<&Value> = calls.iter().filter(|v| v["method"] == "transfer").collect();
    assert_eq!(transfer_calls.len(), 2, "both pending entries should be processed");

    // First payout: pending=2T at that moment, denom = 4T + 2T = 6T.
    //   payout = 1T × 6e11 / 6T = 1e11 atomic = 0.1 XMR. ✓
    let p1 = transfer_calls[0]["params"]["destinations"][0]["amount"]
        .as_u64()
        .unwrap();
    assert_eq!(p1, 100_000_000_000, "1st payout should be 0.1 XMR");

    // Second payout: by the time it runs, pending has dropped to 1T (since
    // the 1st's mark_sent debited 1T) and wallet hasn't been re-snapshotted.
    //   payout = 1T × 6e11 / (4T + 1T) = 1.2e11 atomic = 0.12 XMR
    // This is correct: pending dropped, the remaining holders' fair share
    // grew. (In production the refresher would pick up the new wallet
    // balance before this drift compounds.)
    let p2 = transfer_calls[1]["params"]["destinations"][0]["amount"]
        .as_u64()
        .unwrap();
    assert_eq!(p2, 120_000_000_000, "2nd payout uses fresh pending count");
}

#[tokio::test]
async fn pauses_when_no_snapshot() {
    let Some(redis_url) = std::env::var("ANVIL_TEST_REDIS_URL").ok() else { return };
    let store = Store::connect(&redis_url).await.unwrap();
    {
        let mut c = store.conn();
        let _: () = redis::cmd("FLUSHDB").query_async(&mut c).await.unwrap();
    }
    let user: alloy::primitives::Address = "0x000000000000000000000000000000000000eff0".parse().unwrap();
    store.enqueue_redemption(99, user, DECIMALS as i64, "44addr").await.unwrap();
    // NO snapshot set.

    let (wallet, port) = fake_wallet_rpc().await;
    let payouts = Payouts::new(
        store.clone(),
        MoneroConfig {
            wallet_rpc: format!("http://127.0.0.1:{port}/json_rpc"),
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
    .await
    .unwrap();
    let _ = payouts.drain_once().await.unwrap();

    let transfer_calls: usize = wallet
        .calls
        .lock()
        .iter()
        .filter(|v| v["method"] == "transfer")
        .count();
    assert_eq!(transfer_calls, 0, "no payout without a treasury snapshot");
    // State should still be `pending` (rolled back from `in_flight`).
    assert_eq!(
        store.redemption_state(99).await.unwrap().as_deref(),
        Some("pending")
    );
}

/// Wallet has more XMR than the issued token value × premium cap. The cap
/// truncates the effective balance so redeemers don't get the full surplus.
///
/// Setup: mined 1.2 XMR, issued 1.0 mining-pool token (active), 0 pending.
/// With `max_payout_premium_bp = 1000` (10% cap) the effective balance is
/// `min(1.2, 1.0 × 1.10) = 1.1 XMR`. A 1.0-token redemption should be paid
/// 1.0 × 1.1 / 1.0 = 1.1 XMR atomic, NOT the uncapped 1.2 XMR.
#[tokio::test]
async fn premium_cap_truncates_surplus_payout() {
    let Some(redis_url) = std::env::var("ANVIL_TEST_REDIS_URL").ok() else { return };
    let store = Store::connect(&redis_url).await.unwrap();
    {
        let mut c = store.conn();
        let _: () = redis::cmd("FLUSHDB").query_async(&mut c).await.unwrap();
    }
    let user: alloy::primitives::Address = "0x0000000000000000000000000000000000c0ffee".parse().unwrap();
    store.enqueue_redemption(7, user, DECIMALS as i64, "44addr").await.unwrap();

    let snap = TreasurySnapshot {
        monero_balance_atomic: 12 * 10u128.pow(11),  // 1.2 XMR
        monero_unlocked_atomic: 12 * 10u128.pow(11),
        pending_redemptions_atomic: 1 * DECIMALS,    // this redemption is pending
        pending_redemptions_count: 1,
        mining_pool_token_total_supply: 0,                  // already counted via pending
        as_of_unix: chrono::Utc::now().timestamp(),
    };
    store.set_treasury_snapshot(&snap).await.unwrap();

    let (wallet, port) = fake_wallet_rpc().await;
    let payouts = Payouts::new(
        store.clone(),
        MoneroConfig {
            wallet_rpc: format!("http://127.0.0.1:{port}/json_rpc"),
            wallet_rpc_user: None,
            wallet_rpc_pass: None,
            min_reserve_ratio: 1.0,
            per_tx_cap_atomic: u64::MAX,
            per_day_cap_atomic: u64::MAX,
            max_payout_premium_bp: 1000,
            confirm_wait_secs: 0, // 10% premium cap
            network: pool_core::config::MoneroNetwork::Testnet,
            wallet_filename: "pool".into(),
            wallet_password: "".into(),
            restore_height_lookback: 720,
        },
    )
    .await
    .unwrap();
    let _ = payouts.drain_once().await.unwrap();

    let calls = wallet.calls.lock().clone();
    let transfer_calls: Vec<&Value> = calls.iter().filter(|v| v["method"] == "transfer").collect();
    assert_eq!(transfer_calls.len(), 1);
    let amt = transfer_calls[0]["params"]["destinations"][0]["amount"]
        .as_u64()
        .unwrap();
    // Capped: 1.0 × min(1.2, 1.0×1.10) / 1.0 = 1.1 XMR = 1.1e12 atomic.
    assert_eq!(amt, 1_100_000_000_000, "payout should be cap-truncated to 1.1 XMR");
}

/// Same surplus setup but the default `max_payout_premium_bp = 0` →
/// redeemer gets exactly the token value (1.0 XMR), no surplus shared.
#[tokio::test]
async fn premium_cap_default_zero_pays_strict_one_to_one() {
    let Some(redis_url) = std::env::var("ANVIL_TEST_REDIS_URL").ok() else { return };
    let store = Store::connect(&redis_url).await.unwrap();
    {
        let mut c = store.conn();
        let _: () = redis::cmd("FLUSHDB").query_async(&mut c).await.unwrap();
    }
    let user: alloy::primitives::Address = "0x000000000000000000000000000000000000fade".parse().unwrap();
    store.enqueue_redemption(9, user, DECIMALS as i64, "44addr").await.unwrap();

    let snap = TreasurySnapshot {
        monero_balance_atomic: 12 * 10u128.pow(11),  // 1.2 XMR
        monero_unlocked_atomic: 12 * 10u128.pow(11),
        pending_redemptions_atomic: 1 * DECIMALS,
        pending_redemptions_count: 1,
        mining_pool_token_total_supply: 0,
        as_of_unix: chrono::Utc::now().timestamp(),
    };
    store.set_treasury_snapshot(&snap).await.unwrap();

    let (wallet, port) = fake_wallet_rpc().await;
    let payouts = Payouts::new(
        store.clone(),
        MoneroConfig {
            wallet_rpc: format!("http://127.0.0.1:{port}/json_rpc"),
            wallet_rpc_user: None,
            wallet_rpc_pass: None,
            min_reserve_ratio: 1.0,
            per_tx_cap_atomic: u64::MAX,
            per_day_cap_atomic: u64::MAX,
            max_payout_premium_bp: 0,
            confirm_wait_secs: 0, // explicit default
            network: pool_core::config::MoneroNetwork::Testnet,
            wallet_filename: "pool".into(),
            wallet_password: "".into(),
            restore_height_lookback: 720,
        },
    )
    .await
    .unwrap();
    let _ = payouts.drain_once().await.unwrap();

    let calls = wallet.calls.lock().clone();
    let amt = calls
        .iter()
        .find(|v| v["method"] == "transfer")
        .unwrap()["params"]["destinations"][0]["amount"]
        .as_u64()
        .unwrap();
    // Strict 1:1: 1.0 × min(1.2, 1.0×1.0) / 1.0 = 1.0 XMR
    assert_eq!(amt, 1_000_000_000_000, "default cap should be strict 1:1");
}

/// Same setup but `max_payout_premium_bp = u32::MAX` → cap is effectively
/// disabled, redeemer gets the full uncapped 1.2 XMR.
#[tokio::test]
async fn premium_cap_disabled_pays_full_surplus() {
    let Some(redis_url) = std::env::var("ANVIL_TEST_REDIS_URL").ok() else { return };
    let store = Store::connect(&redis_url).await.unwrap();
    {
        let mut c = store.conn();
        let _: () = redis::cmd("FLUSHDB").query_async(&mut c).await.unwrap();
    }
    let user: alloy::primitives::Address = "0x0000000000000000000000000000000000beefed".parse().unwrap();
    store.enqueue_redemption(8, user, DECIMALS as i64, "44addr").await.unwrap();

    let snap = TreasurySnapshot {
        monero_balance_atomic: 12 * 10u128.pow(11),  // 1.2 XMR
        monero_unlocked_atomic: 12 * 10u128.pow(11),
        pending_redemptions_atomic: 1 * DECIMALS,
        pending_redemptions_count: 1,
        mining_pool_token_total_supply: 0,
        as_of_unix: chrono::Utc::now().timestamp(),
    };
    store.set_treasury_snapshot(&snap).await.unwrap();

    let (wallet, port) = fake_wallet_rpc().await;
    let payouts = Payouts::new(
        store.clone(),
        MoneroConfig {
            wallet_rpc: format!("http://127.0.0.1:{port}/json_rpc"),
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
    .await
    .unwrap();
    let _ = payouts.drain_once().await.unwrap();

    let calls = wallet.calls.lock().clone();
    let amt = calls
        .iter()
        .find(|v| v["method"] == "transfer")
        .unwrap()["params"]["destinations"][0]["amount"]
        .as_u64()
        .unwrap();
    assert_eq!(amt, 1_200_000_000_000, "uncapped → full 1.2 XMR");
}
