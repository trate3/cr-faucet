//! Pure cumulative-voucher math.
//!
//! A voucher commits the user to a `new_cumulative` value that the contract's
//! `claimed[user]` mapping must be at-most when the voucher is redeemed. The
//! contract pays `new_cumulative - claimed[user]` on redemption. Replay is
//! prevented by the contract requiring strict monotonicity.
//!
//! The signer's job is to choose `new_cumulative` such that:
//!   1. It exceeds both the user's prior on-chain `claimed` and the highest
//!      cumulative we've ever signed (so successive vouchers compose).
//!   2. It does not exceed the user's actually earned total (solvency cap).
//!
//! These properties are pure; integration with the DB + chain is the HTTP
//! layer's job. We keep the math here so it can be exhaustively tested.

#[derive(Debug, Clone, Copy)]
pub struct VoucherInputs {
    /// Sum of all share-driven credit ever accrued to this miner.
    pub earned_cumulative: i64,
    /// Highest cumulative this signer has previously issued for this miner
    /// (or 0 if never).
    pub last_voucher_cumulative: i64,
    /// `MiningPoolToken.claimed[user]` as read from the L2 (or 0 if unknown).
    pub on_chain_claimed: i64,
    /// Marginal amount the user wants this voucher worth. `None` = "max
    /// remaining".
    pub requested_amount: Option<i64>,
}

#[derive(Debug, Clone, Copy)]
pub struct VoucherDecision {
    /// Value to encode as `cumulativeAmount` in the EIP-712 voucher payload.
    pub new_cumulative: i64,
    /// Amount this voucher slice represents (== requested_amount or the
    /// remaining-available if request was `None`).
    pub marginal: i64,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VoucherError {
    #[error("amount must be positive")]
    NonPositive,
    #[error("only {available} available beyond prior vouchers/claims; requested {requested}")]
    Insufficient { requested: i64, available: i64 },
    #[error("nothing to issue: already covered by prior vouchers or claims")]
    Empty,
}

pub fn decide(inputs: VoucherInputs) -> Result<VoucherDecision, VoucherError> {
    let base = inputs.last_voucher_cumulative.max(inputs.on_chain_claimed).max(0);
    let earned = inputs.earned_cumulative.max(0);
    let available = earned.saturating_sub(base);
    if available <= 0 {
        return Err(VoucherError::Empty);
    }
    let amount = inputs.requested_amount.unwrap_or(available);
    if amount <= 0 {
        return Err(VoucherError::NonPositive);
    }
    if amount > available {
        return Err(VoucherError::Insufficient { requested: amount, available });
    }
    Ok(VoucherDecision {
        new_cumulative: base + amount,
        marginal: amount,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(earned: i64, last: i64, claimed: i64, requested: Option<i64>) -> VoucherInputs {
        VoucherInputs {
            earned_cumulative: earned,
            last_voucher_cumulative: last,
            on_chain_claimed: claimed,
            requested_amount: requested,
        }
    }

    #[test]
    fn first_voucher_with_amount() {
        let d = decide(inputs(1000, 0, 0, Some(100))).unwrap();
        assert_eq!(d.new_cumulative, 100);
        assert_eq!(d.marginal, 100);
    }

    #[test]
    fn first_voucher_max() {
        let d = decide(inputs(1000, 0, 0, None)).unwrap();
        assert_eq!(d.new_cumulative, 1000);
        assert_eq!(d.marginal, 1000);
    }

    #[test]
    fn second_voucher_stacks_above_first_unclaimed() {
        // User got V1 for 100 (last_voucher_cum=100), didn't claim it.
        // Requests another for 50: should give cumulative=150 (stacked).
        let d = decide(inputs(1000, 100, 0, Some(50))).unwrap();
        assert_eq!(d.new_cumulative, 150);
        assert_eq!(d.marginal, 50);
    }

    #[test]
    fn voucher_after_partial_claim() {
        // User claimed V1 (cum=100, on-chain claimed=100). Has earned more.
        // last_voucher_cum is still 100 from before. Request another 200.
        let d = decide(inputs(1000, 100, 100, Some(200))).unwrap();
        assert_eq!(d.new_cumulative, 300);
        assert_eq!(d.marginal, 200);
    }

    #[test]
    fn voucher_after_third_party_claim_advanced_on_chain() {
        // Somehow on-chain claimed advanced past our last_voucher_cum (e.g.
        // signer state was wiped). Base should track on-chain.
        let d = decide(inputs(1000, 100, 400, Some(50))).unwrap();
        assert_eq!(d.new_cumulative, 450);
        assert_eq!(d.marginal, 50);
    }

    #[test]
    fn rejects_overdrawn_request() {
        let err = decide(inputs(1000, 800, 0, Some(500))).unwrap_err();
        assert_eq!(err, VoucherError::Insufficient { requested: 500, available: 200 });
    }

    #[test]
    fn rejects_negative_or_zero_amount() {
        assert_eq!(decide(inputs(1000, 0, 0, Some(0))).unwrap_err(), VoucherError::NonPositive);
        assert_eq!(decide(inputs(1000, 0, 0, Some(-1))).unwrap_err(), VoucherError::NonPositive);
    }

    #[test]
    fn empty_when_everything_already_covered() {
        // last_voucher_cum already at earned.
        assert_eq!(decide(inputs(1000, 1000, 0, None)).unwrap_err(), VoucherError::Empty);
        // on-chain claimed already at earned.
        assert_eq!(decide(inputs(1000, 0, 1000, None)).unwrap_err(), VoucherError::Empty);
    }

    #[test]
    fn max_after_claim_returns_remainder() {
        // Claimed 400 on chain, earned 1000, last_voucher_cum was 400 (we
        // recorded the value we signed before claim). Max-request: 600.
        let d = decide(inputs(1000, 400, 400, None)).unwrap();
        assert_eq!(d.new_cumulative, 1000);
        assert_eq!(d.marginal, 600);
    }

    #[test]
    fn compositional_marginals_sum_to_total() {
        // Simulate: user requests V1 for 100, then V2 for 200, then V3 for max.
        // All three vouchers' marginals should sum to the user's earned total
        // when claimed in order.
        let earned = 1_000;
        let mut last = 0;
        let mut claimed_onchain = 0;

        let v1 = decide(inputs(earned, last, claimed_onchain, Some(100))).unwrap();
        assert_eq!(v1.marginal, 100);
        last = v1.new_cumulative;

        let v2 = decide(inputs(earned, last, claimed_onchain, Some(200))).unwrap();
        assert_eq!(v2.marginal, 200);
        last = v2.new_cumulative;

        let v3 = decide(inputs(earned, last, claimed_onchain, None)).unwrap();
        assert_eq!(v3.marginal, 700);
        let _ = v3.new_cumulative;

        // Total marginals = earned.
        assert_eq!(v1.marginal + v2.marginal + v3.marginal, earned);

        // Even if the user claims them out-of-order, the contract caps
        // claimed[user] at the highest cumulative claimed so far. Simulate
        // claiming V3 first (largest):
        claimed_onchain = v3.new_cumulative;
        // Now V1 and V2 are no-ops (their cum < claimed); no double spend.
        let could_claim_after = (v1.new_cumulative - claimed_onchain).max(0);
        assert_eq!(could_claim_after, 0);
        assert_eq!(claimed_onchain, earned);
    }
}
