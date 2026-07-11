//! Local peer discovery for the relay fallback (Fix 2b).
//!
//! Health workers/ambulances operating in the same flood-affected area are plausibly within
//! WiFi range of each other even when each one's own long-range radio link to the hospital is
//! degraded (stated as an assumption, not a certainty). This module lets a device announce its
//! presence on the local subnet by UDP broadcast and collect the relay addresses of peers that
//! do the same — no manual IP entry in the field, and deliberately NOT a DHT or mesh routing
//! protocol (Fix 2 is one relay hop only).
//!
//! The announcement carries only a device's per-run instance id and its relay-listen address —
//! never key material, never any bundle content.
//!
//! # Local convergence (Fix F3)
//! The discovery socket is built with `SO_REUSEADDR` + `SO_REUSEPORT` so two instances on one
//! host can co-bind the discovery port — without this, discovery could not be proven in CI/dev.
//! Announcements are self-filtered by a per-run **instance id**, not by address: with the
//! default `relay_listen_addr` every device advertises the same `0.0.0.0:…` string, so filtering
//! on address would make each peer discard the others as "self." When the discovery address is a
//! multicast group the socket joins it with multicast loopback enabled, so co-bound instances
//! reliably receive each other's announces on a single host (loopback included); a plain
//! broadcast address still works for a production LAN segment.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use uuid::Uuid;

/// Magic prefix identifying a TGW presence announcement (`TGW Announce`).
const ANNOUNCE_MAGIC: &[u8; 4] = b"TGWA";
/// Announcement format version. Bumped to 2 for the instance-id field (Fix F3).
const ANNOUNCE_VERSION: u8 = 2;
/// Fixed header length: magic(4) + version(1) + instance id(16).
const ANNOUNCE_HEADER: usize = 4 + 1 + 16;
/// Cap on an announcement's advertised-address length, so a malformed datagram can't allocate.
const MAX_ANNOUNCE: usize = 128;

/// A discovered peer: the sender's per-run instance id and its advertised relay-listen address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerAnnounce {
    /// The announcing device's per-run instance id (used for self-filtering).
    pub instance_id: Uuid,
    /// The advertised relay-listen address to forward bundles to.
    pub relay_addr: String,
}

/// Encode a presence announcement advertising this device's `instance_id` and `relay_addr`.
#[must_use]
pub fn encode_announce(instance_id: Uuid, relay_addr: &str) -> Vec<u8> {
    let mut msg = Vec::with_capacity(ANNOUNCE_HEADER + relay_addr.len());
    msg.extend_from_slice(ANNOUNCE_MAGIC);
    msg.push(ANNOUNCE_VERSION);
    msg.extend_from_slice(instance_id.as_bytes());
    msg.extend_from_slice(relay_addr.as_bytes());
    msg
}

/// Decode a presence announcement into its instance id and advertised relay-listen address.
///
/// Returns `None` for anything that is not a well-formed, current-version announcement — a
/// stray datagram on the discovery port is ignored, never trusted.
#[must_use]
pub fn decode_announce(msg: &[u8]) -> Option<PeerAnnounce> {
    if msg.len() < ANNOUNCE_HEADER || msg.len() > ANNOUNCE_HEADER + MAX_ANNOUNCE {
        return None;
    }
    if &msg[0..4] != ANNOUNCE_MAGIC || msg[4] != ANNOUNCE_VERSION {
        return None;
    }
    let instance_id = Uuid::from_slice(&msg[5..ANNOUNCE_HEADER]).ok()?;
    let relay_addr = std::str::from_utf8(&msg[ANNOUNCE_HEADER..])
        .ok()?
        .to_string();
    Some(PeerAnnounce {
        instance_id,
        relay_addr,
    })
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

/// Build the discovery socket bound to `discovery_addr`'s port on `0.0.0.0`, with
/// `SO_REUSEADDR` + `SO_REUSEPORT` so multiple instances can co-bind on one host (Fix F3). A
/// multicast `discovery_addr` is joined with loopback delivery enabled (reliable one-host
/// delivery); a broadcast address enables `SO_BROADCAST` for a production LAN segment.
fn bind_discovery_socket(discovery_addr: &str) -> Result<UdpSocket> {
    let target: SocketAddr = discovery_addr
        .parse()
        .with_context(|| format!("discovery: parse address {discovery_addr}"))?;
    let bind_addr = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), target.port());

    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .context("discovery: create socket")?;
    sock.set_reuse_address(true)
        .context("discovery: set SO_REUSEADDR")?;
    #[cfg(unix)]
    sock.set_reuse_port(true)
        .context("discovery: set SO_REUSEPORT")?;
    sock.set_nonblocking(true)
        .context("discovery: set non-blocking")?;
    sock.bind(&bind_addr.into())
        .with_context(|| format!("discovery: bind {bind_addr}"))?;

    if target.ip().is_multicast() {
        if let std::net::IpAddr::V4(group) = target.ip() {
            sock.join_multicast_v4(&group, &Ipv4Addr::UNSPECIFIED)
                .with_context(|| format!("discovery: join multicast {group}"))?;
            // Deliver our own multicast to co-bound sockets on this host (one-host convergence).
            sock.set_multicast_loop_v4(true)
                .context("discovery: enable multicast loopback")?;
        }
    } else {
        sock.set_broadcast(true)
            .context("discovery: enable broadcast")?;
    }

    let std_sock: std::net::UdpSocket = sock.into();
    UdpSocket::from_std(std_sock).context("discovery: adopt socket into tokio")
}

/// Run presence discovery: announce our `own_relay_addr` every `interval` and record peers we
/// hear, keeping `table` current. Announcements are self-filtered by `own_instance_id` (not by
/// address), so devices sharing a `0.0.0.0:…` relay-listen string still discover one another.
///
/// Bound to `discovery_addr` (a multicast group or broadcast address). Runs until the process
/// exits; a bind failure is surfaced so the daemon can report it.
pub async fn run_discovery(
    discovery_addr: &str,
    own_instance_id: Uuid,
    own_relay_addr: &str,
    interval: Duration,
    table: PeerTable,
) -> Result<()> {
    let sock = bind_discovery_socket(discovery_addr)?;

    let announce = encode_announce(own_instance_id, own_relay_addr);
    let mut ticker = tokio::time::interval(interval);
    let mut buf = vec![0u8; ANNOUNCE_HEADER + MAX_ANNOUNCE];

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
                            && peer.instance_id != own_instance_id
                        {
                            table.observe(peer.relay_addr, Instant::now());
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
        let id = Uuid::new_v4();
        let msg = encode_announce(id, "192.168.1.7:47556");
        assert_eq!(
            decode_announce(&msg),
            Some(PeerAnnounce {
                instance_id: id,
                relay_addr: "192.168.1.7:47556".to_string(),
            })
        );
    }

    #[test]
    fn decode_rejects_foreign_or_truncated_datagrams() {
        let good = encode_announce(Uuid::new_v4(), "10.0.0.2:47556");
        assert!(decode_announce(b"").is_none());
        assert!(decode_announce(b"TGW").is_none(), "too short (no header)");
        // Right length but wrong magic / version.
        let mut wrong_magic = good.clone();
        wrong_magic[0] = b'X';
        assert!(decode_announce(&wrong_magic).is_none(), "wrong magic");
        let mut wrong_version = good.clone();
        wrong_version[4] = 0x01;
        assert!(decode_announce(&wrong_version).is_none(), "wrong version");
        // A header with no address bytes is still a valid (empty-addr) announce; a header short
        // one byte is not.
        assert!(
            decode_announce(&good[..ANNOUNCE_HEADER - 1]).is_none(),
            "truncated header"
        );
    }

    #[test]
    fn self_filter_is_by_instance_id_not_address() {
        // Two devices announcing the SAME default relay address must still be distinguishable —
        // the instance id is what separates self from peer (Fix F3).
        let me = Uuid::new_v4();
        let peer = Uuid::new_v4();
        let shared_addr = "0.0.0.0:47556";
        let mine = decode_announce(&encode_announce(me, shared_addr)).expect("mine");
        let theirs = decode_announce(&encode_announce(peer, shared_addr)).expect("theirs");
        assert_eq!(
            mine.relay_addr, theirs.relay_addr,
            "same advertised address"
        );
        assert_ne!(
            mine.instance_id, theirs.instance_id,
            "distinct instance ids keep them from filtering each other out as self"
        );
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
