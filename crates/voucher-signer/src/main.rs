//! Standalone voucher-signer binary. In the TEE you'd use the unified
//! `mining-pool` binary instead; this one stays as an ops convenience for
//! running just the signer against an existing Redis + L2 RPC.

use alloy::primitives::Address;
use alloy::providers::ProviderBuilder;
use alloy::signers::local::PrivateKeySigner;
use anyhow::Result;
use pool_core::store::Store;
use pool_core::Config;
use std::{env, str::FromStr, sync::Arc};
use tracing::info;
use voucher_signer::{router, AlloyClaimed, Service};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cfg = Config::load(env::var("POOL_CONFIG").unwrap_or_else(|_| "pool.toml".into()))?;
    let store = Store::connect(&cfg.redis.url).await?;
    let key = std::fs::read_to_string(&cfg.l2.signer_key_path)?;
    let signer = PrivateKeySigner::from_str(key.trim())?;
    info!(signer=%signer.address(), "voucher-signer using key");

    let rpc_url = cfg.l2.rpc_ws.replace("wss://", "https://").replace("ws://", "http://");
    let provider = ProviderBuilder::new().on_http(rpc_url.parse()?);

    let claimed_reader = AlloyClaimed {
        provider,
        mining_pool_token: Address::from_str(&cfg.l2.mining_pool_token_address)?,
    };

    let svc = Arc::new(Service {
        store,
        signer,
        chain_id: cfg.l2.chain_id,
        mining_pool_token: Address::from_str(&cfg.l2.mining_pool_token_address)?,
        claimed_reader,
        voucher_ttl_secs: 3600,
    });

    let bind = "0.0.0.0:8081";
    info!(bind, "voucher-signer listening");
    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, router(svc)).await?;
    Ok(())
}
