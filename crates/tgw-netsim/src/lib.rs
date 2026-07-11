//! `tgw-netsim` — a deterministic, seeded lossy UDP proxy. OWNER: Twaha.
//!
//! This is a **test instrument**, kept small. It sits between the field client and the
//! gateway and drops/delays/rate-limits datagrams reproducibly so the integration test can
//! assert delivery under the stated constraints. Implement `run_proxy` in **Phase C**.

use std::net::SocketAddr;
use std::time::Duration;

/// Degradation profile for the proxy. Deterministic for a given `seed`.
#[derive(Debug, Clone)]
pub struct NetsimConfig {
    /// Random per-packet drop probability, 0.0..=1.0 (e.g. 0.25 for 25% loss).
    pub loss: f64,
    /// How often a burst-loss episode begins.
    pub burst_every: Duration,
    /// How long each burst-loss episode lasts (all packets dropped during it).
    pub burst_len: Duration,
    /// Token-bucket rate cap in bits per second (e.g. 64_000).
    pub rate_bps: u64,
    /// Max added latency jitter per packet.
    pub jitter: Duration,
    /// RNG seed — the same seed reproduces the exact same drop pattern.
    pub seed: u64,
}

impl Default for NetsimConfig {
    fn default() -> Self {
        Self {
            loss: 0.25,
            burst_every: Duration::from_secs(5),
            burst_len: Duration::from_millis(800),
            rate_bps: 64_000,
            jitter: Duration::from_millis(40),
            seed: 1,
        }
    }
}

/// Run the proxy: receive datagrams on `listen`, forward survivors to `forward`.
///
/// TODO(PHASE-C): bind a `tokio::net::UdpSocket` on `listen`; for each datagram, use a
/// seeded `rand::rngs::StdRng` to decide drop (per-packet `loss` OR inside a burst window),
/// enforce the `rate_bps` token bucket, add up to `jitter` delay, then send to `forward`.
/// Keep it deterministic per `seed`.
pub async fn run_proxy(
    cfg: NetsimConfig,
    listen: SocketAddr,
    forward: SocketAddr,
) -> anyhow::Result<()> {
    let _ = (cfg, listen, forward);
    todo!("PHASE-C: seeded lossy UDP proxy (loss, burst, token-bucket rate, jitter)")
}
