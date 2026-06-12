//! Voucher signer library. Reads/writes state via `pool_core::store::Store`
//! (Redis). Per-user `tokio::Mutex` serializes the read-modify-write inside
//! `issue` so concurrent voucher requests for the same miner can't both
//! advance `last_voucher_cumulative` past `earned`.

use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, RootProvider};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::Signer;
use alloy::sol;
use alloy::sol_types::{eip712_domain, SolCall, SolStruct};
use alloy::transports::http::{Client as HttpTransport, Http};
use anyhow::Result;
use async_trait::async_trait;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use pool_core::store::Store;
use pool_core::voucher::{decide, VoucherInputs};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::sync::Arc;

sol! {
    struct Voucher {
        address user;
        uint256 cumulativeAmount;
        uint256 signedAt;
    }

    interface IMiningPoolTokenClaimed {
        function claimed(address user) external view returns (uint256);
    }
}

/// Production `ClaimedReader` — reads `MiningPoolToken.claimed[user]` over HTTP RPC.
/// The same alloy provider can be shared with other tasks (e.g. the
/// redemption event poller) since it's cheap to clone.
pub struct AlloyClaimed {
    pub provider: RootProvider<Http<HttpTransport>>,
    pub mining_pool_token: Address,
}

#[async_trait]
impl ClaimedReader for AlloyClaimed {
    async fn read(&self, user: Address) -> Result<U256> {
        let call = IMiningPoolTokenClaimed::claimedCall { user };
        let req = TransactionRequest::default()
            .to(self.mining_pool_token)
            .input(call.abi_encode().into());
        // Sapphire requires the block-tag arg on eth_call (alloy 0.5
        // omits it when None). Pin to `latest`.
        let res = self
            .provider
            .call(&req)
            .block(alloy::eips::BlockId::latest())
            .await?;
        let d = IMiningPoolTokenClaimed::claimedCall::abi_decode_returns(&res, true)?;
        Ok(d._0)
    }
}

#[async_trait]
pub trait ClaimedReader: Send + Sync {
    async fn read(&self, user: Address) -> Result<U256>;
}

#[derive(Default)]
pub struct StubClaimedReader {
    pub fixed: parking_lot::Mutex<std::collections::HashMap<Address, U256>>,
}

impl StubClaimedReader {
    pub fn set(&self, user: Address, v: U256) {
        self.fixed.lock().insert(user, v);
    }
}

#[async_trait]
impl ClaimedReader for StubClaimedReader {
    async fn read(&self, user: Address) -> Result<U256> {
        Ok(self.fixed.lock().get(&user).copied().unwrap_or(U256::ZERO))
    }
}

pub struct Service<R: ClaimedReader> {
    pub store: Store,
    pub signer: PrivateKeySigner,
    pub chain_id: u64,
    pub mining_pool_token: Address,
    pub claimed_reader: R,
    pub voucher_ttl_secs: i64,
}

#[derive(Debug, Serialize, Clone)]
pub struct VoucherOut {
    pub user: String,
    pub cumulative_amount: String,
    pub marginal: i64,
    /// Unix time the voucher was signed. Recorded (and signed over) but not a
    /// validity deadline — see the `signedAt` note on `MiningPoolToken.claim`.
    pub signed_at: u64,
    pub signature: String,
    pub earned_cumulative: i64,
    pub last_voucher_cumulative: i64,
    pub on_chain_claimed: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct StateOut {
    pub user: String,
    pub earned_cumulative: i64,
    pub last_voucher_cumulative: i64,
    pub on_chain_claimed: String,
    pub available_to_voucher: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("internal: {0}")]
    Internal(String),
}

impl<R: ClaimedReader> Service<R> {
    /// The EIP-712 domain for our vouchers. Must match `MiningPoolToken`'s
    /// `_hashTypedDataV4` domain so signatures verify both on-chain (claim) and
    /// off-chain (restore).
    fn domain(&self) -> alloy::sol_types::Eip712Domain {
        eip712_domain! {
            name: "MiningPoolToken",
            version: "1",
            chain_id: self.chain_id,
            verifying_contract: self.mining_pool_token,
        }
    }

    pub async fn state(&self, user: Address) -> Result<StateOut, ServiceError> {
        let state = self
            .store
            .balance_state(user)
            .await
            .map_err(|e| ServiceError::Internal(e.to_string()))?;
        let claimed = self.claimed_reader.read(user).await.unwrap_or(U256::ZERO);
        let claimed_i64 = u256_to_i64_or(claimed, i64::MAX);
        let base = state.last_voucher_cumulative.max(claimed_i64);
        let available = (state.earned - base).max(0);
        Ok(StateOut {
            user: format!("{:#x}", user),
            earned_cumulative: state.earned,
            last_voucher_cumulative: state.last_voucher_cumulative,
            on_chain_claimed: claimed.to_string(),
            available_to_voucher: available,
        })
    }

    pub async fn issue(
        &self,
        user: Address,
        amount: Option<i64>,
    ) -> Result<VoucherOut, ServiceError> {
        // Serialize concurrent requests for the same user.
        let lock = self.store.user_lock(user);
        let _g = lock.lock().await;

        let state = self
            .store
            .balance_state(user)
            .await
            .map_err(|e| ServiceError::Internal(e.to_string()))?;
        let claimed = self.claimed_reader.read(user).await.unwrap_or(U256::ZERO);
        let claimed_i64 = u256_to_i64_or(claimed, i64::MAX);

        let decision = decide(VoucherInputs {
            earned_cumulative: state.earned,
            last_voucher_cumulative: state.last_voucher_cumulative,
            on_chain_claimed: claimed_i64,
            requested_amount: amount,
        })
        .map_err(|e| ServiceError::BadRequest(e.to_string()))?;

        self.store
            .set_last_voucher_cumulative(user, decision.new_cumulative)
            .await
            .map_err(|e| ServiceError::Internal(e.to_string()))?;

        let signed_at = chrono::Utc::now().timestamp().max(0) as u64;
        let v = Voucher {
            user,
            cumulativeAmount: U256::from(decision.new_cumulative as u64),
            signedAt: U256::from(signed_at),
        };
        let digest = v.eip712_signing_hash(&self.domain());
        let sig = self
            .signer
            .sign_hash(&digest)
            .await
            .map_err(|e| ServiceError::Internal(e.to_string()))?;

        Ok(VoucherOut {
            user: format!("{:#x}", user),
            cumulative_amount: decision.new_cumulative.to_string(),
            marginal: decision.marginal,
            signed_at,
            signature: format!("0x{}", hex::encode(sig.as_bytes())),
            earned_cumulative: state.earned,
            last_voucher_cumulative: decision.new_cumulative,
            on_chain_claimed: claimed.to_string(),
        })
    }

    /// Restore a miner's credited balance after a state loss, from a voucher this
    /// pool previously signed. A voucher is a self-authenticating `(user,
    /// cumulativeAmount, signedAt)` signature; we re-derive the signer and require
    /// it to equal our own KMS-derived address (so vouchers from a *different*
    /// app's signer — a different deployment — are rejected). On success the
    /// miner's `earned`/`last_voucher` are raised MONOTONICALLY to at least
    /// `cumulativeAmount` (never lowered; replaying the same voucher is
    /// idempotent). `signedAt` is intentionally NOT checked — a stale voucher is
    /// still valid proof of cumulative work. Incentive-compatible: only the miner,
    /// who *wants* their credit back, will present it.
    pub async fn restore(
        &self,
        user: Address,
        cumulative_amount: i64,
        signed_at: u64,
        signature: &str,
    ) -> Result<StateOut, ServiceError> {
        if cumulative_amount <= 0 {
            return Err(ServiceError::BadRequest(
                "cumulative_amount must be positive".into(),
            ));
        }
        let v = Voucher {
            user,
            cumulativeAmount: U256::from(cumulative_amount as u64),
            signedAt: U256::from(signed_at),
        };
        let digest = v.eip712_signing_hash(&self.domain());
        let raw = hex::decode(signature.trim().trim_start_matches("0x"))
            .map_err(|e| ServiceError::BadRequest(format!("bad signature hex: {e}")))?;
        let sig = alloy::primitives::PrimitiveSignature::try_from(raw.as_slice())
            .map_err(|e| ServiceError::BadRequest(format!("bad signature: {e}")))?;
        let recovered = sig
            .recover_address_from_prehash(&digest)
            .map_err(|e| ServiceError::BadRequest(format!("cannot recover signer: {e}")))?;
        if recovered != self.signer.address() {
            return Err(ServiceError::BadRequest(
                "voucher not signed by this pool's signer".into(),
            ));
        }
        // Serialize against concurrent issuance for the same user.
        let lock = self.store.user_lock(user);
        let _g = lock.lock().await;
        self.store
            .restore_cumulative(user, cumulative_amount)
            .await
            .map_err(|e| ServiceError::Internal(e.to_string()))?;
        self.state(user).await
    }
}

pub fn u256_to_i64_or(v: U256, fallback: i64) -> i64 {
    let limbs = v.into_limbs();
    if limbs[1] | limbs[2] | limbs[3] != 0 {
        return fallback;
    }
    let lo = limbs[0];
    if lo > i64::MAX as u64 {
        fallback
    } else {
        lo as i64
    }
}

// ----------------- HTTP router -----------------

#[derive(Deserialize)]
struct VoucherReq {
    user: String,
    #[serde(default)]
    amount: Option<i64>,
}

/// A voucher being replayed by its holder to restore lost credit. Fields mirror
/// what `POST /voucher` returned (`cumulative_amount` as a decimal string).
#[derive(Deserialize)]
struct RestoreReq {
    user: String,
    cumulative_amount: String,
    signed_at: u64,
    signature: String,
}

/// Build the voucher-signer HTTP router. Mount at any prefix; routes are
/// `GET /state/:addr`, `POST /voucher`, and `POST /restore`.
pub fn router<R: ClaimedReader + 'static>(svc: Arc<Service<R>>) -> Router {
    Router::new()
        .route("/state/:addr", get(http_state::<R>))
        .route("/voucher", post(http_issue::<R>))
        .route("/restore", post(http_restore::<R>))
        .with_state(svc)
}

async fn http_state<R: ClaimedReader + 'static>(
    State(s): State<Arc<Service<R>>>,
    Path(addr): Path<String>,
) -> std::result::Result<Json<StateOut>, (StatusCode, String)> {
    let user = Address::from_str(&addr).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    s.state(user).await.map(Json).map_err(into_http)
}

async fn http_issue<R: ClaimedReader + 'static>(
    State(s): State<Arc<Service<R>>>,
    Json(r): Json<VoucherReq>,
) -> std::result::Result<Json<VoucherOut>, (StatusCode, String)> {
    let user = Address::from_str(r.user.trim()).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    s.issue(user, r.amount).await.map(Json).map_err(into_http)
}

async fn http_restore<R: ClaimedReader + 'static>(
    State(s): State<Arc<Service<R>>>,
    Json(r): Json<RestoreReq>,
) -> std::result::Result<Json<StateOut>, (StatusCode, String)> {
    let user =
        Address::from_str(r.user.trim()).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let cumulative: i64 = r
        .cumulative_amount
        .trim()
        .parse()
        .map_err(|e: std::num::ParseIntError| {
            (StatusCode::BAD_REQUEST, format!("bad cumulative_amount: {e}"))
        })?;
    s.restore(user, cumulative, r.signed_at, &r.signature)
        .await
        .map(Json)
        .map_err(into_http)
}

fn into_http(e: ServiceError) -> (StatusCode, String) {
    match e {
        ServiceError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
        ServiceError::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
    }
}
