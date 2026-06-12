//! Redemption payout stamping.
//!
//! To survive a disk wipe + wallet restore-from-seed, we must be able to tell —
//! from the *restored* wallet's transaction history alone — whether a given
//! redemption was already paid. After a restore, Monero exposes neither the
//! destination address nor any local notes; the only recoverable property of an
//! outgoing payment is its **amount**. So we stamp the redemption id into the
//! low 16 bits of the payout amount: a paid redemption is then identifiable in
//! `get_transfers` (including the mempool/`pool` set, so in-flight txs count) by
//! `amount mod 2^16 == id mod 2^16`. On boot we only disambiguate among the
//! handful of frontier redemptions near the last-processed id, so 16 bits is
//! ample and collisions (ids 65536 apart) never co-occur.
//!
//! The stamp NEVER increases what the pool pays: we round the payout down to a
//! 16-bit boundary and OR in the tag; if that would exceed the original payout
//! we subtract one more unit of the next-highest bit (2^16). Result ≤ payout,
//! and result ≡ id (mod 2^16).

/// Number of low bits used for the id stamp.
pub const STAMP_BITS: u32 = 16;
/// 2^STAMP_BITS.
pub const STAMP_MOD: u128 = 1 << STAMP_BITS;

/// The 16-bit tag for a redemption id.
#[inline]
pub fn stamp_tag(id: u64) -> u128 {
    (id as u128) % STAMP_MOD
}

/// Stamp `id`'s tag into the low 16 bits of `payout`, never exceeding `payout`.
/// Returns the stamped amount to actually transfer. Caller must ensure
/// `payout >= STAMP_MOD` (dust below that is dead-lettered, not stamped).
pub fn stamp_amount(payout: u128, id: u64) -> u128 {
    let tag = stamp_tag(id);
    let base = payout & !(STAMP_MOD - 1); // round down to a 16-bit boundary
    let stamped = base | tag; // == base + tag (low bits were cleared)
    if stamped > payout {
        // Setting the tag overshot the original payout — drop the next-highest
        // bit so we never pay more than computed. Low 16 bits are unchanged.
        stamped - STAMP_MOD
    } else {
        stamped
    }
}

/// Does an outgoing `amount` carry redemption `id`'s stamp?
#[inline]
pub fn matches_stamp(amount: u128, id: u64) -> bool {
    amount % STAMP_MOD == stamp_tag(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_sets_low_bits_to_id_mod() {
        let p = 5_000_000_000u128;
        let s = stamp_amount(p, 42);
        assert_eq!(s % STAMP_MOD, 42);
        assert!(matches_stamp(s, 42));
    }

    #[test]
    fn stamp_never_increases_payout() {
        // Sweep a range of payouts and ids, including ids whose tag exceeds the
        // payout's own low bits (which would otherwise overshoot).
        for p in [STAMP_MOD, 1_000_000u128, 1_000_017, 9_999_999_999, u128::MAX >> 1] {
            for id in [0u64, 1, 1234, 65535, 70000, u32::MAX as u64] {
                let s = stamp_amount(p, id);
                assert!(s <= p, "stamp {s} > payout {p} (id={id})");
                assert_eq!(s % STAMP_MOD, stamp_tag(id), "wrong tag (p={p}, id={id})");
                // Never gives up more than ~2^17 atomic.
                assert!(p - s < 2 * STAMP_MOD, "lost too much: p={p} s={s}");
            }
        }
    }

    #[test]
    fn overpay_case_subtracts_next_bit() {
        // payout low bits = 1; id tag = 5 → base|tag would be +4 over → subtract 2^16.
        let payout = (3u128 << STAMP_BITS) | 1; // ...0001
        let s = stamp_amount(payout, 5);
        assert!(s <= payout);
        assert_eq!(s % STAMP_MOD, 5);
        assert_eq!(s, (2u128 << STAMP_BITS) | 5); // dropped one high unit
    }

    #[test]
    fn id_beyond_16_bits_wraps() {
        // Two ids 2^16 apart share a tag — fine, they never co-occur on the
        // reconciliation frontier.
        assert_eq!(stamp_tag(7), stamp_tag(7 + (1 << 16)));
        assert!(matches_stamp(stamp_amount(1_000_000, 7 + (1 << 16)), 7));
    }
}
