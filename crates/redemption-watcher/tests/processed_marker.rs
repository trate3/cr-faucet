//! Durable on-chain processed-marker integration with the payouts loop:
//!   1. If the marker reports a redemption already processed on-chain (a
//!      prior instance paid it; our Redis state was wiped + the id-poller
//!      re-enqueued), the loop must SKIP it — no second `transfer`.
//!   2. After a successful payout the loop must call `mark_processed`
//!      with the Monero txid.
//!
//! Run serially: `cargo test --test processed_marker -- --test-threads=1`.

use axum::{extract::State, routing::post, Json, Router};
use pool_core::config::MoneroConfig;
use pool_core::store::{Store, TreasurySnapshot};
use redemption_watcher::payouts::{Payouts, RedemptionMarker};
use serde_json::{json, Value};
use std::sync::Arc;

const DECIMALS: i64 = 1_000_000_000_000;

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
                    "result": { "tx_hash": "marker-test-tx", "fee": 0, "amount": 0 }
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

/// Stub marker with configurable `is_processed` answer; records
/// `mark_processed` calls.
struct StubMarker {
    already_processed: bool,
    marked: parking_lot::Mutex<Vec<(u64, String)>>,
}

#[async_trait::async_trait]
impl RedemptionMarker for StubMarker {
    async fn is_processed(&self, _id: u64) -> anyhow::Result<bool> {
        Ok(self.already_processed)
    }
    async fn mark_processed(&self, id: u64, txid: &str, _restore_height: u64) -> anyhow::Result<()> {
        self.marked.lock().push((id, txid.to_string()));
        Ok(())
    }
    async fn restore_height(&self) -> anyhow::Result<u64> {
        Ok(0)
    }
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

async fn seed(store: &Store, id: u64) {
    {
        let mut c = store.conn();
        let _: () = redis::cmd("FLUSHDB").query_async(&mut c).await.unwrap();
    }
    let user: alloy::primitives::Address =
        "0x00000000000000000000000000000000cafef00d".parse().unwrap();
    store
        .enqueue_redemption(id, user, DECIMALS, "44addr")
        .await
        .unwrap();
    let snap = TreasurySnapshot {
        monero_balance_atomic: 5 * DECIMALS as u128,
        monero_unlocked_atomic: 5 * DECIMALS as u128,
        pending_redemptions_atomic: DECIMALS as u128,
        pending_redemptions_count: 1,
        mining_pool_token_total_supply: 0,
        as_of_unix: chrono::Utc::now().timestamp(),
    };
    store.set_treasury_snapshot(&snap).await.unwrap();
}

#[tokio::test]
async fn already_processed_on_chain_is_skipped() {
    let Some(redis_url) = std::env::var("ANVIL_TEST_REDIS_URL").ok() else { return };
    let store = Store::connect(&redis_url).await.unwrap();
    seed(&store, 5).await;

    let (wallet, port) = fake_wallet_rpc().await;
    let marker = Arc::new(StubMarker {
        already_processed: true,
        marked: parking_lot::Mutex::new(vec![]),
    });
    let payouts = Payouts::with_marker(store.clone(), cfg_for(port), Some(marker.clone()))
        .await
        .unwrap();

    let _ = payouts.drain_once().await.unwrap();

    // No transfer should have happened — it was already paid on-chain.
    let transfers = wallet
        .calls
        .lock()
        .iter()
        .filter(|v| v["method"] == "transfer")
        .count();
    assert_eq!(transfers, 0, "already-processed redemption must not be re-paid");
    assert_eq!(
        store.redemption_state(5).await.unwrap().as_deref(),
        Some("sent")
    );
}

#[tokio::test]
async fn successful_payout_marks_processed_on_chain() {
    let Some(redis_url) = std::env::var("ANVIL_TEST_REDIS_URL").ok() else { return };
    let store = Store::connect(&redis_url).await.unwrap();
    seed(&store, 8).await;

    let (wallet, port) = fake_wallet_rpc().await;
    let marker = Arc::new(StubMarker {
        already_processed: false,
        marked: parking_lot::Mutex::new(vec![]),
    });
    let payouts = Payouts::with_marker(store.clone(), cfg_for(port), Some(marker.clone()))
        .await
        .unwrap();

    let _ = payouts.drain_once().await.unwrap();

    // The transfer happened...
    let transfers = wallet
        .calls
        .lock()
        .iter()
        .filter(|v| v["method"] == "transfer")
        .count();
    assert_eq!(transfers, 1, "redemption should be paid once");
    // ...and was recorded on-chain with the txid.
    let marked = marker.marked.lock().clone();
    assert_eq!(marked.len(), 1, "exactly one markProcessed call");
    assert_eq!(marked[0], (8, "marker-test-tx".to_string()));
}
