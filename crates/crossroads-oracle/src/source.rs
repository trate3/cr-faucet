//! Source-chain reader: the RPC committee. Given a block number, poll every
//! committee member (a fixed set of independent RPCs, capped at 8) and require a
//! quorum to agree on the block hash. "Just requests from RPCs": one
//! `eth_getBlockByNumber("latest")` per member for the tip (→ confirmations),
//! then one `eth_getBlockByNumber(N)` for the hash. No RLP, no extra methods.

use alloy::primitives::B256;
use anyhow::{bail, Context, Result};
use std::collections::HashMap;

/// What the committee agreed on for a requested block.
pub struct Confirmed {
    pub block_hash: B256,
    pub quorum_tip: u64,
    pub observed_confirmations: u64,
    pub observed_quorum: u64,
    pub finalized_block_number: u64,
}

/// The `quorum`-th largest tip (so at least `quorum` members are at or above it).
/// `None` if fewer than `quorum` members reported a tip.
pub fn quorum_tip(mut tips: Vec<u64>, quorum: usize) -> Option<u64> {
    if tips.len() < quorum {
        return None;
    }
    tips.sort_unstable_by(|a, b| b.cmp(a));
    Some(tips[quorum - 1])
}

/// The block hash returned by the most members, with its count — iff that count
/// meets `quorum`. Committee agreement on the value itself.
pub fn quorum_hash(votes: &[B256], quorum: usize) -> Option<(B256, usize)> {
    let mut counts: HashMap<B256, usize> = HashMap::new();
    for v in votes {
        *counts.entry(*v).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .max_by_key(|&(_, n)| n)
        .filter(|&(_, n)| n >= quorum)
}

async fn rpc(
    client: &reqwest::Client,
    url: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    let body = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let resp = client.post(url).json(&body).send().await?.error_for_status()?;
    let v: serde_json::Value = resp.json().await?;
    if let Some(e) = v.get("error").filter(|e| !e.is_null()) {
        bail!("rpc {method} error: {e}");
    }
    Ok(v.get("result").cloned().unwrap_or(serde_json::Value::Null))
}

fn parse_u64_hex(v: &serde_json::Value) -> Option<u64> {
    let s = v.as_str()?;
    let s = s.strip_prefix("0x")?;
    u64::from_str_radix(s, 16).ok()
}

/// One committee member's vote: (tip, finalized_number, block-N hash). Any failure
/// just drops the member from the quorum (None fields).
async fn poll_member(
    client: &reqwest::Client,
    url: &str,
    block_number: u64,
    want_finalized: bool,
) -> (Option<u64>, Option<u64>, Option<B256>) {
    let tip = rpc(client, url, "eth_getBlockByNumber", serde_json::json!(["latest", false]))
        .await
        .ok()
        .and_then(|b| parse_u64_hex(b.get("number")?));

    let finalized = if want_finalized {
        rpc(client, url, "eth_getBlockByNumber", serde_json::json!(["finalized", false]))
            .await
            .ok()
            .and_then(|b| parse_u64_hex(b.get("number")?))
    } else {
        None
    };

    let tag = format!("0x{block_number:x}");
    let hash = rpc(client, url, "eth_getBlockByNumber", serde_json::json!([tag, false]))
        .await
        .ok()
        .and_then(|b| {
            let h = b.get("hash")?.as_str()?;
            h.parse::<B256>().ok()
        });

    (tip, finalized, hash)
}

/// Poll the committee for `block_number` and require quorum agreement on the hash
/// plus `min_confirmations` confirmations (and finality if `mandate_finalized`).
pub async fn fetch_confirmed(
    client: &reqwest::Client,
    urls: &[String],
    quorum: usize,
    block_number: u64,
    min_confirmations: u64,
    mandate_finalized: bool,
) -> Result<Confirmed> {
    let votes =
        futures::future::join_all(urls.iter().map(|u| poll_member(client, u, block_number, mandate_finalized)))
            .await;

    let tips: Vec<u64> = votes.iter().filter_map(|v| v.0).collect();
    let tip = quorum_tip(tips, quorum).context("not enough committee members returned a tip")?;
    if block_number + min_confirmations > tip {
        bail!("block {block_number} not yet {min_confirmations}-confirmed (committee tip {tip})");
    }

    let finalized_block_number = if mandate_finalized {
        let fins: Vec<u64> = votes.iter().filter_map(|v| v.1).collect();
        let f = quorum_tip(fins, quorum).context("finalized quorum unavailable")?;
        if f < block_number {
            bail!("block {block_number} not finalized by committee quorum (finalized {f})");
        }
        f
    } else {
        0
    };

    let hashes: Vec<B256> = votes.iter().filter_map(|v| v.2).collect();
    let (block_hash, observed_quorum) =
        quorum_hash(&hashes, quorum).context("committee did not reach quorum on the block hash")?;

    Ok(Confirmed {
        block_hash,
        quorum_tip: tip,
        observed_confirmations: tip - block_number,
        observed_quorum: observed_quorum as u64,
        finalized_block_number,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quorum_tip_picks_kth_largest() {
        // 3 members, quorum 2 → second-highest tip (≥2 members are at/above it).
        assert_eq!(quorum_tip(vec![100, 105, 102], 2), Some(102));
        assert_eq!(quorum_tip(vec![100], 2), None); // too few
    }

    #[test]
    fn quorum_hash_needs_majority_agreement() {
        let a = B256::repeat_byte(0xaa);
        let b = B256::repeat_byte(0xbb);
        assert_eq!(quorum_hash(&[a, a, b], 2), Some((a, 2))); // a wins, meets quorum
        assert_eq!(quorum_hash(&[a, b], 2), None); // tie, neither meets quorum
        assert_eq!(quorum_hash(&[a], 2), None); // one vote, below quorum
    }
}
