//! Graceful-shutdown contract for the payouts loop:
//!   1. Once `shutdown` is cancelled, `run_loop_with_shutdown` returns
//!      promptly — it does not block on the next XREADGROUP forever.
//!   2. If the loop is mid-iteration when the cancel arrives, it still
//!      finishes the current entry's full state transition (transfer
//!      success → `sent` + txid persisted). We never leave a redemption
//!      stuck in `in_flight` because of shutdown.

use axum::{extract::State, routing::post, Json, Router};
use pool_core::config::MoneroConfig;
use pool_core::store::Store;
use redemption_watcher::payouts::Payouts;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
struct WalletState {
    calls: Arc<parking_lot::Mutex<Vec<Value>>>,
    /// When true, the wallet sleeps `slow_for` before responding — to
    /// exercise the "shutdown arrives mid-transfer" case.
    slow_for: Arc<parking_lot::Mutex<Duration>>,
}

async fn fake_wallet_rpc() -> (WalletState, u16) {
    let state = WalletState::default();
    let s2 = state.clone();
    let app = Router::new()
        .route(
            "/json_rpc",
            post(|State(s): State<WalletState>, Json(body): Json<Value>| async move {
                let dur = *s.slow_for.lock();
                if dur > Duration::ZERO {
                    tokio::time::sleep(dur).await;
                }
                s.calls.lock().push(body.clone());
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": body["id"].clone(),
                    "result": { "tx_hash": "graceful-tx", "fee": 0, "amount": 0 }
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

/// No traffic on the stream → shutdown fires → loop must return promptly
/// (not hang on the 5-second XREADGROUP BLOCK).
#[tokio::test]
async fn idle_loop_exits_promptly_on_shutdown() {
    let Some(redis_url) = std::env::var("ANVIL_TEST_REDIS_URL").ok() else { return };
    let store = Store::connect(&redis_url).await.unwrap();
    {
        let mut c = store.conn();
        let _: () = redis::cmd("FLUSHDB").query_async(&mut c).await.unwrap();
    }

    let (_wallet, port) = fake_wallet_rpc().await;
    let payouts = Payouts::new(store, cfg_for(port)).await.unwrap();
    let shutdown = CancellationToken::new();

    let task = {
        let shutdown = shutdown.clone();
        tokio::spawn(async move { payouts.run_loop_with_shutdown(shutdown).await })
    };

    // Let it block on XREADGROUP, then cancel.
    tokio::time::sleep(Duration::from_millis(50)).await;
    shutdown.cancel();

    // Should exit within ~one BLOCK window (5s) + a little slack.
    let r = tokio::time::timeout(Duration::from_secs(7), task).await;
    assert!(r.is_ok(), "loop did not exit within 7s of cancel");
}

/// Cancel fires while a `transfer` is in flight. The loop must NOT abort
/// mid-transfer — the entry's final state must be `sent` with a txid, not
/// `in_flight`.
#[tokio::test]
async fn mid_transfer_shutdown_finishes_current_entry() {
    let Some(redis_url) = std::env::var("ANVIL_TEST_REDIS_URL").ok() else { return };
    let store = Store::connect(&redis_url).await.unwrap();
    {
        let mut c = store.conn();
        let _: () = redis::cmd("FLUSHDB").query_async(&mut c).await.unwrap();
    }

    // Set up a snapshot + one queued redemption so drain_once has work.
    use pool_core::store::TreasurySnapshot;
    let user: alloy::primitives::Address = "0x000000000000000000000000000000000beefeed".parse().unwrap();
    store.enqueue_redemption(42, user, 1_000_000_000_000, "44addr").await.unwrap();
    let snap = TreasurySnapshot {
        monero_balance_atomic: 5_000_000_000_000,
        monero_unlocked_atomic: 5_000_000_000_000,
        pending_redemptions_atomic: 1_000_000_000_000,
        pending_redemptions_count: 1,
        mining_pool_token_total_supply: 0,
        as_of_unix: chrono::Utc::now().timestamp(),
    };
    store.set_treasury_snapshot(&snap).await.unwrap();

    // Wallet RPC artificially slow so we can cancel before it responds.
    let (wallet, port) = fake_wallet_rpc().await;
    *wallet.slow_for.lock() = Duration::from_secs(2);

    let payouts = Payouts::new(store.clone(), cfg_for(port)).await.unwrap();
    let shutdown = CancellationToken::new();
    let task = {
        let shutdown = shutdown.clone();
        tokio::spawn(async move { payouts.run_loop_with_shutdown(shutdown).await })
    };

    // Wait until the wallet starts processing the request (transfer in
    // flight), then cancel.
    let cancel_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while wallet.calls.lock().is_empty() && std::time::Instant::now() < cancel_deadline {
        // The wallet's slow_for delay sleeps BEFORE it pushes the call. So
        // "calls is empty" while waiting actually means "in flight." Sleep
        // briefly to give the request time to land on the server.
        tokio::time::sleep(Duration::from_millis(100)).await;
        // After ~500ms the request has reached the server and is sleeping.
        if std::time::Instant::now()
            > cancel_deadline - Duration::from_secs(4)
        {
            break;
        }
    }
    shutdown.cancel();

    // Task should finish within the slow-wallet window + a little slack.
    let r = tokio::time::timeout(Duration::from_secs(15), task).await;
    assert!(r.is_ok(), "loop did not exit within 15s after cancel");

    // Crucial: the redemption is in `sent` state with a txid, NOT
    // `in_flight`. The shutdown did not abort mid-transfer.
    let state = store.redemption_state(42).await.unwrap();
    assert_eq!(
        state.as_deref(),
        Some("sent"),
        "redemption ended in {state:?}; should be `sent` (shutdown must not abort mid-transfer)"
    );
    let mut c = store.conn();
    let txid: Option<String> = redis::AsyncCommands::hget(
        &mut c,
        pool_core::store::HASH_REDEMPTION_TXID,
        "42",
    )
    .await
    .unwrap();
    assert_eq!(txid.as_deref(), Some("graceful-tx"));
}
