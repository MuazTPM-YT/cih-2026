//! The delivery loop (muaz.md H6–10): burst → listen → NACK repair / receipt / timeout
//! re-burst with linear backoff (`retry_backoff_ms × attempt`) → `delivered` or `stuck`.
//! Nothing is ever silently dropped.
//!
//! ```text
//! queued(redb) → sending(burst @ overhead×) → await(receipt | nack, timeout T)
//!     ▲                                        │            │
//!     │           ┌── NACK: mint + send fresh repair ◀──────┘
//!     └─ timeout: re-burst w/ backoff (max R tries → 'stuck', kept & visible)
//!                                       RECEIPT (AEAD-verified) → delivered
//! ```

use std::time::Duration;

use anyhow::Result;
use tgw_core::{BundleSender, Frame, Key, RetryConfig, parse_frame, verify_receipt};
use tokio::net::UdpSocket;
use tokio::time::timeout;
use uuid::Uuid;

use crate::pacer::Pacer;

/// Fraction of a block's source symbols minted per silence re-burst. Half a window per
/// retry balances convergence speed against the bandwidth budget.
const REBURST_FRACTION: f32 = 0.5;

/// Why a delivery attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Authenticated receipt received — the bundle really is at the gateway.
    Delivered,
    /// `max_retries` exhausted with no receipt: flagged, kept, retryable later.
    Stuck,
    /// A higher-priority bundle arrived; this transfer stepped aside (daemon mode).
    Preempted,
}

/// Deliver one bundle over `socket` (already `connect`ed to the gateway).
///
/// * Sends the initial burst, then serves NACKs and waits for the receipt.
/// * On silence past the attempt deadline, re-bursts fresh repair symbols. The deadline
///   grows **linearly** — `retry_backoff_ms × attempt` (`3s, 6s, 9s, …`), not
///   exponentially — a deliberately gentle schedule: on a delay-tolerant link we prefer
///   steady, bandwidth-frugal retries over aggressive exponential fallback that would
///   idle the link for minutes after a burst.
/// * `should_preempt` is polled at each timeout; returning `true` pauses this transfer
///   (the caller re-queues it) so vitals can preempt an in-flight image.
///
/// All sends are paced — resilience never exceeds the bandwidth budget.
pub async fn deliver(
    socket: &UdpSocket,
    fec_sender: &mut BundleSender,
    pacer: &mut Pacer,
    key: &Key,
    retry: &RetryConfig,
    mut should_preempt: impl FnMut() -> bool,
) -> Result<Outcome> {
    let bundle_id = fec_sender.bundle_id();

    let burst = fec_sender.initial_burst();
    tracing::info!(
        bundle_id = %bundle_id,
        datagrams = burst.len(),
        source_symbols = fec_sender.total_source_symbols(),
        "initial burst"
    );
    send_paced(socket, pacer, &burst).await?;

    let mut receive_buffer = vec![0u8; 2048];
    for attempt in 1..=retry.max_retries {
        let deadline =
            Duration::from_millis(retry.retry_backoff_ms.saturating_mul(u64::from(attempt)));
        let wait_started = tokio::time::Instant::now();

        // Serve NACKs / wait for the receipt until this attempt's deadline.
        loop {
            let remaining = deadline.saturating_sub(wait_started.elapsed());
            if remaining.is_zero() {
                break;
            }
            let received = match timeout(remaining, socket.recv(&mut receive_buffer)).await {
                Err(_elapsed) => break, // silence — fall through to re-burst
                Ok(Ok(len)) => len,
                Ok(Err(e)) => {
                    // Connected-UDP surfaces ICMP unreachable (gateway down/restarting
                    // — demo step 4) as recv errors. That's link weather, not failure:
                    // the retry/backoff machinery owns recovery.
                    tracing::debug!(bundle_id = %bundle_id, error = %e, "recv error; treating as loss");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };
            match parse_frame(&receive_buffer[..received]) {
                Ok(Frame::Receipt { .. }) => {
                    // Trust only the AEAD tag, not the parse.
                    match verify_receipt(&receive_buffer[..received], key) {
                        Ok(receipt_id) if receipt_id == bundle_id => {
                            tracing::info!(bundle_id = %bundle_id, "authenticated receipt — delivered");
                            return Ok(Outcome::Delivered);
                        }
                        Ok(other_id) => {
                            tracing::debug!(bundle_id = %other_id, "receipt for a different bundle; ignoring");
                        }
                        Err(e) => {
                            tracing::warn!(bundle_id = %bundle_id, error = %e, "receipt failed authentication; ignoring");
                        }
                    }
                }
                Ok(Frame::Nack(nack)) if nack.bundle_id == bundle_id => {
                    let repairs = fec_sender.respond_to_nack(&nack);
                    tracing::info!(
                        bundle_id = %bundle_id,
                        requested = nack.needed.iter().sum::<u32>(),
                        minted = repairs.len(),
                        "NACK — sending fresh repair symbols"
                    );
                    send_paced(socket, pacer, &repairs).await?;
                }
                Ok(_) => {} // DATA or foreign NACK: not for us
                Err(e) => {
                    tracing::debug!(error = %e, "ignoring malformed datagram");
                }
            }
        }

        if should_preempt() {
            tracing::info!(bundle_id = %bundle_id, "vitals waiting — preempting this transfer");
            return Ok(Outcome::Preempted);
        }

        let repairs = fec_sender.repair_burst(REBURST_FRACTION);
        tracing::info!(
            bundle_id = %bundle_id,
            attempt,
            max_retries = retry.max_retries,
            datagrams = repairs.len(),
            "silence past deadline — re-bursting fresh repair symbols"
        );
        send_paced(socket, pacer, &repairs).await?;
    }

    tracing::warn!(
        bundle_id = %bundle_id,
        retries = retry.max_retries,
        "retries exhausted — bundle flagged stuck (kept, never dropped)"
    );
    Ok(Outcome::Stuck)
}

/// Send each datagram through the token bucket, then the socket.
///
/// Send errors are logged and the datagram dropped — over UDP a failed send is
/// indistinguishable from loss on the wire, and the FEC + NACK + re-burst machinery is
/// built to absorb exactly that. (ICMP port-unreachable while the gateway restarts is
/// the common case; aborting would break kill-and-resume.)
async fn send_paced(socket: &UdpSocket, pacer: &mut Pacer, datagrams: &[Vec<u8>]) -> Result<()> {
    for datagram in datagrams {
        pacer.acquire(datagram.len()).await;
        if let Err(e) = socket.send(datagram).await {
            tracing::debug!(error = %e, "udp send failed; counting it as loss");
        }
    }
    Ok(())
}

/// Verify a receipt datagram against `key` and return the bundle it acknowledges.
/// (Convenience re-export for daemon/CLI callers that handle sockets themselves.)
pub fn authenticated_receipt(datagram: &[u8], key: &Key) -> Option<Uuid> {
    verify_receipt(datagram, key).ok()
}
