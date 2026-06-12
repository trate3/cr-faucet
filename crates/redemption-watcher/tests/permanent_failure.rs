//! A redemption whose destination address is invalid / for the wrong
//! Monero network makes wallet-rpc `transfer` fail permanently. The
//! payouts loop must quarantine it (state `failed`, XACK'd off the
//! stream) rather than retry forever — otherwise it blocks the queue and
//! spams the daemon.
//!
//! Run serially: `cargo test --test permanent_failure -- --test-threads=1`.

use axum::{extract::State, routing::post, Json, Router};
use pool_core::config::MoneroConfig;
use pool_core::store::{Store, TreasurySnapshot};
use redemption_watcher::payouts::Payouts;
use serde_json::{json, Value};
use std::sync::Arc;

const DECIMALS: i64 = 1_000_000_000_000;

#[derive(Clone, Default)]
struct WalletState {
    calls: Arc<parking_lot::Mutex<Vec<Value>>>,
}

/// Fake wallet-rpc that rejects every `transfer` with the wrong-address
/// error code (-2), mimicking a mainnet/garbage destination.
async fn fake_wallet_rpc_wrong_address() -> (WalletState, u16) {
    let state = WalletState::default();
    let s2 = state.clone();
    let app = Router::new()
        .route(
            "/json_rpc",
            post(|State(s): State<WalletState>, Json(body): Json<Value>| async move {
                s.calls.lock().push(body.clone());
                let method = body["method"].as_str().unwrap_or("");
                if method == "transfer" {
                    Json(json!({
                        "jsonrpc": "2.0",
                        "id": body["id"].clone(),
                        "error": { "code": -2, "message": "WALLET_RPC_ERROR_CODE_WRONG_ADDRESS" }
                    }))
                } else {
                    Json(json!({
                        "jsonrpc": "2.0",
                        "id": body["id"].clone(),
                        "result": {}
                    }))
                }
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

fn cfg_for(port: u16) -> MoneroConfig {
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
    }
}

#[tokio::test]
async fn wrong_address_redemption_is_quarantined_not_retried() {
    let Some(redis_url) = std::env::var("ANVIL_TEST_REDIS_URL").ok() else { return };
    let store = Store::connect(&redis_url).await.unwrap();
    {
        let mut c = store.conn();
        let _: () = redis::cmd("FLUSHDB").query_async(&mut c).await.unwrap();
    }

    let user: alloy::primitives::Address =
        "0x00000000000000000000000000000000deadbeef".parse().unwrap();
    // A destination string the fake wallet will reject. (The real
    // rejection is the wallet's job; here it always says wrong-address.)
    store
        .enqueue_redemption(7, user, DECIMALS, "44mainnet_addr_on_stagenet_wallet")
        .await
        .unwrap();

    // Funded snapshot so we get past the unlocked-balance check and
    // actually attempt the transfer.
    let snap = TreasurySnapshot {
        monero_balance_atomic: 5 * DECIMALS as u128,
        monero_unlocked_atomic: 5 * DECIMALS as u128,
        pending_redemptions_atomic: DECIMALS as u128,
        pending_redemptions_count: 1,
        mining_pool_token_total_supply: 0,
        as_of_unix: chrono::Utc::now().timestamp(),
    };
    store.set_treasury_snapshot(&snap).await.unwrap();

    let (wallet, port) = fake_wallet_rpc_wrong_address().await;
    let payouts = Payouts::new(store.clone(), cfg_for(port)).await.unwrap();

    // First drain: attempts the transfer, gets wrong-address, quarantines.
    let n = payouts.drain_once().await.unwrap();
    assert!(n >= 1, "the entry should have been processed (quarantined)");

    // State must be `failed`.
    assert_eq!(
        store.redemption_state(7).await.unwrap().as_deref(),
        Some("failed"),
        "permanent failure should mark the redemption `failed`"
    );

    let transfer_calls_after_first = wallet
        .calls
        .lock()
        .iter()
        .filter(|v| v["method"] == "transfer")
        .count();
    assert_eq!(
        transfer_calls_after_first, 1,
        "exactly one transfer attempt before quarantine"
    );

    // Second drain: the entry was XACK'd, so it must NOT be re-attempted.
    // (XAUTOCLAIM only reclaims entries idle > 60s, and this one is gone
    // from the PEL entirely.)
    let _ = payouts.drain_once().await.unwrap();
    let transfer_calls_after_second = wallet
        .calls
        .lock()
        .iter()
        .filter(|v| v["method"] == "transfer")
        .count();
    assert_eq!(
        transfer_calls_after_second, 1,
        "quarantined redemption must not be retried"
    );
}
