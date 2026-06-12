//! Hot-path throughput + accrual correctness against Redis. Requires
//! `ACCOUNTANT_TEST_REDIS_URL` (skips when unset, so plain `cargo test` is green).
//!
//! Run serially — every test `FLUSHDB`s the shared DB on entry, so the
//! count-asserting accrual tests would race a sibling's flush under parallelism:
//!   `cargo test -p accountant --test throughput -- --test-threads=1`

use alloy::primitives::Address;
use pool_core::cache::RateCache;
use pool_core::metrics::Metrics;
use pool_core::store::Store;
use pool_core::{EvmAddress, ShareAccepted};
use std::sync::Arc;
use std::time::Instant;

fn redis_url() -> Option<String> {
    std::env::var("ACCOUNTANT_TEST_REDIS_URL").ok()
}

async fn fresh_store() -> Option<Store> {
    let url = redis_url()?;
    let store = Store::connect(&url).await.expect("redis connect");
    let mut c = store.conn();
    let _: () = redis::cmd("FLUSHDB").query_async(&mut c).await.unwrap();
    Some(store)
}

fn make_share(addr_byte: u8, diff: u64) -> ShareAccepted {
    let mut a = [0u8; 20];
    a[19] = addr_byte;
    ShareAccepted {
        miner: EvmAddress(Address::from(a)),
        job_id: "j".into(),
        difficulty: diff,
        accepted_at: chrono::Utc::now(),
        forwarded_upstream: false,
    }
}

fn report(label: &str, n: usize, dt: std::time::Duration) {
    let ops = n as f64 / dt.as_secs_f64();
    let us = dt.as_micros() as f64 / n as f64;
    eprintln!(
        "{label}: {n} ops in {:.2?}  ({ops:>8.0} ops/sec, ~{us:.1} µs/op)",
        dt
    );
}

#[tokio::test]
async fn redis_serial() {
    let Some(store) = fresh_store().await else { return };
    let rate = Arc::new(RateCache::new());
    rate.set(1.0, 0);
    let metrics = Arc::new(Metrics::new());
    for i in 0..16 {
        let s = make_share((i % 250) as u8, 1_000);
        accountant::credit(&store, &rate, &metrics, &s).await.unwrap();
    }
    let n = 1_000;
    let start = Instant::now();
    for i in 0..n {
        let s = make_share((i % 250) as u8, 1_000);
        accountant::credit(&store, &rate, &metrics, &s).await.unwrap();
    }
    report("redis serial", n, start.elapsed());
}

#[tokio::test]
async fn redis_concurrent_8() {
    let Some(store) = fresh_store().await else { return };
    let rate = Arc::new(RateCache::new());
    rate.set(1.0, 0);
    let metrics = Arc::new(Metrics::new());

    let workers = 8usize;
    let per_worker = 500usize;
    let n = workers * per_worker;
    let start = Instant::now();
    let mut handles = Vec::new();
    for w in 0..workers {
        let store = store.clone();
        let rate = rate.clone();
        let metrics = metrics.clone();
        handles.push(tokio::spawn(async move {
            for i in 0..per_worker {
                let s = make_share(((w * per_worker + i) % 250) as u8, 1_000);
                accountant::credit(&store, &rate, &metrics, &s).await.unwrap();
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    report("redis concurrent(8)", n, start.elapsed());
}

#[tokio::test]
async fn redis_pipelined() {
    let Some(store) = fresh_store().await else { return };
    let rate = Arc::new(RateCache::new());
    rate.set(1.0, 0);
    let metrics = Arc::new(Metrics::new());
    for i in 0..16 {
        let s = make_share((i % 250) as u8, 1_000);
        accountant::credit(&store, &rate, &metrics, &s).await.unwrap();
    }
    let n = 1_000;
    let start = Instant::now();
    let mut futs = Vec::with_capacity(n);
    for i in 0..n {
        let s = make_share((i % 250) as u8, 1_000);
        let store = store.clone();
        let rate = rate.clone();
        let metrics = metrics.clone();
        futs.push(async move {
            accountant::credit(&store, &rate, &metrics, &s).await.unwrap()
        });
    }
    let _ = futures::future::join_all(futs).await;
    report("redis pipelined", n, start.elapsed());
}

#[tokio::test]
async fn credit_accrues_pool_fee() {
    let Some(store) = fresh_store().await else { return };
    let rate = Arc::new(RateCache::new());
    rate.set(1.0, 0);
    rate.set_fee_rate(0.1); // pool keeps 0.1 atomic per unit difficulty
    let metrics = Arc::new(Metrics::new());

    assert_eq!(store.fee_accrued().await.unwrap(), 0, "starts empty");

    // difficulty 1000 → miner credited 1000 (net), pool fee accrues 100.
    let s = make_share(1, 1_000);
    let credited = accountant::credit(&store, &rate, &metrics, &s).await.unwrap();
    assert_eq!(credited, 1_000, "miner credited net");
    assert_eq!(store.earned(s.miner.0).await.unwrap(), 1_000);
    assert_eq!(store.fee_accrued().await.unwrap(), 100, "fee accrued");

    // a second share accrues monotonically and independently of the miner.
    let s2 = make_share(2, 2_000);
    accountant::credit(&store, &rate, &metrics, &s2).await.unwrap();
    assert_eq!(store.fee_accrued().await.unwrap(), 300, "100 + 200");
    assert_eq!(store.earned(s2.miner.0).await.unwrap(), 2_000);
}

#[tokio::test]
async fn no_fee_rate_accrues_nothing() {
    // With fee_rate left at 0 (e.g. pool_fee == 0), miners are still credited but
    // the pool accrues nothing to swap.
    let Some(store) = fresh_store().await else { return };
    let rate = Arc::new(RateCache::new());
    rate.set(1.0, 0); // fee_rate defaults to 0.0
    let metrics = Arc::new(Metrics::new());

    let s = make_share(3, 5_000);
    let credited = accountant::credit(&store, &rate, &metrics, &s).await.unwrap();
    assert_eq!(credited, 5_000);
    assert_eq!(store.fee_accrued().await.unwrap(), 0);
}

#[tokio::test]
async fn add_fee_accrued_is_monotonic_counter() {
    let Some(store) = fresh_store().await else { return };
    assert_eq!(store.add_fee_accrued(40).await.unwrap(), 40);
    assert_eq!(store.add_fee_accrued(60).await.unwrap(), 100);
    assert_eq!(store.fee_accrued().await.unwrap(), 100);
}
