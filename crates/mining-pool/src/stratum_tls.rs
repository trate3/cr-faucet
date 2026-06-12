//! Deterministic, KMS-derived self-signed TLS cert for the downstream stratum
//! listener.
//!
//! Purpose: give miners an AUTHENTICATED clearnet endpoint via the ROFL proxy's
//! `passthrough` mode without Tor. The proxy forwards raw TLS (it can't decrypt
//! or impersonate — it doesn't hold the key); miners pin the cert fingerprint
//! (`xmrig --tls-fingerprint <hex>`) so a MITM can't pose as the pool. This is
//! the same identity guarantee the v3 onion provides, carried to clearnet.
//!
//! Deterministic across redeploys: the keypair is derived from the app's stable
//! KMS ed25519 identity and the cert uses FIXED validity dates + serial, so the
//! DER bytes — and thus the fingerprint — never change. Ed25519 signatures are
//! themselves deterministic (RFC 8032), so miners pin once and never re-pin.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

/// Wrap a raw 32-byte Ed25519 seed as a PKCS#8 v1 OneAsymmetricKey (RFC 8410).
/// The 16-byte prefix is constant for Ed25519; the seed follows.
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

pub struct StratumTls {
    pub acceptor: TlsAcceptor,
    /// Lowercase hex SHA-256 of the leaf cert DER — what `xmrig --tls-fingerprint`
    /// pins. Log it so the operator can publish it out-of-band.
    pub fingerprint_sha256_hex: String,
}

/// Build the deterministic self-signed cert + rustls acceptor from a KMS
/// ed25519 seed.
pub fn build(seed32: &[u8; 32]) -> Result<StratumTls> {
    let pkcs8 = ed25519_pkcs8(seed32);
    let key_der = PrivatePkcs8KeyDer::from(pkcs8.clone());
    let key = rcgen::KeyPair::from_pkcs8_der_and_sign_algo(&key_der, &rcgen::PKCS_ED25519)
        .context("rcgen keypair from KMS ed25519 seed")?;

    let mut params = rcgen::CertificateParams::new(vec!["mining-pool".to_string()])
        .context("cert params")?;
    // FIXED validity + serial → deterministic DER → stable fingerprint. Long
    // expiry (year 4096) so miners never have to re-pin.
    params.not_before = rcgen::date_time_ymd(2000, 1, 1);
    params.not_after = rcgen::date_time_ymd(4096, 1, 1);
    params.serial_number = Some(rcgen::SerialNumber::from(1u64));
    let mut dn = rcgen::DistinguishedName::new();
    dn.push(rcgen::DnType::CommonName, "mining-pool");
    params.distinguished_name = dn;

    let cert = params.self_signed(&key).context("self-sign stratum cert")?;
    let cert_der: CertificateDer<'static> = cert.der().clone();

    let fingerprint_sha256_hex = Sha256::digest(cert_der.as_ref())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();

    // rustls 0.23 needs a default crypto provider installed; pick ring to match
    // the rest of the stack. Idempotent.
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    let server_cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], PrivatePkcs8KeyDer::from(pkcs8).into())
        .context("rustls server config")?;

    Ok(StratumTls {
        acceptor: TlsAcceptor::from(Arc::new(server_cfg)),
        fingerprint_sha256_hex,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_fingerprint() {
        let seed = [7u8; 32];
        let a = build(&seed).unwrap();
        let b = build(&seed).unwrap();
        assert_eq!(a.fingerprint_sha256_hex, b.fingerprint_sha256_hex);
        assert_eq!(a.fingerprint_sha256_hex.len(), 64);
    }

    #[test]
    fn different_seed_different_fingerprint() {
        let a = build(&[1u8; 32]).unwrap();
        let b = build(&[2u8; 32]).unwrap();
        assert_ne!(a.fingerprint_sha256_hex, b.fingerprint_sha256_hex);
    }
}
