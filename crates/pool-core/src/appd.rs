//! rofl-appd tx/query client for the self-funding agent (`self_fund.rs`).
//!
//! Extends what `rofl_kms.rs` does for `/rofl/v1/keys/generate` to the three
//! other appd endpoints the self-top-up loop needs (all raw HTTP/1.1 over the
//! `/run/rofl-appd.sock` unix socket — present only inside a ROFL TEE):
//!   - `GET  /rofl/v1/app/id`        → our app id (`rofl1…`).
//!   - `POST /rofl/v1/query`         → a runtime-module query (`roflmarket.Instance`
//!                                     for runway+prices, `accounts.Balances` for
//!                                     the RentPayer reserve). args/result are CBOR.
//!   - `POST /rofl/v1/tx/sign-submit`→ submit an `evm.Call` signed by the app's
//!                                     endorsed key (APP ORIGIN — what lets
//!                                     RentPayer's `roflEnsureAuthorizedOrigin`
//!                                     pass). `roflmarket.*` is NOT in appd's
//!                                     allow-list, so we route via the contract.
//!
//! Ported from the verified PoC research/rofl-trustless-faucet/selffund-poc/
//! selffund-faucet/selffund.py. The wire shapes of `/query` and `/tx/sign-submit`
//! are confirmed there; this module mirrors them.

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha512_256};
use std::collections::BTreeMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Raw HTTP/1.1 request over the appd unix socket. Returns `(status, body)`.
async fn request(
    socket: &str,
    method: &str,
    path: &str,
    json_body: Option<&str>,
) -> Result<(u16, String)> {
    let req = match json_body {
        Some(b) => format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{b}",
            b.len()
        ),
        None => format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        ),
    };
    let mut sock = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connecting to rofl-appd socket at {socket}"))?;
    sock.write_all(req.as_bytes()).await.context("send")?;
    let mut raw = Vec::new();
    sock.read_to_end(&mut raw).await.context("recv")?;
    let resp = String::from_utf8(raw).context("appd response not utf-8")?;
    let code: u16 = resp
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| anyhow!("malformed appd status line: {resp}"))?;
    let body = resp
        .find("\r\n\r\n")
        .map(|i| resp[i + 4..].to_string())
        .ok_or_else(|| anyhow!("appd response had no body separator: {resp}"))?;
    Ok((code, body))
}

/// `GET /rofl/v1/app/id` → the app's on-chain id (`rofl1…`).
pub async fn app_id(socket: &str) -> Result<String> {
    let (code, body) = request(socket, "GET", "/rofl/v1/app/id", None).await?;
    if code != 200 {
        bail!("appd /app/id returned {code}: {body}");
    }
    // Body is a JSON string ("rofl1…") or a bare string; strip quotes/space.
    Ok(body.trim().trim_matches('"').to_string())
}

/// The app id as its raw 21-byte form (bech32-decoded), to compare against an
/// instance record's `deployment.app_id`.
pub async fn app_id_bytes(socket: &str) -> Result<[u8; 21]> {
    bech32_to_21(&app_id(socket).await?)
}

/// Minimal bech32 decode of an `rofl1…` / `oasis1…` address → 21 bytes
/// (version || 20). Not a full validator — we only need the data bytes.
fn bech32_to_21(s: &str) -> Result<[u8; 21]> {
    const CH: &[u8] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";
    let s = s.trim();
    let pos = s.rfind('1').ok_or_else(|| anyhow!("no bech32 separator in {s}"))?;
    let mut vals = Vec::new();
    for c in s[pos + 1..].bytes() {
        vals.push(CH.iter().position(|&x| x == c).ok_or_else(|| anyhow!("bad bech32 char"))? as u32);
    }
    if vals.len() < 6 {
        bail!("bech32 too short");
    }
    vals.truncate(vals.len() - 6); // drop the 6-symbol checksum
    let (mut acc, mut bits, mut out) = (0u32, 0u32, Vec::new());
    for v in vals {
        acc = (acc << 5) | v;
        bits += 5;
        while bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    if out.len() != 21 {
        bail!("bech32 decoded to {} bytes, expected 21", out.len());
    }
    let mut a = [0u8; 21];
    a.copy_from_slice(&out);
    Ok(a)
}

/// List the `accepted` instance ids on `provider` whose `deployment.app_id`
/// equals ours. NOTE: app-id match does NOT uniquely identify *our own* machine
/// — anyone may permissionlessly deploy a same-app-id instance — so this is a
/// CROSS-CHECK signal only (see `self_fund`: the config instance id stays
/// authoritative; we never top up a merely-discovered instance). Returns all
/// matches so the caller can detect a missing/ambiguous configured instance.
pub async fn discover_instances(socket: &str, provider: &[u8; 21], app_id: &[u8; 21]) -> Result<Vec<[u8; 8]>> {
    let args = {
        use ciborium::Value;
        let v = Value::Map(vec![(Value::Text("provider".into()), Value::Bytes(provider.to_vec()))]);
        let mut buf = Vec::new();
        ciborium::into_writer(&v, &mut buf).expect("cbor");
        buf
    };
    let raw = query(socket, "roflmarket.Instances", &args).await?;
    let v: ciborium::Value = ciborium::from_reader(&raw[..]).context("decode Instances")?;
    let ciborium::Value::Array(items) = v else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for inst in items {
        if map_get(&inst, "status").and_then(cbor_to_u128) != Some(1) {
            continue; // not accepted/live
        }
        let dep_app = map_get(&inst, "deployment").and_then(|d| map_get(d, "app_id"));
        if !matches!(dep_app, Some(ciborium::Value::Bytes(b)) if b.as_slice() == app_id.as_slice()) {
            continue;
        }
        if let Some(ciborium::Value::Bytes(id)) = map_get(&inst, "id") {
            if id.len() == 8 {
                let mut a = [0u8; 8];
                a.copy_from_slice(id);
                out.push(a);
            }
        }
    }
    Ok(out)
}

/// `POST /rofl/v1/query {method, args:<hex>}` → the CBOR-decoded `data` bytes.
pub async fn query(socket: &str, method: &str, args: &[u8]) -> Result<Vec<u8>> {
    let body = serde_json::json!({ "method": method, "args": hex::encode(args) }).to_string();
    let (code, resp) = request(socket, "POST", "/rofl/v1/query", Some(&body)).await?;
    if code != 200 {
        bail!("appd /query {method} returned {code}: {resp}");
    }
    let v: serde_json::Value =
        serde_json::from_str(&resp).with_context(|| format!("parsing /query response: {resp}"))?;
    let data_hex = v
        .get("data")
        .and_then(|d| d.as_str())
        .ok_or_else(|| anyhow!("/query response had no string `data`: {resp}"))?;
    hex::decode(data_hex.strip_prefix("0x").unwrap_or(data_hex)).context("decoding /query data hex")
}

/// `POST /rofl/v1/tx/sign-submit` an `evm.Call` to `to` with `data`, signed by
/// the app's endorsed key (app origin). Returns appd's raw JSON result.
pub async fn sign_submit_eth(
    socket: &str,
    to: [u8; 20],
    data: &[u8],
    gas_limit: u64,
) -> Result<String> {
    let body = serde_json::json!({
        "encrypt": true,
        "tx": { "kind": "eth", "data": {
            "gas_limit": gas_limit,
            "to": hex::encode(to),
            "value": "0",
            "data": hex::encode(data),
        }},
    })
    .to_string();
    let (code, resp) = request(socket, "POST", "/rofl/v1/tx/sign-submit", Some(&body)).await?;
    if code >= 300 {
        bail!("appd /tx/sign-submit returned {code}: {resp}");
    }
    Ok(resp)
}

/// Derive the 21-byte oasis-runtime address for an Ethereum address, exactly as
/// runtime-sdk `Address::from_eth`: `version(0) || sha512_256(ctx || 0x00 ||
/// eth)[:20]`. Network-independent. Used to read the RentPayer contract's native
/// balance (the rent reserve) via the `accounts.Balances` appd query.
pub fn oasis_addr_from_eth(eth: &[u8; 20]) -> [u8; 21] {
    let mut h = Sha512_256::new();
    h.update(b"oasis-runtime-sdk/address: secp256k1eth");
    h.update([0u8]);
    h.update(eth);
    let digest = h.finalize();
    let mut out = [0u8; 21];
    out[0] = 0; // version
    out[1..].copy_from_slice(&digest[..20]);
    out
}

/// Parsed `roflmarket.Instance` record: the runway (`paid_until`) and the live
/// per-term native rent prices (`payment.native.terms = {1:hour,2:month,3:year}`).
#[derive(Debug, Default, Clone)]
pub struct InstanceInfo {
    pub paid_until: Option<u64>,
    /// term (1/2/3) → price in base units (wei). Empty if not native-paid.
    pub terms: BTreeMap<u8, u128>,
}

/// Canonical-CBOR encode the `roflmarket.Instance` query args `{id, provider}`.
/// Length-first key order (`id`(2) < `provider`(8)) matches the SDK's canonical
/// encoder.
pub fn encode_instance_args(provider: &[u8; 21], instance_id: &[u8; 8]) -> Vec<u8> {
    use ciborium::Value;
    let v = Value::Map(vec![
        (Value::Text("id".into()), Value::Bytes(instance_id.to_vec())),
        (Value::Text("provider".into()), Value::Bytes(provider.to_vec())),
    ]);
    let mut out = Vec::new();
    ciborium::into_writer(&v, &mut out).expect("cbor encode");
    out
}

/// Canonical-CBOR encode the `accounts.Balances` query args `{address}`.
pub fn encode_balances_args(addr: &[u8; 21]) -> Vec<u8> {
    use ciborium::Value;
    let v = Value::Map(vec![(Value::Text("address".into()), Value::Bytes(addr.to_vec()))]);
    let mut out = Vec::new();
    ciborium::into_writer(&v, &mut out).expect("cbor encode");
    out
}

fn cbor_to_u128(v: &ciborium::Value) -> Option<u128> {
    match v {
        ciborium::Value::Integer(i) => u128::try_from(*i).ok(),
        ciborium::Value::Bytes(b) if b.len() <= 16 => {
            let mut buf = [0u8; 16];
            buf[16 - b.len()..].copy_from_slice(b);
            Some(u128::from_be_bytes(buf))
        }
        _ => None,
    }
}

fn map_get<'a>(v: &'a ciborium::Value, key: &str) -> Option<&'a ciborium::Value> {
    if let ciborium::Value::Map(m) = v {
        for (k, val) in m {
            if matches!(k, ciborium::Value::Text(t) if t == key) {
                return Some(val);
            }
        }
    }
    None
}

/// Parse a CBOR `roflmarket.Instance` record into runway + live term prices.
pub fn parse_instance(data: &[u8]) -> Result<InstanceInfo> {
    let v: ciborium::Value =
        ciborium::from_reader(data).context("decoding roflmarket.Instance CBOR")?;
    let mut info = InstanceInfo::default();
    info.paid_until = map_get(&v, "paid_until").and_then(cbor_to_u128).map(|x| x as u64);
    if let Some(terms) = map_get(&v, "payment")
        .and_then(|p| map_get(p, "native"))
        .and_then(|n| map_get(n, "terms"))
    {
        if let ciborium::Value::Map(m) = terms {
            for (k, val) in m {
                if let (Some(term), Some(price)) = (cbor_to_u128(k), cbor_to_u128(val)) {
                    if term >= 1 && term <= 3 {
                        info.terms.insert(term as u8, price);
                    }
                }
            }
        }
    }
    Ok(info)
}

/// Parse a CBOR `accounts.Balances` record → the native-denomination balance
/// (empty-string denomination key). 0 if absent.
pub fn parse_native_balance(data: &[u8]) -> Result<u128> {
    let v: ciborium::Value =
        ciborium::from_reader(data).context("decoding accounts.Balances CBOR")?;
    let balances = map_get(&v, "balances");
    if let Some(ciborium::Value::Map(m)) = balances {
        // native denom = empty byte string.
        for (k, val) in m {
            if matches!(k, ciborium::Value::Bytes(b) if b.is_empty()) {
                return Ok(cbor_to_u128(val).unwrap_or(0));
            }
        }
        // single-denom account → take the only entry.
        if m.len() == 1 {
            return Ok(cbor_to_u128(&m[0].1).unwrap_or(0));
        }
    }
    Ok(0)
}

/// Term durations in seconds: hour, month (30 d), year (365 d).
pub const TERM_SECS: [(u8, u64); 3] = [(1, 3600), (2, 2_592_000), (3, 31_536_000)];

/// Pick the best top-up `(term, count=1)` to buy now, or `None` if the spendable
/// reserve can't even fund the shortest term.
///
/// Policy (per operator intent): **prefer a longer term when it's CHEAPER per
/// unit time** (volume discount + fewer txs) — but only up to `max_term` so we
/// never lock too much into non-refundable prepaid rent — and **fall back to the
/// shortest term (1 hour) when the reserve can't afford a longer one**. So a
/// flush reserve buys a cheap month and tops up rarely; a reserve that's just
/// scraping by buys an hour at a time, just-in-time. Among affordable terms it
/// chooses the lowest price-per-second (longer wins ties); count is always 1.
pub fn plan_topup(balance: u128, floor: u128, terms: &BTreeMap<u8, u128>, max_term: u8) -> Option<(u8, u8)> {
    let spendable = balance.saturating_sub(floor);
    let mut best: Option<(u8, f64, u64)> = None; // (term, price_per_sec, duration)
    for &(t, dur) in &TERM_SECS {
        if t > max_term {
            continue;
        }
        let Some(&price) = terms.get(&t) else { continue };
        if price == 0 || price > spendable {
            continue;
        }
        let per_sec = price as f64 / dur as f64;
        let better = match best {
            None => true,
            // strictly cheaper per second, or equal value but longer term
            Some((_, bp, bd)) => per_sec < bp - 1e-15 || ((per_sec - bp).abs() <= 1e-15 && dur > bd),
        };
        if better {
            best = Some((t, per_sec, dur));
        }
    }
    best.map(|(t, _, _)| (t, 1u8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bech32_decodes_app_id_to_21_bytes() {
        // Verified against the live testnet app id (and the oasis CLI's own decode).
        let got = bech32_to_21("rofl1qqpmwjehvysjceewhedefzy223w782vwlgeuvwrt").unwrap();
        assert_eq!(hex::encode(got), "0003b74b3761212c672ebe5b94888a545de3a98efa");
    }

    #[test]
    fn oasis_addr_is_versioned_and_deterministic() {
        let eth = [0x11u8; 20];
        let a = oasis_addr_from_eth(&eth);
        let b = oasis_addr_from_eth(&eth);
        assert_eq!(a, b);
        assert_eq!(a[0], 0, "version byte 0");
        assert_ne!(oasis_addr_from_eth(&[0x22u8; 20]), a, "sensitive to input");
    }

    #[test]
    fn instance_args_canonical_order_id_before_provider() {
        let args = encode_instance_args(&[0xAB; 21], &[0xCD; 8]);
        // map(2) 0xa2, then text(2) "id" 0x62 6964 ... before text(8) "provider".
        assert_eq!(args[0], 0xa2);
        assert_eq!(&args[1..6], &[0x62, b'i', b'd', 0x48, 0xCD]);
    }

    #[test]
    fn parses_instance_runway_and_terms() {
        use ciborium::Value;
        let rec = Value::Map(vec![
            (Value::Text("paid_until".into()), Value::Integer(1_900_000_000u64.into())),
            (
                Value::Text("payment".into()),
                Value::Map(vec![(
                    Value::Text("native".into()),
                    Value::Map(vec![(
                        Value::Text("terms".into()),
                        Value::Map(vec![
                            (Value::Integer(1.into()), Value::Integer(5_000_000_000u64.into())),
                            (Value::Integer(2.into()), Value::Integer(3_000_000_000_000u64.into())),
                        ]),
                    )]),
                )]),
            ),
        ]);
        let mut buf = Vec::new();
        ciborium::into_writer(&rec, &mut buf).unwrap();
        let info = parse_instance(&buf).unwrap();
        assert_eq!(info.paid_until, Some(1_900_000_000));
        assert_eq!(info.terms.get(&1), Some(&5_000_000_000));
        assert_eq!(info.terms.get(&2), Some(&3_000_000_000_000));
    }

    #[test]
    fn parses_native_balance_empty_denom() {
        use ciborium::Value;
        let rec = Value::Map(vec![(
            Value::Text("balances".into()),
            Value::Map(vec![(Value::Bytes(vec![]), Value::Integer(42_000u64.into()))]),
        )]);
        let mut buf = Vec::new();
        ciborium::into_writer(&rec, &mut buf).unwrap();
        assert_eq!(parse_native_balance(&buf).unwrap(), 42_000);
    }

    #[test]
    fn plan_topup_prefers_cheaper_longer_else_shortest() {
        // hour = 5/hr; month = 2_592_000s priced 1_800_000 → 0.694/s vs hour 0.00139/s.
        // Wait: make month clearly CHEAPER per second than the hour.
        let mut terms = BTreeMap::new();
        terms.insert(1u8, 5_000u128); // hour: 5000/3600 = 1.389/s
        terms.insert(2u8, 1_800_000u128); // month: 1.8M/2.592M = 0.694/s  (cheaper per second)

        // Flush + month affordable + cheaper per second → buy the month.
        assert_eq!(plan_topup(2_000_000, 0, &terms, 2), Some((2, 1)), "cheap month wins");
        // Can't afford the month, can afford an hour → shortest term, 1 hour.
        assert_eq!(plan_topup(10_000, 0, &terms, 2), Some((1, 1)), "scrape an hour");
        // Can't afford even an hour → None (reserve low).
        assert_eq!(plan_topup(100, 0, &terms, 2), None);
        // max_term caps prepay: even flush, max_term=1 never buys the month.
        assert_eq!(plan_topup(2_000_000, 0, &terms, 1), Some((1, 1)), "capped to hour");
        // If the month is NOT cheaper per second, prefer the better-value hour.
        let mut pricey = BTreeMap::new();
        pricey.insert(1u8, 1_000u128); // 0.278/s
        pricey.insert(2u8, 2_592_000u128); // 1.0/s (worse value)
        assert_eq!(plan_topup(5_000_000, 0, &pricey, 2), Some((1, 1)), "hour is better value");
        // Reserve floor respected.
        assert_eq!(plan_topup(5_500, 5_000, &terms, 2), None, "floor leaves 500 < hour");
    }
}
