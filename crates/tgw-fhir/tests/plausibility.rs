//! Executable spec for `tgw_fhir::plausibility_flags` (Fix 1c).
//!
//! Flags are ADDITIVE metadata: an out-of-range or inconsistent reading is never rejected or
//! hidden — it is flagged so the dashboard can distinguish "clean" from "verify" while still
//! showing a possibly-real emergency value. Every numeric threshold these tests pin is a
//! placeholder pending clinician sign-off; see the constants in `src/lib.rs`.

use tgw_core::{Component, Measure, VitalsObservation};
use tgw_fhir::plausibility_flags;
use time::macros::datetime;

fn base() -> VitalsObservation {
    VitalsObservation {
        patient_id: "P-1023".into(),
        loinc: "8867-4".into(),
        effective: datetime!(2026-07-11 14:03:22 UTC),
        value: None,
        components: vec![],
        device_id: "field-device".into(),
        performer_id: "field-worker".into(),
    }
}

fn single(loinc: &str, value: f64, unit: &str) -> VitalsObservation {
    VitalsObservation {
        loinc: loinc.into(),
        value: Some(Measure {
            value,
            ucum_unit: unit.into(),
        }),
        ..base()
    }
}

fn bp(systolic: f64, diastolic: f64) -> VitalsObservation {
    VitalsObservation {
        loinc: "85354-9".into(),
        value: None,
        components: vec![
            Component {
                loinc: "8480-6".into(),
                value: Measure {
                    value: systolic,
                    ucum_unit: "mm[Hg]".into(),
                },
            },
            Component {
                loinc: "8462-4".into(),
                value: Measure {
                    value: diastolic,
                    ucum_unit: "mm[Hg]".into(),
                },
            },
        ],
        ..base()
    }
}

#[test]
fn in_range_single_values_produce_no_flags() {
    assert!(plausibility_flags(&single("8867-4", 78.0, "/min")).is_empty());
    assert!(plausibility_flags(&single("59408-5", 97.0, "%")).is_empty());
}

#[test]
fn in_range_consistent_bp_produces_no_flags() {
    assert!(
        plausibility_flags(&bp(142.0, 95.0)).is_empty(),
        "a normal, systolic>diastolic BP panel is clean"
    );
}

#[test]
fn out_of_range_heart_rate_is_flagged() {
    let flags = plausibility_flags(&single("8867-4", 400.0, "/min"));
    assert!(
        flags.iter().any(|f| f == "heart-rate-out-of-range"),
        "an impossible heart rate must be flagged, got {flags:?}"
    );
}

#[test]
fn out_of_range_spo2_is_flagged() {
    let flags = plausibility_flags(&single("59408-5", 105.0, "%"));
    assert!(
        flags.iter().any(|f| f == "spo2-out-of-range"),
        "SpO2 over 100% is physically impossible and must be flagged, got {flags:?}"
    );
}

#[test]
fn inconsistent_bp_systolic_not_greater_than_diastolic_is_flagged() {
    let flags = plausibility_flags(&bp(90.0, 120.0));
    assert!(
        flags
            .iter()
            .any(|f| f == "systolic-not-greater-than-diastolic"),
        "systolic <= diastolic is internally inconsistent and must be flagged, got {flags:?}"
    );
}

#[test]
fn out_of_range_bp_component_is_flagged_but_not_rejected() {
    let flags = plausibility_flags(&bp(400.0, 95.0));
    assert!(
        flags.iter().any(|f| f == "systolic-out-of-range"),
        "an impossible systolic component must be flagged, got {flags:?}"
    );
}

#[test]
fn unsupported_loinc_gets_no_plausibility_opinion() {
    // A code with no defined range must never be flagged (and never rejected).
    assert!(plausibility_flags(&single("00000-0", 9_999.0, "x")).is_empty());
}
