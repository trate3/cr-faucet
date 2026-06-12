//! crossroads-oracle — the mining pool's absorbed EVM block-hash oracle.
//!
//! Request-driven (Model B): the TEE polls an RPC committee (a fixed set of
//! independent source-chain RPCs, of which a quorum must agree), then signs a
//! `BlockHashReport` that anyone relays to `EvmBlockHashOracle.submitBlockHash`.
//! The contract re-validates the floors (committee quorum, confirmations, …).
//!
//! This module provides the report type + the signed digest, locked field-for-
//! field to the Solidity `BlockHashReportLib`. The source-RPC quorum fetch,
//! signing, HTTP router, and one-shot signer registration build on top.

use alloy::primitives::{keccak256, Address, B256, U256};
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::Signer;
use anyhow::Result;

pub mod proxy;
pub mod register;
pub mod server;
pub mod source;
pub mod ssrf;

/// Static, operator-supplied runtime config (the rest is read from the contract).
#[derive(Debug, Clone)]
pub struct Settings {
    pub sapphire_chain_id: u64,
    /// The deployed `BlockHashSignerRegistry` — the one contract we register our
    /// signer + onion on. Per-chain oracles are read per-request, not configured.
    pub registry: Address,
    /// Cap on RPC-committee fan-out (we take the first N of the on-chain list).
    pub max_source_rpcs: usize,
    /// Per-Tor-circuit request rate (each circuit gets its own token bucket).
    pub rate_limit_per_sec: u32,
    /// Aggregate ceiling across all circuits (backstop against many circuits).
    pub global_rate_limit_per_sec: u32,
    pub report_ttl_secs: u64,
    pub allow_signer_rotation: bool,
}

/// One permissionless per-chain oracle's config, read on demand from its contract
/// and cached. The RPC committee is SSRF-checked and capped before use.
#[derive(Debug, Clone)]
pub struct OracleInfo {
    pub source_chain_id: u64,
    pub min_confirmations: u64,
    pub mandate_finalized: bool,
    pub rpc_urls: Vec<String>,
    pub quorum: usize,
}

/// What the HTTP endpoint returns. The requester relays this to
/// `EvmBlockHashOracle.submitBlockHash` — the oracle itself never posts on chain.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SignedReport {
    /// The per-chain oracle this report is bound to — where the requester relays it.
    pub oracle: String,
    pub source_chain_id: u64,
    pub block_number: u64,
    pub block_hash: String,
    pub required_confirmations: u64,
    pub observed_confirmations: u64,
    pub quorum_tip: u64,
    pub observed_quorum: u64,
    pub require_finalized: bool,
    pub finalized_block_number: u64,
    pub expires_at: u64,
    pub signer_epoch: u64,
    pub signer: String,
    /// 0x + 65-byte r||s||v (v in {27,28}); `ecrecover(digest, v, r, s)` on-chain.
    pub signature: String,
}

/// Sign a report's digest with the TEE-derived key. Raw digest, no EIP-191 — the
/// contract recovers via `ecrecover` over the same digest.
pub async fn sign_report(
    signer: &PrivateKeySigner,
    sapphire_chain_id: u64,
    oracle: Address,
    report: &BlockHashReport,
) -> Result<[u8; 65]> {
    let digest = report_digest(sapphire_chain_id, oracle, report);
    let sig = signer.sign_hash(&digest).await?;
    Ok(sig.as_bytes())
}

/// Mirrors `BlockHashReportLib.BlockHashReport` (Solidity). The off-chain signer
/// fills these from an RPC-committee quorum; the contract re-validates the floors.
#[derive(Debug, Clone)]
pub struct BlockHashReport {
    pub source_chain_id: u64,
    pub block_number: u64,
    pub block_hash: B256,
    pub required_confirmations: u64,
    pub observed_confirmations: u64,
    pub quorum_tip: u64,
    /// Number of RPC-committee members that returned `block_hash` for `block_number`.
    pub observed_quorum: u64,
    pub require_finalized: bool,
    pub finalized_block_number: u64,
    pub expires_at: u64,
    pub signer_epoch: u64,
}

/// EIP-712-style domain separator string, matching the Solidity constant.
pub const BLOCK_HASH_DOMAIN: &[u8] = b"CROSSROADS_EVM_BLOCK_HASH_V1";

/// The signed report digest: `keccak256(abi.encode(DOMAIN, sapphireChainId,
/// oracle, …fields…))`, byte-for-byte identical to `BlockHashReportLib.digest`.
///
/// Every field is a static ABI type, so `abi.encode` is just 14 left-padded
/// 32-byte words concatenated — we build that directly (no ABI lib needed), which
/// is also exactly what `cast abi-encode` produces.
pub fn report_digest(sapphire_chain_id: u64, oracle: Address, r: &BlockHashReport) -> B256 {
    fn w(v: u64) -> [u8; 32] {
        U256::from(v).to_be_bytes::<32>()
    }
    let domain = keccak256(BLOCK_HASH_DOMAIN);
    let mut buf = Vec::with_capacity(14 * 32);
    buf.extend_from_slice(domain.as_slice()); // bytes32 DOMAIN
    buf.extend_from_slice(&w(sapphire_chain_id)); // uint256
    let mut addr_word = [0u8; 32]; // address → left-padded to 32 bytes
    addr_word[12..].copy_from_slice(oracle.as_slice());
    buf.extend_from_slice(&addr_word);
    buf.extend_from_slice(&w(r.source_chain_id));
    buf.extend_from_slice(&w(r.block_number));
    buf.extend_from_slice(r.block_hash.as_slice()); // bytes32
    buf.extend_from_slice(&w(r.required_confirmations));
    buf.extend_from_slice(&w(r.observed_confirmations));
    buf.extend_from_slice(&w(r.quorum_tip));
    buf.extend_from_slice(&w(r.observed_quorum));
    let mut bool_word = [0u8; 32]; // bool → 32-byte word, last byte 0/1
    bool_word[31] = r.require_finalized as u8;
    buf.extend_from_slice(&bool_word);
    buf.extend_from_slice(&w(r.finalized_block_number));
    buf.extend_from_slice(&w(r.expires_at));
    buf.extend_from_slice(&w(r.signer_epoch)); // uint64 → 32-byte word
    keccak256(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    // Independently reproduces the digest the Solidity library + `cast` compute
    // for the same fixed vector — the cross-implementation lock. If Rust and the
    // contract ever disagree on field order/width/domain, on-chain signature
    // recovery would fail; this catches it in CI instead. The expected value is
    // verifiable with `cast` (see EvmBlockHashOracle.t.sol).
    #[test]
    fn digest_matches_solidity_vector() {
        let oracle = Address::from_str("0x00000000000000000000000000000000DeaDBeef").unwrap();
        let r = BlockHashReport {
            source_chain_id: 11155111,
            block_number: 12345,
            block_hash: B256::repeat_byte(0x11),
            required_confirmations: 12,
            observed_confirmations: 20,
            quorum_tip: 12365,
            observed_quorum: 2,
            require_finalized: false,
            finalized_block_number: 0,
            expires_at: 2_000_000_000,
            signer_epoch: 1,
        };
        let d = report_digest(23295, oracle, &r);
        assert_eq!(
            hex::encode(d),
            "e729619105bbf2d621c1f41bd3cbe29f4c970943623585c72e13ab4930dbd796"
        );
    }
}
