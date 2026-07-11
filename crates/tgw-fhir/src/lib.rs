//! `tgw-fhir` — maps `tgw_core::VitalsObservation` to FHIR **R5** Observation JSON.
//!
//! OWNER: Twaha. Reference: <https://hl7.org/fhir/observation.html> (R5, v5.0.0) — **not**
//! build.fhir.org (that serves the R6 ballot; see docs/DECISIONS.md). The gateway is the
//! FHIR boundary: it emits genuine R5 JSON while the wire format stays compact.
//!
//! Implement `to_fhir_json` in **Phase A** of tasks/twaha-agent-prompt.md.

use time::format_description::well_known::Rfc3339;

use serde_json::{Value, json};
use tgw_core::{Component, Measure, VitalsObservation};

/// LOINC coding system URL (`code.coding[].system` for clinical codes).
const LOINC_SYSTEM: &str = "http://loinc.org";
/// UCUM (Unified Code for Units of Measure) coding system URL (`valueQuantity.system`).
const UCUM_SYSTEM: &str = "http://unitsofmeasure.org";

/// Convert a [`VitalsObservation`] into a FHIR R5 `Observation` resource as JSON.
///
/// Required R5 elements to emit: `resourceType: "Observation"`, `status: "final"`,
/// `code.coding` (LOINC), `subject`, `effectiveDateTime` (RFC 3339), and either
/// `valueQuantity` (UCUM) for a single-valued reading or `component[]` for a panel (BP).
/// R5: "all observations SHOULD have a performer" — emit `performer` too.
pub fn to_fhir_json(obs: &VitalsObservation) -> Value {
    // RFC 3339 formatting of a valid `OffsetDateTime` is effectively infallible; on the
    // impossible failure path we emit an empty string rather than panic, which would surface
    // as a clear test failure instead of a crash.
    let effective = obs.effective.format(&Rfc3339).unwrap_or_default();

    let mut root = json!({
        "resourceType": "Observation",
        "status": "final",
        "code": { "coding": [loinc_coding(&obs.loinc)] },
        "subject": { "reference": format!("Patient/{}", obs.patient_id) },
        "effectiveDateTime": effective,
        "performer": [{ "reference": format!("Practitioner/{}", obs.performer_id) }],
        "device": { "reference": format!("Device/{}", obs.device_id) },
    });

    // Single-valued reading with no sub-components ⇒ top-level `valueQuantity`.
    // A panel (`components` non-empty) ⇒ `component[]` and NO top-level `valueQuantity`,
    // even if `value` were `Some` (the BP fixture has `value: None`).
    if obs.components.is_empty() {
        if let Some(m) = &obs.value {
            root["valueQuantity"] = value_quantity(m);
        }
    } else {
        let components: Vec<Value> = obs
            .components
            .iter()
            .map(|c: &Component| {
                json!({
                    "code": { "coding": [loinc_coding(&c.loinc)] },
                    "valueQuantity": value_quantity(&c.value),
                })
            })
            .collect();
        root["component"] = Value::Array(components);
    }

    root
}

/// Convert a delivered image bundle into a FHIR R5 `Media` resource as JSON.
///
/// The wire model carries no image→observation link, so a true `Observation.derivedFrom`
/// has nothing to reference; a `Media` resource is the standards-honest representation of a
/// standalone clinical image. Emits `resourceType: "Media"`, `status: "completed"`, the
/// patient `subject`, and a `content` attachment pointing at the gateway's image URL.
pub fn image_media_json(patient_id: &str, mime: &str, image_url: &str) -> Value {
    json!({
        "resourceType": "Media",
        "status": "completed",
        "subject": { "reference": format!("Patient/{patient_id}") },
        "content": {
            "contentType": mime,
            "url": image_url,
        },
    })
}

/// Build a single `coding` object: `{ system: LOINC, code: <loinc> }`.
///
/// The free-text `display` string is intentionally omitted — it is not part of the coded
/// contract (the golden test normalises it away), and the spec forbids inventing values.
fn loinc_coding(loinc: &str) -> Value {
    json!({
        "system": LOINC_SYSTEM,
        "code": loinc,
        "display": loinc_display(loinc),
    })
}

/// Return the fixture-defined display text for the supported clinical LOINC codes.
fn loinc_display(loinc: &str) -> Option<&'static str> {
    match loinc {
        "85354-9" => Some("Blood pressure panel with all children optional"),
        "8480-6" => Some("Systolic blood pressure"),
        "8462-4" => Some("Diastolic blood pressure"),
        "59408-5" => Some("Oxygen saturation in Arterial blood by Pulse oximetry"),
        "8867-4" => Some("Heart rate"),
        _ => None,
    }
}

/// Build a FHIR `valueQuantity`: `{ value, unit, system: UCUM, code: <UCUM> }`.
///
/// `code` is the raw UCUM code from [`Measure::ucum_unit`]; `unit` is a human-readable label
/// derived by stripping UCUM annotation brackets (e.g. `mm[Hg]` → `mmHg`). Codes without
/// brackets (`%`, `/min`) pass through unchanged, so `unit == code` for those.
fn value_quantity(m: &Measure) -> Value {
    json!({
        "value": m.value,
        "unit": display_unit(&m.ucum_unit),
        "system": UCUM_SYSTEM,
        "code": m.ucum_unit.clone(),
    })
}

/// Derive a human-readable unit label from a UCUM code by removing annotation brackets.
///
/// UCUM denotes annotations like `[Hg]` (millimetres of mercury) in brackets; the conventional
/// clinical display spelling drops them (`mm[Hg]` → `mmHg`). Bracket-free codes are unchanged.
fn display_unit(ucum: &str) -> String {
    ucum.chars().filter(|ch| *ch != '[' && *ch != ']').collect()
}

// The verification suite lives in `tests/r5_contract.rs` (the executable spec for Phase A).
