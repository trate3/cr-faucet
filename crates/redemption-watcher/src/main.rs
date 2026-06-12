//! Binary: runs both the L2 event poller (producer) and the wallet-rpc
//! consumer (payouts) against shared Redis state.

use alloy::primitives::Address;
use alloy::providers::ProviderBuilder;
use anyhow::Result;
use pool_core::store::Store;
use pool_core::Config;
use redemption_watcher::events::EventPoller;
use redemption_watcher::payouts::Payouts;
use redemption_watcher::treasury::{AlloySupplyReader, TreasuryRefresher};
use std::env;
use std::str::FromStr;
use std::time::Duration;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cfg = Config::load(env::var("POOL_CONFIG").unwrap_or_else(|_| "pool.toml".into()))?;
    let store = Store::connect(&cfg.redis.url).await?;

    let provider = ProviderBuilder::new().on_http(cfg.l2.http_url().parse()?);
    let mining_pool_token = Address::from_str(&cfg.l2.mining_pool_token_address)?;
    let poller = EventPoller {
        provider,
        store: store.clone(),
        mining_pool_token,
        start_block: cfg.l2.events_from_block,
        chunk_size: cfg.l2.events_chunk_size,
        poll_interval: Duration::from_secs(cfg.l2.events_poll_secs),
    };
    let payouts = Payouts::new(store.clone(), cfg.monero.clone()).await?;
    let supply_reader = AlloySupplyReader {
        provider: ProviderBuilder::new().on_http(cfg.l2.http_url().parse()?),
        mining_pool_token,
    };
    let treasury = TreasuryRefresher::new(
        store.clone(),
        cfg.monero.clone(),
        supply_reader,
        Duration::from_secs(10),
    );

    info!("redemption-watcher: producer + consumer + treasury refresher up");
    tokio::select! {
        _ = poller.run_loop() => {},
        _ = payouts.run_loop() => {},
        _ = treasury.run_loop() => {},
    }
    Ok(())
}
