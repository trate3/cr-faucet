//! On-chain redemption-processed marker, backed by
//! `MiningPoolToken.processed` / `.markProcessed`.
//!
//! This is the durable half of the anti-double-pay guard. The pool's
//! per-redemption processing state otherwise lives only in its ROFL
//! local disk (Redis AOF), which Oasis treats as a single-machine cache
//! that can be silently re-formatted on a provider switch. Without an
//! off-disk record, a wipe would make the id-poller re-enqueue and
//! re-pay every already-settled redemption.
//!
//! Reads use `eth_call` (pinned to `latest` — Sapphire rejects calls
//! that omit the block tag). Writes send a real transaction signed by
//! the enclave's KMS-derived key, which is also the contract's
//! `authorizedSigner`, so `markProcessed`'s auth check passes. The
//! signer's L2 account is funded by the `msg.value` forwarded from each
//! `redeem` (see `MiningPoolToken.redeem`).

use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder, RootProvider};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};
use alloy::transports::http::{Client as HttpTransport, Http};
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::time::Duration;

use crate::payouts::RedemptionMarker;

sol! {
    interface IMiningPoolToken {
        function processed(uint256 id) external view returns (bool);
        function restoreHeight() external view returns (uint256);
        function markProcessed(uint256 id, string moneroTxid, uint256 newRestoreHeight) external;
    }
}

/// Reads via a plain RPC provider; writes via a wallet-bearing provider
/// signed with the KMS key. Both target the same `MiningPoolToken`.
pub struct AlloyMarker {
    /// Read-only provider for `processed(id)` eth_calls.
    pub read_provider: RootProvider<Http<HttpTransport>>,
    /// Wallet-bearing provider for sending `markProcessed` txs. Built from
    /// the same KMS `PrivateKeySigner` used for vouchers (= authorizedSigner).
    pub signer: PrivateKeySigner,
    pub rpc_url: String,
    pub contract: Address,
}

impl AlloyMarker {
    pub fn new(rpc_url: String, contract: Address, signer: PrivateKeySigner) -> Result<Self> {
        let read_provider =
            ProviderBuilder::new().on_http(rpc_url.parse().context("invalid l2 rpc url")?);
        Ok(Self {
            read_provider,
            signer,
            rpc_url,
            contract,
        })
    }
}

#[async_trait]
impl RedemptionMarker for AlloyMarker {
    async fn is_processed(&self, id: u64) -> Result<bool> {
        let call = IMiningPoolToken::processedCall { id: U256::from(id) };
        let req = TransactionRequest::default()
            .to(self.contract)
            .input(call.abi_encode().into());
        let res = self
            .read_provider
            .call(&req)
            .block(alloy::eips::BlockId::latest())
            .await
            .context("eth_call processed(id)")?;
        let v = bool::abi_decode(&res, true).context("decode processed(id)")?;
        Ok(v)
    }

    async fn restore_height(&self) -> Result<u64> {
        let call = IMiningPoolToken::restoreHeightCall {};
        let req = TransactionRequest::default()
            .to(self.contract)
            .input(call.abi_encode().into());
        let res = self
            .read_provider
            .call(&req)
            .block(alloy::eips::BlockId::latest())
            .await
            .context("eth_call restoreHeight()")?;
        let v = U256::abi_decode(&res, true).context("decode restoreHeight()")?;
        Ok(v.try_into().unwrap_or(u64::MAX))
    }

    async fn mark_processed(&self, id: u64, txid: &str, restore_height: u64) -> Result<()> {
        // Build a wallet-bearing provider per call. Cheap relative to the
        // network round-trips, and keeps `AlloyMarker` Clone-free.
        let wallet = EthereumWallet::from(self.signer.clone());
        let provider = ProviderBuilder::new()
            .with_recommended_fillers()
            .wallet(wallet)
            .on_http(self.rpc_url.parse().context("invalid l2 rpc url")?);

        // Sapphire's Web3 gateway rejects the EIP-1559 fee/gas estimation that
        // alloy's gas filler would otherwise perform (eth_feeHistory + an
        // estimate that also trips Sapphire's block-tag requirement). Pin an
        // explicit LEGACY gas price + limit so the tx is type-0 and no
        // estimation runs — the exact shape `cast send` uses successfully on
        // Sapphire. Nonce + chain id are still auto-filled (plain calls that
        // Sapphire accepts).
        let gas_price = self
            .read_provider
            .get_gas_price()
            .await
            .context("eth_gasPrice")?;

        let call = IMiningPoolToken::markProcessedCall {
            id: U256::from(id),
            moneroTxid: txid.to_string(),
            newRestoreHeight: U256::from(restore_height),
        };
        let req = TransactionRequest::default()
            .to(self.contract)
            .input(call.abi_encode().into())
            .with_gas_price(gas_price)
            .with_gas_limit(300_000);

        // Submit and wait (bounded) for inclusion so a failed tx surfaces
        // here rather than silently. The caller already paid the XMR, so a
        // failure is logged, not fatal.
        let pending = provider
            .send_transaction(req)
            .await
            .context("send markProcessed tx")?;
        let receipt = tokio::time::timeout(Duration::from_secs(30), pending.get_receipt())
            .await
            .context("timed out waiting for markProcessed receipt")?
            .context("markProcessed receipt")?;
        if !receipt.status() {
            anyhow::bail!("markProcessed tx reverted: {:?}", receipt.transaction_hash);
        }
        Ok(())
    }
}
