//! Integration tests for the voucher signer's `Service`. Requires Redis at
//! `$VOUCHER_TEST_REDIS_URL`; skipped (and passes) otherwise.

use alloy::primitives::{Address, U256};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::{eip712_domain, SolStruct};
use pool_core::store::Store;
use voucher_signer::{Service, StubClaimedReader};

sol! {
    struct Voucher {
        address user;
        uint256 cumulativeAmount;
        uint256 signedAt;
    }
}

async fn store() -> Option<Store> {
    let url = std::env::var("VOUCHER_TEST_REDIS_URL").ok()?;
    Some(Store::connect(&url).await.expect("redis connect"))
}

fn make_service(store: Store, reader: StubClaimedReader) -> Service<StubClaimedReader> {
    let signer = PrivateKeySigner::random();
    Service {
        store,
        signer,
        chain_id: 31337,
        mining_pool_token: Address::repeat_byte(0x42),
        claimed_reader: reader,
        voucher_ttl_secs: 3600,
    }
}

async fn seed(store: &Store, user: Address, earned: i64) {
    store.add_earned(user, earned).await.unwrap();
}

fn fresh_user() -> Address {
    PrivateKeySigner::random().address()
}

#[tokio::test]
async fn stacked_unclaimed_vouchers_compose_marginals() {
    let Some(store) = store().await else { return };
    let svc = make_service(store.clone(), StubClaimedReader::default());
    let user = fresh_user();
    seed(&store, user, 1_000).await;

    let v1 = svc.issue(user, Some(100)).await.unwrap();
    assert_eq!(v1.cumulative_amount, "100");
    assert_eq!(v1.marginal, 100);

    let v2 = svc.issue(user, Some(200)).await.unwrap();
    assert_eq!(v2.cumulative_amount, "300", "v2 stacks above v1");
    assert_eq!(v2.marginal, 200);

    let v3 = svc.issue(user, None).await.unwrap();
    assert_eq!(v3.cumulative_amount, "1000");
    assert_eq!(v3.marginal, 700);

    assert_eq!(v1.marginal + v2.marginal + v3.marginal, 1_000);
}

#[tokio::test]
async fn voucher_after_on_chain_claim_reset() {
    let Some(store) = store().await else { return };
    let svc = make_service(store.clone(), StubClaimedReader::default());
    let user = fresh_user();
    seed(&store, user, 1_000).await;

    let v1 = svc.issue(user, Some(400)).await.unwrap();
    assert_eq!(v1.cumulative_amount, "400");

    svc.claimed_reader.set(user, U256::from(400u64));

    let v2 = svc.issue(user, Some(300)).await.unwrap();
    assert_eq!(v2.cumulative_amount, "700");
    assert_eq!(v2.marginal, 300);
}

#[tokio::test]
async fn voucher_after_signer_state_loss_uses_on_chain() {
    // Simulate: signer Redis was wiped, so last_voucher_cumulative=0 for user.
    // On-chain claimed=600. Base must respect on-chain.
    let Some(store) = store().await else { return };
    let svc = make_service(store.clone(), StubClaimedReader::default());
    let user = fresh_user();
    seed(&store, user, 1_000).await;
    svc.claimed_reader.set(user, U256::from(600u64));

    let v = svc.issue(user, Some(200)).await.unwrap();
    assert_eq!(v.cumulative_amount, "800");
}

#[tokio::test]
async fn rejects_overdrawn_request() {
    let Some(store) = store().await else { return };
    let svc = make_service(store.clone(), StubClaimedReader::default());
    let user = fresh_user();
    seed(&store, user, 100).await;
    let err = svc.issue(user, Some(101)).await.unwrap_err();
    assert!(format!("{err}").contains("available"));
}

#[tokio::test]
async fn signed_voucher_recovers_to_signer_address() {
    let Some(store) = store().await else { return };
    let svc = make_service(store.clone(), StubClaimedReader::default());
    let user = fresh_user();
    seed(&store, user, 1_000).await;

    let v = svc.issue(user, Some(500)).await.unwrap();
    let inner = Voucher {
        user,
        cumulativeAmount: U256::from(500u64),
        signedAt: U256::from(v.signed_at),
    };
    let domain = eip712_domain! {
        name: "MiningPoolToken",
        version: "1",
        chain_id: 31337,
        verifying_contract: svc.mining_pool_token,
    };
    let digest = inner.eip712_signing_hash(&domain);
    let sig_bytes = hex::decode(v.signature.trim_start_matches("0x")).unwrap();
    let sig = alloy::primitives::PrimitiveSignature::try_from(sig_bytes.as_slice()).unwrap();
    let recovered = sig.recover_address_from_prehash(&digest).unwrap();
    assert_eq!(recovered, svc.signer.address());
}

/// Build a voucher signed by `svc`'s own signer and return its hex signature.
async fn sign_voucher<R: voucher_signer::ClaimedReader>(
    svc: &Service<R>,
    user: Address,
    cumulative: u64,
    signed_at: u64,
) -> String {
    use alloy::signers::Signer;
    let v = Voucher {
        user,
        cumulativeAmount: U256::from(cumulative),
        signedAt: U256::from(signed_at),
    };
    let domain = eip712_domain! {
        name: "MiningPoolToken",
        version: "1",
        chain_id: 31337,
        verifying_contract: svc.mining_pool_token,
    };
    let digest = v.eip712_signing_hash(&domain);
    let sig = svc.signer.sign_hash(&digest).await.unwrap();
    format!("0x{}", hex::encode(sig.as_bytes()))
}

#[tokio::test]
async fn restore_from_voucher_raises_credit_and_is_idempotent() {
    let Some(store) = store().await else { return };
    let svc = make_service(store.clone(), StubClaimedReader::default());
    let user = fresh_user();
    // Simulate a wipe: user has no credit in the store.
    assert_eq!(store.earned(user).await.unwrap(), 0);

    let sig = sign_voucher(&svc, user, 750, 1_700_000_000).await;
    let out = svc.restore(user, 750, 1_700_000_000, &sig).await.unwrap();
    assert_eq!(out.earned_cumulative, 750);
    assert_eq!(store.earned(user).await.unwrap(), 750);
    assert_eq!(store.last_voucher_cumulative(user).await.unwrap(), 750);

    // Replaying the same voucher is idempotent (monotonic max).
    svc.restore(user, 750, 1_700_000_000, &sig).await.unwrap();
    assert_eq!(store.earned(user).await.unwrap(), 750);

    // An older/lower voucher never lowers credit.
    let lower = sign_voucher(&svc, user, 500, 1_600_000_000).await;
    svc.restore(user, 500, 1_600_000_000, &lower).await.unwrap();
    assert_eq!(store.earned(user).await.unwrap(), 750, "max-merge never lowers");
}

#[tokio::test]
async fn restore_rejects_foreign_signer() {
    let Some(store) = store().await else { return };
    let svc = make_service(store.clone(), StubClaimedReader::default());
    let other = make_service(store.clone(), StubClaimedReader::default()); // different random signer
    let user = fresh_user();

    // Voucher signed by a DIFFERENT pool's signer must be rejected.
    let foreign_sig = sign_voucher(&other, user, 750, 1_700_000_000).await;
    let err = svc
        .restore(user, 750, 1_700_000_000, &foreign_sig)
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("not signed by this pool"));
    assert_eq!(store.earned(user).await.unwrap(), 0, "rejected restore changes nothing");
}

#[tokio::test]
async fn failed_request_does_not_advance_state() {
    let Some(store) = store().await else { return };
    let svc = make_service(store.clone(), StubClaimedReader::default());
    let user = fresh_user();
    seed(&store, user, 500).await;

    assert!(svc.issue(user, Some(1_000)).await.is_err());
    let v = svc.issue(user, Some(100)).await.unwrap();
    assert_eq!(v.cumulative_amount, "100");
}

#[tokio::test]
async fn concurrent_voucher_requests_for_same_user_serialize() {
    // Two concurrent requests for the same user should not both succeed in a
    // way that exceeds earned. The per-user lock serializes them.
    let Some(store) = store().await else { return };
    let svc = std::sync::Arc::new(make_service(store.clone(), StubClaimedReader::default()));
    let user = fresh_user();
    seed(&store, user, 100).await;

    let svc1 = svc.clone();
    let svc2 = svc.clone();
    let f1 = tokio::spawn(async move { svc1.issue(user, Some(80)).await });
    let f2 = tokio::spawn(async move { svc2.issue(user, Some(80)).await });
    let r1 = f1.await.unwrap();
    let r2 = f2.await.unwrap();
    // One succeeds with cum=80, the other is rejected (only 20 available
    // afterwards but it requested 80).
    let ok = [&r1, &r2].iter().filter(|r| r.is_ok()).count();
    let err = [&r1, &r2].iter().filter(|r| r.is_err()).count();
    assert_eq!(ok, 1, "exactly one should succeed: {r1:?} {r2:?}");
    assert_eq!(err, 1);
}
