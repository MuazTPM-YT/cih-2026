//! End-to-end receiving-end validation over the real `run_udp_listener` loop (Fix 1a/1b).
//!
//! These drive the actual gateway UDP path with real FEC frames across a loopback socket and
//! assert the receiving-end guarantees the mentors flagged:
//!   * corrupt datagrams never persist and never earn a receipt (nothing is accepted on faith);
//!   * a single corrupt datagram does not discard an in-progress bundle — the clean symbols
//!     still reconstruct and deliver (the latent "one bad packet nukes the receiver" bug is
//!     gone), which is also the "subsequent clean retransmission succeeds" property of Fix 1b.

use std::sync::Arc;
use std::time::Duration;

use tgw_core::{Bundle, FecConfig, Key, encode_bundle, verify_receipt};
use tgw_gateway::Store;
use tokio::net::UdpSocket;
use tokio::time::timeout;

/// 2x FEC overhead so the clean survivors of a single burst decode without a NACK round
/// (the gateway does not run a stall timer; this isolates the receiving-end behavior).
fn fec() -> FecConfig {
    FecConfig {
        symbol_size: 1100,
        overhead_factor: 2.0,
    }
}

/// Flip a byte solidly inside the symbol region, breaking the integrity tag the way radio
/// interference (or a forged packet) would.
fn corrupt(dgram: &[u8]) -> Vec<u8> {
    let mut c = dgram.to_vec();
    let idx = 2 + 16 + 12 + 6; // header + uuid + oti + into the PayloadId/symbol
    c[idx] ^= 0x01;
    c
}

/// Open a fresh temp-file store unique to this test process + tag.
fn fresh_store(tag: &str) -> Arc<Store> {
    let path = std::env::temp_dir().join(format!(
        "tgw-udp-listener-{}-{tag}.redb",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    Arc::new(Store::open(&path).expect("open store"))
}

/// Bind an ephemeral loopback port for the gateway, then spawn the listener on it.
async fn spawn_gateway(store: Arc<Store>, key: Key) -> std::net::SocketAddr {
    let probe = UdpSocket::bind("127.0.0.1:0").await.expect("probe bind");
    let addr = probe.local_addr().expect("probe addr");
    drop(probe); // release so the listener can claim the same port
    tokio::spawn(async move {
        // Runs until the test process exits; a bind error here fails the assertions below.
        let _ = tgw_gateway::run_udp_listener(addr, store, key, Duration::from_secs(5)).await;
    });
    // Give the listener a moment to bind before we send.
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

#[tokio::test(flavor = "current_thread")]
async fn corrupt_datagrams_never_persist_and_get_no_receipt() {
    let key = Key::generate();
    let store = fresh_store("corrupt-only");
    let gw_addr = spawn_gateway(store.clone(), key.clone()).await;

    let bundle = Bundle::new_image("image/jpeg".into(), vec![0x5Au8; 8_000], "P-1".into());
    let datagrams = encode_bundle(&bundle, &key, &fec()).expect("encode");

    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client bind");
    client.connect(gw_addr).await.expect("connect");

    // Every datagram corrupted — the gateway must accept none of them.
    for dgram in &datagrams {
        client.send(&corrupt(dgram)).await.expect("send");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    // No receipt may come back for a bundle that never authenticated.
    let mut buf = vec![0u8; 2048];
    let got = timeout(Duration::from_millis(400), client.recv(&mut buf)).await;
    assert!(
        got.is_err(),
        "a bundle built only from corrupt datagrams must never earn a receipt"
    );

    assert!(
        !store.is_delivered(bundle.id).expect("is_delivered"),
        "corrupt-only input must never be persisted as delivered"
    );
    assert!(
        store.list_delivered().expect("list").is_empty(),
        "no delivered rows may exist"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn partial_corruption_still_delivers_with_a_receipt() {
    let key = Key::generate();
    let store = fresh_store("partial");
    let gw_addr = spawn_gateway(store.clone(), key.clone()).await;

    let bundle = Bundle::new_image("image/jpeg".into(), vec![0xA5u8; 8_000], "P-2".into());
    let datagrams = encode_bundle(&bundle, &key, &fec()).expect("encode");

    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client bind");
    client.connect(gw_addr).await.expect("connect");

    // Corrupt every 4th datagram. A single corrupt packet must not discard the receiver's
    // accumulated clean symbols; the clean majority still reconstructs the bundle.
    for (i, dgram) in datagrams.iter().enumerate() {
        let to_send = if i % 4 == 1 {
            corrupt(dgram)
        } else {
            dgram.clone()
        };
        client.send(&to_send).await.expect("send");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    // A genuine, authenticated receipt for exactly this bundle must arrive.
    let mut buf = vec![0u8; 2048];
    let received = timeout(Duration::from_secs(2), client.recv(&mut buf))
        .await
        .expect("a receipt must arrive despite partial corruption")
        .expect("recv ok");
    let verified = verify_receipt(&buf[..received], &key).expect("receipt must authenticate");
    assert_eq!(verified, bundle.id, "receipt must acknowledge this bundle");

    assert!(
        store.is_delivered(bundle.id).expect("is_delivered"),
        "the bundle must be persisted as delivered"
    );
}
