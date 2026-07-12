//! Process-wide transmission counters for the telemetry dashboard.
//!
//! Two numbers tell the whole efficiency story on a lossy link:
//!
//! * **bytes attempted** — every byte handed to the UDP socket, including FEC repair
//!   overhead, NACK-triggered repairs, and re-bursts. This is what the link *cost*.
//! * **bytes acked** — the sealed-envelope bytes of bundles that reached an
//!   authenticated `DELIVERED` receipt. This is what the link *achieved*.
//!
//! `acked / attempted` is the goodput ratio the metrics dashboard plots live. Counters
//! are plain atomics (no locks, no allocation on the send path) and reset with the
//! process; they describe the current session, not history — history lives in the
//! queue and the gateway store.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

/// Total bytes handed to the UDP socket (source + repair symbols, all retries).
static BYTES_ATTEMPTED: AtomicU64 = AtomicU64::new(0);
/// Total datagrams handed to the UDP socket.
static DATAGRAMS_SENT: AtomicU64 = AtomicU64::new(0);
/// Sealed-envelope bytes of bundles confirmed by an authenticated receipt.
static BYTES_ACKED: AtomicU64 = AtomicU64::new(0);
/// Bundles confirmed delivered (direct or via peer relay) this session.
static BUNDLES_ACKED: AtomicU64 = AtomicU64::new(0);

/// Record `len` bytes of one datagram entering the socket. Called on the hot send
/// path, so it is two relaxed atomic adds and nothing else.
pub fn record_attempted(len: usize) {
    BYTES_ATTEMPTED.fetch_add(len as u64, Ordering::Relaxed);
    DATAGRAMS_SENT.fetch_add(1, Ordering::Relaxed);
}

/// Record a bundle of `envelope_len` sealed bytes reaching a verified receipt.
pub fn record_acked(envelope_len: usize) {
    BYTES_ACKED.fetch_add(envelope_len as u64, Ordering::Relaxed);
    BUNDLES_ACKED.fetch_add(1, Ordering::Relaxed);
}

/// A point-in-time copy of the counters, shaped for the `/api/status` JSON.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Snapshot {
    /// Bytes handed to the UDP socket this session (incl. FEC overhead + retries).
    pub bytes_attempted: u64,
    /// Datagrams handed to the UDP socket this session.
    pub datagrams_sent: u64,
    /// Envelope bytes of bundles with a verified `DELIVERED` receipt.
    pub bytes_acked: u64,
    /// Bundles with a verified `DELIVERED` receipt.
    pub bundles_acked: u64,
}

/// Read all counters. Individually relaxed reads are fine: the dashboard polls every
/// couple of seconds and only ever needs a consistent-enough picture.
#[must_use]
pub fn snapshot() -> Snapshot {
    Snapshot {
        bytes_attempted: BYTES_ATTEMPTED.load(Ordering::Relaxed),
        datagrams_sent: DATAGRAMS_SENT.load(Ordering::Relaxed),
        bytes_acked: BYTES_ACKED.load(Ordering::Relaxed),
        bundles_acked: BUNDLES_ACKED.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Counters are process-global, so this single test exercises both paths and asserts
    // only monotonic growth (other tests in the binary may also bump them).
    #[test]
    fn counters_accumulate_monotonically() {
        let before = snapshot();
        record_attempted(1100);
        record_attempted(900);
        record_acked(4096);
        let after = snapshot();
        assert!(after.bytes_attempted >= before.bytes_attempted + 2000);
        assert!(after.datagrams_sent >= before.datagrams_sent + 2);
        assert!(after.bytes_acked >= before.bytes_acked + 4096);
        assert!(after.bundles_acked > before.bundles_acked);
    }
}
