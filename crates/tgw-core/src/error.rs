//! Library error type. `thiserror` in libs, `anyhow` in bins (tasks/muaz.md ground rule).

/// Errors surfaced by `tgw-core`.
///
/// Variants deliberately avoid carrying key material or decrypted payload bytes so an
/// error can always be logged verbatim without leaking PHI or secrets.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// A payload failed to decode (CBOR, lz4, or FEC reconstruction).
    #[error("decode failed: {0}")]
    Decode(String),
    /// A payload failed to encode (CBOR serialization).
    #[error("encode failed: {0}")]
    Encode(String),
    /// AEAD authentication failure: tampered ciphertext, wrong key, or mismatched AAD.
    /// The bundle must be dropped whole — never partially accepted (ARCHITECTURE.md §4).
    #[error("AEAD authentication failure")]
    Crypto,
    /// A datagram that does not parse as a Contract 2 wire frame.
    #[error("malformed frame")]
    MalformedFrame,
    /// Key material problems: unreadable file, wrong length, bad encoding.
    /// Never embeds the offending content, only the reason.
    #[error("key error: {0}")]
    Key(String),
    /// Configuration problems: unreadable file, TOML syntax, invalid knob values.
    #[error("config error: {0}")]
    Config(String),
    /// Underlying I/O failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
