//! Peer-relay fallback path (Fix 2b): forward a still-sealed bundle through a nearby peer
//! when a device's own direct link to the hospital is degraded.
//!
//! # Zero-trust, opaque forwarding
//! The relay peer only ever holds and forwards **ciphertext**. Device A, whose direct hop is
//! failing, hands the peer the exact set of sealed, integrity-tagged DATA datagrams it would
//! otherwise have sent to the gateway. The peer forwards those opaque bytes to its own gateway
//! next-hop and forwards the gateway's authenticated receipt back to A — it never has A's PSK,
//! never decodes a symbol, and never sees plaintext. Because the receipt is AEAD-authenticated
//! under the shared key (`verify_receipt`), A accepts a relayed receipt without trusting the
//! peer's honesty: a peer that fabricates or tampers with a receipt is rejected outright.
//!
//! Scope (Fix 2): a single relay hop, one relay request per bundle carried in one UDP
//! datagram. Bundles whose full burst exceeds one datagram (large images) and relay-of-a-relay
//! are explicitly out of scope; vitals and small bundles are the target case.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use tgw_core::{Datagram, Key, verify_receipt};
use tokio::net::UdpSocket;
use tokio::time::timeout;
use uuid::Uuid;

use crate::sender::Outcome;

/// Magic prefix identifying a relay request (`TGW Relay`).
const RELAY_MAGIC: &[u8; 4] = b"TGWR";
/// Relay request format version.
const RELAY_VERSION: u8 = 1;

/// A relay request: an opaque bundle to forward, addressed only by its id.
///
/// `datagrams` are sealed + integrity-tagged DATA frames built by the originator; the relay
/// treats them as opaque bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayRequest {
    /// The bundle these datagrams reconstruct at the gateway.
    pub bundle_id: Uuid,
    /// The opaque, sealed FEC datagrams to forward.
    pub datagrams: Vec<Datagram>,
}

/// Encode a relay request into a single datagram:
/// `MAGIC | version | uuid(16) | count(u16) | [len(u16) | datagram]...`.
#[must_use]
pub fn encode_relay_request(req: &RelayRequest) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(RELAY_MAGIC);
    out.push(RELAY_VERSION);
    out.extend_from_slice(req.bundle_id.as_bytes());
    let count = u16::try_from(req.datagrams.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&count.to_be_bytes());
    for dgram in req.datagrams.iter().take(usize::from(count)) {
        let len = u16::try_from(dgram.len()).unwrap_or(u16::MAX);
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&dgram[..usize::from(len)]);
    }
    out
}

/// Decode a relay request, returning `None` for anything malformed (a stray datagram on the
/// relay port is ignored, never acted on).
#[must_use]
pub fn decode_relay_request(msg: &[u8]) -> Option<RelayRequest> {
    if msg.len() < 4 + 1 + 16 + 2 || &msg[0..4] != RELAY_MAGIC || msg[4] != RELAY_VERSION {
        return None;
    }
    let bundle_id = Uuid::from_slice(&msg[5..21]).ok()?;
    let count = u16::from_be_bytes([msg[21], msg[22]]);
    let mut cursor = 23;
    let mut datagrams = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        let len = usize::from(u16::from_be_bytes([
            *msg.get(cursor)?,
            *msg.get(cursor + 1)?,
        ]));
        cursor += 2;
        let end = cursor.checked_add(len)?;
        datagrams.push(msg.get(cursor..end)?.to_vec());
        cursor = end;
    }
    Some(RelayRequest {
        bundle_id,
        datagrams,
    })
}

/// A-side: deliver `datagrams` for `bundle_id` through a discovered `peer` rather than
/// directly, and wait up to `budget` for a relayed, authenticated receipt.
///
/// Returns [`Outcome::Delivered`] only when a receipt that `verify_receipt` accepts for exactly
/// this `bundle_id` comes back — a fabricated or misdirected receipt from a dishonest peer is
/// ignored and the attempt ends [`Outcome::Stuck`], leaving the bundle in A's queue to retry.
pub async fn deliver_via_relay(
    peer: SocketAddr,
    bundle_id: Uuid,
    datagrams: &[Datagram],
    key: &Key,
    budget: Duration,
) -> Result<Outcome> {
    let sock = UdpSocket::bind("0.0.0.0:0")
        .await
        .context("relay: bind sender socket")?;
    let request = encode_relay_request(&RelayRequest {
        bundle_id,
        datagrams: datagrams.to_vec(),
    });
    crate::metrics::record_attempted(request.len());
    sock.send_to(&request, peer)
        .await
        .context("relay: send request to peer")?;
    tracing::info!(%bundle_id, %peer, "direct hop failed — relaying via peer");

    let mut buf = vec![0u8; 2048];
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            tracing::warn!(%bundle_id, %peer, "relay produced no verified receipt in budget");
            return Ok(Outcome::Stuck);
        }
        match timeout(remaining, sock.recv(&mut buf)).await {
            Ok(Ok(n)) => match verify_receipt(&buf[..n], key) {
                Ok(id) if id == bundle_id => {
                    tracing::info!(%bundle_id, %peer, "authenticated receipt via relay — delivered");
                    return Ok(Outcome::Delivered);
                }
                // A receipt for another bundle, or one that fails authentication (a tampered
                // or fabricated receipt from a dishonest relay): ignore and keep waiting.
                _ => {
                    tracing::debug!(%bundle_id, "ignoring unauthenticated/foreign relayed datagram")
                }
            },
            Ok(Err(e)) => tracing::debug!(error = %e, "relay: recv error"),
            Err(_) => return Ok(Outcome::Stuck),
        }
    }
}

/// A-side failover: try each candidate peer in order until one relays the bundle to a verified
/// receipt. Returns [`Outcome::Delivered`] on the first success, else [`Outcome::Stuck`] (the
/// bundle stays queued for a later pass). With no peers this returns `Stuck` immediately without
/// touching the network — nothing to fail over to.
pub async fn relay_failover(
    peers: &[SocketAddr],
    bundle_id: Uuid,
    datagrams: &[Datagram],
    key: &Key,
    budget_per_peer: Duration,
) -> Result<Outcome> {
    for peer in peers {
        if let Outcome::Delivered =
            deliver_via_relay(*peer, bundle_id, datagrams, key, budget_per_peer).await?
        {
            return Ok(Outcome::Delivered);
        }
        tracing::debug!(%peer, %bundle_id, "relay peer did not deliver; trying next");
    }
    Ok(Outcome::Stuck)
}

/// B-side: run the relay service, forwarding peers' opaque bundles to `gateway_addr` and
/// forwarding the gateway's authenticated receipts back to the originators.
///
/// This process is given **no key** for relayed bundles and never decodes a symbol — it is a
/// pure ciphertext forwarder. Runs until the process exits.
pub async fn run_relay_service(relay_listen_addr: &str, gateway_addr: SocketAddr) -> Result<()> {
    let sock = UdpSocket::bind(relay_listen_addr)
        .await
        .with_context(|| format!("relay service: bind {relay_listen_addr}"))?;
    tracing::info!(%relay_listen_addr, %gateway_addr, "relay service up (forwards ciphertext only)");
    let mut buf = vec![0u8; 65_535];

    loop {
        let (n, from) = sock
            .recv_from(&mut buf)
            .await
            .context("relay service: recv")?;
        let Some(request) = decode_relay_request(&buf[..n]) else {
            tracing::debug!(%from, "relay service: ignoring non-relay datagram");
            continue;
        };
        tracing::info!(
            bundle_id = %request.bundle_id,
            %from,
            datagrams = request.datagrams.len(),
            "relay service: forwarding a peer's sealed bundle"
        );
        // Forward opaquely; a failure here is logged, not fatal to the service.
        if let Err(e) = forward_to_gateway(&request, gateway_addr, from, &sock).await {
            tracing::warn!(bundle_id = %request.bundle_id, error = %e, "relay service: forward failed");
        }
    }
}

/// Forward one relay request's datagrams to the gateway and relay the receipt back to `origin`.
///
/// The relay's own next-hop delivery: it sends the opaque datagrams to the gateway and, on the
/// gateway's authenticated receipt, sends that receipt verbatim back to `origin`. It never
/// inspects, decrypts, or re-encodes the payload.
async fn forward_to_gateway(
    request: &RelayRequest,
    gateway_addr: SocketAddr,
    origin: SocketAddr,
    back_to_origin: &UdpSocket,
) -> Result<()> {
    let up = UdpSocket::bind("0.0.0.0:0")
        .await
        .context("relay: bind gateway-facing socket")?;
    up.connect(gateway_addr)
        .await
        .context("relay: connect gateway")?;
    for dgram in &request.datagrams {
        if let Err(e) = up.send(dgram).await {
            tracing::debug!(error = %e, "relay: gateway send failed (treated as loss)");
        }
    }

    // Wait for the gateway's receipt, then forward the raw bytes back to the originator. We do
    // not (and cannot) verify it here — only the originator holds the key — but a forged one is
    // rejected there by `verify_receipt`, so an honest forward of a bad receipt is harmless.
    let mut buf = vec![0u8; 2048];
    match timeout(Duration::from_secs(5), up.recv(&mut buf)).await {
        Ok(Ok(n)) => {
            back_to_origin
                .send_to(&buf[..n], origin)
                .await
                .context("relay: forward receipt to origin")?;
            tracing::info!(bundle_id = %request.bundle_id, %origin, "relay: receipt forwarded to origin");
        }
        Ok(Err(e)) => tracing::debug!(error = %e, "relay: gateway recv error"),
        Err(_) => {
            tracing::warn!(bundle_id = %request.bundle_id, "relay: no gateway receipt in budget")
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RelayRequest {
        RelayRequest {
            bundle_id: Uuid::new_v4(),
            datagrams: vec![vec![1, 2, 3, 4], vec![9; 1104], vec![]],
        }
    }

    #[test]
    fn relay_request_round_trips() {
        let req = sample();
        let encoded = encode_relay_request(&req);
        assert_eq!(decode_relay_request(&encoded).as_ref(), Some(&req));
    }

    #[tokio::test]
    async fn relay_failover_with_no_peers_is_stuck_without_network() {
        let outcome = relay_failover(
            &[],
            Uuid::new_v4(),
            &[vec![1, 2, 3]],
            &Key::generate(),
            Duration::from_millis(10),
        )
        .await
        .expect("failover");
        assert_eq!(
            outcome,
            Outcome::Stuck,
            "no discovered peers means nothing to fail over to"
        );
    }

    #[test]
    fn decode_rejects_malformed_requests() {
        assert!(decode_relay_request(b"").is_none());
        assert!(decode_relay_request(b"XXXX\x01").is_none(), "wrong magic");
        let mut good = encode_relay_request(&sample());
        good[4] = 0x02; // bad version
        assert!(decode_relay_request(&good).is_none());
        // Truncated mid-datagram: a claimed length that runs off the end must not panic.
        let mut truncated = encode_relay_request(&sample());
        truncated.truncate(30);
        assert!(decode_relay_request(&truncated).is_none());
    }
}
