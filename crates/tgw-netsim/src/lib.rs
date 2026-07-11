//! `tgw-netsim` — a deterministic, seeded lossy UDP proxy. OWNER: Twaha.
//!
//! This is a **test instrument**, kept small. It sits between the field client and the
//! gateway and drops/delays/rate-limits datagrams reproducibly so the integration test can
//! assert delivery under the stated constraints. Implement `run_proxy` in **Phase C**.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Context;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tokio::net::UdpSocket;
use tokio::time::{Instant, sleep};

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

/// Maximum UDP datagram size the proxy buffers (fits a 65535-byte IP payload).
const MAX_DATAGRAM: usize = 65_535;

/// Run the proxy: receive datagrams on `listen`, forward survivors to `forward`.
///
/// Composes a [`LossModel`] (drop decisions) and a [`Pacer`] (token-bucket rate cap) over a
/// real [`UdpSocket`], adding up to [`NetsimConfig::jitter`] of extra latency. The drop/pace
/// pattern is fully deterministic for a given [`NetsimConfig::seed`]. The most recently seen
/// field source address is remembered so the reverse path (gateway → field receipts/NACKs) can
/// be wired through the same proxy in the Phase D end-to-end harness.
pub async fn run_proxy(
    cfg: NetsimConfig,
    listen: SocketAddr,
    forward: SocketAddr,
) -> anyhow::Result<()> {
    let sock = UdpSocket::bind(listen)
        .await
        .context("netsim: bind listener")?;
    let forward_sock = UdpSocket::bind("0.0.0.0:0")
        .await
        .context("netsim: bind forwarder")?;

    let mut loss = LossModel::new(&cfg);
    let mut pacer = Pacer::new(cfg.rate_bps);
    // Separate, seeded RNG for jitter so it does not perturb the drop sequence.
    let mut jitter_rng = StdRng::seed_from_u64(cfg.seed.wrapping_add(1));
    let start = Instant::now();
    let mut buf = vec![0u8; MAX_DATAGRAM];

    loop {
        let (n, src) = sock
            .recv_from(&mut buf)
            .await
            .context("netsim: recv_from")?;
        let payload = &buf[..n];

        let elapsed = start.elapsed();
        if loss.decide(elapsed) {
            tracing::trace!(bytes = n, "netsim: dropping datagram");
            continue;
        }

        // Serialise at the rate cap; then add bounded jitter on top of the paced delay.
        let packet_bits = u64::try_from(n.saturating_mul(8)).unwrap_or(u64::MAX);
        let pace_delay = pacer.schedule(elapsed, packet_bits);
        let jitter_nanos = if cfg.jitter.is_zero() {
            0
        } else {
            jitter_rng.gen_range(0..cfg.jitter.as_nanos().max(1))
        };
        let jitter_delay = Duration::from_nanos(jitter_nanos as u64);
        let total_delay = pace_delay + jitter_delay;
        if !total_delay.is_zero() {
            sleep(total_delay).await;
        }

        forward_sock
            .send_to(payload, forward)
            .await
            .context("netsim: send_to forward")?;
        tracing::trace!(bytes = n, from = %src, delay = ?total_delay, "netsim: forwarded");
    }
}

// --- Testable seams: pure, deterministic policy separated from socket I/O ----------------
//
// `run_proxy` MUST be implemented by composing `LossModel` (drop decisions) and `Pacer`
// (rate cap) over a real `UdpSocket`. The policy lives in these types so the test suite can
// assert behaviour without any networking. Do not fold this logic back inline into
// `run_proxy` — the tests target these APIs.

/// What the proxy does with one datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Forward the datagram after waiting `delay`.
    Forward { delay: Duration },
    /// Drop the datagram entirely.
    Drop,
}

/// Deterministic drop policy: per-packet random loss, plus fixed burst-loss windows.
///
/// # Burst schedule (implement EXACTLY this — the tests depend on it)
/// Burst windows occupy `[n * burst_every, n * burst_every + burst_len)` for every integer
/// `n >= 1`. Therefore `elapsed < burst_every` is always burst-free. Inside a burst window
/// `decide` returns `true` (drop) unconditionally; outside, it returns `true` with
/// probability `loss` using a `StdRng` seeded from `cfg.seed`. Given the same seed and the
/// same sequence of `decide` calls, the output is identical run-to-run.
pub struct LossModel {
    cfg: NetsimConfig,
    rng: StdRng,
}

impl LossModel {
    /// Build a loss model from the config (seeds the RNG from `cfg.seed`).
    pub fn new(cfg: &NetsimConfig) -> Self {
        Self {
            cfg: cfg.clone(),
            rng: StdRng::seed_from_u64(cfg.seed),
        }
    }

    /// Decide whether to drop the current datagram at `elapsed` since start.
    /// `true` = drop. See the type docs for the exact, deterministic semantics.
    pub fn decide(&mut self, elapsed: Duration) -> bool {
        if self.in_burst_window(elapsed) {
            return true;
        }
        self.rng.gen_bool(self.cfg.loss)
    }

    /// True when `elapsed` falls inside any burst window `[n*burst_every, n*burst_every+burst_len)`
    /// for some integer `n >= 1`. `elapsed < burst_every` is therefore always burst-free.
    fn in_burst_window(&self, elapsed: Duration) -> bool {
        let every_ns = self.cfg.burst_every.as_nanos();
        if elapsed.as_nanos() < every_ns {
            return false;
        }
        let phase = elapsed.as_nanos() % every_ns;
        phase < self.cfg.burst_len.as_nanos()
    }
}

/// Token-bucket pacer that serialises datagrams at a fixed bits-per-second cap.
///
/// # Semantics (implement EXACTLY this — the tests depend on it)
/// Track `available_at`, the earliest instant the link is free (starts at `Duration::ZERO`).
/// For a datagram of `packet_bits` bits arriving at `now`:
/// `send_at = max(now, available_at)`, the returned delay is `send_at - now`, and
/// `available_at` advances to `send_at + packet_bits / rate_bps` seconds. Thus a single
/// packet arriving on an idle link schedules ~zero delay, and back-to-back packets serialise
/// so that sending `N` bits takes at least `N / rate_bps` seconds.
pub struct Pacer {
    rate_bps: u64,
    available_at: Duration,
}

impl Pacer {
    /// Create a pacer capped at `rate_bps` bits per second.
    pub fn new(rate_bps: u64) -> Self {
        Self {
            rate_bps,
            available_at: Duration::ZERO,
        }
    }

    /// Return the delay (from `now`) before a `packet_bits`-bit datagram may be sent.
    pub fn schedule(&mut self, now: Duration, packet_bits: u64) -> Duration {
        let send_at = now.max(self.available_at);
        let delay = send_at.saturating_sub(now);
        // Transmit time for this packet: packet_bits / rate_bps seconds.
        let transmit = if self.rate_bps == 0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(packet_bits as f64 / self.rate_bps as f64)
        };
        self.available_at = send_at + transmit;
        delay
    }
}
