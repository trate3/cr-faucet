//! `finalize` — the last deploy step: turn a running pool PERMISSIONLESS.
//!
//! Run post-boot, once you've read the enclave's KMS signer address from the
//! pool logs. It (in order, while the deployer is still owner):
//!   1. deploys a PoolGovernance (if one isn't supplied),
//!   2. rotates the token's voucher signer to the enclave (token.setSigner). The
//!      FeeSwapper has no operator EOA — it's gated on the ROFL app origin.
//!   3. hands the Ownable surface to governance (transferOwnership),
//!   4. optionally renounces governance — after which setSigner can never be
//!      called again, so the only minter is permanently the attested enclave.
//!
//! Why a Rust tool and not a Foundry script: Sapphire encrypts contract
//! storage, so `forge script`'s fork simulation reads existing contracts'
//! state as zero and every `onlyOwner` call reverts locally. Direct txs
//! (alloy, legacy/type-0) + `eth_call` reads work fine — that's what this does.
//!
//! Config: TOML at $FINALIZE_CONFIG (default deploy/finalize.toml). Secret key
//! via $DEPLOYER_PK.

use std::str::FromStr;

use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder, RootProvider};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};
use alloy::transports::http::{Client, Http};
use anyhow::{bail, Context, Result};
use serde::Deserialize;

sol! {
    interface IOwnable {
        function owner() external view returns (address);
        function transferOwnership(address newOwner) external;
    }
    interface IToken { function setSigner(address signer) external; }
    interface IGov {
        function renounce() external;
        function renounced() external view returns (bool);
    }
}

#[derive(Deserialize)]
struct Cfg {
    rpc_url: String,
    token: String,
    kms_signer: String,
    #[serde(default)]
    fee_swapper: String, // "" or 0x0…0 to skip
    #[serde(default)]
    governance: String, // "" or 0x0…0 to deploy a fresh one
    #[serde(default)]
    governor: String,
    #[serde(default)]
    delay_secs: u64,
    #[serde(default)]
    renounce: bool,
    #[serde(default = "default_gov_artifact")]
    governance_artifact: String,
}

fn default_gov_artifact() -> String {
    "contracts/out/PoolGovernance.sol/PoolGovernance.json".into()
}

#[tokio::main]
async fn main() -> Result<()> {
    let cfg_path = std::env::var("FINALIZE_CONFIG").unwrap_or_else(|_| "deploy/finalize.toml".into());
    let cfg: Cfg = toml::from_str(&std::fs::read_to_string(&cfg_path).context("read finalize config")?)
        .context("parse finalize config")?;
    let signer = PrivateKeySigner::from_str(std::env::var("DEPLOYER_PK")?.trim())
        .context("invalid DEPLOYER_PK")?;
    let deployer = signer.address();
    let read = ProviderBuilder::new().on_http(cfg.rpc_url.parse()?);

    let token = Address::from_str(cfg.token.trim())?;
    let kms = Address::from_str(cfg.kms_signer.trim())?;
    let fee_swapper = parse_opt(&cfg.fee_swapper)?;

    // Must be the current owner to do any of this.
    let owner = owner_of(&read, token).await?;
    if owner != deployer {
        bail!("deployer {deployer} is not the token owner ({owner}); cannot finalize");
    }

    // 1. governance
    let gov = match parse_opt(&cfg.governance)? {
        Some(g) => g,
        None => {
            let governor = Address::from_str(cfg.governor.trim()).context("governor required to deploy governance")?;
            let art: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&cfg.governance_artifact).context("read governance artifact")?)?;
            let bc = art["bytecode"]["object"].as_str().context("artifact missing bytecode.object")?;
            let mut code = hex::decode(bc.trim_start_matches("0x"))?;
            code.extend_from_slice(&(governor, U256::from(cfg.delay_secs)).abi_encode());
            let rcpt = send(&read, &signer, &cfg.rpc_url, None, code, 1_500_000).await?;
            let g = rcpt.contract_address.context("no contract_address in deploy receipt")?;
            println!("PoolGovernance deployed:  {g}  (governor={governor}, delay={}s)", cfg.delay_secs);
            g
        }
    };

    // 2. rotate the token's voucher signer to the enclave (while still owner).
    // The FeeSwapper has no operator EOA anymore — it's gated on the ROFL app
    // origin (roflEnsureAuthorizedOrigin), so there's nothing to rotate there.
    send(&read, &signer, &cfg.rpc_url, Some(token), IToken::setSignerCall { signer: kms }.abi_encode(), 150_000).await?;
    println!("token.setSigner ->        {kms}");

    // 3. hand ownership to governance (deployer's last owner action).
    send(&read, &signer, &cfg.rpc_url, Some(token), IOwnable::transferOwnershipCall { newOwner: gov }.abi_encode(), 100_000).await?;
    println!("token owner ->            {gov}");
    if let Some(fs) = fee_swapper {
        send(&read, &signer, &cfg.rpc_url, Some(fs), IOwnable::transferOwnershipCall { newOwner: gov }.abi_encode(), 100_000).await?;
        println!("feeSwapper owner ->       {gov}");
    }

    // 4. optionally renounce -> permissionless.
    if cfg.renounce {
        send(&read, &signer, &cfg.rpc_url, Some(gov), IGov::renounceCall {}.abi_encode(), 100_000).await?;
        println!("governance RENOUNCED -> pool is permissionless");
    }

    // Verify end-state.
    let new_owner = owner_of(&read, token).await?;
    println!("\n== verify ==");
    println!("token.owner():     {new_owner}  (== governance: {})", new_owner == gov);
    if cfg.renounce {
        let r = read
            .call(&TransactionRequest::default().to(gov).input(IGov::renouncedCall {}.abi_encode().into()))
            .block(alloy::eips::BlockId::latest())
            .await?;
        println!("governance.renounced(): {}", bool::abi_decode(&r, true)?);
    }
    Ok(())
}

fn parse_opt(s: &str) -> Result<Option<Address>> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let a = Address::from_str(s)?;
    Ok(if a == Address::ZERO { None } else { Some(a) })
}

async fn owner_of(read: &RootProvider<Http<Client>>, token: Address) -> Result<Address> {
    let r = read
        .call(&TransactionRequest::default().to(token).input(IOwnable::ownerCall {}.abi_encode().into()))
        .block(alloy::eips::BlockId::latest())
        .await
        .context("eth_call owner()")?;
    Ok(IOwnable::ownerCall::abi_decode_returns(&r, true)?._0)
}

/// Send a LEGACY tx (Sapphire is type-0 only): explicit gas price + limit, no
/// estimation. `to = None` deploys a contract. Waits for the receipt; fails on revert.
async fn send(
    read: &RootProvider<Http<Client>>,
    signer: &PrivateKeySigner,
    rpc_url: &str,
    to: Option<Address>,
    data: Vec<u8>,
    gas_limit: u64,
) -> Result<alloy::rpc::types::TransactionReceipt> {
    let provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(EthereumWallet::from(signer.clone()))
        .on_http(rpc_url.parse()?);
    let gas_price = read.get_gas_price().await.context("eth_gasPrice")?;
    let mut req = TransactionRequest::default()
        .with_gas_price(gas_price)
        .with_gas_limit(gas_limit);
    match to {
        Some(to) => req = req.to(to).input(data.into()),
        None => req = req.with_deploy_code(data), // contract creation
    }
    let pending = provider.send_transaction(req).await.context("send tx")?;
    let rcpt = pending.get_receipt().await.context("tx receipt")?;
    if !rcpt.status() {
        bail!("tx reverted: {:?}", rcpt.transaction_hash);
    }
    Ok(rcpt)
}
