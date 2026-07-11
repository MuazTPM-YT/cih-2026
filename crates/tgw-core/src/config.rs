//! Configuration types — Contract 4 (tasks/CONTRACTS.md).
//!
//! Twaha owns the sample files in `config/*.toml`; this module owns parsing them.
//! "Config over magic numbers" (docs/ARCHITECTURE.md §7): every tunable lives here, TOML
//! first, then `TGW_*` environment overrides.

use serde::Deserialize;

use crate::error::CoreError;

/// FEC tuning knobs consumed by the framing layer (Contract 1).
#[derive(Debug, Clone)]
pub struct FecConfig {
    /// RaptorQ symbol size in bytes; one symbol rides per UDP DATA frame.
    pub symbol_size: u16,
    /// First-burst transmit factor: 1.4 ⇒ send ~40% repair on top of source symbols.
    pub overhead_factor: f32,
}

/// `[link]` — bandwidth honesty and symbol sizing.
#[derive(Debug, Clone, Deserialize)]
pub struct LinkConfig {
    /// Hard transmit budget in bits/second (paced below the 64 kbps ceiling).
    pub bandwidth_bps: u32,
    /// RaptorQ symbol size in bytes.
    pub symbol_size: u16,
    /// First-burst repair margin (≥ 1.0).
    pub overhead_factor: f32,
}

/// `[retry]` — NACK/re-burst schedule.
#[derive(Debug, Clone, Deserialize)]
pub struct RetryConfig {
    /// Receiver decode-stall trigger for emitting a NACK (milliseconds).
    pub nack_timeout_ms: u64,
    /// Sender re-burst base backoff when the link stays silent (milliseconds).
    pub retry_backoff_ms: u64,
    /// Re-burst attempts before a bundle is flagged `stuck` (never silently dropped).
    pub max_retries: u32,
}

/// `[net]` — addresses.
#[derive(Debug, Clone, Deserialize)]
pub struct NetConfig {
    /// Where the gateway (or netsim) listens for DATA frames, e.g. `"192.168.1.50:47000"`.
    pub gateway_addr: String,
    /// Local UDP bind for return frames (receipts/NACKs on the field side).
    pub listen_addr: String,
    /// Dashboard/API bind (gateway side; informational for the field client).
    pub http_addr: String,
}

/// `[crypto]` — key location. No default on purpose (docs/ARCHITECTURE.md §7).
#[derive(Debug, Clone, Deserialize)]
pub struct CryptoConfig {
    /// Path to the 64-hex-char PSK file. Generate with `tgw-field keygen`.
    pub key_file: std::path::PathBuf,
}

/// `[media]` — image handling.
#[derive(Debug, Clone, Deserialize)]
pub struct MediaConfig {
    /// Maximum accepted image payload in bytes (photos are recompressed/pre-sized to fit).
    pub image_max_bytes: usize,
}

/// Full Contract 4 configuration, one struct for both binaries.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Bandwidth and FEC sizing.
    pub link: LinkConfig,
    /// NACK/re-burst schedule.
    pub retry: RetryConfig,
    /// Socket addresses.
    pub net: NetConfig,
    /// PSK location.
    pub crypto: CryptoConfig,
    /// Media limits.
    pub media: MediaConfig,
}

impl Config {
    /// Load from a TOML file, apply `TGW_*` environment overrides, then validate.
    pub fn load(path: &std::path::Path) -> Result<Self, CoreError> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            CoreError::Config(format!("cannot read config {}: {e}", path.display()))
        })?;
        let mut config: Config = toml::from_str(&text)
            .map_err(|e| CoreError::Config(format!("{}: {e}", path.display())))?;
        config.apply_env_overrides(|name| std::env::var(name).ok());
        config.validate()?;
        Ok(config)
    }

    /// Parse from a TOML string (no env, no I/O — used by tests and embedders).
    pub fn from_toml_str(text: &str) -> Result<Self, CoreError> {
        let config: Config = toml::from_str(text).map_err(|e| CoreError::Config(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    /// Environment overrides (highest precedence). The getter is injected so tests can
    /// exercise overrides without mutating process-global env (which is `unsafe` to set
    /// in edition 2024).
    ///
    /// Recognized: `TGW_GATEWAY_ADDR`, `TGW_LISTEN_ADDR`, `TGW_HTTP_ADDR`, `TGW_KEY_FILE`,
    /// `TGW_BANDWIDTH_BPS`.
    pub fn apply_env_overrides(&mut self, get: impl Fn(&str) -> Option<String>) {
        if let Some(v) = get("TGW_GATEWAY_ADDR") {
            self.net.gateway_addr = v;
        }
        if let Some(v) = get("TGW_LISTEN_ADDR") {
            self.net.listen_addr = v;
        }
        if let Some(v) = get("TGW_HTTP_ADDR") {
            self.net.http_addr = v;
        }
        if let Some(v) = get("TGW_KEY_FILE") {
            self.crypto.key_file = std::path::PathBuf::from(v);
        }
        if let Some(Ok(bps)) = get("TGW_BANDWIDTH_BPS").map(|v| v.parse::<u32>()) {
            self.link.bandwidth_bps = bps;
        }
    }

    /// Reject configurations that would misbehave at runtime, with actionable messages.
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.link.bandwidth_bps == 0 {
            return Err(CoreError::Config("link.bandwidth_bps must be > 0".into()));
        }
        if self.link.symbol_size < 64 {
            return Err(CoreError::Config(
                "link.symbol_size must be ≥ 64 (one symbol per UDP datagram)".into(),
            ));
        }
        if !(self.link.overhead_factor >= 1.0 && self.link.overhead_factor <= 4.0) {
            return Err(CoreError::Config(
                "link.overhead_factor must be in [1.0, 4.0]".into(),
            ));
        }
        if self.retry.max_retries == 0 {
            return Err(CoreError::Config(
                "retry.max_retries must be ≥ 1 (bundles must get at least one re-burst)".into(),
            ));
        }
        if self.media.image_max_bytes == 0 {
            return Err(CoreError::Config(
                "media.image_max_bytes must be > 0".into(),
            ));
        }
        Ok(())
    }

    /// The FEC subset handed to `encode_bundle` / `BundleSender` (Contract 1).
    #[must_use]
    pub fn fec(&self) -> FecConfig {
        FecConfig {
            symbol_size: self.link.symbol_size,
            overhead_factor: self.link.overhead_factor,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors config/field.toml (Contract 4 shape, frozen).
    const SAMPLE: &str = r#"
        [link]
        bandwidth_bps = 56000
        symbol_size = 1100
        overhead_factor = 1.4

        [retry]
        nack_timeout_ms = 2000
        retry_backoff_ms = 3000
        max_retries = 8

        [net]
        gateway_addr = "192.168.1.50:47000"
        listen_addr = "0.0.0.0:47000"
        http_addr = "0.0.0.0:8080"

        [crypto]
        key_file = "./keys/device-a.key"

        [media]
        image_max_bytes = 30000
    "#;

    #[test]
    fn parses_contract4_shape() {
        let config = match Config::from_toml_str(SAMPLE) {
            Ok(c) => c,
            Err(e) => panic!("Contract 4 sample must parse: {e}"),
        };
        assert_eq!(config.link.bandwidth_bps, 56_000);
        assert_eq!(config.link.symbol_size, 1100);
        assert!((config.link.overhead_factor - 1.4).abs() < f32::EPSILON);
        assert_eq!(config.retry.nack_timeout_ms, 2000);
        assert_eq!(config.retry.max_retries, 8);
        assert_eq!(config.net.gateway_addr, "192.168.1.50:47000");
        assert_eq!(
            config.crypto.key_file,
            std::path::PathBuf::from("./keys/device-a.key")
        );
        assert_eq!(config.media.image_max_bytes, 30_000);
    }

    #[test]
    fn parses_committed_sample_files() {
        // The actual files Twaha owns must always parse — this is the cross-owner seam.
        for sample in ["field.toml", "gateway.toml"] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../config")
                .join(sample);
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => panic!("cannot read {}: {e}", path.display()),
            };
            if let Err(e) = Config::from_toml_str(&text) {
                panic!("config/{sample} must parse per Contract 4: {e}");
            }
        }
    }

    #[test]
    fn env_overrides_take_precedence() {
        let mut config = match Config::from_toml_str(SAMPLE) {
            Ok(c) => c,
            Err(e) => panic!("sample must parse: {e}"),
        };
        config.apply_env_overrides(|name| match name {
            "TGW_GATEWAY_ADDR" => Some("127.0.0.1:9999".into()),
            "TGW_BANDWIDTH_BPS" => Some("48000".into()),
            _ => None,
        });
        assert_eq!(config.net.gateway_addr, "127.0.0.1:9999");
        assert_eq!(config.link.bandwidth_bps, 48_000);
        assert_eq!(
            config.net.listen_addr, "0.0.0.0:47000",
            "untouched knobs stay"
        );
    }

    #[test]
    fn validation_rejects_bad_knobs() {
        let mut config = match Config::from_toml_str(SAMPLE) {
            Ok(c) => c,
            Err(e) => panic!("sample must parse: {e}"),
        };
        config.link.overhead_factor = 0.5;
        assert!(matches!(config.validate(), Err(CoreError::Config(_))));

        let bad = SAMPLE.replace("bandwidth_bps = 56000", "bandwidth_bps = 0");
        assert!(matches!(
            Config::from_toml_str(&bad),
            Err(CoreError::Config(_))
        ));
    }
}
