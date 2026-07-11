//! Hardened gateway-side pairing responder for the PUBLIC UDP port.
//!
//! Security posture (a public port faces the whole internet):
//! * **Anti-spoof cookie:** the first `PAIR_INIT` from a source gets only a stateless cookie
//!   challenge — no SPAKE2 work, no allocation. Only a datagram echoing a cookie valid for its
//!   own source address triggers the (relatively costly) curve computation, so a spoofed source
//!   IP can neither exhaust state nor be used for reflection (the challenge is small).
//! * **Single online guess + confirmation:** SPAKE2 is a balanced PAKE, so an active attacker
//!   gets one online guess per handshake and learns nothing for an offline dictionary attack.
//! * **Lockout:** after `max_failed_confirms` bad confirmations the responder gives up and the
//!   operator must re-run `pair` with a fresh code, defeating sustained online guessing.
//! * **Bounded window:** the responder runs only until the first success or the deadline.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tgw_core::{CookieKey, Key, PairFrame, decode_pair, encode_pair, start_responder};
use tokio::net::UdpSocket;
use tokio::time::{Instant, timeout};

/// Coarse epoch (30-second bucket) for cookie freshness.
fn epoch_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 30)
        .unwrap_or(0)
}

/// Limits for the pairing window.
pub struct PairLimits {
    /// Bad confirmations tolerated before the code is abandoned (online-guess lockout).
    pub max_failed_confirms: u32,
    /// Hard deadline for the whole pairing window.
    pub deadline: Duration,
}

impl Default for PairLimits {
    fn default() -> Self {
        PairLimits {
            max_failed_confirms: 5,
            deadline: Duration::from_secs(120),
        }
    }
}

/// Serve the pairing handshake on `bind_addr`; return the derived key on first confirmed pair.
pub async fn run_pair_responder(
    bind_addr: SocketAddr,
    code: &str,
    limits: PairLimits,
) -> Result<Key> {
    let sock = UdpSocket::bind(bind_addr)
        .await
        .with_context(|| format!("pair responder: bind {bind_addr}"))?;
    tracing::info!(%bind_addr, "pairing window open (waiting for field to connect)");
    let cookie_key = CookieKey::random();
    let mut buf = vec![0u8; 2048];
    let mut failed_confirms: u32 = 0;
    // The SPAKE2 session for the peer we are mid-handshake with (source-pinned).
    let mut pending: Option<(SocketAddr, tgw_core::PairSession)> = None;
    let end = Instant::now() + limits.deadline;

    loop {
        let remaining = end.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("pairing window expired with no successful pairing");
        }
        let (n, src) = match timeout(remaining, sock.recv_from(&mut buf)).await {
            Err(_) => bail!("pairing window expired with no successful pairing"),
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "pair responder: recv error");
                continue;
            }
        };
        let Some(frame) = decode_pair(&buf[..n]) else {
            tracing::debug!(from = %src, "pair responder: ignoring non-pair datagram");
            continue;
        };

        match frame {
            PairFrame::Init { cookie, msg } => {
                let ep = epoch_now();
                // No/invalid cookie → issue a stateless challenge, do NOTHING else.
                if cookie.is_empty() || !cookie_key.verify(&src, ep, &cookie) {
                    let challenge = PairFrame::Resp {
                        cookie: cookie_key.mint(&src, ep).to_vec(),
                        msg: Vec::new(),
                        confirm: Vec::new(),
                    };
                    let _ = sock.send_to(&encode_pair(&challenge), src).await;
                    continue;
                }
                // Cookie valid for THIS source: run SPAKE2 B now.
                let (responder, msg_b) = start_responder(code);
                let session = match responder.finish(&msg) {
                    Ok(s) => s,
                    Err(_) => {
                        tracing::debug!(from = %src, "pair responder: bad SPAKE2 message");
                        continue;
                    }
                };
                let resp = PairFrame::Resp {
                    cookie: cookie.clone(),
                    msg: msg_b,
                    confirm: session.responder_confirm().to_vec(),
                };
                let _ = sock.send_to(&encode_pair(&resp), src).await;
                pending = Some((src, session));
            }
            PairFrame::Confirm { confirm } => {
                // Only a confirm from the source we are mid-handshake with is considered.
                let matches_pending = matches!(&pending, Some((psrc, _)) if *psrc == src);
                if !matches_pending {
                    tracing::debug!(from = %src, "pair responder: confirm without matching handshake");
                    continue;
                }
                // Re-take ownership to move the key out without cloning the session.
                if let Some((_, session)) = pending.take() {
                    if session.verify_initiator_confirm(&confirm) {
                        tracing::info!(from = %src, "pairing confirmed — session key established");
                        return Ok(session.into_key());
                    }
                    failed_confirms += 1;
                    tracing::warn!(from = %src, failed_confirms, "pairing confirmation failed (wrong code?)");
                    if failed_confirms >= limits.max_failed_confirms {
                        bail!("too many failed pairing attempts — re-run `pair` with a fresh code");
                    }
                }
            }
            PairFrame::Resp { .. } => {
                tracing::debug!(from = %src, "pair responder: ignoring inbound RESP");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tgw_core::{PairFrame, decode_pair, encode_pair, start_initiator};

    #[tokio::test]
    async fn completes_a_cookie_gated_handshake_and_returns_the_key() {
        // Bind first to learn the port, then hand the addr to the responder (rebinds on loopback).
        let probe = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);

        let server = tokio::spawn(async move {
            run_pair_responder(addr, "pair-xyz", PairLimits::default()).await
        });
        // Give the responder a moment to bind.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let cli = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        cli.connect(addr).await.unwrap();
        let (initiator, msg_a) = start_initiator("pair-xyz");
        // First INIT (no cookie) → expect a cookie challenge.
        cli.send(&encode_pair(&PairFrame::Init {
            cookie: vec![],
            msg: msg_a.clone(),
        }))
        .await
        .unwrap();
        let mut buf = vec![0u8; 2048];
        let n = cli.recv(&mut buf).await.unwrap();
        let PairFrame::Resp { cookie, msg, .. } = decode_pair(&buf[..n]).unwrap() else {
            panic!("expected resp challenge")
        };
        assert!(
            msg.is_empty() && !cookie.is_empty(),
            "first reply is a cookie challenge"
        );
        // Echo the cookie → real RESP.
        cli.send(&encode_pair(&PairFrame::Init { cookie, msg: msg_a }))
            .await
            .unwrap();
        let n = cli.recv(&mut buf).await.unwrap();
        let PairFrame::Resp {
            msg: msg_b,
            confirm,
            ..
        } = decode_pair(&buf[..n]).unwrap()
        else {
            panic!("expected real resp")
        };
        let session = initiator.finish(&msg_b).unwrap();
        assert!(session.verify_responder_confirm(&confirm));
        cli.send(&encode_pair(&PairFrame::Confirm {
            confirm: session.initiator_confirm().to_vec(),
        }))
        .await
        .unwrap();

        let key = server.await.unwrap().expect("responder returns key");
        assert_eq!(key.to_hex(), session.into_key().to_hex());
    }
}
