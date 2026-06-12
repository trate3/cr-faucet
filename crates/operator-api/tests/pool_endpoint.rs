//! `/pool` exposes upstream connection state so any miner can tell
//! whether their shares are actually being submitted upstream.

use operator_api::{router, AppState};
use pool_core::cache::RateCache;
use pool_core::metrics::Metrics;
use pool_core::store::Store;
use serde_json::Value;
use std::sync::Arc;

async fn spawn(state: AppState) -> String {
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn pool_endpoint_reflects_upstream_state() {
    // Needs Redis only because AppState holds a Store; we don't actually
    // touch it via /pool. Skip if not available.
    let Some(redis_url) = std::env::var("ANVIL_TEST_REDIS_URL").ok() else {
        return;
    };
    let store = Store::connect(&redis_url).await.unwrap();
    let metrics = Arc::new(Metrics::new());
    let rate = Arc::new(RateCache::new());
    let base = spawn(AppState {
        store,
        metrics: metrics.clone(),
        rate,
        upstream_stats: Arc::new(parking_lot::RwLock::new(None)),
        upstream_stats_as_of_unix: Arc::new(std::sync::atomic::AtomicI64::new(0)),
        onion: None,
    })
    .await;

    // Disconnected by default.
    let v: Value = reqwest::get(format!("{base}/pool"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["upstream"]["connected"], false);
    assert_eq!(v["upstream"]["consecutive_failures"], 0);
    assert_eq!(v["upstream"]["submit_rejects_total"], 0);
    assert_eq!(v["upstream"]["submit_accepts_total"], 0);

    // Simulate the upstream client marking itself connected, logging a
    // reject, and logging two accepts.
    metrics.mark_upstream_connected();
    metrics.record_upstream_submit_reject();
    metrics.record_upstream_submit_accept();
    metrics.record_upstream_submit_accept();

    let v: Value = reqwest::get(format!("{base}/pool"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["upstream"]["connected"], true);
    assert!(
        v["upstream"]["last_change_unix"].as_i64().unwrap() > 0,
        "last_change_unix should be populated after mark_upstream_connected"
    );
    assert_eq!(v["upstream"]["submit_rejects_total"], 1);
    assert_eq!(v["upstream"]["submit_accepts_total"], 2);

    // Now simulate disconnect + a couple of consecutive failures.
    metrics.mark_upstream_disconnected();
    let _ = metrics.record_upstream_failure();
    let _ = metrics.record_upstream_failure();

    let v: Value = reqwest::get(format!("{base}/pool"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["upstream"]["connected"], false);
    assert_eq!(v["upstream"]["consecutive_failures"], 2);
}
