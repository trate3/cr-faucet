//! Encrypt the reveal-once Monero wallet address to the deployer.
//!
//! The pool's KMS-derived Monero wallet address is also its upstream stratum
//! login — whoever knows it can change the upstream min-payout amount, so it must
//! not leak. ROFL node logs are NOT encrypted at rest (the provider can read the
//! serial console), so we never write the address there in the clear. Instead, on
//! a fresh deploy we encrypt it to the deployer's `age` X25519 recipient (a public
//! key baked into the deploy config) and log only the ciphertext. The deployer —
//! and only the deployer, holding the matching `age` secret key off-box — decrypts
//! it with the standard `age` CLI:
//!
//! ```text
//! echo '<ciphertext-b64>' | base64 -d | age -d -i your-age-key.txt
//! ```
//!
//! No persistence: the address is in the durable on-chain/KMS derivation anyway,
//! and the deployer only needs to capture it once to set up upstream monitoring.

use anyhow::{Context, Result};
use std::io::Write;

/// Encrypt `plaintext` to the `age` X25519 recipient string (`age1…`), returning
/// the binary age ciphertext base64-encoded onto a single line (log-friendly).
pub fn encrypt_to_recipient(recipient: &str, plaintext: &str) -> Result<String> {
    let recipient: age::x25519::Recipient = recipient
        .parse()
        .map_err(|e: &str| anyhow::anyhow!("invalid age recipient: {e}"))?;
    let encryptor =
        age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
            .context("build age encryptor (no recipients?)")?;
    let mut ciphertext = Vec::new();
    let mut writer = encryptor
        .wrap_output(&mut ciphertext)
        .context("wrap age output")?;
    writer
        .write_all(plaintext.as_bytes())
        .context("write plaintext")?;
    writer.finish().context("finish age encryption")?;
    Ok(data_encoding::BASE64.encode(&ciphertext))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// Round-trip: encrypt to a generated recipient, decrypt with its identity.
    #[test]
    fn encrypt_then_decrypt_recovers_plaintext() {
        let id = age::x25519::Identity::generate();
        let recipient = id.to_public();
        let addr = "44AFFq5kSiGBoZ4NMDwYtN18obc8AemS33DBLWs3H7otXft3XjrpDtQGv7SqSsaBYBb98uNbr2VBBEt7f2wfn3RVGQBEP3A";

        let b64 = encrypt_to_recipient(&recipient.to_string(), addr).unwrap();
        // single line, no newlines — fits one log entry
        assert!(!b64.contains('\n'));

        let ciphertext = data_encoding::BASE64.decode(b64.as_bytes()).unwrap();
        let decryptor = age::Decryptor::new(&ciphertext[..]).unwrap();
        let mut out = Vec::new();
        decryptor
            .decrypt(std::iter::once(&id as &dyn age::Identity))
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), addr);
    }

    #[test]
    fn rejects_bad_recipient() {
        assert!(encrypt_to_recipient("not-an-age-key", "x").is_err());
    }

    #[test]
    fn wrong_identity_cannot_decrypt() {
        let id = age::x25519::Identity::generate();
        let other = age::x25519::Identity::generate();
        let b64 = encrypt_to_recipient(&id.to_public().to_string(), "secret").unwrap();
        let ciphertext = data_encoding::BASE64.decode(b64.as_bytes()).unwrap();
        let decryptor = age::Decryptor::new(&ciphertext[..]).unwrap();
        // a different identity must fail to decrypt
        assert!(decryptor
            .decrypt(std::iter::once(&other as &dyn age::Identity))
            .is_err());
    }
}
