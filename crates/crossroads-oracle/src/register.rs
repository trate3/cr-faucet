//! Boot-time, one-shot signer + endpoint registration on the canonical
//! `BlockHashSignerRegistry` — the ONLY contract the pool writes to for the
//! oracle, and NOT part of serving requests. The per-chain oracles are read
//! per-request by the server; the pool never writes to them. Gated by the
//! app-origin appd (`roflEnsureAuthorizedOrigin`), skipped if already ours.

use crate::server::OracleState;
use crate::Settings;
use alloy::primitives::{keccak256, Address, FixedBytes, B256, U256};
use alloy::providers::ProviderBuilder;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};
use anyhow::{bail, Context, Result};
use pool_core::appd;

sol! {
    #[sol(rpc)]
    interface IBlockHashSignerRegistry {
        function signer() external view returns (address);
        function signerEpoch() external view returns (uint64);
        function endpoint() external view returns (string memory);
        function registerSigner(address signer, bytes32 commitment) external;
        function rotateSigner(address signer, bytes32 commitment) external;
        function setEndpoint(string onion) external;
    }
}

const SIGNER_DOMAIN: &[u8] = b"CROSSROADS_EVM_BLOCK_HASH_SIGNER_V1";

/// Binds the signer to (chain, registry, app id, epoch). Stored opaquely on-chain.
fn commitment(chain_id: u64, registry: Address, app_id: [u8; 21], signer: Address, epoch: u64) -> B256 {
    let enc = (
        B256::from(keccak256(SIGNER_DOMAIN)),
        U256::from(chain_id),
        registry,
        FixedBytes::<21>::from(app_id),
        signer,
        epoch,
    )
        .abi_encode();
    keccak256(&enc)
}

/// Register the signer + publish the onion on the registry if needed, and return a
/// ready `OracleState`. `onion` is the pool's dedicated hidden-service address.
pub async fn boot(
    appd_socket: &str,
    sapphire_rpc_http: &str,
    settings: Settings,
    signer: PrivateKeySigner,
    onion: Option<String>,
) -> Result<OracleState> {
    let provider =
        ProviderBuilder::new().on_http(sapphire_rpc_http.parse().context("bad sapphire rpc url")?);
    let reg = IBlockHashSignerRegistry::new(settings.registry, &provider);

    let signer_addr = signer.address();
    let onchain_signer: Address = reg.signer().call().await?._0;
    let onchain_epoch: u64 = reg.signerEpoch().call().await?._0;

    // One-shot signer registration (the only on-chain write besides the endpoint).
    let effective_epoch = if onchain_signer == Address::ZERO {
        register(appd_socket, &settings, signer_addr, onchain_epoch + 1, false).await?
    } else if onchain_signer == signer_addr {
        onchain_epoch // already ours — skip
    } else if settings.allow_signer_rotation {
        register(appd_socket, &settings, signer_addr, onchain_epoch + 1, true).await?
    } else {
        bail!("registry signer {onchain_signer} != ours {signer_addr}; set allow_signer_rotation to rotate");
    };

    // Publish the serving onion, write-if-changed.
    if let Some(onion) = onion {
        let current: String = reg.endpoint().call().await?._0;
        if current != onion {
            let data = IBlockHashSignerRegistry::setEndpointCall { onion }.abi_encode();
            appd::sign_submit_eth(appd_socket, settings.registry.into_array(), &data, 200_000)
                .await
                .context("setEndpoint via appd")?;
        }
    }

    Ok(OracleState::new(settings, signer, effective_epoch, sapphire_rpc_http.to_string()))
}

async fn register(
    appd_socket: &str,
    settings: &Settings,
    signer_addr: Address,
    epoch: u64,
    rotate: bool,
) -> Result<u64> {
    let app_id = appd::app_id_bytes(appd_socket).await.context("reading rofl app id")?;
    let com = commitment(settings.sapphire_chain_id, settings.registry, app_id, signer_addr, epoch);
    let data = if rotate {
        IBlockHashSignerRegistry::rotateSignerCall { signer: signer_addr, commitment: com }.abi_encode()
    } else {
        IBlockHashSignerRegistry::registerSignerCall { signer: signer_addr, commitment: com }.abi_encode()
    };
    appd::sign_submit_eth(appd_socket, settings.registry.into_array(), &data, 300_000)
        .await
        .context("signer registration via appd")?;
    Ok(epoch)
}
