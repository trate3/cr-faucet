//! Local TLS test harness: the REAL run_listener + TLS mux, fed by HashVault for
//! jobs, served on 127.0.0.1:3334 — NO ROFL proxy in the path. Lets us test
//! whether xmrig can do TLS + cert-pinning against our listener directly, to
//! isolate the pool's TLS code from the rofl.app passthrough/SNI layer.
//!
//!   cargo run -q -p stratum-proxy --example local_tls_pipeline --no-default-features
//!   # CERT_ALG=ed25519 (default, matches the deployed pool) | p256
//!   xmrig -o 127.0.0.1:3334 --tls --tls-fingerprint <printed> -u 0xMINER -p t --coin monero

use anyhow::Result;
use pool_core::config::{StratumConfig, UpstreamConfig};
use pool_core::metrics::Metrics;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;
use stratum_proxy::session::{run_listener, ProxyServices};
use stratum_proxy::{spawn_upstream, InMemorySink, JobStore};
use tokio_rustls::rustls::pki_types::PrivatePkcs8KeyDer;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;
use tracing::info;

fn ed25519_pkcs8(seed32: &[u8; 32]) -> Vec<u8> {
    const PREFIX: [u8; 16] = [
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ];
    let mut der = Vec::with_capacity(48);
    der.extend_from_slice(&PREFIX);
    der.extend_from_slice(seed32);
    der
}

fn build_tls(alg: &str) -> Result<(TlsAcceptor, String)> {
    let (cert_der, key_pkcs8) = if alg == "p256" {
        let ck = rcgen::generate_simple_self_signed(vec!["mining-pool".to_string()])?;
        (ck.cert.der().clone(), ck.key_pair.serialize_der())
    } else {
        // Ed25519, mirroring the deployed pool's stratum_tls.rs exactly.
        let seed = [7u8; 32];
        let pkcs8 = ed25519_pkcs8(&seed);
        let key = rcgen::KeyPair::from_pkcs8_der_and_sign_algo(
            &PrivatePkcs8KeyDer::from(pkcs8.clone()),
            &rcgen::PKCS_ED25519,
        )?;
        let mut params = rcgen::CertificateParams::new(vec!["mining-pool".to_string()])?;
        params.not_before = rcgen::date_time_ymd(2000, 1, 1);
        params.not_after = rcgen::date_time_ymd(4096, 1, 1);
        let cert = params.self_signed(&key)?;
        (cert.der().clone(), pkcs8)
    };
    let fp = Sha256::digest(cert_der.as_ref())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    let cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], PrivatePkcs8KeyDer::from(key_pkcs8).into())?;
    Ok((TlsAcceptor::from(Arc::new(cfg)), fp))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let alg = std::env::var("CERT_ALG").unwrap_or_else(|_| "ed25519".into());
    let (acceptor, fingerprint) = build_tls(&alg)?;
    println!("\n=== LOCAL TLS POOL ===");
    println!("cert alg:    {alg}");
    println!("fingerprint: {fingerprint}");
    println!("connect:     xmrig -o 127.0.0.1:3334 --tls --tls-fingerprint {fingerprint} -u 0xbEEF -p t --coin monero\n");

    let upstream_cfg = UpstreamConfig {
        url: "stratum+ssl://pool.hashvault.pro:443".into(),
        user: "44AFFq5kSiGBoZ4NMDwYtN18obc8AemS33DBLWs3H7otXft3XjrpDtQGv7SqSsaBYBb98uNbr2VBBEt7f2wfn3RVGQBEP3A".into(),
        password: "tiny-pool-test".into(),
        keepalive_secs: 60,
        socks5h_proxy: None,
        tls_pin_sha256: None,
        login_address_network: None,
        direct: false,
    };
    let jobs = JobStore::new();
    let (upstream, _u) = spawn_upstream(upstream_cfg, jobs.clone(), Arc::new(Metrics::new()));

    let stratum_cfg = StratumConfig {
        bind: "127.0.0.1:3334".into(),
        tls_cert: None,
        tls_key: None,
        min_share_difficulty: 1000,
        target_seconds_per_share: 10,
        max_submits_per_second: 100,
        verification_warmup: 5,
        verification_sample_rate: 0.10,
        share_grace_secs: 20,
        idle_timeout_secs: 600,
        vardiff: Default::default(),
    };
    let verifier = Arc::new(randomx_verify::StubVerifier);
    let services = Arc::new(ProxyServices {
        cfg: stratum_cfg,
        jobs: jobs.clone(),
        upstream,
        verifier,
        sink: Arc::new(InMemorySink::default()),
        tls_acceptor: Some(acceptor),
    });
    tokio::spawn(async move {
        if let Err(e) = run_listener(services).await {
            tracing::error!(error=%e, "listener died");
        }
    });

    let mut last = String::new();
    loop {
        tokio::time::sleep(Duration::from_secs(3)).await;
        if let Some(j) = jobs.current() {
            if j.job_id != last {
                info!(job_id=%j.job_id, height=?j.height, "serving HashVault job to local miners");
                last = j.job_id;
            }
        }
    }
}
