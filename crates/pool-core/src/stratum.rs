//! Pure helpers for Monero/cryptonote stratum.

/// Offset of the 4-byte `nonce` field inside a Monero block-template hashing
/// blob. xmrig + xmr-stratum-proxy both place it here.
pub const NONCE_OFFSET: usize = 39;
pub const NONCE_LEN: usize = 4;

/// Convert a per-share difficulty to the 4-byte little-endian target that
/// xmrig-style miners expect: `target32 = floor(0xFFFFFFFF / difficulty)`.
/// Hex-encoded as 8 lowercase chars.
pub fn diff_to_target_hex(difficulty: u64) -> String {
    let target = if difficulty == 0 {
        u32::MAX
    } else {
        let t = u32::MAX as u64 / difficulty;
        if t > u32::MAX as u64 { u32::MAX } else { t as u32 }
    };
    hex::encode(target.to_le_bytes())
}

/// Inverse of `diff_to_target_hex`. Returns `u64::MAX` for an all-zero target.
pub fn target_hex_to_diff(target_hex: &str) -> Result<u64, hex::FromHexError> {
    let bytes = hex::decode(target_hex)?;
    let target = match bytes.len() {
        4 => u32::from_le_bytes(bytes.try_into().unwrap()) as u64,
        8 => u64::from_le_bytes(bytes.try_into().unwrap()),
        _ => return Err(hex::FromHexError::InvalidStringLength),
    };
    Ok(if target == 0 { u64::MAX } else { u32::MAX as u64 / target.max(1) })
}

#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error("blob too short: {0} bytes")]
    TooShort(usize),
    #[error("invalid nonce hex")]
    BadHex,
    #[error("nonce must be {NONCE_LEN} bytes")]
    BadNonceLen,
}

/// Take a hex-encoded blob and inject a hex-encoded 4-byte nonce at
/// `NONCE_OFFSET`. Returns the raw assembled bytes (caller hashes these).
pub fn assemble_blob_with_nonce(blob_hex: &str, nonce_hex: &str) -> Result<Vec<u8>, BlobError> {
    let mut blob = hex::decode(blob_hex).map_err(|_| BlobError::BadHex)?;
    let nonce = hex::decode(nonce_hex).map_err(|_| BlobError::BadHex)?;
    if blob.len() < NONCE_OFFSET + NONCE_LEN {
        return Err(BlobError::TooShort(blob.len()));
    }
    if nonce.len() != NONCE_LEN {
        return Err(BlobError::BadNonceLen);
    }
    blob[NONCE_OFFSET..NONCE_OFFSET + NONCE_LEN].copy_from_slice(&nonce);
    Ok(blob)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_target_roundtrip_within_target_precision() {
        // The 32-bit truncated target loses precision proportional to the
        // difficulty. We tolerate that the round-trip lands within the same
        // target bucket (i.e. agrees to within 1 LSB of the target).
        for &d in &[1u64, 100, 5_000, 100_000, 1_000_000] {
            let t = diff_to_target_hex(d);
            let back = target_hex_to_diff(&t).unwrap();
            // tolerance: one target-LSB worth of diff drift
            let bucket = (u32::MAX as u64 / d.max(1)).max(1);
            let tol = u32::MAX as u64 / bucket;
            assert!(
                back.abs_diff(d) <= tol.max(1),
                "diff {} -> target {} -> diff {} (tol {})",
                d, t, back, tol
            );
        }
    }

    #[test]
    fn blob_nonce_injected_at_offset_39() {
        let mut blob = vec![0u8; 76];
        for i in 0..76 {
            blob[i] = i as u8;
        }
        let blob_hex = hex::encode(&blob);
        let out = assemble_blob_with_nonce(&blob_hex, "deadbeef").unwrap();
        assert_eq!(&out[NONCE_OFFSET..NONCE_OFFSET + 4], &[0xde, 0xad, 0xbe, 0xef]);
        for i in 0..NONCE_OFFSET {
            assert_eq!(out[i], i as u8);
        }
        for i in NONCE_OFFSET + 4..76 {
            assert_eq!(out[i], i as u8);
        }
    }

    #[test]
    fn rejects_short_blob() {
        let r = assemble_blob_with_nonce("aabb", "deadbeef");
        assert!(matches!(r, Err(BlobError::TooShort(_))));
    }
}
