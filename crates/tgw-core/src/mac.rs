//! Keyed integrity primitives built on `sha2` (already a workspace dependency).
//!
//! Two things live here, deliberately tiny and dependency-free:
//! - [`hmac_sha256`] — HMAC-SHA256 (RFC 2104), validated against RFC 4231 test vectors.
//! - [`hkdf_sha256`] — HKDF-SHA256 (RFC 5869) restricted to a single 32-byte output block,
//!   which is all a subkey derivation needs; validated against RFC 5869 Test Case 1.
//!
//! These back the per-datagram DATA-frame integrity tag (`wire.rs`): a subkey is derived
//! from the pre-shared [`crate::key::Key`] with a domain-separation label so the integrity
//! MAC never shares key material with the XChaCha20-Poly1305 envelope/receipt layer.

use sha2::{Digest, Sha256};

/// SHA-256 block size in bytes (the HMAC ipad/opad width).
const BLOCK_LEN: usize = 64;
/// SHA-256 output length in bytes.
const HASH_LEN: usize = 32;

/// HMAC-SHA256 of `msg` under `key` (RFC 2104). Constant-length 32-byte output.
pub(crate) fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; HASH_LEN] {
    // Keys longer than the block are first hashed down (RFC 2104 §2).
    let mut block = [0u8; BLOCK_LEN];
    if key.len() > BLOCK_LEN {
        let digest = Sha256::digest(key);
        block[..HASH_LEN].copy_from_slice(&digest);
    } else {
        block[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0x36u8; BLOCK_LEN];
    let mut opad = [0x5cu8; BLOCK_LEN];
    for i in 0..BLOCK_LEN {
        ipad[i] ^= block[i];
        opad[i] ^= block[i];
    }

    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(msg);
    let inner_digest = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_digest);
    outer.finalize().into()
}

/// HKDF-SHA256 (RFC 5869) producing a single 32-byte output block.
///
/// Extract-then-expand: `PRK = HMAC(salt, ikm)`, then `OKM = HMAC(PRK, info || 0x01)`.
/// One block (32 bytes) is sufficient for every subkey we derive, so the counter never
/// advances past `0x01`.
pub(crate) fn hkdf_sha256(salt: &[u8], ikm: &[u8], info: &[u8]) -> [u8; HASH_LEN] {
    let prk = hmac_sha256(salt, ikm);
    let mut expand_input = Vec::with_capacity(info.len() + 1);
    expand_input.extend_from_slice(info);
    expand_input.push(0x01);
    hmac_sha256(&prk, &expand_input)
}

/// Constant-time equality for two byte slices of equal length.
///
/// Returns `false` immediately on a length mismatch (length is not secret here — a truncated
/// datagram is a framing error, not a timing oracle), and otherwise compares every byte with
/// no early exit so a forged tag cannot be recovered byte-by-byte via timing.
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
            .collect()
    }

    #[test]
    fn hmac_matches_rfc4231_case1() {
        // RFC 4231 Test Case 1: key = 0x0b × 20, data = "Hi There".
        let key = [0x0bu8; 20];
        let tag = hmac_sha256(&key, b"Hi There");
        assert_eq!(
            tag.to_vec(),
            hex("b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7")
        );
    }

    #[test]
    fn hmac_matches_rfc4231_case2() {
        // RFC 4231 Test Case 2: key = "Jefe", data = "what do ya want for nothing?".
        let tag = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            tag.to_vec(),
            hex("5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843")
        );
    }

    #[test]
    fn hmac_matches_rfc4231_case6_long_key() {
        // RFC 4231 Test Case 6: key = 0xaa × 131 (> block size, so it is hashed first).
        let key = [0xaau8; 131];
        let tag = hmac_sha256(
            &key,
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        );
        assert_eq!(
            tag.to_vec(),
            hex("60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54")
        );
    }

    #[test]
    fn hkdf_matches_rfc5869_case1_first_block() {
        // RFC 5869 Test Case 1. The first 32 output bytes are exactly T(1), which is what
        // our single-block HKDF returns.
        let ikm = [0x0bu8; 22];
        let salt = hex("000102030405060708090a0b0c");
        let info = hex("f0f1f2f3f4f5f6f7f8f9");
        let okm = hkdf_sha256(&salt, &ikm, &info);
        assert_eq!(
            okm.to_vec(),
            hex("3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf")
        );
    }

    #[test]
    fn ct_eq_is_true_only_for_identical_slices() {
        assert!(ct_eq(b"abcdef", b"abcdef"));
        assert!(!ct_eq(b"abcdef", b"abcdeg"));
        assert!(!ct_eq(b"abcdef", b"abcde"), "length mismatch must be false");
    }
}
