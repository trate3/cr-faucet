//! End-to-end test for the redemption flow:
//!   1. Spawn Anvil.
//!   2. Deploy MiningPoolToken with a known signer.
//!   3. Issue a voucher off-chain (matching the on-chain EIP-712 domain).
//!   4. User calls claim() → mints MiningPoolToken to user.
//!   5. User calls redeem(amount, xmrAddress) → emits Redemption event.
//!   6. Run EventPoller.tick() → it should pick up the event and enqueue.
//!   7. Run Payouts.drain_once() against a fake wallet-rpc → transfer call
//!      is observed; Redis state moves to "sent" with the fake txid.
//!
//! Skipped (passes) when ANVIL_TEST_REDIS_URL is unset.

use alloy::dyn_abi::{DynSolValue, JsonAbiExt};
use alloy::json_abi::JsonAbi;
use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{Bytes, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::Signer;
use alloy::sol;
use alloy::sol_types::{eip712_domain, SolStruct};
use anyhow::Result;
use axum::{extract::State, routing::post, Json, Router};
use pool_core::store::Store;
use redemption_watcher::events::EventPoller;
use redemption_watcher::payouts::Payouts;
use serde_json::{json, Value};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

sol! {
    struct Voucher {
        address user;
        uint256 cumulativeAmount;
        uint256 signedAt;
    }
}

// ----------------- helpers -----------------

fn redis_url() -> Option<String> {
    std::env::var("ANVIL_TEST_REDIS_URL").ok()
}

struct Anvil {
    child: Child,
    pub rpc: String,
}

impl Drop for Anvil {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

async fn spawn_anvil() -> Result<Anvil> {
    let mut child = Command::new("anvil")
        .args(["--port", "0", "--host", "127.0.0.1", "--block-time", "1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout).lines();
    let mut port = None;
    while let Some(line) = tokio::time::timeout(Duration::from_secs(20), reader.next_line()).await?? {
        if let Some(rest) = line.strip_prefix("Listening on 127.0.0.1:") {
            port = Some(rest.trim().parse::<u16>()?);
            break;
        }
    }
    let port = port.ok_or_else(|| anyhow::anyhow!("anvil didn't report listening port"))?;
    // Drain remaining stdout in the background.
    tokio::spawn(async move { while reader.next_line().await.unwrap_or(None).is_some() {} });
    Ok(Anvil {
        child,
        rpc: format!("http://127.0.0.1:{port}"),
    })
}

/// Read forge-built MiningPoolToken bytecode + ABI.
fn mining_pool_token_artifact() -> Result<(Vec<u8>, JsonAbi)> {
    let raw = std::fs::read_to_string("../../contracts/out/MiningPoolToken.sol/MiningPoolToken.json")?;
    let v: Value = serde_json::from_str(&raw)?;
    let bytecode_hex = v["bytecode"]["object"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("no bytecode in artifact"))?;
    let bytecode = hex::decode(bytecode_hex.trim_start_matches("0x"))?;
    let abi: JsonAbi = serde_json::from_value(v["abi"].clone())?;
    Ok((bytecode, abi))
}

// ----------------- fake monero-wallet-rpc -----------------

#[derive(Clone, Default)]
struct WalletState {
    calls: Arc<parking_lot::Mutex<Vec<Value>>>,
}

async fn fake_wallet_rpc(port_tx: tokio::sync::oneshot::Sender<u16>) -> WalletState {
    let state = WalletState::default();
    let s2 = state.clone();
    let app = Router::new()
        .route(
            "/json_rpc",
            post(|State(s): State<WalletState>, Json(body): Json<Value>| async move {
                s.calls.lock().push(body.clone());
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": body["id"].clone(),
                    "result": { "tx_hash": "fake-txid-deadbeef" }
                }))
            }),
        )
        .with_state(s2);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    port_tx.send(port).ok();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    state
}

// ----------------- the test -----------------

#[tokio::test]
async fn burn_to_xmr_end_to_end() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let Some(redis_url) = redis_url() else {
        eprintln!("ANVIL_TEST_REDIS_URL not set, skipping");
        return Ok(());
    };

    // 1. Anvil.
    let anvil = spawn_anvil().await?;
    eprintln!("anvil at {}", anvil.rpc);

    // 2. Fake wallet-rpc.
    let (port_tx, port_rx) = tokio::sync::oneshot::channel();
    let wallet = fake_wallet_rpc(port_tx).await;
    let wallet_port = port_rx.await?;
    let wallet_rpc = format!("http://127.0.0.1:{wallet_port}/json_rpc");

    // 3. Deploy MiningPoolToken via alloy.
    let pool_signer = PrivateKeySigner::random();
    let deployer_key: PrivateKeySigner =
        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".parse()?;
    let deployer_addr = deployer_key.address();
    let provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(EthereumWallet::from(deployer_key.clone()))
        .on_http(anvil.rpc.parse()?);
    let (bytecode, abi) = mining_pool_token_artifact()?;
    // Constructor: (address signer_, uint256 redemptionGasSubsidy_, address uniswapRouter).
    // Router = 0 → no pool created (this test only exercises claim/redeem).
    let ctor = abi
        .constructor
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no constructor"))?;
    let encoded_args = ctor.abi_encode_input(&[
        DynSolValue::Address(pool_signer.address()),
        DynSolValue::Uint(alloy::primitives::U256::ZERO, 256),
        DynSolValue::Address(alloy::primitives::Address::ZERO),
    ])?;
    let mut deploy_data = bytecode.clone();
    deploy_data.extend_from_slice(&encoded_args);
    let tx = TransactionRequest::default()
        .with_from(deployer_addr)
        .with_deploy_code(deploy_data);
    let pending = provider.send_transaction(tx).await?;
    let receipt = pending.get_receipt().await?;
    let mining_pool_token_addr = receipt
        .contract_address
        .ok_or_else(|| anyhow::anyhow!("no contract address on receipt"))?;
    eprintln!("MiningPoolToken deployed at {mining_pool_token_addr}");

    // 4. Off-chain voucher for the deployer (= our test miner).
    let miner = deployer_addr;
    let cumulative = U256::from(1_000_000u64);
    let signed_at = U256::from(1_700_000_000u64);
    let voucher = Voucher {
        user: miner,
        cumulativeAmount: cumulative,
        signedAt: signed_at,
    };
    let chain_id = provider.get_chain_id().await?;
    let domain = eip712_domain! {
        name: "MiningPoolToken",
        version: "1",
        chain_id: chain_id,
        verifying_contract: mining_pool_token_addr,
    };
    let digest = voucher.eip712_signing_hash(&domain);
    let sig = pool_signer.sign_hash(&digest).await?;
    let sig_bytes = sig.as_bytes();

    // 5. Call claim(user, cum, signedAt, sig).
    let claim_fn = abi
        .function("claim")
        .and_then(|fs| fs.first())
        .ok_or_else(|| anyhow::anyhow!("no claim()"))?;
    let claim_data = claim_fn.abi_encode_input(&[
        DynSolValue::Address(miner),
        DynSolValue::Uint(cumulative, 256),
        DynSolValue::Uint(signed_at, 256),
        DynSolValue::Bytes(sig_bytes.to_vec()),
    ])?;
    let tx = TransactionRequest::default()
        .with_from(deployer_addr)
        .with_to(mining_pool_token_addr)
        .with_input(Bytes::from(claim_data));
    provider.send_transaction(tx).await?.get_receipt().await?;
    eprintln!("claim() executed");

    // 6. Call redeem(amount, xmrAddress).
    let xmr_addr = "44stagenetTestAddrXYZ...";
    let redeem_amount = U256::from(500_000u64);
    let redeem_fn = abi
        .function("redeem")
        .and_then(|fs| fs.first())
        .ok_or_else(|| anyhow::anyhow!("no redeem()"))?;
    let redeem_data = redeem_fn.abi_encode_input(&[
        DynSolValue::Uint(redeem_amount, 256),
        DynSolValue::String(xmr_addr.into()),
    ])?;
    let tx = TransactionRequest::default()
        .with_from(deployer_addr)
        .with_to(mining_pool_token_addr)
        .with_input(Bytes::from(redeem_data));
    let receipt = provider.send_transaction(tx).await?.get_receipt().await?;
    let redeem_block = receipt.block_number.unwrap_or(0);
    eprintln!("redeem() executed at block {redeem_block}");

    // 7. Run the EventPoller for one tick.
    let store = Store::connect(&redis_url).await?;
    // Wipe so a previous run can't pollute.
    {
        let mut c = store.conn();
        let _: () = redis::cmd("FLUSHDB").query_async(&mut c).await?;
    }
    let read_provider = ProviderBuilder::new().on_http(anvil.rpc.parse()?);
    let poller = EventPoller {
        provider: read_provider,
        store: store.clone(),
        mining_pool_token: mining_pool_token_addr,
        start_block: 0,
        chunk_size: 1000,
        poll_interval: Duration::from_millis(100),
    };
    // May need a couple ticks because anvil's block-time=1 means our claim and
    // redeem blocks may have arrived just before we query latest.
    let mut enqueued = 0usize;
    for _ in 0..30 {
        enqueued += poller.tick().await?;
        if enqueued > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(enqueued, 1, "expected 1 redemption event picked up");
    let state = store.redemption_state(1).await?;
    assert_eq!(state.as_deref(), Some("pending"));

    // 7.5. Seed a treasury snapshot for the pro-rata math. We want this
    // test's payout to equal the burn amount (500_000) so the existing
    // assertion holds: payout = burned × balance / (totalSupply + pending).
    // Setting balance = totalSupply + pending makes the rate exactly 1.
    let snapshot = pool_core::store::TreasurySnapshot {
        monero_balance_atomic: 1_000_000,
        monero_unlocked_atomic: 1_000_000,
        pending_redemptions_atomic: 500_000,
        pending_redemptions_count: 1,
        mining_pool_token_total_supply: 500_000,
        as_of_unix: chrono::Utc::now().timestamp(),
    };
    store.set_treasury_snapshot(&snapshot).await?;

    // 8. Run the consumer once. It should call our fake wallet-rpc.
    let payouts = Payouts::new(
        store.clone(),
        pool_core::config::MoneroConfig {
            wallet_rpc: wallet_rpc.clone(),
            wallet_rpc_user: None,
            wallet_rpc_pass: None,
            min_reserve_ratio: 1.0,
            per_tx_cap_atomic: 1_000_000_000,
            per_day_cap_atomic: 1_000_000_000_000,
            max_payout_premium_bp: u32::MAX,
            confirm_wait_secs: 0,
            network: pool_core::config::MoneroNetwork::Testnet,
            wallet_filename: "pool".into(),
            wallet_password: "".into(),
            restore_height_lookback: 720,
        },
    )
    .await?;
    let processed = payouts.drain_once().await?;
    assert_eq!(processed, 1, "consumer should process 1 entry");

    // 9. Assert the fake wallet-rpc saw the transfer call shape we expected.
    let calls = wallet.calls.lock().clone();
    assert_eq!(calls.len(), 1, "fake wallet-rpc should have one call");
    let call = &calls[0];
    assert_eq!(call["method"], "transfer");
    let dest = &call["params"]["destinations"][0];
    assert_eq!(dest["address"].as_str(), Some(xmr_addr));
    assert_eq!(
        dest["amount"].as_u64(),
        Some(500_000),
        "amount should match redeem() argument"
    );

    // 10. State should be marked sent.
    assert_eq!(
        store.redemption_state(1).await?.as_deref(),
        Some("sent")
    );

    // 11. Idempotency: running the poller again should NOT re-enqueue.
    let again = poller.tick().await?;
    assert_eq!(again, 0, "no new events on re-poll");

    Ok(())
}
