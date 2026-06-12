//! One-shot, on-chain advertisement of the pool's miner-facing endpoints to the
//! `PoolEndpointRegistry` contract.
//!
//! Reads the current entry first and writes ONLY when it's missing or differs
//! from what the enclave derives (onion + stratum TLS fingerprint are both
//! KMS-derived and stable across redeploys, so steady state is a no-op — no
//! transaction, no gas). The write is submitted via `rofl-appd` app-origin, so
//! the contract's `roflEnsureAuthorizedOrigin(APP_ID)` gate passes and the funded
//! app account pays gas (no separately-seeded EOA).

use alloy::primitives::{Address, FixedBytes};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{Context, Result};
use tracing::info;

sol! {
    interface IPoolEndpointRegistry {
        function endpoints() external view returns (string onion, bytes32 tlsFingerprint, uint64 updatedAt);
        function setEndpoints(string onion, bytes32 tlsFingerprint) external;
    }
}

/// Publish `(onion, tls_fingerprint)` to the registry iff the on-chain values are
/// missing or stale. `tls_fingerprint_hex` is the lowercase SHA-256 hex of the
/// stratum cert (as logged at boot). No-op (returns Ok) when already current.
pub async fn advertise(
    http_url: &str,
    appd_socket: &str,
    registry_addr: &str,
    onion: &str,
    tls_fingerprint_hex: &str,
) -> Result<()> {
    let registry: Address = registry_addr.parse().context("registry address")?;
    let fp = FixedBytes::<32>::from(parse_fingerprint(tls_fingerprint_hex)?);

    let provider = ProviderBuilder::new().on_http(http_url.parse().context("l2 http url")?);

    // Read current on-chain endpoints (free eth_call) for the "write only if
    // necessary" check.
    let req = TransactionRequest::default()
        .to(registry)
        .input(IPoolEndpointRegistry::endpointsCall {}.abi_encode().into());
    let res = provider
        .call(&req)
        .block(alloy::eips::BlockId::latest())
        .await
        .context("eth_call endpoints()")?;
    let cur = IPoolEndpointRegistry::endpointsCall::abi_decode_returns(&res, true)
        .context("decode endpoints()")?;

    let needs_update =
        cur.updatedAt == 0 || cur.onion.as_str() != onion || cur.tlsFingerprint != fp;
    if !needs_update {
        info!(registry = %registry, "on-chain endpoints already current; skipping (no transaction)");
        return Ok(());
    }

    let data = IPoolEndpointRegistry::setEndpointsCall {
        onion: onion.to_string(),
        tlsFingerprint: fp,
    }
    .abi_encode();
    let txh = pool_core::appd::sign_submit_eth(appd_socket, registry.into_array(), &data, 250_000)
        .await
        .context("submit setEndpoints via rofl-appd")?;
    info!(
        registry = %registry,
        tx = %txh,
        onion = %onion,
        tls_fingerprint = %tls_fingerprint_hex,
        reason = if cur.updatedAt == 0 { "unset" } else { "stale" },
        "advertised pool endpoints on-chain"
    );
    Ok(())
}

fn parse_fingerprint(hex_str: &str) -> Result<[u8; 32]> {
    let h = hex_str.trim().trim_start_matches("0x");
    let bytes = hex::decode(h).context("tls fingerprint is not valid hex")?;
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("tls fingerprint is {} bytes, expected 32", bytes.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_64_hex() {
        let hex = "414ec220dcc261e8ecfe68f105e8037816115fdeebfbc672b5f856d06ded140d";
        let a = parse_fingerprint(hex).unwrap();
        assert_eq!(a[0], 0x41);
        assert_eq!(a[31], 0x0d);
        // 0x-prefixed and whitespace tolerated.
        assert_eq!(parse_fingerprint(&format!("  0x{hex}\n")).unwrap(), a);
    }

    #[test]
    fn rejects_bad_length_and_nonhex() {
        assert!(parse_fingerprint("deadbeef").is_err()); // too short
        assert!(parse_fingerprint("zz").is_err()); // not hex
    }
}
