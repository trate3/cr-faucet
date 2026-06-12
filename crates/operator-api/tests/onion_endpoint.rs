//! `/onion` publishes the pool's Tor v3 onion address (the only place it is
//! surfaced) plus the stratum/API endpoints reachable over it.

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

fn state(store: Store, onion: Option<String>) -> AppState {
    AppState {
        store,
        metrics: Arc::new(Metrics::new()),
        rate: Arc::new(RateCache::new()),
        upstream_stats: Arc::new(parking_lot::RwLock::new(None)),
        upstream_stats_as_of_unix: Arc::new(std::sync::atomic::AtomicI64::new(0)),
        onion,
    }
}

#[tokio::test]
async fn onion_endpoint_advertises_address_and_derived_urls() {
    // AppState holds a Store; /onion doesn't touch it. Skip without Redis.
    let Some(redis_url) = std::env::var("ANVIL_TEST_REDIS_URL").ok() else {
        return;
    };
    let store = Store::connect(&redis_url).await.unwrap();
    let onion = "abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion";

    // When configured: the address plus ready-to-use stratum + API URLs,
    // matching the HiddenServicePort map in deploy/torrc (3333 → stratum,
    // 80 → HTTP API).
    let base = spawn(state(store.clone(), Some(onion.to_string()))).await;
    let v: Value = reqwest::get(format!("{base}/onion"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["onion"], onion);
    assert_eq!(v["stratum"], format!("{onion}:3333"));
    assert_eq!(v["api"], format!("http://{onion}"));

    // When no hidden service is configured: all three fields are null, so a
    // client can tell the pool isn't reachable over Tor.
    let base = spawn(state(store, None)).await;
    let v: Value = reqwest::get(format!("{base}/onion"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(v["onion"].is_null());
    assert!(v["stratum"].is_null());
    assert!(v["api"].is_null());
}
