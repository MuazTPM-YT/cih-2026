//! THE resilience evidence test — brief §7 ("evidence, not claims"). OWNER: Twaha.
//!
//! This is the one suite that CANNOT go green on Twaha's branch alone: it exercises Muaz's
//! real `tgw-core` FEC (encode/decode) and the netsim `LossModel`, all currently `todo!()`.
//! It compiles today (so `cargo test --no-run` is clean) and is `#[ignore]`d; run it jointly
//! with Muaz at the H10 checkpoint:
//!
//!   cargo test --test lossy_delivery -- --ignored
//!
//! What it proves: a clinical bundle, fountain-coded and sent with repair overhead, is
//! reconstructed byte-for-byte after 25% of its datagrams are deterministically dropped.
//! The fuller gateway+receipt end-to-end (store, DELIVERED receipts, priority) is layered on
//! in Phase D/E once those gateway APIs exist — see tasks/twaha-agent-prompt.md.

use std::path::Path;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tgw_core::{
    Absorb, Bundle, BundlePayload, BundleReceiver, FecConfig, Key, Measure, Priority,
    VitalsObservation, encode_bundle,
};
use tgw_netsim::{LossModel, NetsimConfig};
use time::macros::datetime;
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
