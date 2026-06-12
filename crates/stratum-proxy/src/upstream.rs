//! Upstream pool client. We hold one logical connection to the upstream pool
//! on behalf of the whole downstream miner population. The pool sees us as one
//! big worker logged in with the operator's XMR address.
//!
//! Supports both plain TCP and TLS via the URL scheme:
//!   stratum+tcp://host:port   plain
//!   stratum+ssl://host:port   TLS (used by e.g. pool.hashvault.pro:443)
//! For backwards compatibility, `tcp://` and `ssl://` also work.

use crate::jobs::{JobStore, UpstreamJob};
use crate::protocol::{ErrorObj, Request};
use anyhow::{anyhow, Context, Result};
use pool_core::config::UpstreamConfig;
use pool_core::metrics::Metrics;
use pool_core::stratum::target_hex_to_diff;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use tokio_rustls::TlsConnector;
use tokio_socks::tcp::Socks5Stream;
use tracing::{info, warn};

/// A share to forward upstream because it meets upstream difficulty.
#[derive(Debug, Clone)]
pub struct UpstreamSubmit {
    pub job_id: String,
    pub nonce_hex: String,
    pub result_hex: String,
}

#[derive(Clone)]
pub struct UpstreamClient {
    submit_tx: mpsc::Sender<UpstreamSubmit>,
}

impl UpstreamClient {
    pub async fn submit(&self, s: UpstreamSubmit) {
        let _ = self.submit_tx.send(s).await;
    }
}

/// Run the upstream client in a background task. Returns the client handle
/// and the task's JoinHandle.
pub fn spawn(
    cfg: UpstreamConfig,
    jobs: JobStore,
    metrics: Arc<Metrics>,
) -> (UpstreamClient, tokio::task::JoinHandle<()>) {
    let (submit_tx, submit_rx) = mpsc::channel::<UpstreamSubmit>(1024);
    let client = UpstreamClient { submit_tx };
    let handle = tokio::spawn(run_loop(cfg, jobs, metrics, submit_rx));
    (client, handle)
}

/// Upper bound on the reconnect delay. The pool is non-functional while
/// disconnected, so we want to retry frequently — but not so frequently
/// that a stuck upstream gives us a 5-Hz log flood.
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(10);
/// First-failure delay. Doubles each subsequent failure up to the cap.
const RECONNECT_BASE_DELAY_MS: u64 = 500;
/// A session that stayed connected for at least this long counts as
/// "healthy" — its eventual end resets the backoff counter to 1 (first
/// failure of a new incident). Anything shorter is treated as a flap and
/// keeps incrementing.
const HEALTHY_SESSION_THRESHOLD: Duration = Duration::from_secs(30);

/// Exponential backoff with ±20% jitter. Capped at [`RECONNECT_MAX_DELAY`].
///
/// failures=1 → ~500 ms, =2 → ~1 s, =3 → ~2 s, =4 → ~4 s, =5 → ~8 s,
/// ≥6 → ~10 s (cap). Jitter is multiplicative in [0.8, 1.2). Minimum
/// returned delay is 50 ms so a degenerate jitter draw can't busy-loop.
fn reconnect_delay(consecutive_failures: u32) -> Duration {
    if consecutive_failures == 0 {
        return Duration::from_millis(0);
    }
    let exp = consecutive_failures.saturating_sub(1).min(30);
    let raw_ms = RECONNECT_BASE_DELAY_MS.saturating_mul(1u64 << exp);
    let capped_ms = raw_ms.min(RECONNECT_MAX_DELAY.as_millis() as u64);
    let jitter = 0.8 + 0.4 * rand::random::<f64>();
    let jittered = ((capped_ms as f64) * jitter) as u64;
    Duration::from_millis(jittered.max(50))
}

async fn run_loop(
    cfg: UpstreamConfig,
    jobs: JobStore,
    metrics: Arc<Metrics>,
    mut submit_rx: mpsc::Receiver<UpstreamSubmit>,
) {
    loop {
        let session_start = std::time::Instant::now();
        let result = try_session(&cfg, &jobs, &metrics, &mut submit_rx).await;
        let was_connected = metrics.mark_upstream_disconnected();
        let healthy = was_connected && session_start.elapsed() >= HEALTHY_SESSION_THRESHOLD;

        // Healthy session that just ended → reset backoff to "first failure
        // of a new incident". Flap (logged in but dropped quickly) or never
        // got to login at all → keep growing.
        let failures = if healthy {
            metrics
                .upstream_consecutive_failures
                .store(1, std::sync::atomic::Ordering::Relaxed);
            1
        } else {
            metrics.record_upstream_failure()
        };
        match result {
            Ok(()) => warn!(failures, "upstream session ended; reconnecting"),
            Err(e) => warn!(error=%e, failures, "upstream session failed; reconnecting"),
        }
        let delay = reconnect_delay(failures);
        tokio::time::sleep(delay).await;
    }
}

/// Parsed connection target.
#[derive(Debug, Clone)]
struct Endpoint {
    host: String,
    port: u16,
    use_tls: bool,
}

fn parse_endpoint(url: &str) -> Result<Endpoint> {
    let (use_tls, rest) = if let Some(r) = url.strip_prefix("stratum+ssl://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("stratum+tcp://") {
        (false, r)
    } else if let Some(r) = url.strip_prefix("ssl://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("tcp://") {
        (false, r)
    } else {
        // No scheme: assume plain TCP for backwards compat.
        (false, url)
    };
    let (host, port) = rest
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("upstream url missing port: {url}"))?;
    let port: u16 = port.parse().context("upstream port not a number")?;
    Ok(Endpoint {
        host: host.to_string(),
        port,
        use_tls,
    })
}

async fn try_session(
    cfg: &UpstreamConfig,
    jobs: &JobStore,
    metrics: &Metrics,
    submit_rx: &mut mpsc::Receiver<UpstreamSubmit>,
) -> Result<()> {
    let endpoint = parse_endpoint(&cfg.url)?;
    let target = format!("{}:{}", endpoint.host, endpoint.port);
    info!(
        host = %endpoint.host,
        port = endpoint.port,
        tls = endpoint.use_tls,
        socks5h = cfg.socks5h_proxy.as_deref().unwrap_or("(direct)"),
        pinned = cfg.tls_pin_sha256.is_some(),
        "upstream connecting"
    );

    // Step 1: get a TCP stream, either direct or via SOCKS5h.
    let sock = if let Some(proxy_url) = cfg.socks5h_proxy.as_deref() {
        let proxy_addr = proxy_url
            .trim_start_matches("socks5h://")
            .trim_start_matches("socks5://");
        let s = Socks5Stream::connect(proxy_addr, target.as_str())
            .await
            .context("SOCKS5h dial")?;
        s.into_inner()
    } else {
        TcpStream::connect((endpoint.host.as_str(), endpoint.port))
            .await
            .context("direct TCP connect")?
    };
    sock.set_nodelay(true).ok();

    // Step 2: optionally wrap in TLS.
    if endpoint.use_tls {
        let connector = tls_connector(cfg)?;
        let dns_name = ServerName::try_from(endpoint.host.clone())
            .map_err(|e| anyhow!("bad TLS server name {}: {e}", endpoint.host))?;
        let tls = connector
            .connect(dns_name, sock)
            .await
            .context("TLS handshake")?;
        let (rd, wr) = tokio::io::split(tls);
        run_session_io(rd, wr, cfg, jobs, metrics, submit_rx).await
    } else {
        let (rd, wr) = tokio::io::split(sock);
        run_session_io(rd, wr, cfg, jobs, metrics, submit_rx).await
    }
}

fn tls_connector(cfg: &UpstreamConfig) -> Result<TlsConnector> {
    install_crypto_provider();
    let builder = ClientConfig::builder();
    let config = if let Some(pin_hex) = cfg.tls_pin_sha256.as_deref() {
        // Strongest mode: accept *only* the leaf cert whose SHA-256 matches.
        let pin = parse_fingerprint(pin_hex)
            .with_context(|| format!("tls_pin_sha256 not a 32-byte hex: {pin_hex:?}"))?;
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedVerifier { pin }))
            .with_no_client_auth()
    } else {
        // No pin: match the xmrig convention for stratum-over-TLS. The
        // Monero-pool ecosystem ships self-signed certs (HashVault's
        // `CN=HashVault` doesn't even match the hostname) so CA-validation
        // is not viable. We get encryption on the wire but no identity
        // proof. A determined active MITM could steal our submitted
        // shares; if that's in your threat model, set `tls_pin_sha256`.
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyVerifier))
            .with_no_client_auth()
    };
    Ok(TlsConnector::from(Arc::new(config)))
}

/// Accept any server cert. Same default as xmrig: encrypt the connection
/// without authenticating the peer. See [`tls_connector`] for the
/// rationale.
#[derive(Debug)]
struct AcceptAnyVerifier;

impl ServerCertVerifier for AcceptAnyVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, tokio_rustls::rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ED25519,
        ]
    }
}

/// Accept a leaf cert iff its SHA-256 DER fingerprint matches the pin. The
/// hostname is *not* verified — we identify the pool exclusively by the
/// fingerprint the operator put in config. This is the same trust model
/// xmrig uses with `--tls-fingerprint`.
#[derive(Debug)]
struct PinnedVerifier {
    pin: [u8; 32],
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, tokio_rustls::rustls::Error> {
        let actual = Sha256::digest(end_entity.as_ref());
        if actual[..] == self.pin {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(tokio_rustls::rustls::Error::General(format!(
                "tls cert fingerprint mismatch: expected {} got {}",
                hex::encode(self.pin),
                hex::encode(actual)
            )))
        }
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ED25519,
        ]
    }
}

fn parse_fingerprint(s: &str) -> Result<[u8; 32]> {
    let cleaned: String = s
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ':')
        .collect();
    let bytes = hex::decode(cleaned).context("not hex")?;
    if bytes.len() != 32 {
        anyhow::bail!("expected 32 bytes (SHA-256), got {}", bytes.len());
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// rustls 0.23 requires an explicitly-installed default crypto provider.
/// We pick `ring` (built in via the `tokio-rustls/ring` feature). Idempotent
/// across calls; only the first install wins.
fn install_crypto_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    });
}

async fn run_session_io<R, W>(
    rd: R,
    mut wr: W,
    cfg: &UpstreamConfig,
    jobs: &JobStore,
    metrics: &Metrics,
    submit_rx: &mut mpsc::Receiver<UpstreamSubmit>,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut rd = BufReader::new(rd);
    let login_id = 1u64;
    let login = Request {
        id: json!(login_id),
        jsonrpc: Some("2.0".into()),
        method: "login".into(),
        params: json!({
            "login": cfg.user,
            "pass": cfg.password,
            "agent": "tiny-pool/0.1",
        }),
    };
    write_req(&mut wr, &login).await?;

    // The session id the upstream assigns us in the login response. Monero
    // stratum requires it back in every `submit` so the pool can match the
    // share to our connection — an empty/absent id makes HashVault reject
    // every share. Captured from the login `result.id` below.
    let mut session_id = String::new();
    let mut line = String::new();
    loop {
        tokio::select! {
            biased;
            n = rd.read_line(&mut line) => {
                let n = n?;
                if n == 0 {
                    return Err(anyhow!("upstream closed"));
                }
                let raw = line.trim().to_owned();
                line.clear();
                let v: Value = match serde_json::from_str(&raw) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(error=%e, raw=%raw, "bad upstream json");
                        continue;
                    }
                };
                if v.get("method").and_then(|m| m.as_str()) == Some("job") {
                    if let Some(p) = v.get("params") {
                        if let Some(job) = parse_job(p) {
                            jobs.publish(job);
                        }
                    }
                } else if v.get("id") == Some(&json!(login_id)) {
                    if let Some(result) = v.get("result") {
                        info!(?result, "upstream login ok");
                        metrics.mark_upstream_connected();
                        // Capture our upstream session id — required in every
                        // `submit` (see session_id decl). Without it HashVault
                        // rejects all shares.
                        if let Some(id) = result.get("id").and_then(|x| x.as_str()) {
                            session_id = id.to_owned();
                        }
                        if let Some(job) = result.get("job") {
                            if let Some(j) = parse_job(job) {
                                jobs.publish(j);
                            }
                        }
                    } else if let Some(err) = v.get("error") {
                        let e: ErrorObj = serde_json::from_value(err.clone())
                            .unwrap_or(ErrorObj { code: -1, message: "login failed".into() });
                        return Err(anyhow!("upstream login failed: {} {}", e.code, e.message));
                    }
                } else if v
                    .get("id")
                    .and_then(|x| x.as_str())
                    .is_some_and(|s| s.starts_with("submit:"))
                {
                    // Responses to our `submit` calls — id is the
                    // "submit:<job_id>" sentinel we set when forwarding.
                    //
                    // HashVault sends an ACCEPT as
                    //   {"result":{"status":"OK"},"error":null}
                    // and a REJECT as {"error":{code,message}}. `get("error")`
                    // returns Some(Null) for an explicit `"error": null`, so we
                    // MUST treat a null error as "no error" — otherwise every
                    // accept is miscounted as a reject (submit_accepts stuck at 0
                    // while shares were really being accepted, logged err=Null).
                    if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
                        metrics.record_upstream_submit_reject();
                        warn!(?err, "upstream rejected submit");
                    } else if v.get("result").is_some() {
                        metrics.record_upstream_submit_accept();
                        info!("upstream accepted submit");
                    }
                }
            },
            Some(s) = submit_rx.recv() => {
                let req = Request {
                    id: json!(format!("submit:{}", s.job_id)),
                    jsonrpc: Some("2.0".into()),
                    method: "submit".into(),
                    params: json!({
                        "id": "",
                        "job_id": s.job_id,
                        "nonce": s.nonce_hex,
                        "result": s.result_hex,
                    }),
                };
                write_req(&mut wr, &req).await?;
            },
        }
    }
}

fn parse_job(p: &Value) -> Option<UpstreamJob> {
    let job_id = p.get("job_id")?.as_str()?.to_owned();
    let blob_hex = p.get("blob")?.as_str()?.to_owned();
    let seed_hex = p
        .get("seed_hash")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_owned();
    let target_hex = p.get("target")?.as_str()?.to_owned();
    let upstream_diff = target_hex_to_diff(&target_hex).unwrap_or(1);
    let height = p.get("height").and_then(|x| x.as_u64());
    Some(UpstreamJob {
        job_id,
        blob_hex,
        seed_hex,
        upstream_target_hex: target_hex,
        upstream_diff,
        height,
    })
}

async fn write_req<W: AsyncWriteExt + Unpin>(w: &mut W, req: &Request) -> Result<()> {
    let mut s = serde_json::to_string(req)?;
    s.push('\n');
    w.write_all(s.as_bytes()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_url_schemes() {
        let e = parse_endpoint("stratum+ssl://pool.hashvault.pro:443").unwrap();
        assert_eq!(e.host, "pool.hashvault.pro");
        assert_eq!(e.port, 443);
        assert!(e.use_tls);

        let e = parse_endpoint("stratum+tcp://pool.example:3333").unwrap();
        assert!(!e.use_tls);
        assert_eq!(e.port, 3333);

        let e = parse_endpoint("ssl://host:5555").unwrap();
        assert!(e.use_tls);

        let e = parse_endpoint("tcp://host:3333").unwrap();
        assert!(!e.use_tls);

        // No scheme defaults to plain (back-compat).
        let e = parse_endpoint("host:3333").unwrap();
        assert!(!e.use_tls);

        assert!(parse_endpoint("ssl://no-port").is_err());
    }

    #[test]
    fn fingerprint_parses_colon_and_case_insensitive() {
        let a = parse_fingerprint(
            "aa:BB:cc:DD:ee:FF:00:11:22:33:44:55:66:77:88:99:aa:bb:cc:dd:ee:ff:00:11:22:33:44:55:66:77:88:99",
        )
        .unwrap();
        let b = parse_fingerprint(
            "AABBCCDDEEFF00112233445566778899aabbccddeeff00112233445566778899",
        )
        .unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn fingerprint_rejects_wrong_length() {
        assert!(parse_fingerprint("dead").is_err());
        assert!(parse_fingerprint("zz").is_err()); // not hex
    }

    #[test]
    fn reconnect_delay_grows_then_caps_at_10s() {
        // Zero failures shouldn't sleep at all (we don't sleep before the
        // very first connect attempt).
        assert_eq!(reconnect_delay(0), Duration::from_millis(0));

        // Failures 1..=5 should produce monotonically-growing delays inside
        // their expected nominal-±20% window.
        let nominal_ms = [500u64, 1000, 2000, 4000, 8000];
        for (i, &nom) in nominal_ms.iter().enumerate() {
            let d = reconnect_delay((i + 1) as u32);
            let ms = d.as_millis() as u64;
            let lo = (nom as f64 * 0.8) as u64;
            let hi = (nom as f64 * 1.2) as u64 + 1;
            assert!(
                ms >= lo && ms <= hi,
                "failure {} nominal {} got {}",
                i + 1,
                nom,
                ms
            );
        }

        // From failure 6 onward we cap at 10s ±20% — never above ~12s.
        for n in [6u32, 7, 20, 1_000_000] {
            let d = reconnect_delay(n);
            assert!(
                d <= Duration::from_millis(12_000),
                "failure {} returned {:?}",
                n,
                d
            );
            assert!(
                d >= Duration::from_millis(7_900),
                "failure {} returned {:?}",
                n,
                d
            );
        }
    }
}
