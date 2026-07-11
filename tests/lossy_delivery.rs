//! THE resilience evidence test — brief §8 ("evidence, not claims"). OWNER: Twaha.
//!
//! Two levels, both run by default (no `#[ignore]`):
//!
//! 1. `vitals_and_image_survive_25pct_loss` — a library-level proof: a clinical bundle,
//!    fountain-coded with repair overhead, reconstructs byte-for-byte after 25% of its
//!    datagrams are deterministically dropped (real `tgw-core` FEC + netsim `LossModel`).
//! 2. `full_lossy_delivery_through_proxy_and_gateway` — the full socket end-to-end: field
//!    `deliver` → `tgw-netsim` proxy (25% loss + burst + 64 kbps + jitter) → real gateway
//!    `run_udp_listener` → AEAD `DELIVERED` receipts, asserting every bundle lands in the
//!    gateway's redb store within a bounded timeout, at the default 1.4× overhead.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tgw_core::{
    Absorb, Bundle, BundlePayload, BundleReceiver, BundleSender, FecConfig, Key, Measure, Priority,
    RetryConfig, VitalsObservation, encode_bundle,
};
use tgw_field::pacer::Pacer;
use tgw_field::sender::{Outcome, deliver};
use tgw_gateway::{Store, run_udp_listener};
use tgw_netsim::{LossModel, NetsimConfig, run_proxy};
use time::macros::datetime;
use tokio::net::UdpSocket;
use uuid::Uuid;

/// Stable digest of a payload's canonical serialization, for intact-delivery checks.
fn payload_digest(p: &BundlePayload) -> [u8; 32] {
    let bytes = serde_json::to_vec(p).expect("payload serializes");
    Sha256::digest(&bytes).into()
}

fn vitals_bundle(n: usize) -> Bundle {
    let obs = VitalsObservation {
        patient_id: format!("P-{n}"),
        loinc: "8867-4".into(),
        effective: datetime!(2026-07-11 14:03:22 UTC),
        value: Some(Measure {
            value: 100.0 + n as f64,
            ucum_unit: "/min".into(),
        }),
        components: vec![],
        device_id: "field-ecg-01".into(),
        performer_id: "fieldworker-7".into(),
    };
    Bundle {
        id: Uuid::new_v4(),
        priority: Priority::Vitals,
        payload: BundlePayload::Vitals(vec![obs]),
    }
}

fn image_bundle() -> Bundle {
    Bundle {
        id: Uuid::new_v4(),
        priority: Priority::Image,
        payload: BundlePayload::Image {
            mime: "image/jpeg".into(),
            data: vec![0xAB; 25_000],
            patient_id: "P-1023".into(),
        },
    }
}

/// Encode a bundle, drop `loss` of its datagrams deterministically, and require that it still
/// reconstructs intact.
fn assert_survives_loss(bundle: &Bundle, key: &Key, loss: f64) {
    // Repair overhead well above the loss rate so a single burst decodes without a NACK loop
    // (the NACK/repair path is exercised by the full gateway e2e, not this library-level test).
    let cfg = FecConfig {
        symbol_size: 1100,
        overhead_factor: 2.0,
    };
    let datagrams = encode_bundle(bundle, key, &cfg).expect("encode_bundle");

    let mut model = LossModel::new(&NetsimConfig {
        loss,
        ..NetsimConfig::default()
    });
    let mut rx = BundleReceiver::new(key.clone());
    let mut decoded = None;
    for (i, dg) in datagrams.iter().enumerate() {
        if model.decide(Duration::from_millis(i as u64)) {
            continue; // dropped by the lossy link
        }
        if let Absorb::Complete(b) = rx.absorb(dg).expect("absorb") {
            decoded = Some(b);
            break;
        }
    }

    let got = decoded.expect("bundle must reconstruct despite 25% loss");
    assert_eq!(got.id, bundle.id, "decoded bundle id must match");
    assert_eq!(
        payload_digest(&got.payload),
        payload_digest(&bundle.payload),
        "decoded payload must be byte-identical to the original"
    );
}

#[test]
fn vitals_and_image_survive_25pct_loss() {
    // A 32-byte PSK on disk (never committed). `Key::from_file` reads it once Muaz implements it.
    let key_path = std::env::temp_dir().join("tgw-integration-test.key");
    std::fs::write(&key_path, Key::from_bytes([7u8; 32]).to_hex()).expect("write test key");
    let key = Key::from_file(Path::new(&key_path)).expect("load test key");

    for n in 0..5 {
        assert_survives_loss(&vitals_bundle(n), &key, 0.25);
    }
    assert_survives_loss(&image_bundle(), &key, 0.25);
}

/// Grab an ephemeral loopback UDP port, then release it so a server can rebind it.
/// (Standard test trick: the race window on loopback is negligible.)
async fn free_addr() -> SocketAddr {
    let sock = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral");
    sock.local_addr().expect("local addr")
    // `sock` drops here, freeing the port for the real server to bind.
}

/// A ~10 KB image — big enough for multi-symbol FEC through the real proxy, small enough
/// that a 64 kbps paced burst stays inside the OS socket buffer and the test stays fast.
fn small_image_bundle() -> Bundle {
    Bundle {
        id: Uuid::new_v4(),
        priority: Priority::Image,
        payload: BundlePayload::Image {
            mime: "image/jpeg".into(),
            data: vec![0xAB; 10_000],
            patient_id: "P-1023".into(),
        },
    }
}

/// THE full end-to-end resilience evidence (docs/ARCHITECTURE.md §8): field client →
/// `tgw-netsim` lossy proxy (25% loss + burst + 64 kbps + jitter, seeded) → real gateway
/// (`run_udp_listener` with decode-stall NACKs) → AEAD `DELIVERED` receipts back through the
/// proxy's reverse path. Uses the **default `overhead_factor` 1.4** so the NACK/repair loop is
/// genuinely exercised (unlike the library-level test's 2.0). Asserts every bundle is
/// `Delivered` and lands in the gateway's redb store, all inside a bounded timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_lossy_delivery_through_proxy_and_gateway() {
    let key = Key::from_bytes([9u8; 32]);
    let fec = FecConfig {
        symbol_size: 1100,
        overhead_factor: 1.4,
    };
    // Short timers keep the test brisk; ratios mirror the real config.
    let retry = RetryConfig {
        nack_timeout_ms: 200,
        retry_backoff_ms: 300,
        max_retries: 40,
    };

    let gateway_addr = free_addr().await;
    let proxy_listen = free_addr().await;

    // Real gateway with a temp redb store.
    let store_path = std::env::temp_dir().join(format!("tgw-e2e-{}.redb", std::process::id()));
    let _ = std::fs::remove_file(&store_path);
    let store = Arc::new(Store::open(&store_path).expect("open store"));
    let gw_key = key.clone();
    let gw_store = Arc::clone(&store);
    tokio::spawn(async move {
        let _ = run_udp_listener(
            gateway_addr,
            gw_store,
            gw_key,
            Duration::from_millis(retry.nack_timeout_ms),
        )
        .await;
    });

    // Lossy link: 25% loss + burst + 64 kbps + jitter, forwarding to the gateway.
    let netsim = NetsimConfig {
        loss: 0.25,
        rate_bps: 64_000,
        seed: 0x2026_0711,
        ..NetsimConfig::default()
    };
    tokio::spawn(async move {
        let _ = run_proxy(netsim, proxy_listen, gateway_addr).await;
    });

    // Give the servers a moment to bind.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Field side: one connected socket, deliver 5 vitals + one image through the proxy.
    let field_sock = UdpSocket::bind("127.0.0.1:0").await.expect("bind field");
    field_sock
        .connect(proxy_listen)
        .await
        .expect("connect proxy");
    let mut pacer = Pacer::new(10_000_000, 64 * 1024); // netsim enforces the 64 kbps link

    let mut bundles: Vec<Bundle> = (0..5).map(vitals_bundle).collect();
    bundles.push(small_image_bundle());

    let run = async {
        for bundle in &bundles {
            let mut sender = BundleSender::new(bundle, &key, &fec).expect("sender");
            let outcome = deliver(&field_sock, &mut sender, &mut pacer, &key, &retry, || false)
                .await
                .expect("deliver");
            assert_eq!(
                outcome,
                Outcome::Delivered,
                "bundle {} must be delivered through the lossy link",
                bundle.id
            );
        }
    };
    tokio::time::timeout(Duration::from_secs(90), run)
        .await
        .expect("delivery must complete within the time bound");

    // Every bundle must be durably in the gateway's store (receipts were authentic).
    for bundle in &bundles {
        assert!(
            store.is_delivered(bundle.id).expect("is_delivered"),
            "bundle {} must be persisted at the gateway",
            bundle.id
        );
    }
}
