//! Local peer discovery for the relay fallback (Fix 2b).
//!
//! Health workers/ambulances operating in the same flood-affected area are plausibly within
//! WiFi range of each other even when each one's own long-range radio link to the hospital is
//! degraded (stated as an assumption, not a certainty). This module lets a device announce its
//! presence on the local subnet by UDP broadcast and collect the relay addresses of peers that
//! do the same — no manual IP entry in the field, and deliberately NOT a DHT or mesh routing
//! protocol (Fix 2 is one relay hop only).
//!
//! The announcement carries only a device's relay-listen address — never key material, never
//! any bundle content.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::net::UdpSocket;

/// Magic prefix identifying a TGW presence announcement (`TGW Announce`).
const ANNOUNCE_MAGIC: &[u8; 4] = b"TGWA";
/// Announcement format version.
const ANNOUNCE_VERSION: u8 = 1;
/// Cap on an announcement's advertised-address length, so a malformed datagram can't allocate.
const MAX_ANNOUNCE: usize = 128;

/// Encode a presence announcement advertising this device's `relay_addr`.
#[must_use]
pub fn encode_announce(relay_addr: &str) -> Vec<u8> {
    let mut msg = Vec::with_capacity(5 + relay_addr.len());
    msg.extend_from_slice(ANNOUNCE_MAGIC);
    msg.push(ANNOUNCE_VERSION);
    msg.extend_from_slice(relay_addr.as_bytes());
    msg
}

/// Decode a presence announcement into the advertised relay-listen address.
///
/// Returns `None` for anything that is not a well-formed, current-version announcement — a
/// stray datagram on the discovery port is ignored, never trusted.
#[must_use]
pub fn decode_announce(msg: &[u8]) -> Option<String> {
    if msg.len() < 5 || msg.len() > 5 + MAX_ANNOUNCE {
        return None;
    }
    if &msg[0..4] != ANNOUNCE_MAGIC || msg[4] != ANNOUNCE_VERSION {
        return None;
    }
    std::str::from_utf8(&msg[5..]).ok().map(str::to_string)
}

/// A time-expiring set of discovered peer relay addresses.
///
/// A peer stays "active" only while its last announcement is within `ttl`, so a device that
/// moves out of range or powers off drops out of the candidate next-hop list automatically.
#[derive(Clone)]
pub struct PeerTable {
    ttl: Duration,
    peers: Arc<Mutex<HashMap<String, Instant>>>,
}

impl PeerTable {
    /// New table whose entries expire `ttl` after their last sighting.
    #[must_use]
    pub fn new(ttl: Duration) -> Self {
        PeerTable {
            ttl,
            peers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Record (or refresh) a peer sighting at `now`.
    pub fn observe(&self, relay_addr: String, now: Instant) {
        if let Ok(mut peers) = self.peers.lock() {
            peers.insert(relay_addr, now);
        }
    }

    /// Active peers as of `now`, oldest-first for stable failover ordering.
    #[must_use]
    pub fn active(&self, now: Instant) -> Vec<String> {
        let Ok(peers) = self.peers.lock() else {
            return Vec::new();
        };
        let mut active: Vec<(String, Instant)> = peers
            .iter()
            .filter(|(_, seen)| now.duration_since(**seen) <= self.ttl)
            .map(|(addr, seen)| (addr.clone(), *seen))
            .collect();
        active.sort_by(|a, b| a.1.cmp(&b.1));
        active.into_iter().map(|(addr, _)| addr).collect()
    }

    /// Drop entries whose last sighting is older than `ttl` as of `now`.
    pub fn prune(&self, now: Instant) {
        if let Ok(mut peers) = self.peers.lock() {
            peers.retain(|_, seen| now.duration_since(*seen) <= self.ttl);
        }
    }
}

/// Run presence discovery: broadcast our `relay_addr` every `interval` and record peers we
/// hear, keeping `table` current. `own_relay_addr` is filtered so we never discover ourselves.
///
/// Bound to `discovery_addr` (a broadcast address in production). Runs until the process exits;
/// a bind failure is surfaced so the daemon can report it. The live-broadcast loop is exercised
/// operationally; the pure codec and [`PeerTable`] carry the unit-tested logic.
pub async fn run_discovery(
    discovery_addr: &str,
    own_relay_addr: &str,
    interval: Duration,
    table: PeerTable,
) -> Result<()> {
    let bind_port = discovery_addr
        .rsplit_once(':')
        .map(|(_, port)| format!("0.0.0.0:{port}"))
        .unwrap_or_else(|| "0.0.0.0:47555".to_string());
    let sock = UdpSocket::bind(&bind_port)
        .await
        .with_context(|| format!("discovery: bind {bind_port}"))?;
    sock.set_broadcast(true)
        .context("discovery: enable broadcast")?;

    let announce = encode_announce(own_relay_addr);
    let mut ticker = tokio::time::interval(interval);
    let mut buf = vec![0u8; 5 + MAX_ANNOUNCE];

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if let Err(e) = sock.send_to(&announce, discovery_addr).await {
                    tracing::debug!(error = %e, "discovery: announce send failed");
                }
                table.prune(Instant::now());
            }
            recv = sock.recv_from(&mut buf) => {
                match recv {
                    Ok((n, _from)) => {
                        if let Some(peer) = decode_announce(&buf[..n])
                            && peer != own_relay_addr
                        {
                            table.observe(peer, Instant::now());
                        }
                    }
                    Err(e) => tracing::debug!(error = %e, "discovery: recv failed"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announce_round_trips() {
        let msg = encode_announce("192.168.1.7:47556");
        assert_eq!(decode_announce(&msg).as_deref(), Some("192.168.1.7:47556"));
    }

    #[test]
    fn decode_rejects_foreign_or_truncated_datagrams() {
        assert!(decode_announce(b"").is_none());
        assert!(decode_announce(b"XXXX\x01addr").is_none(), "wrong magic");
        assert!(decode_announce(b"TGWA\x02addr").is_none(), "wrong version");
        assert!(decode_announce(b"TGW").is_none(), "too short");
    }

    #[test]
    fn peer_table_expires_stale_entries() {
        let ttl = Duration::from_secs(5);
        let table = PeerTable::new(ttl);
        let t0 = Instant::now();
        table.observe("10.0.0.2:47556".into(), t0);

        assert_eq!(table.active(t0), vec!["10.0.0.2:47556".to_string()]);
        // Still fresh just before the TTL edge.
        assert_eq!(table.active(t0 + Duration::from_secs(4)).len(), 1);
        // Expired past the TTL.
        assert!(table.active(t0 + Duration::from_secs(6)).is_empty());
    }

    #[test]
    fn peer_table_orders_oldest_first_and_prunes() {
        let table = PeerTable::new(Duration::from_secs(10));
        let t0 = Instant::now();
        table.observe("a:1".into(), t0);
        table.observe("b:2".into(), t0 + Duration::from_secs(1));
        assert_eq!(
            table.active(t0 + Duration::from_secs(2)),
            vec!["a:1".to_string(), "b:2".to_string()],
            "oldest sighting first for stable failover order"
        );

        table.prune(t0 + Duration::from_secs(100));
        assert!(
            table.active(t0 + Duration::from_secs(100)).is_empty(),
            "prune drops everything past the TTL"
        );
    }
}
