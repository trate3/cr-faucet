//! PPS rate formula. Pure function; the I/O (monerod RPC + DB writes) lives
//! in the `pps-rate` crate.
//!
//! ```text
//! raw_per_diff   = block_reward * (1 - upstream_fee) / network_diff
//! after_margin   = raw_per_diff * (1 - pool_fee - risk_buffer)
//! op_cost_term   = operational_cost_atomic_xmr_per_second / pool_hashrate
//! rate           = max(0, after_margin - op_cost_term)
//! ```
//!
//! All XMR values in atomic units (piconero, 1e-12 XMR). All probabilities in
//! [0, 1]. Hashrate is in `hashes/sec` (or any consistent unit; the formula
//! cares only that `op_cost / hashrate` matches `xmr_per_diff` dimensions, and
//! difficulty IS hashes-to-mean-a-hit, so it works out).

#[derive(Debug, Clone, Copy)]
pub struct PpsInputs {
    pub block_reward_atomic: f64,
    pub network_difficulty: f64,
    pub upstream_fee: f64,
    pub pool_fee: f64,
    pub risk_buffer: f64,
    pub operational_cost_atomic_per_second: f64,
    pub pool_hashrate: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct PpsBreakdown {
    pub raw_per_diff: f64,
    pub after_margin: f64,
    pub op_cost_term: f64,
    pub rate: f64,
    /// The pool's fee per unit difficulty = `raw_per_diff * pool_fee` — the cut
    /// the pool keeps (NOT credited to miners). Accrued per share and minted as
    /// fee-MPT for self-funding (the `risk_buffer` is held separately as the
    /// reserve cushion, never swept).
    pub fee_per_diff: f64,
}

pub fn compute(i: PpsInputs) -> PpsBreakdown {
    let net = i.network_difficulty.max(1.0);
    let hash = i.pool_hashrate.max(1.0);
    let raw_per_diff = i.block_reward_atomic * (1.0 - i.upstream_fee) / net;
    let after_margin = raw_per_diff * (1.0 - i.pool_fee - i.risk_buffer);
    let op_cost_term = i.operational_cost_atomic_per_second / hash;
    let rate = (after_margin - op_cost_term).max(0.0);
    let fee_per_diff = raw_per_diff * i.pool_fee.max(0.0);
    PpsBreakdown { raw_per_diff, after_margin, op_cost_term, rate, fee_per_diff }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nominal() -> PpsInputs {
        PpsInputs {
            block_reward_atomic: 600_000_000_000.0, // 0.6 XMR in piconero
            network_difficulty: 300_000_000_000.0,
            upstream_fee: 0.006,
            pool_fee: 0.01,
            risk_buffer: 0.05,
            operational_cost_atomic_per_second: 10_000.0,
            pool_hashrate: 1_000_000.0,
        }
    }

    #[test]
    fn raw_per_diff_matches_expected() {
        let b = compute(nominal());
        // raw = 600e9 * 0.994 / 300e9 ≈ 1.988
        assert!((b.raw_per_diff - 1.988).abs() < 1e-3);
    }

    #[test]
    fn margin_applied_multiplicatively() {
        let b = compute(nominal());
        assert!((b.after_margin / b.raw_per_diff - (1.0 - 0.01 - 0.05)).abs() < 1e-9);
    }

    #[test]
    fn op_cost_drains_with_low_hashrate() {
        let mut i = nominal();
        i.pool_hashrate = 1.0; // tiny pool
        let b = compute(i);
        // op_cost_term = 10_000 / 1 = 10_000 >> after_margin (~1.86), so rate = 0
        assert_eq!(b.rate, 0.0);
    }

    #[test]
    fn higher_hashrate_keeps_rate_close_to_after_margin() {
        let mut i = nominal();
        i.pool_hashrate = 1e12;
        let b = compute(i);
        assert!((b.rate - b.after_margin).abs() / b.after_margin < 1e-3);
    }

    #[test]
    fn never_negative() {
        let mut i = nominal();
        i.pool_fee = 0.5;
        i.risk_buffer = 0.6; // 1 - 0.5 - 0.6 = -0.1
        let b = compute(i);
        assert_eq!(b.rate, 0.0);
    }

    #[test]
    fn fee_per_diff_is_raw_times_pool_fee() {
        let b = compute(nominal());
        assert!((b.fee_per_diff - b.raw_per_diff * 0.01).abs() < 1e-9);
    }

    #[test]
    fn fee_per_diff_zero_when_no_pool_fee() {
        let mut i = nominal();
        i.pool_fee = 0.0;
        let b = compute(i);
        assert_eq!(b.fee_per_diff, 0.0);
    }

    #[test]
    fn fee_per_diff_clamps_negative_pool_fee() {
        let mut i = nominal();
        i.pool_fee = -0.2; // nonsensical; must not accrue negative fee
        let b = compute(i);
        assert_eq!(b.fee_per_diff, 0.0);
    }

    #[test]
    fn fee_per_diff_independent_of_risk_buffer() {
        // The risk_buffer is the reserve cushion; it must NOT change the swept fee.
        let mut a = nominal();
        a.risk_buffer = 0.05;
        let mut b = nominal();
        b.risk_buffer = 0.30;
        assert_eq!(compute(a).fee_per_diff, compute(b).fee_per_diff);
    }

    #[test]
    fn fee_plus_net_plus_buffer_does_not_exceed_raw() {
        // The miner's net rate + the pool fee + the risk buffer must all come out
        // of the same raw credit — never mint more than was earned. (op_cost only
        // shrinks the net, so ignore it here by using a huge hashrate.)
        let mut i = nominal();
        i.pool_hashrate = 1e15;
        let b = compute(i);
        let buffer_per_diff = b.raw_per_diff * i.risk_buffer;
        assert!(b.rate + b.fee_per_diff + buffer_per_diff <= b.raw_per_diff + 1e-6);
    }
}
