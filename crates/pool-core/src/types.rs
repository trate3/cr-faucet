use alloy::primitives::Address;
use serde::{Deserialize, Serialize};

pub type AtomicXmr = u128;
pub type ShareDifficulty = u64;
pub type JobId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EvmAddress(pub Address);

impl EvmAddress {
    pub fn parse(s: &str) -> Result<Self, alloy::primitives::AddressError> {
        Ok(Self(s.parse::<Address>()?))
    }
}

impl std::fmt::Display for EvmAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

pub type MinerId = EvmAddress;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareAccepted {
    pub miner: MinerId,
    pub job_id: JobId,
    pub difficulty: ShareDifficulty,
    pub accepted_at: chrono::DateTime<chrono::Utc>,
    pub forwarded_upstream: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PpsRate {
    pub atomic_xmr_per_diff: f64,
    pub effective_from: chrono::DateTime<chrono::Utc>,
}

impl PpsRate {
    pub fn credit(&self, diff: ShareDifficulty) -> AtomicXmr {
        let raw = self.atomic_xmr_per_diff * diff as f64;
        if raw < 0.0 {
            0
        } else {
            raw as AtomicXmr
        }
    }
}
