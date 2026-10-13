//! Auto-rollback: a per-flag, per-environment policy that turns a flag off
//! when the errors reported behind it say it is hurting the application.
//!
//! This part is pure: the thresholds a policy carries and [`decide`], the one
//! definition of "this flag is misbehaving". `flag_health`'s advice
//! ([`crate::telemetry`]'s `assess`) calls the same function with the default
//! thresholds, so what an agent is told and what the checker does cannot
//! disagree.

use serde::Serialize;

use crate::error::{Error, Result};

/// When a policy trips. The defaults are the column defaults of
/// `rollback_policies` (a test pins that) and are what `flag_health` judges by.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Thresholds {
    /// Evaluations with the flag on needed before judging (>= 1).
    pub min_evaluations: i64,
    /// Errors with the flag on needed before judging (>= 1).
    pub min_errors: i64,
    /// Trip when the on error rate exceeds the off rate by more than this
    /// factor (> 1.0).
    pub max_error_ratio: f64,
    /// Trip when the on error rate exceeds this absolute rate (0 < r <= 1).
    /// The only trigger that works with no off traffic.
    pub max_error_rate: Option<f64>,
    /// How far back the sample reaches, in hours (1 to 24).
    pub window_hours: i32,
    /// Whether errors and evaluations reported by public client keys count.
    pub include_client_reports: bool,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            min_evaluations: 100,
            min_errors: 10,
            max_error_ratio: 2.0,
            max_error_rate: None,
            window_hours: 1,
            include_client_reports: false,
        }
    }
}

impl Thresholds {
    /// Check the values against the same limits as the table's CHECK
    /// constraints, with a message an agent can act on.
    ///
    /// ```
    /// use flags_core::rollback::Thresholds;
    /// assert!(Thresholds::default().validate().is_ok());
    /// let bad = Thresholds { window_hours: 25, ..Thresholds::default() };
    /// assert!(bad.validate().is_err());
    /// ```
    pub fn validate(&self) -> Result<()> {
        if self.min_evaluations < 1 {
            return Err(Error::Invalid(format!(
                "minEvaluations must be at least 1, got {}",
                self.min_evaluations
            )));
        }
        if self.min_errors < 1 {
            return Err(Error::Invalid(format!(
                "minErrors must be at least 1, got {}",
                self.min_errors
            )));
        }
        if !(self.max_error_ratio.is_finite() && self.max_error_ratio > 1.0) {
            return Err(Error::Invalid(format!(
                "maxErrorRatio must be greater than 1.0 (the factor by which the error rate \
                 with the flag on must exceed the rate with it off), got {}",
                self.max_error_ratio
            )));
        }
        if let Some(r) = self.max_error_rate {
            if !(r > 0.0 && r <= 1.0) {
                return Err(Error::Invalid(format!(
                    "maxErrorRate must be greater than 0 and at most 1 (errors per \
                     evaluation), or omitted, got {r}"
                )));
            }
        }
        if !(1..=24).contains(&self.window_hours) {
            return Err(Error::Invalid(format!(
                "windowHours must be between 1 and 24, got {}",
                self.window_hours
            )));
        }
        Ok(())
    }
}

/// Counts over one window, split by whether the flag was on or off.
#[derive(Debug, Clone, Default, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Sample {
    pub evals_on: i64,
    pub evals_off: i64,
    pub errors_on: i64,
    pub errors_off: i64,
}

impl Sample {
    /// Errors per evaluation with the flag on; `None` with no evaluations.
    pub fn rate_on(&self) -> Option<f64> {
        (self.evals_on > 0).then(|| self.errors_on as f64 / self.evals_on as f64)
    }

    /// Errors per evaluation with the flag off; `None` with no evaluations.
    pub fn rate_off(&self) -> Option<f64> {
        (self.evals_off > 0).then(|| self.errors_off as f64 / self.evals_off as f64)
    }
}

/// Which condition tripped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum Trigger {
    /// The on error rate is above `max_error_rate`.
    MaxErrorRate,
    /// The on error rate is more than `max_error_ratio` times the off rate.
    ErrorRatio,
    /// The flag is erroring while on and the off side has traffic but no errors.
    OffHasNoErrors,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(tag = "decision", content = "trigger", rename_all = "camelCase")]
pub enum Decision {
    /// Not enough evaluations or errors with the flag on to judge.
    BelowMinimums,
    Healthy,
    Trip(Trigger),
}

/// The decision rule. Trips when the sample meets both minimums and any of:
/// the on rate exceeds `max_error_rate`; the off side has traffic and errors
/// and the on rate exceeds the off rate times `max_error_ratio` (strictly);
/// the off side has traffic but no errors.
///
/// ```
/// use flags_core::rollback::{decide, Decision, Sample, Thresholds, Trigger};
/// let s = Sample { evals_on: 1000, evals_off: 1000, errors_on: 50, errors_off: 5 };
/// assert_eq!(decide(&s, &Thresholds::default()), Decision::Trip(Trigger::ErrorRatio));
/// ```
pub fn decide(sample: &Sample, t: &Thresholds) -> Decision {
    if sample.evals_on < t.min_evaluations || sample.errors_on < t.min_errors {
        return Decision::BelowMinimums;
    }
    // evals_on >= min_evaluations >= 1 for valid thresholds; guard anyway so
    // invalid ones cannot panic or trip on a missing rate.
    let Some(on) = sample.rate_on() else {
        return Decision::BelowMinimums;
    };
    if t.max_error_rate.is_some_and(|max| on > max) {
        return Decision::Trip(Trigger::MaxErrorRate);
    }
    if let Some(off) = sample.rate_off() {
        if sample.errors_off == 0 {
            return Decision::Trip(Trigger::OffHasNoErrors);
        }
        if on > off * t.max_error_ratio {
            return Decision::Trip(Trigger::ErrorRatio);
        }
    }
    Decision::Healthy
}

/// False when `decide` can never trip on this sample's shape: no off traffic
/// and no `max_error_rate`, so there is nothing to compare against.
///
/// ```
/// use flags_core::rollback::{can_trip, Sample, Thresholds};
/// let at_full_rollout = Sample { evals_on: 5000, errors_on: 400, ..Sample::default() };
/// assert!(!can_trip(&at_full_rollout, &Thresholds::default()));
/// ```
pub fn can_trip(sample: &Sample, t: &Thresholds) -> bool {
    sample.evals_off > 0 || t.max_error_rate.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::{assess, Counts};

    fn s(evals_on: i64, evals_off: i64, errors_on: i64, errors_off: i64) -> Sample {
        Sample {
            evals_on,
            evals_off,
            errors_on,
            errors_off,
        }
    }

    fn d(sample: &Sample) -> Decision {
        decide(sample, &Thresholds::default())
    }

    #[test]
    fn too_few_evaluations_is_below_minimums() {
        assert_eq!(d(&s(99, 1000, 99, 0)), Decision::BelowMinimums);
    }

    #[test]
    fn too_few_errors_is_below_minimums() {
        assert_eq!(d(&s(1000, 1000, 9, 0)), Decision::BelowMinimums);
    }

    #[test]
    fn exactly_the_minimums_is_enough_to_trip() {
        assert_eq!(
            d(&s(100, 1000, 10, 0)),
            Decision::Trip(Trigger::OffHasNoErrors)
        );
    }

    #[test]
    fn decisions_serialize_as_one_object_shape() {
        assert_eq!(
            serde_json::to_value(Decision::Healthy).unwrap(),
            serde_json::json!({"decision": "healthy"})
        );
        assert_eq!(
            serde_json::to_value(Decision::Trip(Trigger::ErrorRatio)).unwrap(),
            serde_json::json!({"decision": "trip", "trigger": "errorRatio"})
        );
    }

    #[test]
    fn validation_messages_use_the_wire_field_names() {
        let ok = Thresholds::default();
        for (bad, name) in [
            (
                Thresholds {
                    min_evaluations: 0,
                    ..ok.clone()
                },
                "minEvaluations",
            ),
            (
                Thresholds {
                    min_errors: 0,
                    ..ok.clone()
                },
                "minErrors",
            ),
            (
                Thresholds {
                    max_error_ratio: 1.0,
                    ..ok.clone()
                },
                "maxErrorRatio",
            ),
            (
                Thresholds {
                    max_error_rate: Some(0.0),
                    ..ok.clone()
                },
                "maxErrorRate",
            ),
            (
                Thresholds {
                    window_hours: 25,
                    ..ok.clone()
                },
                "windowHours",
            ),
        ] {
            let msg = bad.validate().unwrap_err().to_string();
            assert!(msg.contains(name), "{msg}");
        }
    }

    #[test]
    fn a_clear_ratio_trips() {
        assert_eq!(
            d(&s(1000, 1000, 50, 5)),
            Decision::Trip(Trigger::ErrorRatio)
        );
    }

    #[test]
    fn exactly_the_ratio_does_not_trip() {
        // 20/1000 = 2.0 x 10/1000: strictly greater is required.
        assert_eq!(d(&s(1000, 1000, 20, 10)), Decision::Healthy);
    }

    #[test]
    fn errors_only_with_the_flag_on_trip() {
        assert_eq!(
            d(&s(1000, 1000, 20, 0)),
            Decision::Trip(Trigger::OffHasNoErrors)
        );
    }

    #[test]
    fn no_off_traffic_cannot_trip_without_a_max_rate() {
        let sample = s(1000, 0, 500, 0);
        assert_eq!(d(&sample), Decision::Healthy);
        assert!(!can_trip(&sample, &Thresholds::default()));
    }

    #[test]
    fn a_max_rate_trips_without_off_traffic() {
        let t = Thresholds {
            max_error_rate: Some(0.01),
            ..Thresholds::default()
        };
        let sample = s(1000, 0, 50, 0);
        assert!(can_trip(&sample, &t));
        assert_eq!(decide(&sample, &t), Decision::Trip(Trigger::MaxErrorRate));
    }

    #[test]
    fn validate_refuses_values_the_table_would() {
        let ok = Thresholds::default();
        assert!(ok.validate().is_ok());
        for bad in [
            Thresholds {
                max_error_ratio: 1.0,
                ..ok.clone()
            },
            Thresholds {
                window_hours: 25,
                ..ok.clone()
            },
            Thresholds {
                window_hours: 0,
                ..ok.clone()
            },
            Thresholds {
                max_error_rate: Some(0.0),
                ..ok.clone()
            },
            Thresholds {
                max_error_rate: Some(1.5),
                ..ok.clone()
            },
            Thresholds {
                min_evaluations: 0,
                ..ok.clone()
            },
            Thresholds {
                min_errors: 0,
                ..ok.clone()
            },
        ] {
            assert!(matches!(bad.validate(), Err(Error::Invalid(_))), "{bad:?}");
        }
        let edge = Thresholds {
            max_error_rate: Some(1.0),
            window_hours: 24,
            ..ok
        };
        assert!(edge.validate().is_ok());
    }

    /// Wiring check: `assess` says "Consider rolling back" only when `decide`
    /// returns `Trip`, over these cases.
    #[test]
    fn assess_and_decide_agree() {
        let cases = [
            (1000, 1000, 50, 5),
            (10, 0, 0, 0),
            (1000, 1000, 9, 0),
            (150, 1000, 5, 0),
            (100, 1000, 10, 0),
            (99, 1000, 99, 0),
            (1000, 1000, 20, 10),
            (1000, 1000, 20, 0),
            (1000, 0, 500, 0),
            (0, 1000, 0, 50),
            (0, 0, 0, 0),
            (1000, 1000, 30, 10),
            (100, 1, 10, 1),
            (100, 1, 10, 0),
        ];
        for (evals_on, evals_off, errors_on, errors_off) in cases {
            let sample = s(evals_on, evals_off, errors_on, errors_off);
            let text = assess(
                &Counts {
                    enabled: evals_on,
                    disabled: evals_off,
                },
                &Counts {
                    enabled: errors_on,
                    disabled: errors_off,
                },
                sample.rate_on(),
                sample.rate_off(),
            );
            assert_eq!(
                text.contains("Consider rolling back"),
                matches!(d(&sample), Decision::Trip(_)),
                "{sample:?}: {text}"
            );
        }
    }
}
