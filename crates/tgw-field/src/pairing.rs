//! Field-side pairing handshake over UDP: dial the hospital's public port, run SPAKE2 as the
//! initiator, and return the derived session key. Retransmits `PAIR_INIT` on a short timer so
//! the handshake survives the lossy link (it also opens the NAT mapping), bounded by a deadline.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use tgw_core::{Key, PairFrame, decode_pair, encode_pair, start_initiator};
use tokio::net::UdpSocket;
use tokio::time::{Instant, timeout};

/// Retransmit interval for `PAIR_INIT` / `PAIR_CONFIRM` while awaiting a response.
const RETRANSMIT: Duration = Duration::from_millis(500);

/// Pair with the hospital at `hospital_addr` using the human `code`; return the session key.
pub async fn pair_with_hospital(hospital_addr: &str, code: &str, deadline: Duration) -> Result<Key> {
    let sock = UdpSocket::bind("0.0.0.0:0").await.context("pair: bind")?;
    sock.connect(hospital_addr)
        .await
        .with_context(|| format!("pair: connect {hospital_addr}"))?;

    let (initiator, msg_a) = start_initiator(code);
    let mut cookie: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 2048];
    let end = Instant::now() + deadline;

    // Phase 1: send INIT (with whatever cookie we have) until we get a full RESP.
    let (session, responder_confirm) = loop {
        if Instant::now() >= end {
            bail!("pairing timed out — check the code and that the hospital port is reachable");
        }
        let init = PairFrame::Init {
            cookie: cookie.clone(),
            msg: msg_a.clone(),
        };
        sock.send(&encode_pair(&init)).await.context("pair: send init")?;

        match timeout(RETRANSMIT, sock.recv(&mut buf)).await {
            Err(_) => continue, // silence → retransmit
            Ok(Ok(n)) => match decode_pair(&buf[..n]) {
                Some(PairFrame::Resp {
                    cookie: c,
                    msg: msg_b,
                    confirm,
                }) => {
                    // A cookie-only challenge carries an empty SPAKE2 msg: echo the cookie, retry.
                    if msg_b.is_empty() && !c.is_empty() {
                        cookie = c;
                        continue;
                    }
                    let session = initiator.finish(&msg_b).context("pair: finish")?;
                    break (session, confirm);
                }
                _ => continue, // stray/garbage datagram on our socket
            },
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "pair: recv error");
                continue;
            }
        }
    };

    // Verify the hospital proved it derived the same key BEFORE accepting.
    if !session.verify_responder_confirm(&responder_confirm) {
        bail!("pairing failed: wrong code or a man-in-the-middle (key confirmation mismatch)");
    }

    // Phase 2: send our confirmation. Loss of CONFIRM is covered by the hospital re-sending
    // RESP, which we simply answer again; a stretch of silence means it accepted.
    let confirm = PairFrame::Confirm {
        confirm: session.initiator_confirm().to_vec(),
    };
    let encoded = encode_pair(&confirm);
    loop {
        sock.send(&encoded).await.context("pair: send confirm")?;
        match timeout(RETRANSMIT, sock.recv(&mut buf)).await {
            Err(_) => break, // no more RESP retries arriving → the hospital accepted
            Ok(Ok(n)) => match decode_pair(&buf[..n]) {
                Some(PairFrame::Resp { .. }) => {} // hospital retried; re-send confirm
                _ => break,
            },
            Ok(Err(_)) => break,
        }
        if Instant::now() >= end {
            break;
        }
    }

    Ok(session.into_key())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tgw_core::{PairFrame, decode_pair, encode_pair, start_responder};

    #[tokio::test]
    async fn pairs_against_a_minimal_responder() {
        // Minimal cookieless responder: runs SPAKE2 B, confirms, verifies the field's confirm.
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.expect("bind");
        let addr = sock.local_addr().expect("addr").to_string();
        let server = tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            let (n, from) = sock.recv_from(&mut buf).await.expect("recv init");
            let PairFrame::Init { msg: msg_a, .. } = decode_pair(&buf[..n]).expect("init") else {
                panic!("expected init")
            };
            let (responder, msg_b) = start_responder("code-123");
            let session = responder.finish(&msg_a).expect("finish");
            let resp = PairFrame::Resp {
                cookie: vec![],
                msg: msg_b,
                confirm: session.responder_confirm().to_vec(),
            };
            sock.send_to(&encode_pair(&resp), from).await.expect("send resp");
            let (n, _) = sock.recv_from(&mut buf).await.expect("recv confirm");
            let PairFrame::Confirm { confirm } = decode_pair(&buf[..n]).expect("confirm") else {
                panic!("expected confirm")
            };
            assert!(session.verify_initiator_confirm(&confirm), "field confirm verifies");
        });

        let key = pair_with_hospital(&addr, "code-123", Duration::from_secs(5))
            .await
            .expect("pairing succeeds");
        server.await.expect("server ok");
        assert_eq!(key.to_hex().len(), 64);
    }
}
