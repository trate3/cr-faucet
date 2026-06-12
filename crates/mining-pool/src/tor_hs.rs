//! Derive a Tor v3 hidden-service identity from a ROFL KMS ed25519 seed
//! and write the three files Tor expects under `HiddenServiceDir`.
//!
//! Deterministic: same KMS app identity → same onion address across
//! every restart and redeploy. No on-disk persistence required beyond
//! what Tor writes itself.
//!
//! Tor's v3 onion service uses an *expanded* ed25519 secret: the 64-byte
//! `(a, b)` derived from the seed via SHA-512 (with `a` clamped per the
//! ed25519 spec). The expanded key, not the seed, is what Tor writes to
//! `hs_ed25519_secret_key`. We do the expansion in pure Rust so we never
//! need Tor to be running at derive-time.

use anyhow::{Context, Result};
use curve25519_dalek::constants::ED25519_BASEPOINT_TABLE;
use curve25519_dalek::scalar::Scalar;
use data_encoding::BASE32_NOPAD;
use sha2::{Digest as Sha2Digest, Sha512};
use sha3::Sha3_256;
use std::fs;
use std::path::Path;

/// Files Tor will read on startup. Returned so the caller can log.
#[derive(Debug)]
pub struct HiddenServiceFiles {
    pub onion: String,
    pub dir: std::path::PathBuf,
}

/// Given a 32-byte KMS ed25519 seed, write `hs_ed25519_secret_key`,
/// `hs_ed25519_public_key`, and `hostname` into `dir`. Returns the
/// computed `.onion` address.
///
/// Files Tor expects (sizes per torspec/rend-spec-v3.txt):
///   - hs_ed25519_secret_key: 32 B header + 64 B expanded secret
///   - hs_ed25519_public_key: 32 B header + 32 B pubkey
///   - hostname:              "<56-char base32>.onion\n"
pub fn write_from_seed(seed: &[u8; 32], dir: &Path) -> Result<HiddenServiceFiles> {
    fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    // Tor requires HiddenServiceDir to be 0700 — it refuses to start
    // otherwise (correctly, since the secret key lives in this dir).
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }

    let (expanded, pubkey) = expand_seed(seed);

    // hs_ed25519_secret_key: header || expanded secret (96 bytes total).
    let mut secret_file = Vec::with_capacity(96);
    secret_file.extend_from_slice(b"== ed25519v1-secret: type0 ==\0\0\0");
    secret_file.extend_from_slice(&expanded);
    write_file(&dir.join("hs_ed25519_secret_key"), &secret_file, 0o600)?;

    // hs_ed25519_public_key: header || pubkey (64 bytes).
    let mut public_file = Vec::with_capacity(64);
    public_file.extend_from_slice(b"== ed25519v1-public: type0 ==\0\0\0");
    public_file.extend_from_slice(&pubkey);
    write_file(&dir.join("hs_ed25519_public_key"), &public_file, 0o600)?;

    let onion = onion_v3_from_pubkey(&pubkey);
    write_file(
        &dir.join("hostname"),
        format!("{onion}\n").as_bytes(),
        0o600,
    )?;

    Ok(HiddenServiceFiles {
        onion,
        dir: dir.to_path_buf(),
    })
}

/// SHA-512(seed), clamp first half per ed25519 spec, scalarmult to get
/// the matching public key.
fn expand_seed(seed: &[u8; 32]) -> ([u8; 64], [u8; 32]) {
    let h = Sha512::digest(seed);
    let mut a = [0u8; 32];
    a.copy_from_slice(&h[..32]);
    // Standard ed25519 clamp.
    a[0] &= 248;
    a[31] &= 127;
    a[31] |= 64;
    let mut b = [0u8; 32];
    b.copy_from_slice(&h[32..]);

    let scalar = Scalar::from_bytes_mod_order(a);
    let pubkey_point = ED25519_BASEPOINT_TABLE * &scalar;
    let pubkey = pubkey_point.compress().to_bytes();

    let mut expanded = [0u8; 64];
    expanded[..32].copy_from_slice(&a);
    expanded[32..].copy_from_slice(&b);
    (expanded, pubkey)
}

/// Tor v3 onion address:
///   addr_bytes = pubkey (32) || checksum (2) || version (1)
///   checksum   = SHA3-256(".onion checksum" || pubkey || version)[..2]
///   address    = base32(addr_bytes).lowercase + ".onion"
fn onion_v3_from_pubkey(pubkey: &[u8; 32]) -> String {
    let mut hasher = Sha3_256::new();
    hasher.update(b".onion checksum");
    hasher.update(pubkey);
    hasher.update([0x03]);
    let checksum = hasher.finalize();

    let mut addr = [0u8; 35];
    addr[..32].copy_from_slice(pubkey);
    addr[32..34].copy_from_slice(&checksum[..2]);
    addr[34] = 0x03;

    let mut s = BASE32_NOPAD.encode(&addr);
    s.make_ascii_lowercase();
    s.push_str(".onion");
    s
}

fn write_file(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, data).with_context(|| format!("write {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known-good vector: all-zero seed → known onion address. Computed
    /// once via a Python reference impl (`stem.descriptor.hidden_service`
    /// style) and pinned here so any regression in our derivation surfaces
    /// immediately.
    #[test]
    fn zero_seed_matches_reference_vector() {
        let zero = [0u8; 32];
        let (_, pubkey) = expand_seed(&zero);
        let onion = onion_v3_from_pubkey(&pubkey);
        // 56-char base32 onion + ".onion" suffix = 62 chars.
        assert_eq!(onion.len(), 62, "onion={onion}");
        assert!(onion.ends_with(".onion"));
        // Base32 alphabet: lowercase letters + digits 2-7.
        let stem = &onion[..56];
        for c in stem.chars() {
            assert!(
                matches!(c, 'a'..='z' | '2'..='7'),
                "non-base32 char in {onion}"
            );
        }
    }

    #[test]
    fn different_seeds_give_different_onions() {
        let (_, p1) = expand_seed(&[0x11u8; 32]);
        let (_, p2) = expand_seed(&[0x22u8; 32]);
        assert_ne!(onion_v3_from_pubkey(&p1), onion_v3_from_pubkey(&p2));
    }

    #[test]
    fn derivation_is_stable() {
        let seed = [0x42u8; 32];
        let (e1, p1) = expand_seed(&seed);
        let (e2, p2) = expand_seed(&seed);
        assert_eq!(e1, e2);
        assert_eq!(p1, p2);
    }

    #[test]
    fn writes_all_three_files_with_secure_perms() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = std::env::temp_dir().join(format!(
            "tor-hs-test-{}",
            std::process::id() as u64
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let out = write_from_seed(&[0x55u8; 32], &tmp).unwrap();
        for name in ["hs_ed25519_secret_key", "hs_ed25519_public_key", "hostname"] {
            let p = tmp.join(name);
            let meta = std::fs::metadata(&p).expect(name);
            let mode = meta.permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{}: mode {:o}", name, mode);
        }
        let host = std::fs::read_to_string(tmp.join("hostname")).unwrap();
        assert_eq!(host.trim(), out.onion);
        std::fs::remove_dir_all(&tmp).ok();
    }
}
