//! The share verification pipeline. Pure functions plus a small `ShareSink`
//! trait so the session and the integration tests can both consume credited
//! shares without a hard Postgres dependency.

use crate::jobs::UpstreamJob;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use pool_core::stratum::{assemble_blob_with_nonce, NONCE_LEN};
use pool_core::{MinerId, ShareAccepted};
use randomx_verify::{meets_difficulty, ResultHash, Verifier};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareOutcome {
    /// Share meets local diff; credited. `forwarded` = also met upstream diff.
    Accepted { forwarded: bool },
    BelowDifficulty,
    InvalidHash,
    UnknownJob,
    BadBlob,
}

pub struct VerifyInput<'a> {
    pub miner: MinerId,
    pub job: &'a UpstreamJob,
    pub session_difficulty: u64,
    pub nonce_hex: &'a str,
    pub claimed_result_hex: &'a str,
}

/// Accept the miner's claimed `result_hex` without running RandomX. Used after
/// the per-session warmup has been satisfied and the share was not picked for
/// a spot check. We still do the cheap structural checks: the result must
/// be 32 well-formed bytes and must meet local difficulty (otherwise the
/// miner is submitting obvious garbage and the share is rejected, resetting
/// trust).
pub fn accept_claimed_result(input: &VerifyInput<'_>) -> Result<(ShareOutcome, ResultHash)> {
    let hash = match hex::decode(input.claimed_result_hex) {
        Ok(b) if b.len() == 32 => {
            let mut out = [0u8; 32];
            out.copy_from_slice(&b);
            out
        }
        _ => return Ok((ShareOutcome::BadBlob, [0u8; 32])),
    };
    if !meets_difficulty(&hash, input.session_difficulty) {
        return Ok((ShareOutcome::BelowDifficulty, hash));
    }
    let forwarded = meets_difficulty(&hash, input.job.upstream_diff);
    Ok((ShareOutcome::Accepted { forwarded }, hash))
}

/// Returns the locally-computed result hash (so callers can also forward
/// upstream) and the outcome.
pub fn verify_share<V: Verifier>(
    verifier: &V,
    input: &VerifyInput<'_>,
) -> Result<(ShareOutcome, [u8; 32])> {
    // Decode nonce length cheaply before the expensive hash.
    let nonce = hex::decode(input.nonce_hex)
        .map_err(|_| anyhow!("bad nonce hex"))?;
    if nonce.len() != NONCE_LEN {
        return Ok((ShareOutcome::BadBlob, [0; 32]));
    }
    let blob = match assemble_blob_with_nonce(&input.job.blob_hex, input.nonce_hex) {
        Ok(b) => b,
        Err(_) => return Ok((ShareOutcome::BadBlob, [0; 32])),
    };
    let seed: [u8; 32] = match hex::decode(&input.job.seed_hex) {
        Ok(s) if s.len() == 32 => s.try_into().unwrap(),
        _ => [0u8; 32],
    };
    let hash = verifier
        .hash(&seed, &blob)
        .map_err(|e| anyhow!("randomx: {e}"))?;
    // xmrig's claimed result is mostly a sanity tag; we trust our own hash.
    // If a miner reports a non-matching result we still accept based on our
    // local hash. (Some pools reject mismatches; for v1 we don't.)
    let _ = input.claimed_result_hex;

    if !meets_difficulty(&hash, input.session_difficulty) {
        return Ok((ShareOutcome::BelowDifficulty, hash));
    }
    let forwarded = meets_difficulty(&hash, input.job.upstream_diff);
    Ok((ShareOutcome::Accepted { forwarded }, hash))
}

/// Sink for accepted shares. Production uses the Postgres-backed accountant;
/// tests use an in-memory sink.
#[async_trait]
pub trait ShareSink: Send + Sync {
    async fn credit(&self, share: ShareAccepted) -> Result<i64>;

    /// A miner session authenticated (stratum `login` accepted). The production
    /// sink bumps the live-connection gauge here; default no-op for test/example
    /// sinks. Balanced by exactly one `session_closed`.
    fn session_opened(&self) {}

    /// A miner session ended (connection dropped). Default no-op.
    fn session_closed(&self) {}
}

/// In-memory sink used by tests.
#[derive(Default)]
pub struct InMemorySink {
    pub shares: parking_lot::Mutex<Vec<ShareAccepted>>,
}

#[async_trait]
impl ShareSink for InMemorySink {
    async fn credit(&self, share: ShareAccepted) -> Result<i64> {
        self.shares.lock().push(share);
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::UpstreamJob;
    use randomx_verify::StubVerifier;

    fn test_job() -> UpstreamJob {
        // 76-byte zero blob (enough to contain the 4-byte nonce at offset 39).
        let blob = vec![0u8; 76];
        UpstreamJob {
            job_id: "j".into(),
            blob_hex: hex::encode(&blob),
            seed_hex: hex::encode([1u8; 32]),
            upstream_target_hex: hex::encode(1u32.to_le_bytes()),
            upstream_diff: u64::MAX, // unreachable
            height: Some(1),
        }
    }

    #[test]
    fn accepts_at_zero_difficulty() {
        let v = StubVerifier;
        let j = test_job();
        let input = VerifyInput {
            miner: pool_core::EvmAddress::parse("0x0000000000000000000000000000000000000001").unwrap(),
            job: &j,
            session_difficulty: 0,
            nonce_hex: "00000001",
            claimed_result_hex: "",
        };
        let (outcome, _) = verify_share(&v, &input).unwrap();
        assert!(matches!(outcome, ShareOutcome::Accepted { .. }));
    }

    #[test]
    fn accept_claimed_result_passes_when_hash_meets_diff() {
        let j = test_job();
        // claimed result of all-zeros trivially passes any diff.
        let input = VerifyInput {
            miner: pool_core::EvmAddress::parse("0x0000000000000000000000000000000000000001").unwrap(),
            job: &j,
            session_difficulty: u64::MAX / 2,
            nonce_hex: "00000001",
            claimed_result_hex: &"00".repeat(32),
        };
        let (outcome, _) = accept_claimed_result(&input).unwrap();
        assert!(matches!(outcome, ShareOutcome::Accepted { .. }));
    }

    #[test]
    fn accept_claimed_result_rejects_garbage_hex() {
        let j = test_job();
        let input = VerifyInput {
            miner: pool_core::EvmAddress::parse("0x0000000000000000000000000000000000000001").unwrap(),
            job: &j,
            session_difficulty: 1,
            nonce_hex: "00000001",
            claimed_result_hex: "deadbeef", // not 32 bytes
        };
        let (outcome, _) = accept_claimed_result(&input).unwrap();
        assert_eq!(outcome, ShareOutcome::BadBlob);
    }

    #[test]
    fn accept_claimed_result_rejects_below_local_diff() {
        let j = test_job();
        // A result whose trailing 8 bytes are 0xff..ff vs diff=u64::MAX gives
        // u64::MAX/u64::MAX = 1 >= leading; only leading==0 or leading<=1
        // passes. Here leading=u64::MAX, so meets_difficulty returns false.
        let mut result = [0u8; 32];
        result[24..32].copy_from_slice(&u64::MAX.to_le_bytes());
        let input = VerifyInput {
            miner: pool_core::EvmAddress::parse("0x0000000000000000000000000000000000000001").unwrap(),
            job: &j,
            session_difficulty: u64::MAX,
            nonce_hex: "00000001",
            claimed_result_hex: &hex::encode(result),
        };
        let (outcome, _) = accept_claimed_result(&input).unwrap();
        assert_eq!(outcome, ShareOutcome::BelowDifficulty);
    }

    #[test]
    fn rejects_bad_nonce_length() {
        let v = StubVerifier;
        let j = test_job();
        let input = VerifyInput {
            miner: pool_core::EvmAddress::parse("0x0000000000000000000000000000000000000001").unwrap(),
            job: &j,
            session_difficulty: 0,
            nonce_hex: "abcd",
            claimed_result_hex: "",
        };
        let (outcome, _) = verify_share(&v, &input).unwrap();
        assert_eq!(outcome, ShareOutcome::BadBlob);
    }
}
