//! `tgw-netsim` — a deterministic, seeded lossy UDP proxy. OWNER: Twaha.
//!
//! This is a **test instrument**, kept small. It sits between the field client and the
//! gateway and drops/delays/rate-limits datagrams reproducibly so the integration test can
//! assert delivery under the stated constraints. [`run_proxy`] is bidirectional: the forward
//! path applies loss + pacing + jitter, the reverse path relays gateway receipts/NACKs back.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Context;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tokio::net::UdpSocket;
use tokio::time::{Instant, sleep};

/// Live, thread-safe link controls the proxy reads **per packet**, so loss, corruption, and
/// bandwidth can be changed at runtime (e.g. from a slider) while traffic is flowing. Cloneable:
/// every clone shares the same underlying atomics, so an HTTP control handler and the proxy loop
/// see each other's writes immediately. Values outside their valid range are clamped on write.
#[derive(Clone)]
pub struct LinkControls {
    loss: Arc<AtomicU64>,    // f64 bits, 0.0..=1.0
    corrupt: Arc<AtomicU64>, // f64 bits, 0.0..=1.0
    rate_bps: Arc<AtomicU64>,
}

impl LinkControls {
    /// Seed the controls from a static config's starting values.
    #[must_use]
    pub fn from_config(cfg: &NetsimConfig) -> Self {
        Self {
            loss: Arc::new(AtomicU64::new(cfg.loss.clamp(0.0, 1.0).to_bits())),
            corrupt: Arc::new(AtomicU64::new(cfg.corrupt.clamp(0.0, 1.0).to_bits())),
            rate_bps: Arc::new(AtomicU64::new(cfg.rate_bps.max(1))),
        }
    }

    /// Current drop probability (0.0..=1.0).
    #[must_use]
    pub fn loss(&self) -> f64 {
        f64::from_bits(self.loss.load(Ordering::Relaxed))
    }
    /// Set the drop probability; clamped to 0.0..=1.0.
    pub fn set_loss(&self, v: f64) {
        self.loss
            .store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
    /// Current corruption probability (0.0..=1.0).
    #[must_use]
    pub fn corrupt(&self) -> f64 {
        f64::from_bits(self.corrupt.load(Ordering::Relaxed))
    }
    /// Set the corruption probability; clamped to 0.0..=1.0.
    pub fn set_corrupt(&self, v: f64) {
        self.corrupt
            .store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
    /// Current bandwidth cap in bits/second.
    #[must_use]
    pub fn rate_bps(&self) -> u64 {
        self.rate_bps.load(Ordering::Relaxed)
    }
    /// Set the bandwidth cap; forced to at least 1 bps so the pacer stays meaningful.
    pub fn set_rate_bps(&self, v: u64) {
        self.rate_bps.store(v.max(1), Ordering::Relaxed);
    }
}

/// Degradation profile for the proxy. Deterministic for a given `seed`.
#[derive(Debug, Clone)]
pub struct NetsimConfig {
    /// Random per-packet drop probability, 0.0..=1.0 (e.g. 0.25 for 25% loss).
    pub loss: f64,
    /// Random per-packet corruption probability, 0.0..=1.0. A *surviving* datagram is chosen
    /// with this probability to have a single bit flipped — modelling a datagram that reaches
    /// the receiver but arrives damaged. This is what the gateway's integrity tag must reject
    /// before RaptorQ; a pure-loss link never produces it.
    pub corrupt: f64,
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
            loss: DEFAULT_LOSS,
            corrupt: DEFAULT_CORRUPT,
            burst_every: DEFAULT_BURST_EVERY,
            burst_len: DEFAULT_BURST_LEN,
            rate_bps: DEFAULT_RATE_BPS,
            jitter: DEFAULT_JITTER,
            seed: DEFAULT_SEED,
        }
    }
}

impl NetsimConfig {
    /// Reject configurations that would panic or make a rate cap meaningless.
    fn validate(&self) -> anyhow::Result<()> {
        if !(0.0..=1.0).contains(&self.loss) {
            anyhow::bail!("netsim loss must be in 0.0..=1.0");
        }
        if !(0.0..=1.0).contains(&self.corrupt) {
            anyhow::bail!("netsim corrupt must be in 0.0..=1.0");
        }
        if self.burst_every.is_zero() {
            anyhow::bail!("netsim burst_every must be non-zero");
        }
        if self.rate_bps == 0 {
            anyhow::bail!("netsim rate_bps must be non-zero");
        }
        Ok(())
    }
}

/// Default random loss rate used by the deterministic evidence harness.
const DEFAULT_LOSS: f64 = 0.25;
/// Default corruption rate: off, so a plain lossy link stays a plain lossy link.
const DEFAULT_CORRUPT: f64 = 0.0;
/// Time between deterministic burst-loss windows.
const DEFAULT_BURST_EVERY: Duration = Duration::from_secs(5);
/// Duration of each deterministic burst-loss window.
const DEFAULT_BURST_LEN: Duration = Duration::from_millis(800);
/// Link ceiling used by the evidence harness, in bits per second.
const DEFAULT_RATE_BPS: u64 = 64_000;
/// Maximum forward-path jitter in the evidence harness.
const DEFAULT_JITTER: Duration = Duration::from_millis(40);
/// Stable default seed for repeatable evidence runs.
const DEFAULT_SEED: u64 = 1;

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
    // A static run is a controlled run whose controls never change: the LossModel RNG sequence
    // is identical, so determinism (and the evidence harness) is preserved.
    let controls = LinkControls::from_config(&cfg);
    run_proxy_controlled(cfg, listen, forward, controls).await
}

/// Run the proxy with **live** [`LinkControls`]: loss, corruption, and bandwidth are read fresh
/// from `controls` on every packet, so an external writer (the control server / a slider) changes
/// the link while traffic flows. Otherwise identical to [`run_proxy`].
pub async fn run_proxy_controlled(
    cfg: NetsimConfig,
    listen: SocketAddr,
    forward: SocketAddr,
    controls: LinkControls,
) -> anyhow::Result<()> {
    cfg.validate()?;
    let field_sock = UdpSocket::bind(listen)
        .await
        .context("netsim: bind listener")?;
    let gateway_sock = UdpSocket::bind("0.0.0.0:0")
        .await
        .context("netsim: bind forwarder")?;

    let mut loss = LossModel::new(&cfg);
    let mut corruptor = Corruptor::new(&cfg);
    let mut pacer = Pacer::new(cfg.rate_bps);
    // Separate, seeded RNG for jitter so it does not perturb the drop sequence.
    let mut jitter_rng = StdRng::seed_from_u64(cfg.seed.wrapping_add(1));
    let start = Instant::now();
    let mut field_buf = vec![0u8; MAX_DATAGRAM];
    let mut gateway_buf = vec![0u8; MAX_DATAGRAM];
    let mut field_addr = None;

    loop {
        tokio::select! {
            received = field_sock.recv_from(&mut field_buf) => {
                let (n, src) = received.context("netsim: receive field datagram")?;
                field_addr = Some(src);
                let elapsed = start.elapsed();
                if loss.decide_with(elapsed, controls.loss()) {
                    tracing::trace!(bytes = n, "netsim: dropping forward datagram");
                    continue;
                }
                // A survivor may still arrive damaged. The gateway's integrity tag must reject
                // this before RaptorQ absorbs it; here we only inject the damage.
                if corruptor.corrupt_with(&mut field_buf[..n], controls.corrupt()) {
                    tracing::trace!(bytes = n, "netsim: corrupted forward datagram");
                }
                let packet_bits = u64::try_from(n.saturating_mul(8)).unwrap_or(u64::MAX);
                let pace_delay = pacer.schedule_with(elapsed, packet_bits, controls.rate_bps());
                let jitter_nanos = jitter_rng.gen_range(0..=cfg.jitter.as_nanos());
                let total_delay = pace_delay + Duration::from_nanos(jitter_nanos as u64);
                if !total_delay.is_zero() {
                    sleep(total_delay).await;
                }
                gateway_sock.send_to(&field_buf[..n], forward).await.context("netsim: forward datagram")?;
                tracing::trace!(bytes = n, from = %src, delay = ?total_delay, "netsim: forwarded");
            }
            received = gateway_sock.recv_from(&mut gateway_buf) => {
                let (n, src) = received.context("netsim: receive gateway reply")?;
                if src != forward {
                    tracing::warn!(%src, "netsim: ignoring reply from unexpected address");
                    continue;
                }
                if let Some(field) = field_addr {
                    field_sock.send_to(&gateway_buf[..n], field).await.context("netsim: relay reply")?;
                    tracing::trace!(bytes = n, to = %field, "netsim: relayed gateway reply");
                }
            }
        }
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
        self.decide_with(elapsed, self.cfg.loss)
    }

    /// Like [`LossModel::decide`] but with a caller-supplied loss probability, so a live control
    /// can change the drop rate per packet. `loss` is clamped to 0.0..=1.0. The burst schedule
    /// still comes from the config; the RNG stream is unchanged, so a fixed `loss` reproduces
    /// [`LossModel::decide`] exactly.
    pub fn decide_with(&mut self, elapsed: Duration, loss: f64) -> bool {
        let random_drop = self.rng.gen_bool(loss.clamp(0.0, 1.0));
        self.in_burst_window(elapsed) || random_drop
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

/// Deterministic per-packet corruption policy: flip a single bit in a fraction of survivors.
///
/// # Semantics (implement EXACTLY this — the tests depend on it)
/// For each datagram, draw once with probability `corrupt` using a `StdRng` seeded from
/// `cfg.seed.wrapping_add(2)` (a dedicated stream, so corruption never perturbs the drop or
/// jitter sequences). On a hit, flip exactly one bit at a pseudo-random byte/bit position, so
/// the datagram is guaranteed to differ while keeping its length. An empty datagram is a no-op.
/// Returns `true` iff the datagram was modified.
pub struct Corruptor {
    prob: f64,
    rng: StdRng,
}

impl Corruptor {
    /// Build a corruptor from the config (seeds a dedicated RNG stream from `cfg.seed`).
    pub fn new(cfg: &NetsimConfig) -> Self {
        Self {
            prob: cfg.corrupt,
            rng: StdRng::seed_from_u64(cfg.seed.wrapping_add(2)),
        }
    }

    /// Possibly flip one bit of `packet` in place. `true` = the datagram was modified.
    pub fn corrupt(&mut self, packet: &mut [u8]) -> bool {
        self.corrupt_with(packet, self.prob)
    }

    /// Like [`Corruptor::corrupt`] but with a caller-supplied probability, for live control.
    /// `prob` is clamped to 0.0..=1.0.
    pub fn corrupt_with(&mut self, packet: &mut [u8], prob: f64) -> bool {
        if packet.is_empty() || !self.rng.gen_bool(prob.clamp(0.0, 1.0)) {
            return false;
        }
        let byte = self.rng.gen_range(0..packet.len());
        let bit = self.rng.gen_range(0..8u32);
        packet[byte] ^= 1u8 << bit; // single-bit flip ⇒ the byte always changes
        true
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
        self.schedule_with(now, packet_bits, self.rate_bps)
    }

    /// Like [`Pacer::schedule`] but with a caller-supplied rate, for live bandwidth control.
    /// A `rate_bps` of 0 is treated as "no cap" (zero transmit time).
    pub fn schedule_with(&mut self, now: Duration, packet_bits: u64, rate_bps: u64) -> Duration {
        let send_at = now.max(self.available_at);
        let delay = send_at.saturating_sub(now);
        let transmit = if rate_bps == 0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(packet_bits as f64 / rate_bps as f64)
        };
        self.available_at = send_at + transmit;
        delay
    }
}

#[cfg(test)]
mod control_tests {
    use super::*;

    #[test]
    fn link_controls_clamp_and_share() {
        let cfg = NetsimConfig {
            loss: 0.1,
            corrupt: 0.0,
            rate_bps: 64_000,
            ..NetsimConfig::default()
        };
        let a = LinkControls::from_config(&cfg);
        let b = a.clone(); // shares the same atomics
        a.set_loss(0.9);
        assert!(
            (b.loss() - 0.9).abs() < 1e-9,
            "clones observe each other's writes"
        );
        a.set_loss(5.0);
        assert!((b.loss() - 1.0).abs() < 1e-9, "loss clamps to 1.0");
        a.set_loss(-1.0);
        assert!(b.loss().abs() < 1e-9, "loss clamps to 0.0");
        a.set_rate_bps(0);
        assert_eq!(b.rate_bps(), 1, "rate is forced to >= 1");
    }

    #[test]
    fn live_loss_flips_drop_behavior() {
        let cfg = NetsimConfig {
            loss: 0.0,
            burst_every: Duration::from_secs(3600),
            ..NetsimConfig::default()
        };
        let mut model = LossModel::new(&cfg);
        // With loss 0.0 nothing drops (well outside any burst window at t=0).
        let dropped_at_zero: u32 = (0..200)
            .map(|_| u32::from(model.decide_with(Duration::ZERO, 0.0)))
            .sum();
        assert_eq!(dropped_at_zero, 0, "0% loss drops nothing");
        // Turn loss up to 100% live: everything drops now.
        let dropped_at_full: u32 = (0..200)
            .map(|_| u32::from(model.decide_with(Duration::ZERO, 1.0)))
            .sum();
        assert_eq!(dropped_at_full, 200, "100% loss drops everything");
    }
}
