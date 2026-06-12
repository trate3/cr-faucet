//! Thin wrapper over RandomX with seed-cache management.
//!
//! Two implementations live behind the `real` feature flag:
//! - default: a stub that accepts any non-empty hash (lets the rest of the
//!   workspace compile and unit-test without the RandomX C++ dependency).
//! - `real`: backed by the `randomx-rs` crate.

use parking_lot::Mutex;
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("hash below target difficulty")]
    BelowDifficulty,
    #[error("randomx error: {0}")]
    RandomX(String),
}

pub type SeedHash = [u8; 32];
pub type ShareBlob = Vec<u8>;
pub type ResultHash = [u8; 32];

/// Computes a RandomX hash given a seed and a share blob, returning the
/// 32-byte result hash so a difficulty check can be applied by the caller.
pub trait Verifier: Send + Sync {
    fn hash(&self, seed: &SeedHash, blob: &ShareBlob) -> Result<ResultHash, VerifyError>;
}

/// Check that a hash satisfies `difficulty`. RandomX (and Monero) interpret
/// `target = floor(2^256 / difficulty)`; the hash must be <= target when read
/// as a little-endian 256-bit integer.
pub fn meets_difficulty(hash: &ResultHash, difficulty: u64) -> bool {
    if difficulty == 0 {
        return true;
    }
    // Use only the leading 8 bytes (LE) for a fast check; this matches the
    // common Monero stratum convention used by xmrig.
    let leading = u64::from_le_bytes(hash[24..32].try_into().expect("8 bytes"));
    leading == 0 || (u64::MAX / difficulty) >= leading
}

/// Stub verifier — accepts any hash. Used in tests and in CI builds where the
/// RandomX C++ deps aren't available.
#[derive(Default)]
pub struct StubVerifier;

impl Verifier for StubVerifier {
    fn hash(&self, _seed: &SeedHash, blob: &ShareBlob) -> Result<ResultHash, VerifyError> {
        // Deterministic non-zero hash derived from the blob so tests can be
        // written against expected outputs.
        let mut out = [0u8; 32];
        for (i, b) in blob.iter().enumerate() {
            out[i % 32] ^= *b;
        }
        Ok(out)
    }
}

/// Cached verifier that re-keys its underlying VM on seed change. Wrap any
/// `Verifier` to amortize cost across many shares with the same seed.
pub struct CachedVerifier<V: Verifier> {
    inner: V,
    last_seed: Mutex<Option<SeedHash>>,
}

impl<V: Verifier> CachedVerifier<V> {
    pub fn new(inner: V) -> Arc<Self> {
        Arc::new(Self {
            inner,
            last_seed: Mutex::new(None),
        })
    }

    pub fn verify_share(
        &self,
        seed: &SeedHash,
        blob: &ShareBlob,
        difficulty: u64,
    ) -> Result<ResultHash, VerifyError> {
        let mut last = self.last_seed.lock();
        if last.as_ref() != Some(seed) {
            *last = Some(*seed);
        }
        drop(last);
        let h = self.inner.hash(seed, blob)?;
        if !meets_difficulty(&h, difficulty) {
            return Err(VerifyError::BelowDifficulty);
        }
        Ok(h)
    }
}

#[cfg(feature = "real")]
mod real_impl {
    use super::*;
    use randomx_rs::{RandomXCache, RandomXDataset, RandomXFlag, RandomXVM};

    /// `randomx-rs` VMs/caches/datasets hold raw FFI pointers and are not
    /// `Send`. Access is always serialized through the Mutex, so we mark them
    /// `Send + Sync` at the wrapper level. Soundness rests on no other thread
    /// observing the VM while one thread is using it — exactly the Mutex's
    /// contract.
    struct VmCell {
        seed: SeedHash,
        _cache: RandomXCache,
        _dataset: Option<RandomXDataset>,
        vm: RandomXVM,
    }
    unsafe impl Send for VmCell {}

    pub struct RandomXVerifier {
        flags: RandomXFlag,
        use_dataset: bool,
        inner: Mutex<Option<VmCell>>,
    }

    impl RandomXVerifier {
        /// Cache-only verifier. ~256 MB RAM. Single-thread ~30-150 H/s on
        /// commodity hardware; fine for a small pool's verification budget.
        pub fn new_light() -> Self {
            let flags = RandomXFlag::FLAG_JIT | RandomXFlag::FLAG_HARD_AES;
            Self {
                flags,
                use_dataset: false,
                inner: Mutex::new(None),
            }
        }

        /// Dataset-mode verifier. Allocates ~2 GB on first hash for a given
        /// seed. ~10× the hashing throughput of light mode. Use only on a
        /// large VPS where the dataset fits comfortably.
        pub fn new_full() -> Self {
            let flags = RandomXFlag::FLAG_JIT | RandomXFlag::FLAG_HARD_AES | RandomXFlag::FLAG_FULL_MEM;
            Self {
                flags,
                use_dataset: true,
                inner: Mutex::new(None),
            }
        }

        /// Backwards-compatible default = light mode.
        pub fn new() -> Self {
            Self::new_light()
        }
    }

    impl Default for RandomXVerifier {
        fn default() -> Self {
            Self::new_light()
        }
    }

    impl Verifier for RandomXVerifier {
        fn hash(&self, seed: &SeedHash, blob: &ShareBlob) -> Result<ResultHash, VerifyError> {
            let mut guard = self.inner.lock();
            let needs_rekey = guard.as_ref().map(|c| &c.seed != seed).unwrap_or(true);
            if needs_rekey {
                let cache = RandomXCache::new(self.flags, seed)
                    .map_err(|e| VerifyError::RandomX(format!("{e:?}")))?;
                let (vm, dataset) = if self.use_dataset {
                    let ds = RandomXDataset::new(self.flags, cache.clone(), 0)
                        .map_err(|e| VerifyError::RandomX(format!("dataset init: {e:?}")))?;
                    let vm = RandomXVM::new(self.flags, None, Some(ds.clone()))
                        .map_err(|e| VerifyError::RandomX(format!("{e:?}")))?;
                    (vm, Some(ds))
                } else {
                    let vm = RandomXVM::new(self.flags, Some(cache.clone()), None)
                        .map_err(|e| VerifyError::RandomX(format!("{e:?}")))?;
                    (vm, None)
                };
                *guard = Some(VmCell {
                    seed: *seed,
                    _cache: cache,
                    _dataset: dataset,
                    vm,
                });
            }
            let cell = guard.as_ref().unwrap();
            let h = cell
                .vm
                .calculate_hash(blob)
                .map_err(|e| VerifyError::RandomX(format!("{e:?}")))?;
            let mut out = [0u8; 32];
            out.copy_from_slice(&h[..32]);
            Ok(out)
        }
    }
}

#[cfg(feature = "real")]
pub use real_impl::RandomXVerifier;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_zero_always_meets() {
        assert!(meets_difficulty(&[0xff; 32], 0));
    }

    #[test]
    fn high_diff_rejects_high_hash() {
        let mut hash = [0u8; 32];
        hash[24..32].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(!meets_difficulty(&hash, u64::MAX));
    }

    #[test]
    fn cached_verifier_round_trip() {
        let v = CachedVerifier::new(StubVerifier);
        let seed = [1u8; 32];
        let blob = vec![1, 2, 3, 4];
        // diff=0 always passes the difficulty check
        let _ = v.verify_share(&seed, &blob, 0).unwrap();
    }
}
