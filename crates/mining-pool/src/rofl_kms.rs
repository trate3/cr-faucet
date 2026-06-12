//! Client for the in-TEE `rofl-appd` daemon's KMS API.
//!
//! Endpoint we use:
//!     POST /rofl/v1/keys/generate   {"key_id": "...", "kind": "secp256k1" | "raw-256"}
//! Response: {"key": "<hex>"}
//!
//! The daemon listens on a unix socket at `/run/rofl-appd.sock`, which must
//! be mounted into our container via the compose.yaml `volumes` block.
//!
//! We talk raw HTTP/1.1 over the socket — the API surface is two requests so
//! adding `hyperlocal` would be more overhead than it's worth. Connection:
//! close + read-to-EOF keeps the parser tiny.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Default socket path; overridable for local-dev / tests.
pub const DEFAULT_SOCKET: &str = "/run/rofl-appd.sock";

#[derive(Debug, Clone, Copy)]
pub enum KeyKind {
    /// 32-byte secp256k1 private key. Suitable for ECDSA / EIP-712 signing.
    Secp256k1,
    /// 32-byte ed25519 seed. Used by Tor v3 hidden services (we expand
    /// the seed locally before writing `hs_ed25519_secret_key`).
    Ed25519,
    /// 32 bytes of uniformly-random material. Suitable as seed input.
    Raw256,
}

impl KeyKind {
    fn as_str(self) -> &'static str {
        match self {
            KeyKind::Secp256k1 => "secp256k1",
            KeyKind::Ed25519 => "ed25519",
            KeyKind::Raw256 => "raw-256",
        }
    }
}

#[derive(Deserialize)]
struct KeyResp {
    key: String,
}

/// Returns `true` iff the appd socket is present — i.e. we're running inside
/// a ROFL TEE. Used to decide whether to use KMS-derived keys or fall back
/// to a local-dev key file.
pub fn appd_available() -> bool {
    appd_available_at(DEFAULT_SOCKET)
}

pub fn appd_available_at(path: &str) -> bool {
    Path::new(path).exists()
}

/// Ask the in-TEE KMS for a deterministic key. `key_id` is the domain
/// separator — using the same id from the same app yields the same key
/// across restarts; different ids yield independent keys.
pub async fn derive_key(key_id: &str, kind: KeyKind) -> Result<Vec<u8>> {
    derive_key_at(DEFAULT_SOCKET, key_id, kind).await
}

pub async fn derive_key_at(socket: &str, key_id: &str, kind: KeyKind) -> Result<Vec<u8>> {
    let body =
        serde_json::to_string(&serde_json::json!({"key_id": key_id, "kind": kind.as_str()}))?;
    let req = format!(
        "POST /rofl/v1/keys/generate HTTP/1.1\r\n\
         Host: localhost\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    );
    let mut sock = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connecting to rofl-appd socket at {socket}"))?;
    sock.write_all(req.as_bytes()).await.context("send")?;
    let mut raw = Vec::new();
    sock.read_to_end(&mut raw).await.context("recv")?;
    let resp = String::from_utf8(raw).context("appd response not utf-8")?;

    // Status line: "HTTP/1.1 200 OK"
    let mut lines = resp.split("\r\n");
    let status = lines.next().unwrap_or("");
    let mut parts = status.split_whitespace();
    let _version = parts.next();
    let code = parts.next().unwrap_or("");
    let split_idx = resp
        .find("\r\n\r\n")
        .ok_or_else(|| anyhow!("appd response had no body separator: {resp}"))?;
    let body = &resp[split_idx + 4..];
    if code != "200" {
        bail!("appd /keys/generate returned status {code}: {body}");
    }
    let parsed: KeyResp = serde_json::from_str(body)
        .with_context(|| format!("parsing appd response body: {body}"))?;
    let hex_str = parsed.key.strip_prefix("0x").unwrap_or(&parsed.key);
    let bytes = hex::decode(hex_str).context("decoding key hex")?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stand up a minimal HTTP server on a unix socket, return the path it's
    /// bound to. The server replies `{"key": "<hex>"}` for any POST.
    async fn fake_appd(reply_hex: &'static str) -> String {
        let tmp = format!(
            "{}/test-appd-{}.sock",
            std::env::temp_dir().display(),
            std::process::id() as u64 + reply_hex.len() as u64 // crude uniqueness
        );
        let _ = std::fs::remove_file(&tmp);
        let listener = tokio::net::UnixListener::bind(&tmp).unwrap();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => break,
                };
                let mut buf = vec![0u8; 4096];
                let n = s.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    continue;
                }
                let body = format!("{{\"key\":\"{reply_hex}\"}}");
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
                let _ = s.shutdown().await;
            }
        });
        tmp
    }

    #[tokio::test]
    async fn parses_reply_hex_with_and_without_prefix() {
        let p = fake_appd("deadbeef00000000000000000000000000000000000000000000000000000000").await;
        let k = derive_key_at(&p, "x", KeyKind::Secp256k1).await.unwrap();
        assert_eq!(k.len(), 32);
        assert_eq!(k[0], 0xde);

        let p2 = fake_appd("0xfeedface00000000000000000000000000000000000000000000000000000000").await;
        let k2 = derive_key_at(&p2, "y", KeyKind::Raw256).await.unwrap();
        assert_eq!(k2[0], 0xfe);
    }
}
