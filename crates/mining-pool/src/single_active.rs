//! Single-active-instance guard — read-only, zero gas, zero per-instance config.
//!
//! Running two instances of this pool at once is dangerous: they share the
//! same KMS-derived Monero wallet and would both drain the redemption queue,
//! double-paying real XMR (paying one invoice twice from different outputs is
//! NOT prevented by Monero's double-spend rule). We want exactly one live
//! instance, with the ability to spin up a *replacement* once the old one is
//! gone (e.g. it ran out of rent and was halted).
//!
//! We get that for free from the protocol's own bookkeeping — no heartbeat
//! transactions, no ROSE, and nothing to configure:
//!
//!  * Every ROFL instance must register on-chain to get KMS access, and the
//!    runtime starts the container workload only *after* that registration
//!    (`rofl-containers::post_registration_init`). So by the time this code
//!    runs, our own instance is already in the app's registration set.
//!  * The runtime prunes expired registrations every epoch
//!    (`end_block` → `expire_registrations`), so the read-only query
//!    `rofl.AppInstances(app_id)` returns exactly the set of *currently live*
//!    instances.
//!
//! Therefore we don't need to identify ourselves — we just count. After a short
//! settle delay (so our own already-submitted registration is certainly
//! reflected on-chain), a count of 1 means we're the only instance; a count of
//! ≥2 means another instance is live and we stand down. A replacement simply
//! waits here until the dead predecessor's registration lapses
//! (≤ `max_expiration` epochs), then proceeds.
//!
//! The one identifier we need — our own `app_id` — comes from appd
//! (`GET /rofl/v1/app/id`), not from config.
//!
//! Trade-off worth knowing: if two *fresh* instances are deliberately booted at
//! the same moment, both will see count ≥2 and both stand down (a liveness
//! stall, not a double-pay). For a hot wallet, failing closed — nobody pays
//! until an operator removes one — is the safe default. The single-instance
//! operational model never boots two on purpose.

use anyhow::{anyhow, bail, Context, Result};
use ciborium::value::Value;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tracing::{info, warn};

use pool_core::config::SingleActiveConfig;

pub const DEFAULT_SOCKET: &str = "/run/rofl-appd.sock";

/// Poll cadence while a predecessor is still live.
const POLL_WAIT: Duration = Duration::from_secs(60);
/// Shorter backoff while the check itself is erroring (transient).
const POLL_ERR: Duration = Duration::from_secs(10);
/// After this many consecutive *errors* (not stand-downs) we degrade open and
/// proceed without the guard — bricking the pool because a read failed is worse
/// than the small residual risk, given the one-instance operational model.
const MAX_ERRORS: u32 = 6;

/// Determined outcome of a single check.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// We are the sole live instance — safe to run.
    Proceed,
    /// Another instance is live; we must not act. Carries a human reason.
    StandDown(String),
    /// Guard not applicable (no appd, or disabled).
    Skipped(String),
}

// ---- raw appd HTTP/1.1 over the unix socket (mirrors rofl_kms.rs) ----

async fn appd_request(socket: &str, raw_req: &str) -> Result<String> {
    let mut sock = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connecting to rofl-appd at {socket}"))?;
    sock.write_all(raw_req.as_bytes()).await.context("send")?;
    let mut raw = Vec::new();
    sock.read_to_end(&mut raw).await.context("recv")?;
    let resp = String::from_utf8(raw).context("appd response not utf-8")?;
    let code = resp
        .split("\r\n")
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("")
        .to_string();
    let idx = resp
        .find("\r\n\r\n")
        .ok_or_else(|| anyhow!("appd response had no body separator"))?;
    let body = resp[idx + 4..].to_string();
    if code != "200" {
        bail!("appd returned status {code}: {body}");
    }
    Ok(body)
}

async fn appd_get(socket: &str, path: &str) -> Result<String> {
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    appd_request(socket, &req).await
}

/// `POST /rofl/v1/query` → decoded CBOR response value.
async fn appd_query(socket: &str, method: &str, args: &[u8]) -> Result<Value> {
    let body = serde_json::to_string(
        &serde_json::json!({ "method": method, "args": hex::encode(args) }),
    )?;
    let req = format!(
        "POST /rofl/v1/query HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let resp_body = appd_request(socket, &req).await?;
    #[derive(serde::Deserialize)]
    struct QResp {
        data: String,
    }
    let q: QResp = serde_json::from_str(&resp_body)
        .with_context(|| format!("parsing query response: {resp_body}"))?;
    let cbor = hex::decode(q.data.strip_prefix("0x").unwrap_or(&q.data))
        .context("decoding query data hex")?;
    let val: Value = ciborium::from_reader(&cbor[..]).context("decoding query CBOR")?;
    Ok(val)
}

// ---- CBOR + bech32 helpers ----

/// oasis-cbor encodes structs as string-keyed maps; build one.
fn encode_map(pairs: &[(&str, Value)]) -> Vec<u8> {
    let map = Value::Map(
        pairs
            .iter()
            .map(|(k, v)| (Value::Text((*k).to_string()), v.clone()))
            .collect(),
    );
    let mut out = Vec::new();
    ciborium::into_writer(&map, &mut out).expect("cbor encode never fails to a Vec");
    out
}

/// Minimal bech32 (BIP173) decode → (hrp, data bytes). Checksum is not
/// validated — we trust appd's own output and only need the payload bytes.
fn bech32_decode(s: &str) -> Result<(String, Vec<u8>)> {
    const CHARSET: &[u8] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";
    let s = s.trim();
    let pos = s
        .rfind('1')
        .ok_or_else(|| anyhow!("bech32 missing separator in {s:?}"))?;
    let hrp = s[..pos].to_lowercase();
    let mut vals = Vec::new();
    for c in s[pos + 1..].bytes() {
        let idx = CHARSET
            .iter()
            .position(|&x| x == c.to_ascii_lowercase())
            .ok_or_else(|| anyhow!("bech32 bad char {:?}", c as char))?;
        vals.push(idx as u8);
    }
    if vals.len() < 6 {
        bail!("bech32 data too short");
    }
    let five = &vals[..vals.len() - 6]; // strip 6-symbol checksum
    let (mut acc, mut bits) = (0u32, 0u32);
    let mut out = Vec::new();
    for &v in five {
        acc = (acc << 5) | v as u32;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok((hrp, out))
}

// ---- the count ----

async fn app_id_bytes(socket: &str) -> Result<Vec<u8>> {
    let body = appd_get(socket, "/rofl/v1/app/id").await?;
    let (hrp, data) = bech32_decode(&body)?;
    if hrp != "rofl" {
        bail!("unexpected app-id hrp {hrp:?}");
    }
    Ok(data)
}

/// Look up a string key in a CBOR map value.
fn map_get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    if let Value::Map(pairs) = v {
        for (k, val) in pairs {
            if matches!(k, Value::Text(s) if s == key) {
                return Some(val);
            }
        }
    }
    None
}

/// The `node_id` of every currently-live registration of our app — one entry
/// per registration (a registration missing a node_id contributes `Null`).
async fn live_node_ids(socket: &str) -> Result<Vec<Value>> {
    let app_id = app_id_bytes(socket).await.context("read own app id")?;
    let args = encode_map(&[("id", Value::Bytes(app_id))]);
    let regs = appd_query(socket, "rofl.AppInstances", &args)
        .await
        .context("query rofl.AppInstances")?;
    match regs {
        Value::Array(a) => Ok(a
            .iter()
            .map(|r| map_get(r, "node_id").cloned().unwrap_or(Value::Null))
            .collect()),
        other => bail!("rofl.AppInstances did not return an array: {other:?}"),
    }
}

/// Count distinct values. n is tiny (a handful of registrations) and
/// `ciborium::Value` is neither `Hash` nor `Ord`, so an O(n²) scan is simplest.
fn distinct_count(values: &[Value]) -> usize {
    let mut seen: Vec<&Value> = Vec::new();
    for v in values {
        if !seen.iter().any(|s| *s == v) {
            seen.push(v);
        }
    }
    seen.len()
}

/// Pure decision. `nodes` holds one `node_id` per live registration; we are
/// always among them once running (registration precedes the workload), so a
/// total of ≤1 means we're alone.
///
/// - strict (`node_aware = false`): count REGISTRATIONS. Safest — any second
///   registration stands us down, including a redeploy's stale same-node ghost
///   (so a redeploy pauses until that ghost's registration expires). Use on
///   mainnet.
/// - `node_aware = true`: count DISTINCT NODES. A redeploy's ghost shares our
///   node, so it doesn't inflate the distinct-node count; only a registration
///   on ANOTHER node (a genuine peer elsewhere) stands us down. Fast redeploys.
///   Trade-off: two *deliberately* co-located live instances on one node would
///   both proceed — a case we don't run. Use on testnet.
fn decide(nodes: &[Value], node_aware: bool) -> Decision {
    let raw = nodes.len();
    if node_aware {
        let distinct = distinct_count(nodes);
        if distinct <= 1 {
            Decision::Proceed
        } else {
            Decision::StandDown(format!(
                "{distinct} distinct nodes registered for this app ({raw} registrations)"
            ))
        }
    } else if raw <= 1 {
        Decision::Proceed
    } else {
        Decision::StandDown(format!("{raw} live instances registered for this app"))
    }
}

/// Block until we're the sole live instance (or the guard is skipped / degrades
/// open after persistent read errors). A positively-detected peer makes us wait
/// indefinitely — that's the point: a replacement waits out its predecessor.
pub async fn await_sole_instance(socket: &str, cfg: &SingleActiveConfig) {
    if !std::path::Path::new(socket).exists() {
        info!("single-active guard skipped: no rofl-appd socket (not in a TEE)");
        return;
    }
    if !cfg.enabled {
        warn!("single-active guard disabled by config");
        return;
    }

    // Wait so our own (already-submitted) registration is certainly reflected
    // on-chain before we start counting — otherwise a count of 1 could be a
    // lingering predecessor rather than us.
    info!(
        settle_secs = cfg.settle_secs,
        "single-active guard: settling before counting live instances"
    );
    tokio::time::sleep(Duration::from_secs(cfg.settle_secs)).await;

    let mut errors = 0u32;
    loop {
        match live_node_ids(socket).await {
            Ok(nodes) => match decide(&nodes, cfg.node_aware) {
                Decision::Proceed => {
                    errors = 0;
                    info!(
                        registrations = nodes.len(),
                        node_aware = cfg.node_aware,
                        "single-active guard: sole live instance — proceeding"
                    );
                    return;
                }
                Decision::StandDown(reason) => {
                    errors = 0;
                    info!(%reason, "single-active guard: another instance is live — waiting");
                    tokio::time::sleep(POLL_WAIT).await;
                }
                Decision::Skipped(_) => return,
            },
            Err(e) => {
                errors += 1;
                if errors >= MAX_ERRORS {
                    warn!(
                        error = %format!("{e:#}"), attempts = errors,
                        "single-active guard: could not determine liveness — proceeding WITHOUT the guard"
                    );
                    return;
                }
                warn!(error = %format!("{e:#}"), "single-active guard check failed; retrying");
                tokio::time::sleep(POLL_ERR).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bech32_decodes_known_oasis_address() {
        // trustless_faucet account from RESUME.md; its 21-byte versioned address
        // was cross-checked against the eth derivation in the Python PoC.
        let (hrp, data) =
            bech32_decode("oasis1qq3zu7td2w972zn2m2eyvlseq0fj8t32uc0j9nw7").unwrap();
        assert_eq!(hrp, "oasis");
        assert_eq!(
            hex::encode(&data),
            "00222e796d538be50a6adab2467e1903d323ae2ae6"
        );
        assert_eq!(data.len(), 21);
    }

    #[test]
    fn bech32_decodes_rofl_app_id_to_21_bytes() {
        let (hrp, data) =
            bech32_decode("rofl1qpue9y6ty0edpy53vu6lv6ph4as7u5sahvlljl6y").unwrap();
        assert_eq!(hrp, "rofl");
        assert_eq!(data.len(), 21);
    }

    #[test]
    fn encode_map_roundtrips() {
        let bytes = encode_map(&[("id", Value::Bytes(vec![1, 2, 3]))]);
        let v: Value = ciborium::from_reader(&bytes[..]).unwrap();
        let Value::Map(pairs) = v else { panic!("not a map") };
        assert_eq!(pairs[0].0, Value::Text("id".into()));
        assert_eq!(pairs[0].1, Value::Bytes(vec![1, 2, 3]));
    }

    fn node(b: u8) -> Value {
        Value::Bytes(vec![b; 32])
    }

    #[test]
    fn strict_counts_registrations() {
        // node_aware = false: any 2nd registration stands us down.
        assert_eq!(decide(&[], false), Decision::Proceed);
        assert_eq!(decide(&[node(1)], false), Decision::Proceed);
        assert!(matches!(decide(&[node(1), node(2)], false), Decision::StandDown(_)));
        // Even two registrations on the SAME node (a redeploy ghost) → wait.
        assert!(matches!(decide(&[node(1), node(1)], false), Decision::StandDown(_)));
    }

    #[test]
    fn node_aware_ignores_same_node_ghost() {
        // node_aware = true: a redeploy's stale same-node registration does NOT
        // look like a peer (1 distinct node) → proceed.
        assert_eq!(decide(&[node(1), node(1)], true), Decision::Proceed);
        assert_eq!(decide(&[node(1)], true), Decision::Proceed);
        // A registration on a DIFFERENT node IS a real peer → wait.
        assert!(matches!(decide(&[node(1), node(2)], true), Decision::StandDown(_)));
        assert!(matches!(
            decide(&[node(1), node(1), node(2)], true),
            Decision::StandDown(_)
        ));
    }

    #[test]
    fn distinct_count_works() {
        assert_eq!(distinct_count(&[node(1), node(1), node(1)]), 1);
        assert_eq!(distinct_count(&[node(1), node(2), node(1)]), 2);
        assert_eq!(distinct_count(&[]), 0);
    }
}
