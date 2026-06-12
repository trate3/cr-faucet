//! Adaptive share-verification sampling policy.
//!
//! Every per-session submit asks `should_verify(state, rng)`. The policy:
//!  - For the first `warmup` successful verifications, **always** verify.
//!  - After that, sample at `sample_rate` (e.g. 10%).
//!  - On a failed spot-check, the caller resets `verified_count = 0` so the
//!    miner has to re-earn trust before sampling resumes.

use rand::Rng;

/// Sampling configuration. Tweak via `pool-core::config::StratumConfig` if you
/// want to expose to the operator; today these are constants.
#[derive(Debug, Clone, Copy)]
pub struct SampleConfig {
    pub warmup: u32,
    pub sample_rate: f64,
}

impl Default for SampleConfig {
    fn default() -> Self {
        Self {
            warmup: 5,
            sample_rate: 0.10,
        }
    }
}

impl From<&pool_core::config::StratumConfig> for SampleConfig {
    fn from(c: &pool_core::config::StratumConfig) -> Self {
        Self {
            warmup: c.verification_warmup,
            sample_rate: c.verification_sample_rate.clamp(0.0, 1.0),
        }
    }
}

/// Decide whether to run full RandomX verification on a share, given the
/// session's count of consecutive successful verifications.
pub fn should_verify(verified_count: u32, cfg: SampleConfig, rng: &mut impl Rng) -> bool {
    if verified_count < cfg.warmup {
        return true;
    }
    rng.gen_bool(cfg.sample_rate.clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    #[test]
    fn always_verify_during_warmup() {
        let cfg = SampleConfig::default();
        let mut rng = StdRng::seed_from_u64(42);
        for n in 0..cfg.warmup {
            assert!(should_verify(n, cfg, &mut rng), "n={n}");
        }
    }

    #[test]
    fn samples_at_configured_rate_after_warmup() {
        let cfg = SampleConfig {
            warmup: 5,
            sample_rate: 0.10,
        };
        let mut rng = StdRng::seed_from_u64(7);
        let trials = 10_000;
        let hits = (0..trials).filter(|_| should_verify(100, cfg, &mut rng)).count();
        let rate = hits as f64 / trials as f64;
        assert!((rate - 0.10).abs() < 0.02, "got {rate}");
    }

    #[test]
    fn sample_rate_zero_never_verifies_after_warmup() {
        let cfg = SampleConfig {
            warmup: 5,
            sample_rate: 0.0,
        };
        let mut rng = StdRng::seed_from_u64(1);
        for _ in 0..1_000 {
            assert!(!should_verify(100, cfg, &mut rng));
        }
    }

    #[test]
    fn sample_rate_one_always_verifies() {
        let cfg = SampleConfig {
            warmup: 5,
            sample_rate: 1.0,
        };
        let mut rng = StdRng::seed_from_u64(1);
        for _ in 0..1_000 {
            assert!(should_verify(100, cfg, &mut rng));
        }
    }
}
