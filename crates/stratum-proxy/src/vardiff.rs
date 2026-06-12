//! Per-connection variable-difficulty controller, modelled on the battle-tested
//! node-cryptonote-pool varDiff.
//!
//! Key properties (and why):
//! - **Samples are never mixed across difficulties.** Inter-share times are kept
//!   in a ring that is CLEARED whenever the difficulty changes, so the average
//!   is always taken over shares found at the current difficulty (a 5s share at
//!   diff D1 and a 5s share at D2 are not comparable as raw times).
//! - **Dead-band.** While the average share time is within ±`variance` of the
//!   target, the difficulty is left ALONE. This is what kills steady-state
//!   jitter — the diff stops moving once it's good enough, instead of chasing
//!   every share's randomness.
//! - **Bounded change.** Each retarget is capped to `[max_drop_factor,
//!   max_gain_factor]` of the current diff, and to `[min, effective_max]`
//!   (`min` keeps RandomX-verify cost amortized; `effective_max` is the upstream
//!   job difficulty — we're only rewarded at theirs).
//! - **Idle decay.** A long gap with no share is folded into the average as a
//!   very long share time, so a slowed/stalled miner's diff comes down.

use pool_core::config::VardiffConfig;
use std::collections::VecDeque;
use std::time::Instant;

pub struct Vardiff {
    pub current: u64,
    pub min: u64,
    pub max: u64,
    /// Upstream job difficulty cap (`u64::MAX` until set). We never advertise or
    /// credit above what the upstream rewards us for.
    upstream_cap: u64,
    target_secs: f64,
    last_submit: Option<Instant>,
    /// Inter-share times (seconds) at the CURRENT difficulty — cleared on every
    /// change. Capped at `sample_size`.
    intervals: VecDeque<f64>,
    sample_size: usize,
    /// Dead-band half-width as a fraction of target (e.g. 0.30 = ±30%).
    variance: f64,
    max_gain_factor: f64,
    max_drop_factor: f64,
}

impl Vardiff {
    pub fn new(min: u64, initial: u64, max: u64, target_secs: u32, cfg: &VardiffConfig) -> Self {
        let n = cfg.sample_size.max(1) as usize;
        Self {
            current: initial.max(min),
            min,
            max,
            upstream_cap: u64::MAX,
            target_secs: target_secs as f64,
            // Seed the clock at session start so a too-high initial diff that
            // never finds a share still decays via the idle path.
            last_submit: Some(Instant::now()),
            intervals: VecDeque::with_capacity(n),
            sample_size: n,
            variance: (cfg.variance_percent / 100.0).clamp(0.0, 0.95),
            max_gain_factor: cfg.max_gain_factor,
            max_drop_factor: cfg.max_drop_factor,
        }
    }

    /// Never above `max`/upstream, never below the floor.
    fn effective_max(&self) -> u64 {
        self.max.min(self.upstream_cap).max(self.min)
    }

    /// Pin the upstream difficulty cap (call on each new job). If it forces the
    /// current diff down, the sample window is cleared — those shares were found
    /// at a now-invalid difficulty.
    pub fn set_upstream_cap(&mut self, upstream_diff: u64) {
        self.upstream_cap = upstream_diff;
        let ceil = self.effective_max();
        if self.current > ceil {
            self.current = ceil;
            self.intervals.clear();
        }
    }

    /// Record an accepted share's inter-share interval. Does NOT change the diff
    /// — retargeting happens at job boundaries in `retarget`.
    pub fn on_share(&mut self) {
        let now = Instant::now();
        if let Some(prev) = self.last_submit {
            self.push_interval((now - prev).as_secs_f64());
        }
        self.last_submit = Some(now);
    }

    fn push_interval(&mut self, dt: f64) {
        self.intervals.push_back(dt.max(0.001));
        while self.intervals.len() > self.sample_size {
            self.intervals.pop_front();
        }
    }

    fn window_avg(&self, extra: Option<f64>) -> Option<f64> {
        if self.intervals.is_empty() && extra.is_none() {
            return None;
        }
        let mut sum: f64 = self.intervals.iter().sum();
        let mut n = self.intervals.len();
        if let Some(e) = extra {
            sum += e;
            n += 1;
        }
        Some(sum / n as f64)
    }

    /// Decide the difficulty to advertise for the next job. Call at each job
    /// boundary, AFTER `set_upstream_cap`. Returns the (possibly unchanged) diff
    /// to serve. The `Instant`-based wrapper; `retarget_with_gap` is the
    /// time-injectable core used by tests.
    pub fn retarget(&mut self) -> u64 {
        let gap = self
            .last_submit
            .map(|p| (Instant::now() - p).as_secs_f64())
            .unwrap_or(0.0);
        self.retarget_with_gap(gap)
    }

    fn retarget_with_gap(&mut self, gap_secs: f64) -> u64 {
        // Honor a freshly-lowered upstream cap.
        if self.current > self.effective_max() {
            self.current = self.effective_max();
        }

        let t_min = self.target_secs * (1.0 - self.variance);
        let t_max = self.target_secs * (1.0 + self.variance);
        let idle = gap_secs > t_max;

        let avg = if idle {
            // Stalled/slow miner: fold the elapsed gap in so the diff comes down.
            self.window_avg(Some(gap_secs)).unwrap_or(gap_secs)
        } else {
            // Normal retarget: wait for a full window so we don't react to one or
            // two noisy shares.
            if self.intervals.len() < self.sample_size {
                return self.current;
            }
            match self.window_avg(None) {
                Some(a) => a,
                None => return self.current,
            }
        };

        // Dead-band: good enough → leave it alone (kills steady-state jitter).
        if avg >= t_min && avg <= t_max {
            return self.current;
        }

        // Retarget toward target rate, capped per change and to [min, eff_max].
        let raw = (self.target_secs / avg) * self.current as f64;
        let hi = (self.current as f64 * self.max_gain_factor).round() as u64;
        let lo = (self.current as f64 * self.max_drop_factor).round() as u64;
        let new = (raw.round() as u64)
            .clamp(lo, hi)
            .clamp(self.min, self.effective_max());

        if new != self.current {
            self.current = new;
            self.intervals.clear(); // next average is taken purely at the new diff
            if idle {
                // Don't keep decreasing off the same stale gap.
                self.last_submit = Some(Instant::now());
            }
        }
        self.current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> VardiffConfig {
        // sample_size 16, variance 30, gain 2.0, drop 0.7 (≤ -30% per change)
        VardiffConfig::default()
    }

    fn fill(v: &mut Vardiff, dt: f64, n: usize) {
        for _ in 0..n {
            v.push_interval(dt);
        }
    }

    #[test]
    fn dead_band_no_change() {
        let mut v = Vardiff::new(1_000, 50_000, 10_000_000, 20, &cfg());
        fill(&mut v, 20.0, 16); // exactly at target
        assert_eq!(v.retarget_with_gap(1.0), 50_000);
        fill(&mut v, 24.0, 16); // within +20% (t_max = 26)
        assert_eq!(v.retarget_with_gap(1.0), 50_000);
    }

    #[test]
    fn raises_when_fast_capped_at_2x() {
        let mut v = Vardiff::new(1_000, 50_000, 10_000_000, 20, &cfg());
        fill(&mut v, 5.0, 16); // 4x too fast => raw wants 4x; capped to 2x
        let d = v.retarget_with_gap(1.0);
        assert_eq!(d, 100_000);
    }

    #[test]
    fn lowers_when_slow_capped_at_drop_factor() {
        let mut v = Vardiff::new(1_000, 50_000, 10_000_000, 20, &cfg());
        fill(&mut v, 60.0, 16); // 3x too slow => raw wants ÷3; capped to -30% (0.7×)
        let d = v.retarget_with_gap(1.0);
        assert_eq!(d, 35_000);
    }

    #[test]
    fn needs_full_window_for_normal_retarget() {
        let mut v = Vardiff::new(1_000, 50_000, 10_000_000, 20, &cfg());
        fill(&mut v, 5.0, 4); // only 4 of 16
        assert_eq!(v.retarget_with_gap(1.0), 50_000); // no change yet
    }

    #[test]
    fn window_cleared_on_change_no_mixing() {
        let mut v = Vardiff::new(1_000, 50_000, 10_000_000, 20, &cfg());
        fill(&mut v, 5.0, 16);
        assert_eq!(v.retarget_with_gap(1.0), 100_000); // raised + cleared
        fill(&mut v, 5.0, 4); // partial new window
        assert_eq!(v.retarget_with_gap(1.0), 100_000); // waits for a full window
    }

    #[test]
    fn idle_lowers_diff_without_samples() {
        let mut v = Vardiff::new(1_000, 50_000, 10_000_000, 20, &cfg());
        // No shares; a big idle gap (> t_max=26) decays the diff, capped to 0.7x.
        let d = v.retarget_with_gap(600.0);
        assert_eq!(d, 35_000);
    }

    #[test]
    fn never_below_floor_or_above_upstream() {
        let mut v = Vardiff::new(10_000, 20_000, 10_000_000, 20, &cfg());
        v.set_upstream_cap(50_000);
        // Hammer it fast repeatedly; never exceeds the 50k upstream cap.
        for _ in 0..20 {
            fill(&mut v, 1.0, 16);
            v.retarget_with_gap(1.0);
        }
        assert!(v.current <= 50_000);
        // Hammer it slow; never below the 10k floor.
        for _ in 0..20 {
            fill(&mut v, 1e6, 16);
            v.retarget_with_gap(1.0);
        }
        assert!(v.current >= 10_000);
    }
}
