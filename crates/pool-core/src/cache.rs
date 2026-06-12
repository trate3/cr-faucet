//! In-process caches for values that change far slower than the hot path.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// PPS rate cache: a single f64 plus a "fresh-as-of" timestamp, both lock-free.
/// Hot path reads atomically; pps-rate task overwrites once per refresh.
#[derive(Debug)]
pub struct RateCache {
    bits: AtomicU64,
    /// The pool's fee per unit difficulty (atomic XMR), published alongside the
    /// net rate. The accountant multiplies by share difficulty to accrue the
    /// pool's swappable fee. Separate atomic so `set()` keeps its signature.
    fee_bits: AtomicU64,
    set_at_unix: AtomicI64,
}

impl RateCache {
    pub const fn new() -> Self {
        Self {
            bits: AtomicU64::new(0),
            fee_bits: AtomicU64::new(0),
            set_at_unix: AtomicI64::new(0),
        }
    }

    #[inline]
    pub fn get(&self) -> f64 {
        f64::from_bits(self.bits.load(Ordering::Relaxed))
    }

    /// Pool fee per unit difficulty (atomic XMR).
    #[inline]
    pub fn fee_rate(&self) -> f64 {
        f64::from_bits(self.fee_bits.load(Ordering::Relaxed))
    }

    pub fn set_fee_rate(&self, fee_per_diff: f64) {
        self.fee_bits.store(fee_per_diff.to_bits(), Ordering::Relaxed);
    }

    pub fn set(&self, rate: f64, now_unix: i64) {
        self.bits.store(rate.to_bits(), Ordering::Relaxed);
        self.set_at_unix.store(now_unix, Ordering::Relaxed);
    }

    pub fn set_at_unix(&self) -> i64 {
        self.set_at_unix.load(Ordering::Relaxed)
    }
}

impl Default for RateCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Current effective pool fee (fraction). The pps-rate loop reads this each
/// tick; the adaptive-fee controller overwrites it from the reservoir balance.
/// In fixed mode it's set once to `[pps].pool_fee`. Lock-free f64.
#[derive(Debug)]
pub struct FeeCache {
    bits: AtomicU64,
}

impl FeeCache {
    pub fn new(initial: f64) -> Self {
        Self {
            bits: AtomicU64::new(initial.to_bits()),
        }
    }
    #[inline]
    pub fn get(&self) -> f64 {
        f64::from_bits(self.bits.load(Ordering::Relaxed))
    }
    pub fn set(&self, fee: f64) {
        self.bits.store(fee.to_bits(), Ordering::Relaxed);
    }
}

/// Adaptive pool fee from rent pressure. At/above `healthy_wei` the reservoir is
/// comfortable → `fee_min`; at/below `critical_wei` it's nearly broke → `fee_max`;
/// linear in between. Robust to mis-ordered bounds and a degenerate window.
pub fn effective_pool_fee(
    reservoir_wei: u128,
    critical_wei: u128,
    healthy_wei: u128,
    fee_min: f64,
    fee_max: f64,
) -> f64 {
    let (lo, hi) = if fee_min <= fee_max {
        (fee_min, fee_max)
    } else {
        (fee_max, fee_min)
    };
    if healthy_wei <= critical_wei {
        return hi.clamp(0.0, 0.99); // degenerate window → assume max pressure
    }
    let p = if reservoir_wei >= healthy_wei {
        0.0
    } else if reservoir_wei <= critical_wei {
        1.0
    } else {
        (healthy_wei - reservoir_wei) as f64 / (healthy_wei - critical_wei) as f64
    };
    (lo + p * (hi - lo)).clamp(0.0, 0.99)
}

#[cfg(test)]
mod fee_tests {
    use super::effective_pool_fee;

    #[test]
    fn healthy_reservoir_uses_min() {
        assert_eq!(effective_pool_fee(100, 5, 50, 0.01, 0.10), 0.01);
    }
    #[test]
    fn critical_reservoir_uses_max() {
        assert_eq!(effective_pool_fee(5, 5, 50, 0.01, 0.10), 0.10);
        assert_eq!(effective_pool_fee(0, 5, 50, 0.01, 0.10), 0.10);
    }
    #[test]
    fn scales_linearly_between() {
        // midpoint of [5,50] is 27.5; reservoir 27.5 → p≈0.5 → ~0.055
        let f = effective_pool_fee(27, 5, 50, 0.01, 0.10);
        assert!(f > 0.05 && f < 0.06, "got {f}");
    }
    #[test]
    fn degenerate_window_is_safe_max() {
        assert_eq!(effective_pool_fee(10, 50, 50, 0.01, 0.10), 0.10);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rate_roundtrip() {
        let c = RateCache::new();
        c.set(1.234567e-9, 1700000000);
        assert!((c.get() - 1.234567e-9).abs() < 1e-18);
        assert_eq!(c.set_at_unix(), 1700000000);
    }

    #[test]
    fn default_rate_is_zero() {
        let c = RateCache::default();
        assert_eq!(c.get(), 0.0);
    }

    #[test]
    fn default_fee_rate_is_zero() {
        assert_eq!(RateCache::default().fee_rate(), 0.0);
    }

    #[test]
    fn fee_rate_roundtrip() {
        let c = RateCache::new();
        c.set_fee_rate(9.87654e-10);
        assert!((c.fee_rate() - 9.87654e-10).abs() < 1e-19);
    }

    #[test]
    fn fee_rate_and_net_rate_are_independent() {
        let c = RateCache::new();
        c.set(2.0, 123);
        c.set_fee_rate(0.5);
        assert_eq!(c.get(), 2.0);
        assert_eq!(c.fee_rate(), 0.5);
        // updating one leaves the other untouched
        c.set(3.0, 456);
        assert_eq!(c.fee_rate(), 0.5);
        c.set_fee_rate(0.25);
        assert_eq!(c.get(), 3.0);
        assert_eq!(c.set_at_unix(), 456);
    }
}
