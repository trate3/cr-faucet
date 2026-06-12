//! Bootstrap the Monero wallet on container startup.
//!
//! Two paths:
//!
//!   * **Already initialized.** A previous run created the wallet file on
//!     persistent storage. We just call `open_wallet`; wallet-rpc loads the
//!     file (which carries the last-scanned height) and only needs to scan
//!     forward from there. Restart cost ≈ a few seconds against a fresh
//!     remote `monerod`.
//!
//!   * **Cold first start.** No wallet file yet. We derive a 32-byte seed
//!     from the ROFL KMS, compute the Monero keypair + primary address from
//!     it, and call `generate_from_keys(restore_height = current_height)`
//!     so the wallet skips the entire pre-deployment history.
//!
//! KMS-derivation is deterministic, so wiping persistent storage just rolls
//! us back to the cold-start path against the *same* address — no key loss,
//! just one scan to redo. The shorter the time between deployment and the
//! first wipe, the cheaper that scan.
//!
//! Network selection mirrors `MoneroConfig.network` (mainnet / testnet /
//! stagenet); we use the right address byte and the right restore-height
//! source.

use anyhow::{bail, Context, Result};
use curve25519_dalek::scalar::Scalar;
use monero::cryptonote::hash::Hash as MoneroHash;
use monero::util::address::Address;
use monero::util::key::{PrivateKey, PublicKey};
use monero::Network;
use serde_json::{json, Value};
use std::time::Duration;

/// Result of [`bootstrap_wallet`]. Holds the address + a flag for whether
/// we just created the wallet (useful for boot logs / metrics).
#[derive(Debug)]
pub struct WalletBootstrap {
    pub primary_address: String,
    pub created: bool,
}

/// Derive the keypair + primary address from a 32-byte KMS seed.
pub fn derive_address(seed: &[u8; 32], network: Network) -> Result<DerivedKeys> {
    // Monero spec:
    //   spend_priv = seed reduced mod L (ed25519 group order)
    //   view_priv  = keccak256(spend_priv) reduced mod L
    // `PrivateKey::from_slice` rejects non-canonical bytes, so reduce first.
    let spend_priv = PrivateKey::from_scalar(Scalar::from_bytes_mod_order(*seed));
    let view_priv = {
        let h = MoneroHash::new(spend_priv.as_bytes());
        let mut h_bytes = [0u8; 32];
        h_bytes.copy_from_slice(h.as_bytes());
        PrivateKey::from_scalar(Scalar::from_bytes_mod_order(h_bytes))
    };
    let spend_pub = PublicKey::from_private_key(&spend_priv);
    let view_pub = PublicKey::from_private_key(&view_priv);
    let address = Address::standard(network, spend_pub, view_pub).to_string();
    Ok(DerivedKeys {
        spend_priv_hex: hex::encode(spend_priv.as_bytes()),
        view_priv_hex: hex::encode(view_priv.as_bytes()),
        address,
    })
}

#[derive(Debug)]
pub struct DerivedKeys {
    pub spend_priv_hex: String,
    pub view_priv_hex: String,
    pub address: String,
}

/// Open the existing wallet if present, otherwise generate it from the
/// supplied seed at the supplied restore height.
///
/// `wallet_rpc_url`: e.g. `http://127.0.0.1:18083/json_rpc`
/// `filename`/`password`: wallet-rpc creates/opens `{wallet-dir}/{filename}`
/// `seed`: 32 bytes from KMS (raw-256)
/// `network`: matches the network the upstream monerod is on
/// `current_height_fn`: a closure that returns the current network tip (only
/// called when we need to compute a restore_height for first-time creation).
pub async fn bootstrap_wallet(
    client: &reqwest::Client,
    wallet_rpc_url: &str,
    filename: &str,
    password: &str,
    seed: &[u8; 32],
    network: Network,
    restore_lookback: u64,
    // On-chain `restoreHeight` (oldest still-unspent output we hold). When > 0
    // this is the restore point — it sees every spendable output AND skips the
    // rescan from wallet birth. 0 (fresh deploy, no redemptions yet) falls back
    // to `tip - restore_lookback`.
    onchain_restore_height: u64,
    current_height_fn: impl FnOnce() -> futures::future::BoxFuture<'static, Result<u64>>,
) -> Result<WalletBootstrap> {
    // Always derive — we'll need the address either way (for logging on
    // open, for generate on create). Cheap, ~microseconds.
    let keys = derive_address(seed, network)?;

    // Try-then-fallback. Don't bother classifying the open_wallet error;
    // wallet-rpc's error messages vary across versions and are localized
    // in some builds. If open fails for any reason, attempt generate;
    // only bail if generate also fails (then we surface both errors so a
    // human can tell which path is broken).
    let open_err = match try_open_wallet(client, wallet_rpc_url, filename, password).await {
        Ok(()) => {
            return Ok(WalletBootstrap {
                primary_address: keys.address,
                created: false,
            });
        }
        Err(e) => e,
    };

    // Set the restore height a configurable number of blocks in the
    // past, not at the exact tip. A hot wallet can legitimately receive
    // a deposit in the minutes around its first creation (e.g. a funding
    // tx already broadcast and mined just before bootstrap runs); pinning
    // restore_height = tip would skip the block holding it. The lookback
    // is small enough that the first scan stays fast (a near-empty wallet
    // over a few hundred blocks is seconds) but generous enough to catch
    // those near-boot deposits.
    let restore_height = if onchain_restore_height > 0 {
        // Authoritative, wipe-proof restore point: the oldest unspent output we
        // still hold (advanced on-chain by markProcessed). Sees all spendable
        // funds; no rescan from birth.
        onchain_restore_height
    } else {
        current_height_fn()
            .await
            .context("fetching restore_height from monerod")?
            .saturating_sub(restore_lookback.max(1))
    };
    if let Err(gen_err) = generate_from_keys(
        client,
        wallet_rpc_url,
        filename,
        password,
        &keys.address,
        &keys.spend_priv_hex,
        &keys.view_priv_hex,
        restore_height,
    )
    .await
    {
        return Err(anyhow::anyhow!(
            "wallet bootstrap failed: open_wallet error: {open_err}; generate_from_keys error: {gen_err}"
        ));
    }
    // After generate, wallet-rpc has the wallet *open*. No second open needed.
    Ok(WalletBootstrap {
        primary_address: keys.address,
        created: true,
    })
}

/// Try `open_wallet`. Returns Ok(()) on success, Err on any RPC failure
/// (network error, malformed response, or wallet-rpc returning an
/// `error` object — including the very common "file not found" case on
/// first boot, which the caller handles by falling back to
/// `generate_from_keys`).
async fn try_open_wallet(
    client: &reqwest::Client,
    url: &str,
    filename: &str,
    password: &str,
) -> Result<()> {
    let resp = rpc_call(
        client,
        url,
        "open_wallet",
        json!({ "filename": filename, "password": password }),
    )
    .await?;
    if resp.get("result").is_some() {
        return Ok(());
    }
    let err = resp.get("error").cloned().unwrap_or(Value::Null);
    bail!("open_wallet returned error: {err}")
}

#[allow(clippy::too_many_arguments)]
async fn generate_from_keys(
    client: &reqwest::Client,
    url: &str,
    filename: &str,
    password: &str,
    address: &str,
    spendkey_hex: &str,
    viewkey_hex: &str,
    restore_height: u64,
) -> Result<()> {
    let resp = rpc_call(
        client,
        url,
        "generate_from_keys",
        json!({
            "restore_height": restore_height,
            "filename": filename,
            "password": password,
            "address": address,
            "spendkey": spendkey_hex,
            "viewkey": viewkey_hex,
            "autosave_current": true,
        }),
    )
    .await?;
    if let Some(err) = resp.get("error") {
        if !err.is_null() {
            bail!("generate_from_keys failed: {err}");
        }
    }
    Ok(())
}

async fn rpc_call(
    client: &reqwest::Client,
    url: &str,
    method: &str,
    params: Value,
) -> Result<Value> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": "0",
        "method": method,
        "params": params,
    });
    let v: Value = client
        .post(url)
        .json(&body)
        .timeout(Duration::from_secs(30))
        .send()
        .await?
        .json()
        .await?;
    Ok(v)
}

/// Block until wallet-rpc accepts a `get_version` RPC. Used at startup so
/// the main task doesn't race the supervised wallet-rpc subprocess.
pub async fn wait_for_wallet_rpc(client: &reqwest::Client, url: &str) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        if rpc_call(client, url, "get_version", json!({})).await.is_ok() {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            bail!("wallet-rpc did not become ready within 60s");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_address_is_deterministic() {
        let seed = [0x42u8; 32];
        let a = derive_address(&seed, Network::Stagenet).unwrap();
        let b = derive_address(&seed, Network::Stagenet).unwrap();
        assert_eq!(a.address, b.address);
        assert_eq!(a.spend_priv_hex, b.spend_priv_hex);
        // Stagenet addresses begin with `5` per the network byte (0x18).
        assert!(
            a.address.starts_with('5'),
            "expected stagenet address prefix, got {}",
            a.address
        );
        assert_eq!(a.spend_priv_hex.len(), 64);
        assert_eq!(a.view_priv_hex.len(), 64);
    }

    #[test]
    fn different_seeds_give_different_addresses() {
        let a = derive_address(&[0x01u8; 32], Network::Mainnet).unwrap();
        let b = derive_address(&[0x02u8; 32], Network::Mainnet).unwrap();
        assert_ne!(a.address, b.address);
        // Mainnet addresses start with `4` (network byte 0x12).
        assert!(a.address.starts_with('4'));
    }

    /// Live integration test against a real `monero-wallet-rpc` binary
    /// pointed at a real (regtest) daemon. Skipped unless both env vars
    /// are set, so `cargo test` stays green in environments without the
    /// binary.
    ///
    ///   POOL_TEST_MONERO_WALLET_RPC_BIN=/path/to/monero-wallet-rpc
    ///   POOL_TEST_MONEROD_URL=http://127.0.0.1:38089
    ///
    /// Covers both branches of `bootstrap_wallet`:
    ///   1. First call: wallet file doesn't exist → bootstrap should
    ///      `generate_from_keys` and end up with an open wallet matching
    ///      the KMS-derived primary address. `created = true`.
    ///   2. Second call against the same dir: `open_wallet` should now
    ///      succeed without re-creating. `created = false`, same address.
    #[tokio::test]
    async fn bootstrap_against_real_wallet_rpc_creates_then_reopens() {
        let Ok(bin) = std::env::var("POOL_TEST_MONERO_WALLET_RPC_BIN") else {
            eprintln!("skip: POOL_TEST_MONERO_WALLET_RPC_BIN unset");
            return;
        };
        let Ok(monerod_url) = std::env::var("POOL_TEST_MONEROD_URL") else {
            eprintln!("skip: POOL_TEST_MONEROD_URL unset");
            return;
        };

        // Stable seed → stable address → easy to assert.
        let seed = [0x42u8; 32];
        // Regtest uses mainnet address bytes.
        let expected = derive_address(&seed, Network::Mainnet).unwrap();

        // Per-test wallet-dir under /tmp so concurrent test runs don't
        // collide. The OS cleans /tmp eventually; not worth the noise.
        let dir = std::env::temp_dir().join(format!(
            "drip-wallet-test-{}",
            std::process::id() as u64
                ^ (std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos() as u64),
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // Pick a free port via the OS.
        let port_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let rpc_port = port_listener.local_addr().unwrap().port();
        drop(port_listener);
        let rpc_url = format!("http://127.0.0.1:{rpc_port}/json_rpc");

        // Extract host:port from the monerod URL for --daemon-address.
        let daemon_addr = monerod_url
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .split('/')
            .next()
            .unwrap()
            .to_string();

        let child = std::process::Command::new(&bin)
            .args([
                "--wallet-dir",
                dir.to_str().unwrap(),
                "--rpc-bind-ip",
                "127.0.0.1",
                "--rpc-bind-port",
                &rpc_port.to_string(),
                "--daemon-address",
                &daemon_addr,
                "--allow-mismatched-daemon-version",
                "--disable-rpc-login",
                "--non-interactive",
                "--log-level",
                "0",
                "--confirm-external-bind",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn monero-wallet-rpc");
        // Best-effort kill on test exit.
        let pid = child.id();
        struct Killer(u32);
        impl Drop for Killer {
            fn drop(&mut self) {
                let _ = std::process::Command::new("kill")
                    .arg(self.0.to_string())
                    .status();
            }
        }
        let _killer = Killer(pid);

        let client = reqwest::Client::new();
        wait_for_wallet_rpc(&client, &rpc_url)
            .await
            .expect("wallet-rpc never came up");

        let monerod_for_height = monerod_url.clone();
        let height_fn = move || -> futures::future::BoxFuture<'static, Result<u64>> {
            let url = format!("{}/json_rpc", monerod_for_height.trim_end_matches('/'));
            Box::pin(async move {
                let body = json!({"jsonrpc":"2.0","id":"0","method":"get_info"});
                let v: Value = reqwest::Client::new()
                    .post(&url)
                    .json(&body)
                    .send()
                    .await?
                    .json()
                    .await?;
                let h = v["result"]["height"]
                    .as_u64()
                    .ok_or_else(|| anyhow::anyhow!("no height in get_info"))?;
                Ok(h)
            })
        };

        // First call → wallet doesn't exist → generate_from_keys path.
        let first = bootstrap_wallet(
            &client,
            &rpc_url,
            "pool",
            "",
            &seed,
            Network::Mainnet,
             0,
            0, // onchain_restore_height (test: fall back to tip-lookback)
            height_fn,
        )
        .await
        .expect("first bootstrap failed");
        assert!(first.created, "first bootstrap should have created the wallet");
        assert_eq!(first.primary_address, expected.address);

        // Cross-check: get_address from wallet-rpc agrees with our derived
        // address. This confirms the wallet was actually opened with the
        // right keys, not just that bootstrap_wallet returned Ok.
        let v: Value = client
            .post(&rpc_url)
            .json(&json!({
                "jsonrpc":"2.0","id":"0","method":"get_address","params":{"account_index":0}
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            v["result"]["address"].as_str().unwrap(),
            expected.address,
            "wallet-rpc reports a different address than we derived"
        );

        // Close the wallet so the second bootstrap goes through the
        // open_wallet path against a real on-disk file.
        let _close: Value = client
            .post(&rpc_url)
            .json(&json!({
                "jsonrpc":"2.0","id":"0","method":"close_wallet"
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        // Second call: file exists → open_wallet succeeds, no recreate.
        let monerod_again = monerod_url.clone();
        let height_fn = move || -> futures::future::BoxFuture<'static, Result<u64>> {
            let url = monerod_again.clone();
            Box::pin(async move {
                panic!(
                    "second bootstrap should not have called the height fn (got URL {url})"
                );
            })
        };
        let second = bootstrap_wallet(
            &client,
            &rpc_url,
            "pool",
            "",
            &seed,
            Network::Mainnet,
             0,
            0, // onchain_restore_height (test: fall back to tip-lookback)
            height_fn,
        )
        .await
        .expect("second bootstrap failed");
        assert!(
            !second.created,
            "second bootstrap should have opened the existing wallet, not recreated"
        );
        assert_eq!(second.primary_address, expected.address);

        std::fs::remove_dir_all(&dir).ok();
    }
}
