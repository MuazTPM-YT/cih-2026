//! End-to-end delivery over real UDP sockets with deterministic simulated loss.
//!
//! This is the field client's own evidence (Twaha's `tests/lossy_delivery.rs` with
//! `tgw-netsim` is the workspace's headline test — this one exercises Muaz-owned code
//! only): a mock in-process gateway drops 25% of DATA datagrams by seeded RNG, runs a
//! NACK stall timer, and answers completion with an authenticated receipt. Asserts the
//! full `queued → sending → delivered` loop including repair rounds.

use std::time::Duration;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tgw_core::{
    Absorb, Bundle, BundleReceiver, BundleSender, FecConfig, Frame, Key, RetryConfig,
    build_receipt, encode_nack, parse_frame,
};
use tgw_field::pacer::Pacer;
use tgw_field::sender::{Outcome, deliver};
use tokio::net::UdpSocket;

const SEED: u64 = 0x2026_0711_0003;
const LOSS_PERCENT: u32 = 25;

fn fec() -> FecConfig {
    FecConfig {
        symbol_size: 1100,
        overhead_factor: 1.4,
    }
}

/// Short timers so the test finishes fast; ratios mirror the real config.
fn retry() -> RetryConfig {
    RetryConfig {
        nack_timeout_ms: 150,
        retry_backoff_ms: 400,
        max_retries: 8,
        ..RetryConfig::default()
    }
}

/// Mock gateway: receive → maybe drop (seeded) → absorb → NACK on stall / receipt on
/// completion. Returns the decoded bundle.
async fn mock_gateway(socket: UdpSocket, key: Key, drop_percent: u32, seed: u64) -> Bundle {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut receiver = BundleReceiver::new(key.clone());
    let mut buffer = vec![0u8; 2048];
    let stall = Duration::from_millis(retry().nack_timeout_ms);

    loop {
        let received = match tokio::time::timeout(stall, socket.recv_from(&mut buffer)).await {
            Ok(Ok((len, from))) => {
                socket.connect(from).await.ok();
                len
            }
            Ok(Err(e)) => panic!("gateway recv failed: {e}"),
            Err(_stalled) => {
                // Decode stalled — ask for exactly what's missing (the NACK loop).
                if let Some(nack) = receiver.build_nack() {
                    let datagram = encode_nack(&nack);
                    if socket.send(&datagram).await.is_err() {
                        // Not connected yet (nothing ever arrived) — keep waiting.
                    }
                }
                continue;
            }
        };

        // The lossy link, deterministic per seed.
        if rng.gen_range(0..100) < drop_percent {
            continue;
        }
        let datagram = &buffer[..received];
        if !matches!(parse_frame(datagram), Ok(Frame::Data { .. })) {
            continue;
        }
        match receiver.absorb(datagram) {
            Ok(Absorb::Complete(bundle)) => {
                let receipt = build_receipt(bundle.id, &key);
                if let Err(e) = socket.send(&receipt).await {
                    panic!("gateway receipt send failed: {e}");
                }
                // Linger like a real gateway: keep answering duplicate arrivals with
                // idempotent receipts while the sender finishes its in-flight burst.
                let grace = Duration::from_millis(300);
                let deadline = tokio::time::Instant::now() + grace;
                while tokio::time::Instant::now() < deadline {
                    let remaining = deadline - tokio::time::Instant::now();
                    match tokio::time::timeout(remaining, socket.recv(&mut buffer)).await {
                        Ok(Ok(_)) => {
                            let receipt = build_receipt(bundle.id, &key);
                            let _ = socket.send(&receipt).await;
                        }
                        _ => break,
                    }
                }
                return bundle;
            }
            Ok(_) => {}
            Err(e) => panic!("gateway absorb failed on authentic data: {e}"),
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn image_bundle_survives_25_percent_loss_and_gets_receipt() {
    let key = Key::generate();
    let mut rng = StdRng::seed_from_u64(SEED);
    let mut image = vec![0u8; 25_000];
    rng.fill(image.as_mut_slice());
    let bundle = Bundle::new_image("image/jpeg".into(), image, "P-1023".into());

    let gateway_socket = match UdpSocket::bind("127.0.0.1:0").await {
        Ok(s) => s,
        Err(e) => panic!("bind gateway: {e}"),
    };
    let gateway_addr = match gateway_socket.local_addr() {
        Ok(a) => a,
        Err(e) => panic!("gateway addr: {e}"),
    };
    let gateway = tokio::spawn(mock_gateway(
        gateway_socket,
        key.clone(),
        LOSS_PERCENT,
        SEED + 1,
    ));

    let field_socket = match UdpSocket::bind("127.0.0.1:0").await {
        Ok(s) => s,
        Err(e) => panic!("bind field: {e}"),
    };
    if let Err(e) = field_socket.connect(gateway_addr).await {
        panic!("connect: {e}");
    }

    let mut fec_sender = match BundleSender::new(&bundle, &key, &fec()) {
        Ok(s) => s,
        Err(e) => panic!("sender: {e}"),
    };
    // Generous rate so the test is fast; pacing correctness is proven separately
    // under a paused clock in pacer.rs.
    let mut pacer = Pacer::new(10_000_000, 64 * 1024);

    let outcome = deliver(
        &field_socket,
        &mut fec_sender,
        &mut pacer,
        &key,
        &retry(),
        || false,
    )
    .await;

    match outcome {
        Ok(Outcome::Delivered) => {}
        other => panic!("expected Delivered through 25% loss, got {other:?}"),
    }
    let decoded = match gateway.await {
        Ok(b) => b,
        Err(e) => panic!("gateway task: {e}"),
    };
    assert_eq!(
        decoded, bundle,
        "gateway must hold the byte-identical bundle"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn silent_gateway_yields_stuck_not_hang() {
    let key = Key::generate();
    let bundle = Bundle::new_vitals(vec![]);

    // A black hole: bound socket, nobody reads, nothing answers.
    let black_hole = match UdpSocket::bind("127.0.0.1:0").await {
        Ok(s) => s,
        Err(e) => panic!("bind: {e}"),
    };
    let field_socket = match UdpSocket::bind("127.0.0.1:0").await {
        Ok(s) => s,
        Err(e) => panic!("bind: {e}"),
    };
    let hole_addr = match black_hole.local_addr() {
        Ok(a) => a,
        Err(e) => panic!("addr: {e}"),
    };
    if let Err(e) = field_socket.connect(hole_addr).await {
        panic!("connect: {e}");
    }

    let mut fec_sender = match BundleSender::new(&bundle, &key, &fec()) {
        Ok(s) => s,
        Err(e) => panic!("sender: {e}"),
    };
    let mut pacer = Pacer::new(10_000_000, 64 * 1024);
    let fast_retry = RetryConfig {
        nack_timeout_ms: 50,
        retry_backoff_ms: 50,
        max_retries: 2,
        ..RetryConfig::default()
    };

    let outcome = deliver(
        &field_socket,
        &mut fec_sender,
        &mut pacer,
        &key,
        &fast_retry,
        || false,
    )
    .await;
    match outcome {
        Ok(Outcome::Stuck) => {} // flagged, kept, never silently dropped
        other => panic!("a dead link must end in Stuck, got {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn preemption_probe_pauses_image_transfer() {
    let key = Key::generate();
    let bundle = Bundle::new_image("image/jpeg".into(), vec![7; 20_000], "P-1".into());

    let black_hole = match UdpSocket::bind("127.0.0.1:0").await {
        Ok(s) => s,
        Err(e) => panic!("bind: {e}"),
    };
    let field_socket = match UdpSocket::bind("127.0.0.1:0").await {
        Ok(s) => s,
        Err(e) => panic!("bind: {e}"),
    };
    let hole_addr = match black_hole.local_addr() {
        Ok(a) => a,
        Err(e) => panic!("addr: {e}"),
    };
    if let Err(e) = field_socket.connect(hole_addr).await {
        panic!("connect: {e}");
    }

    let mut fec_sender = match BundleSender::new(&bundle, &key, &fec()) {
        Ok(s) => s,
        Err(e) => panic!("sender: {e}"),
    };
    let mut pacer = Pacer::new(10_000_000, 64 * 1024);
    let fast_retry = RetryConfig {
        nack_timeout_ms: 50,
        retry_backoff_ms: 50,
        max_retries: 8,
        ..RetryConfig::default()
    };

    // "Vitals arrived" from the first probe onward.
    let outcome = deliver(
        &field_socket,
        &mut fec_sender,
        &mut pacer,
        &key,
        &fast_retry,
        || true,
    )
    .await;
    match outcome {
        Ok(Outcome::Preempted) => {}
        other => panic!("image must step aside for vitals, got {other:?}"),
    }
}
