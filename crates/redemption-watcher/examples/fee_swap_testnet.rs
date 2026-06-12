//! Drives the real FeeSwapTask::tick() once against live testnet contracts, with
//! a controlled treasury snapshot in local Redis. Validates the autonomous
//! fee→ROSE loop end-to-end (necessary check → reserve-safe mint → DEX swap →
//! reservoir) without needing a full ROFL deploy.
//!
//! Env: DEPLOYER_PK, RPC_URL, TOKEN, FEE_SWAPPER, APPD_SOCKET (signer must be the
//! token's authorizedSigner AND the FeeSwapper operator — the deployer satisfies
//! both right after deploy). The claim + swap submit as app-origin evm.Calls
//! through APPD_SOCKET (default /run/rofl-appd.sock), so a reachable appd is
//! required for the actual on-chain step.

use std::str::FromStr;

use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use pool_core::config::FeeSwapConfig;
use pool_core::store::{Store, TreasurySnapshot};
use redemption_watcher::fee_swap::FeeSwapTask;

fn main() -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(run())
}

async fn run() -> anyhow::Result<()> {
    let rpc = std::env::var("RPC_URL")?;
    let key = std::env::var("DEPLOYER_PK")?;
    let token = Address::from_str(&std::env::var("TOKEN")?)?;
    let fee_swapper = Address::from_str(&std::env::var("FEE_SWAPPER")?)?;
    let appd_socket =
        std::env::var("APPD_SOCKET").unwrap_or_else(|_| "/run/rofl-appd.sock".to_string());

    let signer = PrivateKeySigner::from_str(key.trim())?;
    let read_provider = ProviderBuilder::new().on_http(rpc.parse()?);
    let store = Store::connect("redis://127.0.0.1:6379").await?;

    // Inject a controlled treasury snapshot: plenty of unlocked backing, small
    // outstanding supply → a real surplus the loop can safely realize as fee.
    store
        .set_treasury_snapshot(&TreasurySnapshot {
            monero_balance_atomic: 10_000_000_000,
            monero_unlocked_atomic: 10_000_000_000, // 0.01 XMR backing
            pending_redemptions_atomic: 0,
            pending_redemptions_count: 0,
            mining_pool_token_total_supply: 1_000_000_000, // 0.001 XMR outstanding
            as_of_unix: 0,
        })
        .await?;

    let cfg = FeeSwapConfig {
        enabled: true,
        fee_swapper_address: format!("{fee_swapper:#x}"),
        rent_floor_wei: "5000000000000000000".into(), // 5 ROSE: reservoir is below → necessary
        rent_target_wei: "50000000000000000000".into(),
        min_swap_atomic: 100_000_000,                 // 0.0001 XMR
        max_swap_atomic: 2_000_000_000,               // 0.002 XMR cap this sweep
        slippage_bps: 300,
        check_interval_secs: 0,
        jitter_secs: 0,
        min_swap_gas_multiple: 0, // example: don't gate on gas
    };

    let task = FeeSwapTask {
        read_provider: read_provider.clone(),
        appd_socket,
        signer,
        chain_id: 23295,
        token,
        fee_swapper,
        reserve_ratio: 1.05,
        store,
        cfg,
    };

    // Observe the reservoir balance across the swap.
    let reservoir = task_reservoir(&read_provider, fee_swapper).await?;
    let before = read_provider.get_balance(reservoir).await?;
    println!("reservoir {reservoir:#x} balance before: {before}");

    match task.tick().await? {
        Some((mpt_in, rose_min)) => {
            let after = read_provider.get_balance(reservoir).await?;
            println!("tick swapped: mpt_in={mpt_in} rose_min={rose_min}");
            println!("reservoir balance after:  {after}");
            println!("reservoir delta:          {}", after - before);
            anyhow::ensure!(after > before, "reservoir balance did not increase");
            println!("✅ autonomous fee→ROSE loop succeeded on testnet");
        }
        None => println!("tick: no swap (not necessary / no surplus / no liquidity)"),
    }
    Ok(())
}

async fn task_reservoir(
    provider: &alloy::providers::RootProvider<
        alloy::transports::http::Http<alloy::transports::http::Client>,
    >,
    fee_swapper: Address,
) -> anyhow::Result<Address> {
    use alloy::sol;
    use alloy::sol_types::SolCall;
    sol! { interface IFS { function reservoir() external view returns (address); } }
    let req = alloy::rpc::types::TransactionRequest::default()
        .to(fee_swapper)
        .input(IFS::reservoirCall {}.abi_encode().into());
    let res = provider
        .call(&req)
        .block(alloy::eips::BlockId::latest())
        .await?;
    Ok(IFS::reservoirCall::abi_decode_returns(&res, true)?._0)
}
