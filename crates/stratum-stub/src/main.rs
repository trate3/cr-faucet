//! Tiny stratum endpoint that serves block templates from a local
//! `monerod` (stagenet, testnet, regtest, …) without doing any actual
//! upstream submission or share validation.
//!
//! Purpose: be the *upstream* the TEE pool's `stratum-proxy::upstream`
//! client connects to during end-to-end testing. We give it real
//! RandomX jobs (so xmrig can mine valid shares the TEE pool can
//! verify) but throw away submitted shares — we don't have a real
//! mining pool's accounting layer, and we don't need one for credit-
//! pipeline validation.
//!
//! Env:
//!   STUB_BIND        listen address (default 0.0.0.0:3333)
//!   STUB_MONEROD     monerod JSON-RPC URL (default http://monerod:38089/json_rpc)
//!   STUB_WALLET      wallet address to put in `get_block_template`
//!                    (any valid address for the daemon's network;
//!                    defaults to a stagenet burn address)

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::env;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;
use tracing::{info, warn};

/// Default wallet address handed to `get_block_template`. We never
/// actually credit the rewards anywhere — this just has to parse on the
/// daemon's network. Defaults to the Monero project donation address
/// (mainnet format), which also works for regtest. Override via the
/// `STUB_WALLET` env var when you point the stub at a stagenet or
/// testnet daemon.
const DEFAULT_BURN_ADDR: &str = "44AFFq5kSiGBoZ4NMDwYtN18obc8AemS33DBLWs3H7otXft3XjrpDtQGv7SqSsaBYBb98uNbr2VBBEt7f2wfn3RVGQBEP3A";

#[derive(Debug, Clone)]
struct Job {
    job_id: String,
    blob: String,
    seed_hash: String,
    next_seed_hash: String,
    target_hex: String,
    height: u64,
}

#[derive(Deserialize)]
struct BlockTemplate {
    blockhashing_blob: String,
    height: u64,
    seed_hash: String,
    #[serde(default)]
    next_seed_hash: String,
}

async fn fetch_template(client: &reqwest::Client, url: &str, wallet: &str) -> Result<BlockTemplate> {
    let body = json!({
        "jsonrpc":"2.0","id":"0","method":"get_block_template",
        "params":{"wallet_address": wallet, "reserve_size": 8}
    });
    let v: Value = client
        .post(url)
        .json(&body)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("send get_block_template")?
        .json()
        .await
        .context("decode get_block_template")?;
    if let Some(e) = v.get("error") {
        if !e.is_null() {
            return Err(anyhow!("monerod error: {e}"));
        }
    }
    let tmpl: BlockTemplate = serde_json::from_value(v["result"].clone())
        .context("parse get_block_template result")?;
    Ok(tmpl)
}

/// Encode a difficulty as a 4-byte little-endian target the way Monero
/// stratum expects (target = floor(2^32 / difficulty)).
fn difficulty_to_target(diff: u64) -> String {
    let target = if diff == 0 { u32::MAX } else { (u64::MAX / diff).min(u32::MAX as u64) as u32 };
    hex::encode(target.to_le_bytes())
}

fn template_to_job(t: BlockTemplate, ix: u64) -> Job {
    Job {
        job_id: format!("stub-{}-{}", t.height, ix),
        blob: t.blockhashing_blob,
        seed_hash: t.seed_hash,
        next_seed_hash: t.next_seed_hash,
        // Hand out a low-ish difficulty so a 1-thread xmrig finds shares
        // quickly. We're not actually upstream'ing them.
        target_hex: difficulty_to_target(1_000),
        height: t.height,
    }
}

async fn template_loop(state: Arc<RwLock<Option<Job>>>, url: String, wallet: String) {
    let client = reqwest::Client::new();
    let mut ix = 0u64;
    loop {
        match fetch_template(&client, &url, &wallet).await {
            Ok(t) => {
                let job = template_to_job(t, ix);
                ix += 1;
                let mut g = state.write().await;
                if g.as_ref().map(|j| j.height) != Some(job.height) {
                    info!(height = job.height, "new template");
                }
                *g = Some(job);
            }
            Err(e) => warn!(error=%e, "template fetch failed"),
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

async fn handle_client(mut sock: TcpStream, state: Arc<RwLock<Option<Job>>>) -> Result<()> {
    let peer = sock.peer_addr().ok();
    info!(?peer, "client connected");
    let (rd, mut wr) = sock.split();
    let mut rd = BufReader::new(rd);
    let mut line = String::new();
    let session_id = format!("stub-session-{}", peer.map(|p| p.port()).unwrap_or(0));
    loop {
        line.clear();
        let n = rd.read_line(&mut line).await?;
        if n == 0 {
            break;
        }
        let req: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let id = req.get("id").cloned().unwrap_or(json!(0));
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let resp = match method {
            "login" => {
                let job = state.read().await.clone();
                match job {
                    Some(j) => json!({
                        "id": id, "jsonrpc":"2.0",
                        "result": { "id": session_id, "job": job_to_json(&j, &session_id), "status":"OK" }
                    }),
                    None => json!({
                        "id": id, "jsonrpc":"2.0",
                        "error": { "code": -1, "message": "no template yet" }
                    }),
                }
            }
            "submit" => json!({"id": id, "jsonrpc":"2.0", "result": {"status":"OK"}}),
            "keepalived" => json!({"id": id, "jsonrpc":"2.0", "result": {"status":"KEEPALIVED"}}),
            other => {
                warn!(method=%other, "unknown method");
                json!({"id": id, "jsonrpc":"2.0", "error":{"code":-32601, "message":"method not found"}})
            }
        };
        let mut s = serde_json::to_string(&resp)?;
        s.push('\n');
        wr.write_all(s.as_bytes()).await?;
    }
    info!(?peer, "client gone");
    Ok(())
}

fn job_to_json(j: &Job, session_id: &str) -> Value {
    json!({
        "job_id": j.job_id,
        "blob": j.blob,
        "seed_hash": j.seed_hash,
        "next_seed_hash": j.next_seed_hash,
        "target": j.target_hex,
        "height": j.height,
        "id": session_id,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let bind = env::var("STUB_BIND").unwrap_or_else(|_| "0.0.0.0:3333".into());
    let monerod = env::var("STUB_MONEROD")
        .unwrap_or_else(|_| "http://monerod:38089/json_rpc".into());
    let wallet = env::var("STUB_WALLET").unwrap_or_else(|_| DEFAULT_BURN_ADDR.into());

    info!(%bind, %monerod, "stratum-stub starting");

    let state: Arc<RwLock<Option<Job>>> = Arc::new(RwLock::new(None));
    tokio::spawn(template_loop(state.clone(), monerod, wallet));

    let listener = TcpListener::bind(&bind).await?;
    loop {
        let (sock, _) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                warn!(error=%e, "accept failed");
                continue;
            }
        };
        let st = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(sock, st).await {
                warn!(error=%e, "client handler errored");
            }
        });
    }
}
