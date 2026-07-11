//! Token-bucket pacer — the bandwidth-honesty mechanism (docs/ARCHITECTURE.md §1).
//!
//! Every UDP send is charged against a bucket refilled at `bandwidth_bps`. We *pace
//! below the constraint* (56 kbps default vs the 64 kbps ceiling) so resilience is never
//! secretly bought with bandwidth the problem statement forbids.
//!
//! Built on `tokio::time` so tests run under a paused clock, deterministically.

use std::time::Duration;

use tokio::time::Instant;

/// Token bucket that hard-caps the transmit rate.
pub struct Pacer {
    /// Refill rate in bits per second.
    rate_bps: u64,
    /// Bucket capacity in bytes (the largest instantaneous burst).
    capacity_bytes: f64,
    /// Current token balance, in bytes.
    tokens_bytes: f64,
    last_refill: Instant,
}

impl Pacer {
    /// New pacer at `rate_bps`, allowing at most `burst_bytes` to leave instantly.
    /// The bucket starts full so the first datagram is never delayed.
    #[must_use]
    pub fn new(rate_bps: u32, burst_bytes: usize) -> Self {
        let capacity = (burst_bytes.max(1)) as f64;
        Pacer {
            rate_bps: u64::from(rate_bps.max(1)),
            capacity_bytes: capacity,
            tokens_bytes: capacity,
            last_refill: Instant::now(),
        }
    }

    /// Wait until `bytes` may be sent, then debit them. A request larger than the
    /// bucket capacity is paced as if the bucket were exactly that large (it waits for
    /// a full refill rather than deadlocking).
    pub async fn acquire(&mut self, bytes: usize) {
        loop {
            self.refill();
            let need = (bytes as f64).min(self.capacity_bytes);
            if self.tokens_bytes >= need {
                // Debit the *true* size so oversized datagrams still pay full price.
                self.tokens_bytes -= bytes as f64;
                return;
            }
            let deficit_bytes = need - self.tokens_bytes;
            let wait_secs = (deficit_bytes * 8.0) / self.rate_bps as f64;
            tokio::time::sleep(Duration::from_secs_f64(wait_secs.max(0.000_1))).await;
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;
        let earned_bytes = elapsed * (self.rate_bps as f64) / 8.0;
        self.tokens_bytes = (self.tokens_bytes + earned_bytes).min(self.capacity_bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The muaz.md H6–10 acceptance test: N bytes queued take ≥ N·8/rate seconds.
    /// Paused tokio clock ⇒ deterministic, instant to run.
    #[tokio::test(start_paused = true)]
    async fn n_bytes_take_at_least_n8_over_rate_seconds() {
        const RATE_BPS: u32 = 56_000;
        const DATAGRAM: usize = 1134; // symbol 1100 + framing overhead
        const COUNT: usize = 100;
        const BURST: usize = 2 * DATAGRAM;

        let mut pacer = Pacer::new(RATE_BPS, BURST);
        let start = Instant::now();
        for _ in 0..COUNT {
            pacer.acquire(DATAGRAM).await;
        }
        let elapsed = start.elapsed().as_secs_f64();

        // Everything beyond the initial full bucket must be paid for at the line rate.
        let total_bytes = (COUNT * DATAGRAM) as f64;
        let min_secs = (total_bytes - BURST as f64) * 8.0 / f64::from(RATE_BPS);
        assert!(
            elapsed >= min_secs,
            "paced {total_bytes} B in {elapsed:.3}s; hard floor is {min_secs:.3}s"
        );
        // And the pacer must not be pathologically slow either (< 2× the ideal time).
        let ideal_secs = total_bytes * 8.0 / f64::from(RATE_BPS);
        assert!(
            elapsed <= ideal_secs * 2.0,
            "pacer is over-throttling: {elapsed:.3}s vs ideal {ideal_secs:.3}s"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn first_datagram_is_not_delayed() {
        let mut pacer = Pacer::new(56_000, 4096);
        let start = Instant::now();
        pacer.acquire(1134).await;
        assert_eq!(
            start.elapsed(),
            Duration::ZERO,
            "a full bucket must let the first datagram out immediately"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn oversized_request_does_not_deadlock() {
        let mut pacer = Pacer::new(56_000, 1024);
        // 4 KiB through a 1 KiB bucket: must complete (paced), not hang forever.
        let start = Instant::now();
        pacer.acquire(4096).await;
        pacer.acquire(4096).await;
        assert!(start.elapsed() > Duration::ZERO);
    }
}
