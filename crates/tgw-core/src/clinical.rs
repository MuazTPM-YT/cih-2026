//! Single source of truth for the numeric clinical bounds used across the system (Fix F5).
//!
//! Two deliberately-distinct tiers, kept in one place so they cannot silently diverge:
//!
//! * [`INPUT_`](self)`*` — **wide hard-reject** bounds applied at capture on the field
//!   client (`tgw-field`). Their job is to catch typos and garbage at the keyboard
//!   ("900/20", a fat-fingered SpO₂ of 150%), not to make a clinical judgement. A value
//!   inside these bounds is *accepted onto the wire*; a value outside is refused before it
//!   is ever sealed.
//! * [`PLAUSIBLE_`](self)`*` — **tighter advisory** bounds applied at the gateway
//!   (`tgw-fhir::plausibility_flags`) after decrypt + FHIR mapping. A value outside these is
//!   never dropped or hidden — it is *flagged* so the dashboard can mark it "verify" while
//!   still surfacing a possibly-real emergency reading.
//!
//! # Invariant: `INPUT ⊇ PLAUSIBLE`
//! Every input band must be at least as wide as the matching plausible band, so the two
//! tiers compose as defense-in-depth rather than fighting each other: anything the gateway
//! would flag as implausible is still *accepted* at capture (never silently rejected at the
//! field), and anything the field rejects outright is so extreme it is implausible too. The
//! `input_superset_of_plausible` test in this module pins that relationship.
//!
//! # F5 note — the BP consistency flag is a hostile-sender-only safeguard
//! The field rejects `diastolic >= systolic` at capture ([`crate::clinical`] is not involved
//! in that ordering check — it lives in `tgw-field`), so a normal CLI capture can never
//! reach the gateway with an inconsistent BP. The gateway's
//! `systolic-not-greater-than-diastolic` plausibility flag is therefore only reachable from a
//! sender that bypasses the field client — it is a safeguard against a hostile or buggy
//! non-CLI source, not something the demo path exercises.
//!
//! !!! EVERY numeric bound below is a **PLACEHOLDER pending clinician sign-off**. These are
//! engineering guesses at "physically possible at all," not validated clinical thresholds,
//! and MUST be reviewed by a clinician before use on a real patient. Sign-off is a one-file
//! edit here.

// ---------------------------------------------------------------------------------------
// INPUT_* — wide hard-reject bounds (field capture guard). PLACEHOLDER, needs clinician review.
// ---------------------------------------------------------------------------------------

/// Systolic hard-reject bounds in mmHg (LOINC 8480-6). NEEDS CLINICIAN REVIEW.
pub const INPUT_SYSTOLIC_MIN: f64 = 20.0;
/// NEEDS CLINICIAN REVIEW.
pub const INPUT_SYSTOLIC_MAX: f64 = 350.0;

/// Diastolic hard-reject bounds in mmHg (LOINC 8462-4). NEEDS CLINICIAN REVIEW.
pub const INPUT_DIASTOLIC_MIN: f64 = 10.0;
/// NEEDS CLINICIAN REVIEW.
pub const INPUT_DIASTOLIC_MAX: f64 = 250.0;

/// Oxygen-saturation hard-reject bounds in percent (LOINC 59408-5). NEEDS CLINICIAN REVIEW.
pub const INPUT_SPO2_MIN: f64 = 0.0;
/// Above 100% is physically impossible. NEEDS CLINICIAN REVIEW.
pub const INPUT_SPO2_MAX: f64 = 100.0;

/// Pulse/heart-rate hard-reject bounds in beats/min (LOINC 8867-4). NEEDS CLINICIAN REVIEW.
pub const INPUT_PULSE_MIN: f64 = 0.0;
/// NEEDS CLINICIAN REVIEW.
pub const INPUT_PULSE_MAX: f64 = 400.0;

// ---------------------------------------------------------------------------------------
// PLAUSIBLE_* — tighter advisory bounds (gateway flagging). PLACEHOLDER, needs clinician review.
// ---------------------------------------------------------------------------------------

/// Heart-rate plausible bounds in beats/min (LOINC 8867-4). NEEDS CLINICIAN REVIEW: placeholder
/// physiological bounds, not a clinical alarm range.
pub const PLAUSIBLE_HEART_RATE_MIN: f64 = 20.0;
/// NEEDS CLINICIAN REVIEW.
pub const PLAUSIBLE_HEART_RATE_MAX: f64 = 300.0;

/// Oxygen-saturation plausible bounds in percent (LOINC 59408-5). NEEDS CLINICIAN REVIEW:
/// below ~50% is rarely measurable/survivable; above 100% is impossible.
pub const PLAUSIBLE_SPO2_MIN: f64 = 50.0;
/// NEEDS CLINICIAN REVIEW.
pub const PLAUSIBLE_SPO2_MAX: f64 = 100.0;

/// Systolic plausible bounds in mmHg (LOINC 8480-6). NEEDS CLINICIAN REVIEW.
pub const PLAUSIBLE_SYSTOLIC_MIN: f64 = 40.0;
/// NEEDS CLINICIAN REVIEW.
pub const PLAUSIBLE_SYSTOLIC_MAX: f64 = 300.0;

/// Diastolic plausible bounds in mmHg (LOINC 8462-4). NEEDS CLINICIAN REVIEW.
pub const PLAUSIBLE_DIASTOLIC_MIN: f64 = 20.0;
/// NEEDS CLINICIAN REVIEW.
pub const PLAUSIBLE_DIASTOLIC_MAX: f64 = 200.0;

#[cfg(test)]
mod tests {
    use super::*;

    /// The core defense-in-depth invariant: every hard-reject (input) band must fully contain
    /// the matching advisory (plausible) band, so the two tiers can never silently diverge into
    /// a state where the gateway would flag a value the field already refused (or vice versa).
    #[test]
    fn input_superset_of_plausible() {
        // (input_min, input_max, plausible_min, plausible_max, label)
        let pairs = [
            (
                INPUT_SYSTOLIC_MIN,
                INPUT_SYSTOLIC_MAX,
                PLAUSIBLE_SYSTOLIC_MIN,
                PLAUSIBLE_SYSTOLIC_MAX,
                "systolic",
            ),
            (
                INPUT_DIASTOLIC_MIN,
                INPUT_DIASTOLIC_MAX,
                PLAUSIBLE_DIASTOLIC_MIN,
                PLAUSIBLE_DIASTOLIC_MAX,
                "diastolic",
            ),
            (
                INPUT_SPO2_MIN,
                INPUT_SPO2_MAX,
                PLAUSIBLE_SPO2_MIN,
                PLAUSIBLE_SPO2_MAX,
                "spo2",
            ),
            (
                INPUT_PULSE_MIN,
                INPUT_PULSE_MAX,
                PLAUSIBLE_HEART_RATE_MIN,
                PLAUSIBLE_HEART_RATE_MAX,
                "pulse/heart-rate",
            ),
        ];
        for (in_min, in_max, pl_min, pl_max, label) in pairs {
            assert!(
                in_min <= pl_min,
                "{label}: input min {in_min} must be ≤ plausible min {pl_min} (input ⊇ plausible)"
            );
            assert!(
                in_max >= pl_max,
                "{label}: input max {in_max} must be ≥ plausible max {pl_max} (input ⊇ plausible)"
            );
            assert!(in_min < in_max, "{label}: input band must be non-empty");
            assert!(pl_min < pl_max, "{label}: plausible band must be non-empty");
        }
    }
}
