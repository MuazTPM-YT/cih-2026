//! Cross-LAN pairing + delivery: the field and gateway agree a key via SPAKE2 with NO shared
//! key file, then a real bundle delivers end-to-end under the derived key. Proves the keyless
//! pairing path carries genuine PHI intact, including through a 25%-loss link.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tgw_core::{
    Bundle, BundlePayload, BundleSender, FecConfig, Priority, RetryConfig, VitalsObservation,
};
use tgw_field::pacer::Pacer;
use tgw_field::sender::{Outcome, deliver};
use tgw_gateway::pairing::run_pair_responder;
use tgw_gateway::{Store, run_udp_listener};
use tgw_netsim::{NetsimConfig, run_proxy};
use time::macros::datetime;
use tokio::net::UdpSocket;
use uuid::Uuid;

/// Grab an ephemeral loopback UDP port, then release it so a server can rebind it.
async fn free_addr() -> SocketAddr {
    let sock = UdpSocket::bind("127.0.0.1:0").await.expect("bind ephemeral");
    sock.local_addr().expect("local addr")
}

fn vitals_bundle(n: usize) -> Bundle {
    Bundle {
        id: Uuid::new_v4(),
        priority: Priority::Vitals,
        payload: BundlePayload::Vitals(vec![VitalsObservation {
            patient_id: format!("P-{n}"),
            loinc: "8867-4".into(),
            effective: datetime!(2026-07-11 14:03:22 UTC),
            value: Some(tgw_core::Measure {
                value: 100.0 + n as f64,
                ucum_unit: "/min".into(),
            }),
            components: vec![],
            device_id: "field-ecg-01".into(),
            performer_id: "fieldworker-7".into(),
        }]),
    }
}

/// Run the pairing handshake (field + hospital) and assert both derive the same session key.
/// Returns (gateway UDP addr, field key, gateway key).
async fn pair_at(addr: SocketAddr) -> (tgw_core::Key, tgw_core::Key) {
    let code = "4-otter-cobalt";
    let responder =
        tokio::spawn(async move { run_pair_responder(addr, code, Default::default()).await });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let field_key = tgw_field::pairing::pair_with_hospital(
        &addr.to_string(),
        code,
        Duration::from_secs(10),
    )
    .await
    .expect("field pairs");
    let gw_key = responder.await.expect("join").expect("gateway pairs");
    assert_eq!(
        field_key.to_hex(),
        gw_key.to_hex(),
        "both sides must derive the same session key from the code"
    );
    (field_key, gw_key)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pair_then_deliver_end_to_end() {
    let gw_addr = free_addr().await;
    let (field_key, gw_key) = pair_at(gw_addr).await;

    // Start the real gateway listener on the SAME addr under the derived key.
    let store_path =
        std::env::temp_dir().join(format!("tgw-paired-{}.redb", std::process::id()));
    let _ = std::fs::remove_file(&store_path);
    let store = Arc::new(Store::open(&store_path).expect("open store"));
    let gw_store = Arc::clone(&store);
    tokio::spawn(async move {
        let _ = run_udp_listener(gw_addr, gw_store, gw_key, Duration::from_millis(200)).await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Field delivers a bundle under the derived key — no key file anywhere.
    let field_sock = UdpSocket::bind("127.0.0.1:0").await.expect("bind field");
    field_sock.connect(gw_addr).await.expect("connect");
    let fec = FecConfig {
        symbol_size: 1100,
        overhead_factor: 1.4,
    };
    let retry = RetryConfig {
        nack_timeout_ms: 200,
        retry_backoff_ms: 300,
        max_retries: 40,
        ..RetryConfig::default()
    };
    let mut pacer = Pacer::new(10_000_000, 64 * 1024);
    let bundle = vitals_bundle(1);
    let mut sender = BundleSender::new(&bundle, &field_key, &fec).expect("sender");
    let outcome = deliver(&field_sock, &mut sender, &mut pacer, &field_key, &retry, || false)
        .await
        .expect("deliver");
    assert_eq!(outcome, Outcome::Delivered, "bundle delivers under the derived key");
    assert!(
        store.is_delivered(bundle.id).expect("is_delivered"),
        "the bundle must be persisted at the gateway"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pair_then_deliver_through_25pct_loss() {
    let gw_addr = free_addr().await;
    let (field_key, gw_key) = pair_at(gw_addr).await;

    let store_path =
        std::env::temp_dir().join(format!("tgw-paired-lossy-{}.redb", std::process::id()));
    let _ = std::fs::remove_file(&store_path);
    let store = Arc::new(Store::open(&store_path).expect("open store"));
    let gw_store = Arc::clone(&store);
    tokio::spawn(async move {
        let _ = run_udp_listener(gw_addr, gw_store, gw_key, Duration::from_millis(200)).await;
    });

    // 25% loss + 64 kbps proxy in front of the gateway.
    let proxy_listen = free_addr().await;
    let netsim = NetsimConfig {
        loss: 0.25,
        rate_bps: 64_000,
        seed: 0x2026_0712,
        ..NetsimConfig::default()
    };
    tokio::spawn(async move {
        let _ = run_proxy(netsim, proxy_listen, gw_addr).await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let field_sock = UdpSocket::bind("127.0.0.1:0").await.expect("bind field");
    field_sock.connect(proxy_listen).await.expect("connect proxy");
    let fec = FecConfig {
        symbol_size: 1100,
        overhead_factor: 1.4,
    };
    let retry = RetryConfig {
        nack_timeout_ms: 200,
        retry_backoff_ms: 300,
        max_retries: 60,
        ..RetryConfig::default()
    };
    let mut pacer = Pacer::new(10_000_000, 64 * 1024);
    let bundle = vitals_bundle(2);
    let mut sender = BundleSender::new(&bundle, &field_key, &fec).expect("sender");

    let run = deliver(&field_sock, &mut sender, &mut pacer, &field_key, &retry, || false);
    let outcome = tokio::time::timeout(Duration::from_secs(90), run)
        .await
        .expect("delivery within time bound")
        .expect("deliver");
    assert_eq!(
        outcome,
        Outcome::Delivered,
        "paired delivery must survive 25% loss under the derived key"
    );
    assert!(
        store.is_delivered(bundle.id).expect("is_delivered"),
        "the bundle must be persisted at the gateway"
    );
}
