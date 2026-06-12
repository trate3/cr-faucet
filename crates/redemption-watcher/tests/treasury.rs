//! Treasury refresher integration test.
//!
//! Verifies that enqueue/mark_sent update the pending counters correctly, and
//! that `TreasuryRefresher::tick` writes a snapshot that the operator-api
//! could serve.
//!
//! Run serially: `cargo test --test treasury -- --test-threads=1`.

use axum::{extract::State, routing::post, Json, Router};
use pool_core::config::MoneroConfig;
use pool_core::store::Store;
use redemption_watcher::treasury::{StubSupplyReader, TreasuryRefresher};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
struct WalletState {
    balance: Arc<parking_lot::Mutex<(u128, u128)>>,
}

async fn fake_wallet_rpc(initial: (u128, u128)) -> (WalletState, u16) {
    let state = WalletState {
        balance: Arc::new(parking_lot::Mutex::new(initial)),
    };
    let s2 = state.clone();
    let app = Router::new()
        .route(
            "/json_rpc",
            post(|State(s): State<WalletState>, Json(body): Json<Value>| async move {
                let method = body["method"].as_str().unwrap_or_default();
                let result = match method {
                    "get_balance" => {
                        let (bal, unl) = *s.balance.lock();
                        json!({"balance": bal, "unlocked_balance": unl})
                    }
                    "refresh" => json!({"received_money": false, "blocks_fetched": 0}),
                    _ => json!({}),
                };
                Json(json!({"jsonrpc":"2.0","id":body["id"].clone(),"result":result}))
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
async fn pending_counters_track_enqueue_and_sent() {
    let Some(redis_url) = std::env::var("ANVIL_TEST_REDIS_URL").ok() else { return };
    let store = Store::connect(&redis_url).await.unwrap();
    {
        let mut c = store.conn();
        let _: () = redis::cmd("FLUSHDB").query_async(&mut c).await.unwrap();
    }

    let user: alloy::primitives::Address = "0x000000000000000000000000000000000000aa01".parse().unwrap();
    store.enqueue_redemption(1, user, 100_000, "44a").await.unwrap();
    store.enqueue_redemption(2, user, 250_000, "44b").await.unwrap();
    store.enqueue_redemption(3, user, 50_000, "44c").await.unwrap();
    assert_eq!(store.pending_atomic().await.unwrap(), 400_000);
    assert_eq!(store.pending_count().await.unwrap(), 3);

    // Duplicate enqueue must be a no-op for counters too.
    store.enqueue_redemption(1, user, 100_000, "44a").await.unwrap();
    assert_eq!(store.pending_atomic().await.unwrap(), 400_000);
    assert_eq!(store.pending_count().await.unwrap(), 3);

    // Marking sent debits the pending counters by exactly the recorded amount.
    store.mark_redemption_sent(2, "txid-2").await.unwrap();
    assert_eq!(store.pending_atomic().await.unwrap(), 150_000);
    assert_eq!(store.pending_count().await.unwrap(), 2);

    // Idempotent: calling mark_sent again must NOT double-debit.
    store.mark_redemption_sent(2, "txid-2").await.unwrap();
    assert_eq!(store.pending_atomic().await.unwrap(), 150_000);
    assert_eq!(store.pending_count().await.unwrap(), 2);
}

#[tokio::test]
async fn treasury_refresher_writes_snapshot() {
    let Some(redis_url) = std::env::var("ANVIL_TEST_REDIS_URL").ok() else { return };
    let store = Store::connect(&redis_url).await.unwrap();
    {
        let mut c = store.conn();
        let _: () = redis::cmd("FLUSHDB").query_async(&mut c).await.unwrap();
    }

    let user: alloy::primitives::Address = "0x000000000000000000000000000000000000bb02".parse().unwrap();
    store.enqueue_redemption(10, user, 333, "44z").await.unwrap();

    let (wallet, port) = fake_wallet_rpc((9_000_000, 7_500_000)).await;
    let _ = wallet;
    let refresher = TreasuryRefresher::new(
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
        StubSupplyReader::new(4 * 1_000_000_000_000),
        Duration::from_secs(60),
    );
    let snap = refresher.tick().await.unwrap();

    assert_eq!(snap.monero_balance_atomic, 9_000_000);
    assert_eq!(snap.monero_unlocked_atomic, 7_500_000);
    assert_eq!(snap.pending_redemptions_atomic, 333);
    assert_eq!(snap.pending_redemptions_count, 1);
    assert_eq!(snap.mining_pool_token_total_supply, 4 * 1_000_000_000_000);
    assert!(snap.as_of_unix > 0);

    // Snapshot was persisted.
    let loaded = store.treasury_snapshot().await.unwrap().expect("snapshot");
    assert_eq!(loaded.monero_unlocked_atomic, 7_500_000);
    assert_eq!(loaded.mining_pool_token_total_supply, 4 * 1_000_000_000_000);
}
