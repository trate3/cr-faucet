//! Verifies the crash-between-transfer-and-xack guard: if a redemption is
//! already in `in_flight` state (because a prior consumer crashed after
//! claiming the slot but before XACKing), the next consumer must NOT call
//! `transfer` a second time.
//!
//! Run serially: `cargo test --test in_flight_guard -- --test-threads=1`.
//! The tests share one Redis DB and `FLUSHDB` between them.

use axum::{extract::State, routing::post, Json, Router};
use pool_core::config::MoneroConfig;
use pool_core::store::{Store, HASH_REDEMPTION_STATE, STREAM_REDEMPTIONS};
use redemption_watcher::payouts::Payouts;
use serde_json::{json, Value};
use std::sync::Arc;

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
                    "result": { "tx_hash": "should-not-have-been-called", "fee": 0, "amount": 0 }
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
async fn in_flight_redemption_is_not_retried() {
    let Some(redis_url) = std::env::var("ANVIL_TEST_REDIS_URL").ok() else {
        eprintln!("skip: set ANVIL_TEST_REDIS_URL");
        return;
    };

    let (wallet, port) = fake_wallet_rpc().await;
    let store = Store::connect(&redis_url).await.unwrap();
    {
        let mut c = store.conn();
        let _: () = redis::cmd("FLUSHDB").query_async(&mut c).await.unwrap();
    }

    // Enqueue a redemption (so it shows up in the stream).
    let user: alloy::primitives::Address = "0x000000000000000000000000000000000000beef".parse().unwrap();
    store
        .enqueue_redemption(1, user, 1_000_000, "44addr...")
        .await
        .unwrap();

    // Simulate: prior consumer claimed the slot, then crashed before XACK.
    let claimed = store.try_mark_redemption_in_flight(1).await.unwrap();
    assert!(claimed);
    assert_eq!(
        store.redemption_state(1).await.unwrap().as_deref(),
        Some("in_flight")
    );

    // Run a fresh consumer; it should refuse to retry the transfer.
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
    let n = payouts.drain_once().await.unwrap();
    assert!(n >= 1, "consumer should have observed the stream entry");

    // No transfer call should have happened.
    let calls = wallet.calls.lock().clone();
    let transfer_calls: usize = calls
        .iter()
        .filter(|v| v["method"] == "transfer")
        .count();
    assert_eq!(
        transfer_calls, 0,
        "transfer must NOT have been called for an in_flight redemption: {calls:?}"
    );

    // State should still be `in_flight` (not "sent").
    assert_eq!(
        store.redemption_state(1).await.unwrap().as_deref(),
        Some("in_flight")
    );

    // The stream entry should be XACKed (so a new pass doesn't redeliver it).
    let mut c = store.conn();
    let pending: redis::Value = redis::cmd("XPENDING")
        .arg(STREAM_REDEMPTIONS)
        .arg(redemption_watcher::payouts::GROUP)
        .query_async(&mut c)
        .await
        .unwrap();
    // XPENDING returns [count, first_id, last_id, [[consumer, n], ...]].
    if let redis::Value::Array(arr) = pending {
        if let Some(redis::Value::Int(count)) = arr.first() {
            assert_eq!(*count, 0, "no pending entries after consumer XACK");
        }
    }
    // And HASH_REDEMPTION_STATE entry remains for the operator to inspect.
    let _ = HASH_REDEMPTION_STATE;
}

#[tokio::test]
async fn pending_then_first_consumer_succeeds() {
    // Positive control: a `pending` redemption with a working wallet-rpc
    // gets transferred exactly once and ends in `sent`.
    let Some(redis_url) = std::env::var("ANVIL_TEST_REDIS_URL").ok() else { return };
    let (wallet, port) = fake_wallet_rpc().await;
    let store = Store::connect(&redis_url).await.unwrap();
    {
        let mut c = store.conn();
        let _: () = redis::cmd("FLUSHDB").query_async(&mut c).await.unwrap();
    }
    let user: alloy::primitives::Address = "0x000000000000000000000000000000000000feed".parse().unwrap();
    store
        .enqueue_redemption(2, user, 500_000, "44addr...")
        .await
        .unwrap();
    // Pro-rata payout requires a treasury snapshot. Pick numbers so rate=1.
    let snapshot = pool_core::store::TreasurySnapshot {
        monero_balance_atomic: 1_000_000,
        monero_unlocked_atomic: 1_000_000,
        pending_redemptions_atomic: 500_000,
        pending_redemptions_count: 1,
        mining_pool_token_total_supply: 500_000,
        as_of_unix: chrono::Utc::now().timestamp(),
    };
    store.set_treasury_snapshot(&snapshot).await.unwrap();

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
    let n = payouts.drain_once().await.unwrap();
    assert_eq!(n, 1);

    let calls = wallet.calls.lock().clone();
    let transfer_calls: usize = calls.iter().filter(|v| v["method"] == "transfer").count();
    assert_eq!(transfer_calls, 1, "exactly one transfer call");
    assert_eq!(
        store.redemption_state(2).await.unwrap().as_deref(),
        Some("sent")
    );
}
