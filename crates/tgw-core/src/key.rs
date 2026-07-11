//! Pre-shared key handling (docs/ARCHITECTURE.md §6).
//!
//! One static 256-bit key per device pair, stored as 64 hex characters in a file that is
//! `.gitignore`d and never logged. Key distribution/rotation is explicitly out of the 24 h
//! scope — a stated limitation, not an oversight.

use chacha20poly1305::XChaCha20Poly1305;
use chacha20poly1305::aead::KeyInit;
use rand::RngCore;
use rand::rngs::OsRng;

use crate::error::CoreError;

/// Key length in raw bytes (256-bit XChaCha20-Poly1305 key).
pub const KEY_LEN: usize = 32;

/// 256-bit pre-shared key for XChaCha20-Poly1305.
///
/// Deliberately opaque: no `Serialize`, no `Display`, and a redacted `Debug`, so key
/// material cannot wander into logs, CBOR payloads, or error messages by accident.
#[derive(Clone)]
pub struct Key {
    bytes: [u8; KEY_LEN],
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(<redacted>)")
    }
}

impl Key {
    /// Wrap raw key bytes (for tests and in-memory construction).
    #[must_use]
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Key { bytes }
    }

    /// Generate a fresh random key from the OS CSPRNG (`tgw-field keygen`).
    #[must_use]
    pub fn generate() -> Self {
        let mut bytes = [0u8; KEY_LEN];
        OsRng.fill_bytes(&mut bytes);
        Key { bytes }
    }

    /// Load a key from `path`: 64 hex characters, surrounding whitespace ignored.
    ///
    /// Error messages name the path and the reason but never echo file content — a
    /// mistyped key must not end up in a log line.
    pub fn from_file(path: &std::path::Path) -> Result<Self, CoreError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| CoreError::Key(format!("cannot read key file {}: {e}", path.display())))?;
        Self::from_hex(text.trim()).map_err(|e| match e {
            CoreError::Key(reason) => {
                CoreError::Key(format!("key file {}: {reason}", path.display()))
            }
            other => other,
        })
    }

    /// Parse a key from its 64-hex-character form.
    pub fn from_hex(hex: &str) -> Result<Self, CoreError> {
        let decoded = decode_hex(hex)?;
        let bytes: [u8; KEY_LEN] = decoded
            .try_into()
            .map_err(|_| CoreError::Key(format!("expected {} hex chars", KEY_LEN * 2)))?;
        Ok(Key { bytes })
    }

    /// Hex form for `keygen` output. The only sanctioned way key material leaves this
    /// type — call it exclusively when writing a key file.
    #[must_use]
    pub fn to_hex(&self) -> String {
        encode_hex(&self.bytes)
    }

    /// Cipher instance for the envelope/receipt code paths.
    pub(crate) fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new((&self.bytes).into())
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // Writing to a String cannot fail; discard the Infallible-in-practice result
        // without unwrap to honor the no-panic rule.
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn decode_hex(hex: &str) -> Result<Vec<u8>, CoreError> {
    if !hex.len().is_multiple_of(2) {
        return Err(CoreError::Key("odd hex length".into()));
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            hex.get(i..i + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .ok_or_else(|| CoreError::Key("invalid hex character".into()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let key = Key::generate();
        let hex = key.to_hex();
        assert_eq!(hex.len(), KEY_LEN * 2);
        let restored = match Key::from_hex(&hex) {
            Ok(k) => k,
            Err(e) => panic!("generated key must re-parse: {e}"),
        };
        assert_eq!(restored.bytes, key.bytes);
    }

    #[test]
    fn rejects_bad_hex() {
        assert!(Key::from_hex("zz").is_err(), "non-hex must be rejected");
        assert!(Key::from_hex("abc").is_err(), "odd length must be rejected");
        assert!(
            Key::from_hex(&"ab".repeat(16)).is_err(),
            "short keys must be rejected"
        );
    }

    #[test]
    fn debug_is_redacted() {
        let key = Key::generate();
        let debug = format!("{key:?}");
        assert_eq!(debug, "Key(<redacted>)");
        assert!(
            !debug.contains(&key.to_hex()[..8]),
            "debug output must not leak key material"
        );
    }

    #[test]
    fn from_file_reads_hex_with_whitespace() {
        let dir = std::env::temp_dir().join(format!("tgw-key-test-{}", std::process::id()));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            panic!("temp dir: {e}");
        }
        let path = dir.join("device.key");
        let key = Key::generate();
        if let Err(e) = std::fs::write(&path, format!("{}\n", key.to_hex())) {
            panic!("write key file: {e}");
        }
        let loaded = match Key::from_file(&path) {
            Ok(k) => k,
            Err(e) => panic!("key file must load: {e}"),
        };
        assert_eq!(loaded.bytes, key.bytes);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_file_error_names_path_not_content() {
        let missing = std::path::Path::new("/definitely/not/here.key");
        match Key::from_file(missing) {
            Err(CoreError::Key(msg)) => assert!(msg.contains("not/here.key")),
            other => panic!("expected Key error, got {other:?}"),
        }
    }
}
