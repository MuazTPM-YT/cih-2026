//! Fix 2 evidence — peer-relay fallback when a device's direct radio link is degraded.
//!
//! Scenario (the mentors' "mesh fallback" concern, bounded to ONE relay hop): Device A's
//! direct path to the hospital is fully unavailable (100% simulated loss via `tgw-netsim`).
//! Device B, on the same local network, has a working direct path. A hands its still-sealed
//! bundle to B; B forwards the ciphertext to the gateway; the gateway persists it and issues an
//! AEAD receipt; B forwards that receipt back; A verifies it and clears its own queue.
//!
//! These tests also pin the security properties: the relay only ever holds ciphertext (never
//! the PSK or plaintext), and a dishonest peer forwarding a tampered or fabricated receipt is
//! rejected by `verify_receipt`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tgw_core::{
    Bundle, BundleSender, FecConfig, Key, Measure, RetryConfig, VitalsObservation, build_receipt,
    encode_bundle,
};
use tgw_field::pacer::Pacer;
use tgw_field::queue::{BundleState, Queue, QueuedBundle};
use tgw_field::relay::{RelayRequest, deliver_via_relay, encode_relay_request, run_relay_service};
use tgw_field::sender::{Outcome, deliver};
use tgw_gateway::Store;
use tgw_netsim::{NetsimConfig, run_proxy};
use time::macros::datetime;
use tokio::net::UdpSocket;
use uuid::Uuid;

/// A recognizable plaintext marker; if it ever appears in bytes the relay handles, encryption
/// has leaked.
const PLAINTEXT_MARKER: &[u8] = b"P-SECRET";

fn cfg() -> FecConfig {
    // 2x overhead so the relayed burst decodes at the gateway in one shot (no cross-relay NACK,
    // which is deliberately out of Fix 2's scope).
    FecConfig {
        symbol_size: 1100,
        overhead_factor: 2.0,
    }
}

fn vitals_bundle() -> Bundle {
    Bundle::new_vitals(vec![VitalsObservation {
        patient_id: "P-SECRET-1023".into(),
        loinc: "8867-4".into(),
        effective: datetime!(2026-07-11 14:03:22 UTC),
        value: Some(Measure {
            value: 84.0,
            ucum_unit: "/min".into(),
        }),
        components: vec![],
        device_id: "field-ecg-01".into(),
        performer_id: "fieldworker-7".into(),
    }])
}

fn fresh_store(tag: &str) -> Arc<Store> {
    let path = std::env::temp_dir().join(format!("tgw-relay-it-{}-{tag}.redb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    Arc::new(Store::open(&path).expect("open store"))
}

fn queue_at(tag: &str) -> Queue {
    let path = std::env::temp_dir().join(format!("tgw-relay-q-{}-{tag}.redb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    Queue::open(&path).expect("open queue")
}

/// Grab a free loopback UDP port (the bind is released before the caller re-claims it).
async fn free_addr() -> SocketAddr {
    let probe = UdpSocket::bind("127.0.0.1:0").await.expect("probe bind");
    let addr = probe.local_addr().expect("probe addr");
    drop(probe);
    addr
}

async fn spawn_gateway(store: Arc<Store>, key: Key) -> SocketAddr {
    let addr = free_addr().await;
    tokio::spawn(async move {
        let _ = tgw_gateway::run_udp_listener(addr, store, key).await;
    });
    tokio::time::sleep(Duration::from_millis(60)).await;
    addr
}

/// Spawn Device B's relay service. It is given ONLY the gateway address — no key — so it
/// cannot decrypt anything it forwards.
async fn spawn_relay(gateway: SocketAddr) -> SocketAddr {
    let addr = free_addr().await;
    let listen = addr.to_string();
    tokio::spawn(async move {
        let _ = run_relay_service(&listen, gateway).await;
    });
    tokio::time::sleep(Duration::from_millis(60)).await;
    addr
}

fn assert_no_plaintext(bytes: &[u8], what: &str) {
    assert!(
        !bytes
            .windows(PLAINTEXT_MARKER.len())
            .any(|w| w == PLAINTEXT_MARKER),
        "{what} exposed patient plaintext to the relay"
    );
}

#[tokio::test]
async fn bundle_delivers_through_peer_relay_and_clears_the_originators_queue() {
    let key = Key::generate();
    let store = fresh_store("happy");
    let gateway = spawn_gateway(store.clone(), key.clone()).await;
    let relay = spawn_relay(gateway).await;

    // Device A seals + frames its bundle and puts it in its own store-and-forward queue.
    let bundle = vitals_bundle();
    let queue = queue_at("happy");
    queue
        .enqueue(&QueuedBundle::from_bundle(&bundle, &key).expect("seal"))
        .expect("enqueue");
    let datagrams = encode_bundle(&bundle, &key, &cfg()).expect("encode");

    // Everything that will cross to the relay is ciphertext only.
    for dgram in &datagrams {
        assert_no_plaintext(dgram, "a sealed DATA datagram");
    }
    let relay_msg = encode_relay_request(&RelayRequest {
        bundle_id: bundle.id,
        datagrams: datagrams.clone(),
    });
    assert_no_plaintext(&relay_msg, "the relay request");

    // A's direct path failed; it relays via B and waits for a forwarded, authenticated receipt.
    let outcome = deliver_via_relay(relay, bundle.id, &datagrams, &key, Duration::from_secs(5))
        .await
        .expect("relay attempt");
    assert_eq!(
        outcome,
        Outcome::Delivered,
        "the bundle must be delivered via the peer relay"
    );

    // The gateway really persisted it (the receipt is sent only after persistence).
    assert!(
        store.is_delivered(bundle.id).expect("is_delivered"),
        "gateway must have persisted the relayed bundle"
    );

    // A clears the bundle from its own queue on the verified receipt.
    queue
        .set_state(bundle.id, BundleState::Delivered)
        .expect("clear");
    assert_eq!(
        queue.get(bundle.id).expect("get").map(|r| r.state),
        Some(BundleState::Delivered),
        "the originator clears its queue after a relayed receipt"
    );
}

#[tokio::test]
async fn direct_path_100pct_loss_via_netsim_is_stuck_then_relay_delivers() {
    let key = Key::generate();
    let store = fresh_store("failover");
    let gateway = spawn_gateway(store.clone(), key.clone()).await;
    let relay = spawn_relay(gateway).await;

    // A netsim link that drops 100% of the field→gateway direction (the degraded radio).
    let netsim_listen = free_addr().await;
    tokio::spawn(async move {
        let dead = NetsimConfig {
            loss: 1.0,
            ..NetsimConfig::default()
        };
        let _ = run_proxy(dead, netsim_listen, gateway).await;
    });
    tokio::time::sleep(Duration::from_millis(60)).await;

    let bundle = vitals_bundle();

    // Direct attempt over the dead link: no receipt ever comes back → Stuck.
    let mut sender = BundleSender::new(&bundle, &key, &cfg()).expect("sender");
    let field_sock = UdpSocket::bind("127.0.0.1:0").await.expect("field bind");
    field_sock
        .connect(netsim_listen)
        .await
        .expect("connect netsim");
    let mut pacer = Pacer::new(10_000_000, 64 * 1024);
    let fast_retry = RetryConfig {
        nack_timeout_ms: 50,
        retry_backoff_ms: 60,
        max_retries: 2,
    };
    let direct = deliver(
        &field_sock,
        &mut sender,
        &mut pacer,
        &key,
        &fast_retry,
        || false,
    )
    .await
    .expect("direct attempt");
    assert_eq!(
        direct,
        Outcome::Stuck,
        "a 100%-loss direct link must end Stuck, triggering failover"
    );

    // Failover to the peer relay delivers the same bundle.
    let datagrams = encode_bundle(&bundle, &key, &cfg()).expect("encode");
    let relayed = deliver_via_relay(relay, bundle.id, &datagrams, &key, Duration::from_secs(5))
        .await
        .expect("relay attempt");
    assert_eq!(
        relayed,
        Outcome::Delivered,
        "failover via relay must deliver"
    );
    assert!(store.is_delivered(bundle.id).expect("is_delivered"));
}

#[tokio::test]
async fn a_dishonest_peer_forwarding_a_bad_receipt_is_rejected() {
    let key = Key::generate();
    let bundle_id = Uuid::new_v4();

    // A dishonest "relay" that lacks the real PSK. It replies to a relay request with both a
    // tampered receipt and a fabricated one (signed under a key it made up). Neither may be
    // accepted — verify_receipt is the originator's only trust anchor.
    let peer_key = Key::generate(); // NOT the real PSK
    let malicious = UdpSocket::bind("127.0.0.1:0").await.expect("mal bind");
    let malicious_addr = malicious.local_addr().expect("mal addr");
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65_535];
        if let Ok((_n, from)) = malicious.recv_from(&mut buf).await {
            let mut tampered = build_receipt(bundle_id, &peer_key);
            let last = tampered.len() - 1;
            tampered[last] ^= 0x01;
            let _ = malicious.send_to(&tampered, from).await;

            let fabricated = build_receipt(bundle_id, &peer_key);
            let _ = malicious.send_to(&fabricated, from).await;
        }
    });

    let datagrams = vec![vec![0xEFu8; 32]]; // opaque; content irrelevant to this test
    let outcome = deliver_via_relay(
        malicious_addr,
        bundle_id,
        &datagrams,
        &key,
        Duration::from_millis(700),
    )
    .await
    .expect("relay attempt");
    assert_eq!(
        outcome,
        Outcome::Stuck,
        "a tampered or fabricated relayed receipt must be rejected, never accepted as delivery"
    );
}
