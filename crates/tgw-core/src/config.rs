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

/// `[retry]` — NACK/re-burst schedule and dead-link fast-fail (Fix F4).
#[derive(Debug, Clone, Deserialize)]
pub struct RetryConfig {
    /// Receiver decode-stall trigger for emitting a NACK (milliseconds).
    pub nack_timeout_ms: u64,
    /// Sender re-burst base backoff when the link stays silent (milliseconds).
    pub retry_backoff_ms: u64,
    /// Re-burst attempts before a bundle is flagged `stuck` (never silently dropped).
    pub max_retries: u32,
    /// Fix F4 — consecutive `stuck` bundles that trip the circuit breaker. Once tripped, the
    /// daemon treats the link as down and probes subsequent bundles with a 1-retry budget plus
    /// a cool-down instead of burning the full linear budget on every bundle. Any delivery
    /// resets the counter and restores the full budget. `#[serde(default)]` keeps existing
    /// Contract-4 configs parsing unchanged.
    #[serde(default = "default_circuit_breaker_threshold")]
    pub circuit_breaker_threshold: u32,
    /// Fix F4 — cool-down between probes once the breaker is tripped (milliseconds).
    #[serde(default = "default_circuit_cooldown_ms")]
    pub circuit_cooldown_ms: u64,
    /// Fix F1 — how long a bundle must sit `stuck` before the daemon auto-re-arms it for another
    /// delivery pass (milliseconds). Paces daemon retries so a flapping link is not hammered.
    #[serde(default = "default_stuck_retry_backoff_ms")]
    pub stuck_retry_backoff_ms: u64,
    /// Fix F1 — cap on daemon auto-re-arm passes for a single bundle. Once a bundle's `retries`
    /// reaches this, it stays `stuck` (kept, visible, never dropped) until an operator requeues
    /// it, so a genuinely dead link converges instead of spinning forever.
    #[serde(default = "default_max_stuck_retries")]
    pub max_stuck_retries: u32,
}

/// Default consecutive-stuck count that trips the F4 circuit breaker.
fn default_circuit_breaker_threshold() -> u32 {
    3
}

/// Default F4 cool-down between probes once the breaker is tripped (milliseconds).
fn default_circuit_cooldown_ms() -> u64 {
    5000
}

/// Default F1 backoff before the daemon auto-re-arms a stuck bundle (milliseconds).
fn default_stuck_retry_backoff_ms() -> u64 {
    15000
}

/// Default F1 cap on daemon auto-re-arm passes for a single bundle.
fn default_max_stuck_retries() -> u32 {
    5
}

impl Default for RetryConfig {
    /// Production-shaped defaults (mirrors `config/field.toml`). Chiefly a convenience for tests
    /// that build a `RetryConfig` literal and only care about a couple of fields.
    fn default() -> Self {
        RetryConfig {
            nack_timeout_ms: 2000,
            retry_backoff_ms: 3000,
            max_retries: 8,
            circuit_breaker_threshold: default_circuit_breaker_threshold(),
            circuit_cooldown_ms: default_circuit_cooldown_ms(),
            stuck_retry_backoff_ms: default_stuck_retry_backoff_ms(),
            max_stuck_retries: default_max_stuck_retries(),
        }
    }
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

/// `[relay]` — local peer-relay fallback (Fix 2). Optional; defaults to disabled so existing
/// Contract-4 configs parse unchanged.
///
/// When enabled, a field device announces its presence on the local subnet and, if its direct
/// gateway hop yields no receipt within the retry budget, hands its still-sealed bundle to a
/// discovered peer to forward. The relay peer only ever holds ciphertext — see `relay.rs`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RelayConfig {
    /// Master switch for the peer-relay fallback.
    pub enabled: bool,
    /// Address peers announce presence on. An administratively-scoped (site-local, 239.x)
    /// **multicast** group (default) is joined with loopback delivery so co-located devices —
    /// including two instances on one host — reliably hear each other; a plain subnet
    /// **broadcast** address (e.g. `"255.255.255.255:47555"`) also works on a production LAN.
    /// Absent a multicast router the group stays on the local segment, matching the one-hop,
    /// same-area relay assumption. See discovery.rs.
    pub discovery_addr: String,
    /// Local UDP bind where this device accepts relay requests from peers.
    pub relay_listen_addr: String,
    /// How often to broadcast a presence announcement (milliseconds).
    pub announce_interval_ms: u64,
    /// How long a discovered peer stays usable without a fresh announcement (milliseconds).
    pub peer_ttl_ms: u64,
}

impl Default for RelayConfig {
    fn default() -> Self {
        RelayConfig {
            enabled: false,
            discovery_addr: "239.255.7.66:47555".to_string(),
            relay_listen_addr: "0.0.0.0:47556".to_string(),
            announce_interval_ms: 2000,
            peer_ttl_ms: 8000,
        }
    }
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
    /// Local peer-relay fallback (Fix 2). Absent section ⇒ disabled defaults.
    #[serde(default)]
    pub relay: RelayConfig,
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
    fn relay_defaults_apply_when_section_is_absent() {
        // Existing Contract-4 configs have no [relay] section; it must default to disabled
        // without breaking the parse (serde defaults).
        let config = match Config::from_toml_str(SAMPLE) {
            Ok(c) => c,
            Err(e) => panic!("config without [relay] must still parse: {e}"),
        };
        assert!(
            !config.relay.enabled,
            "relay is off unless explicitly enabled"
        );
        assert!(
            !config.relay.discovery_addr.is_empty(),
            "a default discovery address must be present"
        );
    }

    #[test]
    fn relay_section_parses_when_present() {
        let with_relay = format!(
            "{SAMPLE}\n[relay]\nenabled = true\ndiscovery_addr = \"255.255.255.255:47555\"\n\
             relay_listen_addr = \"0.0.0.0:47556\"\nannounce_interval_ms = 1500\npeer_ttl_ms = 6000\n"
        );
        let config = match Config::from_toml_str(&with_relay) {
            Ok(c) => c,
            Err(e) => panic!("[relay] section must parse: {e}"),
        };
        assert!(config.relay.enabled);
        assert_eq!(config.relay.discovery_addr, "255.255.255.255:47555");
        assert_eq!(config.relay.announce_interval_ms, 1500);
        assert_eq!(config.relay.peer_ttl_ms, 6000);
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
