//! `tgw-fhir` — maps `tgw_core::VitalsObservation` to FHIR **R5** Observation JSON.
//!
//! OWNER: Twaha. Reference: <https://hl7.org/fhir/observation.html> (R5, v5.0.0) — **not**
//! build.fhir.org (that serves the R6 ballot; see docs/DECISIONS.md). The gateway is the
//! FHIR boundary: it emits genuine R5 JSON while the wire format stays compact.
//!
//! Implement `to_fhir_json` in **Phase A** of tasks/twaha-agent-prompt.md.

use serde_json::Value;
use tgw_core::VitalsObservation;

/// Convert a [`VitalsObservation`] into a FHIR R5 `Observation` resource as JSON.
///
/// Required R5 elements to emit: `resourceType: "Observation"`, `status: "final"`,
/// `code.coding` (LOINC), `subject`, `effectiveDateTime` (RFC 3339), and either
/// `valueQuantity` (UCUM) for a single-valued reading or `component[]` for a panel (BP).
/// R5: "all observations SHOULD have a performer" — emit `performer` too.
pub fn to_fhir_json(obs: &VitalsObservation) -> Value {
    let _ = obs;
    todo!("PHASE-A: build FHIR R5 Observation — see tasks/twaha-agent-prompt.md")
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "PHASE-A: not yet implemented"]
    fn required_elements_present() {
        todo!("PHASE-A: assert resourceType/status/code.coding/subject/effectiveDateTime present")
    }

    #[test]
    #[ignore = "PHASE-A: not yet implemented"]
    fn round_trip_stable() {
        todo!("PHASE-A: struct -> FHIR JSON -> struct equals the original")
    }
}
