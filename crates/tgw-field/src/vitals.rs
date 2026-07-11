//! CLI vitals capture → wire model (muaz.md H2–6).
//!
//! LOINC codes per docs/ARCHITECTURE.md §5: blood pressure panel `85354-9` with
//! systolic `8480-6` / diastolic `8462-4` components, SpO₂ `59408-5`, pulse `8867-4`.
//! UCUM units throughout so the gateway's FHIR R5 mapping is lossless.

use anyhow::{Context, Result, anyhow, bail};
use tgw_core::{Component, Measure, VitalsObservation};
use time::OffsetDateTime;

/// LOINC: blood pressure panel with all children optional.
pub const LOINC_BP_PANEL: &str = "85354-9";
/// LOINC: systolic blood pressure.
pub const LOINC_BP_SYSTOLIC: &str = "8480-6";
/// LOINC: diastolic blood pressure.
pub const LOINC_BP_DIASTOLIC: &str = "8462-4";
/// LOINC: oxygen saturation (pulse oximetry).
pub const LOINC_SPO2: &str = "59408-5";
/// LOINC: heart rate.
pub const LOINC_PULSE: &str = "8867-4";

/// Raw CLI capture values (already parsed by clap).
#[derive(Debug, Clone)]
pub struct VitalsInput {
    /// `"142/95"` systolic/diastolic in mmHg.
    pub bp: Option<String>,
    /// Oxygen saturation, percent.
    pub spo2: Option<f64>,
    /// Heart rate, beats per minute.
    pub pulse: Option<f64>,
    /// Patient identifier.
    pub patient: String,
    /// Capturing device id.
    pub device: String,
    /// Field worker id.
    pub performer: String,
}

/// Build the wire observations for one capture. At least one measurement is required.
pub fn build_observations(input: &VitalsInput) -> Result<Vec<VitalsObservation>> {
    let now = OffsetDateTime::now_utc();
    let base = |loinc: &str| VitalsObservation {
        patient_id: input.patient.clone(),
        loinc: loinc.to_string(),
        effective: now,
        value: None,
        components: Vec::new(),
        device_id: input.device.clone(),
        performer_id: input.performer.clone(),
    };

    let mut observations = Vec::new();

    if let Some(bp) = &input.bp {
        let (systolic, diastolic) = parse_bp(bp)?;
        let mut obs = base(LOINC_BP_PANEL);
        obs.components = vec![
            Component {
                loinc: LOINC_BP_SYSTOLIC.into(),
                value: Measure {
                    value: systolic,
                    ucum_unit: "mm[Hg]".into(),
                },
            },
            Component {
                loinc: LOINC_BP_DIASTOLIC.into(),
                value: Measure {
                    value: diastolic,
                    ucum_unit: "mm[Hg]".into(),
                },
            },
        ];
        observations.push(obs);
    }
    if let Some(spo2) = input.spo2 {
        if !(0.0..=100.0).contains(&spo2) {
            bail!("--spo2 must be a percentage in 0–100, got {spo2}");
        }
        let mut obs = base(LOINC_SPO2);
        obs.value = Some(Measure {
            value: spo2,
            ucum_unit: "%".into(),
        });
        observations.push(obs);
    }
    if let Some(pulse) = input.pulse {
        if !(0.0..=400.0).contains(&pulse) {
            bail!("--pulse must be in 0–400 bpm, got {pulse}");
        }
        let mut obs = base(LOINC_PULSE);
        obs.value = Some(Measure {
            value: pulse,
            ucum_unit: "/min".into(),
        });
        observations.push(obs);
    }

    if observations.is_empty() {
        bail!("nothing to send: provide at least one of --bp, --spo2, --pulse");
    }
    Ok(observations)
}

/// Parse `"142/95"` → `(142.0, 95.0)` with clinical sanity bounds.
fn parse_bp(bp: &str) -> Result<(f64, f64)> {
    let (sys, dia) = bp
        .split_once('/')
        .ok_or_else(|| anyhow!("--bp must look like 142/95, got {bp:?}"))?;
    let systolic: f64 = sys
        .trim()
        .parse()
        .with_context(|| format!("systolic part of --bp {bp:?}"))?;
    let diastolic: f64 = dia
        .trim()
        .parse()
        .with_context(|| format!("diastolic part of --bp {bp:?}"))?;
    if !(20.0..=350.0).contains(&systolic) || !(10.0..=250.0).contains(&diastolic) {
        bail!("--bp {bp:?} is outside plausible clinical bounds");
    }
    if diastolic >= systolic {
        bail!("--bp {bp:?}: diastolic must be below systolic");
    }
    Ok((systolic, diastolic))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo_input() -> VitalsInput {
        VitalsInput {
            bp: Some("142/95".into()),
            spo2: Some(91.0),
            pulse: Some(108.0),
            patient: "P-1023".into(),
            device: "dev-a".into(),
            performer: "fw-7".into(),
        }
    }

    #[test]
    fn demo_capture_maps_to_loinc_observations() {
        let observations = match build_observations(&demo_input()) {
            Ok(o) => o,
            Err(e) => panic!("demo capture must build: {e:#}"),
        };
        assert_eq!(observations.len(), 3);

        let bp = &observations[0];
        assert_eq!(bp.loinc, LOINC_BP_PANEL);
        assert!(bp.value.is_none(), "BP panel is component-valued");
        assert_eq!(bp.components.len(), 2);
        assert_eq!(bp.components[0].loinc, LOINC_BP_SYSTOLIC);
        assert!((bp.components[0].value.value - 142.0).abs() < f64::EPSILON);
        assert_eq!(bp.components[0].value.ucum_unit, "mm[Hg]");
        assert_eq!(bp.components[1].loinc, LOINC_BP_DIASTOLIC);
        assert!((bp.components[1].value.value - 95.0).abs() < f64::EPSILON);

        let spo2 = &observations[1];
        assert_eq!(spo2.loinc, LOINC_SPO2);
        assert_eq!(spo2.value.as_ref().map(|m| m.ucum_unit.as_str()), Some("%"));

        let pulse = &observations[2];
        assert_eq!(pulse.loinc, LOINC_PULSE);
        assert_eq!(
            pulse.value.as_ref().map(|m| m.ucum_unit.as_str()),
            Some("/min")
        );
        for obs in &observations {
            assert_eq!(obs.patient_id, "P-1023");
            assert_eq!(obs.device_id, "dev-a");
            assert_eq!(obs.performer_id, "fw-7");
        }
    }

    #[test]
    fn rejects_nonsense_inputs() {
        assert!(parse_bp("142").is_err(), "missing slash");
        assert!(parse_bp("abc/def").is_err(), "not numbers");
        assert!(parse_bp("95/142").is_err(), "diastolic above systolic");
        assert!(parse_bp("900/20").is_err(), "implausible");

        let mut input = demo_input();
        input.spo2 = Some(150.0);
        assert!(build_observations(&input).is_err(), "SpO2 > 100%");

        let empty = VitalsInput {
            bp: None,
            spo2: None,
            pulse: None,
            ..demo_input()
        };
        assert!(build_observations(&empty).is_err(), "no measurements");
    }
}
