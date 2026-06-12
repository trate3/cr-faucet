//! Benchmark: light-mode (~256 MB cache) vs full-mode (~2 GB dataset) RandomX
//! hashing throughput from a single thread. Skipped without the `real`
//! feature.
//!
//! Run with: `cargo test -p randomx-verify --release --features real
//! --test light_vs_full -- --nocapture --include-ignored`

#![cfg(feature = "real")]

use rand::{rngs::SmallRng, RngCore, SeedableRng};
use randomx_verify::{RandomXVerifier, Verifier};
use std::time::Instant;

fn bench(label: &str, v: &RandomXVerifier, n: usize) {
    let seed = [0xaau8; 32];
    let mut rng = SmallRng::seed_from_u64(0);
    // Warm: build the cache & VM by running one hash.
    let mut blob = vec![0u8; 76];
    rng.fill_bytes(&mut blob);
    let _ = v.hash(&seed, &blob).unwrap();

    let mut blob = vec![0u8; 76];
    let start = Instant::now();
    for _ in 0..n {
        rng.fill_bytes(&mut blob[39..43]); // vary the nonce
        let _ = v.hash(&seed, &blob).unwrap();
    }
    let dt = start.elapsed();
    eprintln!(
        "{label:>10}: {n} hashes in {dt:.2?}  ({:.0} H/s, {:.2} ms/hash)",
        n as f64 / dt.as_secs_f64(),
        dt.as_secs_f64() * 1000.0 / n as f64
    );
}

#[test]
#[ignore = "heavy; run with --include-ignored when comparing RandomX modes"]
fn light_mode_throughput() {
    let v = RandomXVerifier::new_light();
    bench("light", &v, 50);
}

#[test]
#[ignore = "heavy; allocates ~2 GB of dataset memory and takes ~minute to init"]
fn full_mode_throughput() {
    let v = RandomXVerifier::new_full();
    bench("full", &v, 200);
}
