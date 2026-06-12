//! Periodic refresher: pulls `get_balance` from the pool's wallet-rpc and
//! `MiningPoolToken.totalSupply()` from the L2, reads pending counters from Redis,
//! writes the combined snapshot back. The operator-api serves the snapshot in
//! O(1).

use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, RootProvider};
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use alloy::sol_types::SolCall;
use alloy::transports::http::{Client as HttpTransport, Http};
use anyhow::Result;
use async_trait::async_trait;
use pool_core::config::MoneroConfig;
use pool_core::store::{Store, TreasurySnapshot};
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;
use tracing::{debug, warn};

#[derive(Debug, Deserialize)]
struct BalanceResp {
    balance: u128,
    unlocked_balance: u128,
}

#[derive(Debug, Deserialize)]
struct Rpc<T> {
    result: T,
}

sol! {
    interface IMiningPoolToken {
        function totalSupply() external view returns (uint256);
    }
}

/// Reads `MiningPoolToken.totalSupply()` from somewhere. Trait so tests can inject
/// a fixed value without standing up Anvil.
#[async_trait]
pub trait SupplyReader: Send + Sync {
    async fn total_supply(&self) -> Result<u128>;
}

pub struct AlloySupplyReader {
    pub provider: RootProvider<Http<HttpTransport>>,
    pub mining_pool_token: Address,
}

#[async_trait]
impl SupplyReader for AlloySupplyReader {
    async fn total_supply(&self) -> Result<u128> {
        let call = IMiningPoolToken::totalSupplyCall {};
        let req = TransactionRequest::default()
            .to(self.mining_pool_token)
            .input(call.abi_encode().into());
        // Sapphire's `eth_call` rejects requests that omit the block
        // tag (alloy 0.5 leaves it `None` by default, sending only one
        // param). Explicitly pin to `latest` so the call works on
        // Sapphire as well as on chains that tolerate the missing arg.
        let res = self
            .provider
            .call(&req)
            .block(alloy::eips::BlockId::latest())
            .await?;
        let decoded = IMiningPoolToken::totalSupplyCall::abi_decode_returns(&res, true)?;
        Ok(u256_to_u128_or_max(decoded._0))
    }
}

pub struct StubSupplyReader {
    pub value: parking_lot::Mutex<u128>,
}

impl StubSupplyReader {
    pub fn new(initial: u128) -> Self {
        Self {
            value: parking_lot::Mutex::new(initial),
        }
    }
    pub fn set(&self, v: u128) {
        *self.value.lock() = v;
    }
}

#[async_trait]
impl SupplyReader for StubSupplyReader {
    async fn total_supply(&self) -> Result<u128> {
        Ok(*self.value.lock())
    }
}

pub struct TreasuryRefresher<S: SupplyReader> {
    pub store: Store,
    pub monero: MoneroConfig,
    pub supply: S,
    pub interval: Duration,
    pub client: reqwest::Client,
}

impl<S: SupplyReader> TreasuryRefresher<S> {
    pub fn new(store: Store, monero: MoneroConfig, supply: S, interval: Duration) -> Self {
        Self {
            store,
            monero,
            supply,
            interval,
            client: reqwest::Client::new(),
        }
    }

    pub async fn run_loop(&self) {
        loop {
            match self.tick().await {
                Ok(snap) => debug!(?snap, "treasury refreshed"),
                Err(e) => warn!(error=%e, "treasury refresh failed"),
            }
            tokio::time::sleep(self.interval).await;
        }
    }

    pub async fn tick(&self) -> Result<TreasurySnapshot> {
        let _ = self
            .client
            .post(&self.monero.wallet_rpc)
            .json(&json!({"jsonrpc":"2.0","id":"0","method":"refresh"}))
            .send()
            .await;

        let resp: Rpc<BalanceResp> = self
            .client
            .post(&self.monero.wallet_rpc)
            .json(&json!({
                "jsonrpc":"2.0","id":"0","method":"get_balance",
                "params":{"account_index":0}
            }))
            .send()
            .await?
            .json()
            .await?;

        let total_supply = self.supply.total_supply().await.unwrap_or_else(|e| {
            warn!(error=%e, "totalSupply read failed; defaulting to 0");
            0
        });
        let pending_atomic = self.store.pending_atomic().await?.max(0) as u128;
        let pending_count = self.store.pending_count().await?.max(0) as u64;

        let snap = TreasurySnapshot {
            monero_balance_atomic: resp.result.balance,
            monero_unlocked_atomic: resp.result.unlocked_balance,
            pending_redemptions_atomic: pending_atomic,
            pending_redemptions_count: pending_count,
            mining_pool_token_total_supply: total_supply,
            as_of_unix: chrono::Utc::now().timestamp(),
        };
        self.store.set_treasury_snapshot(&snap).await?;
        Ok(snap)
    }
}

fn u256_to_u128_or_max(v: U256) -> u128 {
    let limbs = v.into_limbs();
    if limbs[2] | limbs[3] != 0 {
        return u128::MAX;
    }
    (limbs[0] as u128) | ((limbs[1] as u128) << 64)
}
