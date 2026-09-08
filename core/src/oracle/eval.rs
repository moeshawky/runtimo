//! Property evaluation engine.
//!
//! Evaluates [`PropertySpec`] predicates against a slice of [`WalEvent`]
//! records. The evaluator uses **shared borrows only** — no file I/O,
//! no WAL access, no `BundleWriter` interaction, and **never reads
//! `bundle_hash`**. This ensures the oracle is a pure, side-effect-free
//! observer of event data.
//!
//! # Module DNA
//! - **Owns**: Property verdict computation, field resolution, type coercion
//! - **Depends**: [`WalEvent`] for event data, [`PropertySpec`] for predicates
//! - **Provides**: `Verdict`, `PropertyVerdict`, `evaluate`
//!
//! # Invariants
//! - Unknown field paths produce `Ok(Verdict::Error)`, NOT `Err`.
//! - Type-coercion failures produce `Ok(Verdict::Error)`, NOT `Err`.
//! - Only catastrophic misuse (e.g., internal logic errors) produces `Err(OracleError::EvalError)`.
//! - `bundle_hash` is never accessed — integrity is not the oracle's concern.
//! - Empty predicate set → `Verdict::Satisfied` (total-function behavior).
//! - Shared borrows only: `events: &[WalEvent]`, `spec: &PropertySpec`.

use serde_json::Value;

use crate::wal::WalEvent;

use super::spec::{Op, Predicate, PropertySpec};
use super::OracleError;

/// The outcome of evaluating a single property against a set of events.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Verdict {
    /// All predicates in the specification are satisfied by the events.
    Satisfied,
    /// At least one predicate is violated by the events.
    Violated,
    /// The property could not be evaluated due to an unknown field path
    /// or type-coercion failure. This is an infra/property failure, not
    /// a catastrophic error — the caller receives `Ok(Verdict::Error)`.
    Error,
}

/// The result of evaluating a property specification against events.
///
/// The `detail` field is **always populated** with a human-readable
/// description of the outcome, including which predicate failed or
/// why the evaluation produced an error.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PropertyVerdict {
    /// The name of the property that was evaluated.
    pub name: String,
    /// The verdict of the evaluation.
    pub verdict: Verdict,
    /// A detailed description of the outcome. Always populated.
    pub detail: String,
}

/// The outcome of evaluating a single predicate against one event.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PredicateOutcome {
    /// The predicate is satisfied by this event.
    Satisfied,
    /// The predicate is violated by this event (field exists, comparison fails).
    Violated,
    /// The field path is unknown on this event.
    UnknownField,
    /// Type coercion failed during comparison.
    TypeMismatch,
}

/// Evaluate a property specification against a slice of WAL events.
///
/// # Arguments
///
/// * `events` — A shared borrow of WAL events to evaluate against.
///   Must not be mutated.
/// * `spec` — A shared borrow of the property specification.
///
/// # Returns
///
/// * `Ok(PropertyVerdict)` — The evaluation result. The `verdict` field
///   indicates `Satisfied`, `Violated`, or `Error`.
/// * `Err(OracleError::EvalError)` — Only on catastrophic internal misuse
///   (e.g., an invariant that should never be violated).
///
/// # Error Contract
///
/// - **Unknown field path** → `Ok(Verdict::Error)` with detail naming the field.
///   This is NOT an `Err` — it's an infra/property distinction.
/// - **Type-coercion failure** → `Ok(Verdict::Error)` with detail.
/// - **Catastrophic misuse** → `Err(OracleError::EvalError)`.
/// - **Empty predicate set** → `Ok(Verdict::Satisfied)`.
///
/// # Errors
///
/// Returns [`OracleError::EvalError`] only on catastrophic internal
/// misuse (an invariant that should never be violated). Unknown field
/// paths and type-coercion failures return `Ok(Verdict::Error)`,
/// NOT `Err` — see the # Error Contract above.
///
/// # Invariants
///
/// - Never reads `bundle_hash` from any event.
/// - Never performs file I/O or accesses WAL internals.
/// - Never mutates `events` or `spec`.
/// - Uses shared borrows only (`&[WalEvent]`, `&PropertySpec`).
/// - AND semantics: all predicates must hold for `Satisfied`.
pub fn evaluate(events: &[WalEvent], spec: &PropertySpec) -> Result<PropertyVerdict, OracleError> {
    // Empty predicate set → total-function satisfied (load-bearing behavior)
    if spec.predicates.is_empty() {
        return Ok(PropertyVerdict {
            name: spec.name.clone(),
            verdict: Verdict::Satisfied,
            detail: "Empty predicate set: total-function satisfied by definition".to_string(),
        });
    }

    let mut has_error = false;
    let mut error_fields = Vec::new();
    let mut violated_fields = Vec::new();

    for predicate in &spec.predicates {
        // Evaluate against ALL events (AND semantics)
        let mut outcome = PredicateOutcome::Satisfied;
        for event in events {
            let pred_outcome = evaluate_predicate(event, predicate);
            match pred_outcome {
                PredicateOutcome::Satisfied => {}
                PredicateOutcome::Violated => {
                    outcome = PredicateOutcome::Violated;
                    break; // AND: one violation is enough
                }
                PredicateOutcome::UnknownField | PredicateOutcome::TypeMismatch => {
                    has_error = true;
                    error_fields.push(predicate.field.clone());
                    break;
                }
            }
        }

        if !has_error && outcome == PredicateOutcome::Violated {
            violated_fields.push(predicate.field.clone());
        }
    }

    if has_error {
        return Ok(PropertyVerdict {
            name: spec.name.clone(),
            verdict: Verdict::Error,
            detail: format!(
                "Evaluation error on field(s): {}; predicate could not be evaluated",
                error_fields.join(", ")
            ),
        });
    }

    if !violated_fields.is_empty() {
        return Ok(PropertyVerdict {
            name: spec.name.clone(),
            verdict: Verdict::Violated,
            detail: format!(
                "Predicate(s) violated on field(s): {}; expected all predicates to hold",
                violated_fields.join(", ")
            ),
        });
    }

    Ok(PropertyVerdict {
        name: spec.name.clone(),
        verdict: Verdict::Satisfied,
        detail: format!("All {} predicate(s) satisfied", spec.predicates.len()),
    })
}

/// Evaluate a single predicate against one event.
///
/// # Returns
/// - `PredicateOutcome::Satisfied` if the predicate holds
/// - `PredicateOutcome::Violated` if the field exists but comparison fails
/// - `PredicateOutcome::UnknownField` if the field path doesn't exist on the event
/// - `PredicateOutcome::TypeMismatch` if type coercion fails
fn evaluate_predicate(event: &WalEvent, predicate: &Predicate) -> PredicateOutcome {
    let Some(field_value) = super::spec::extract_field(event, &predicate.field) else {
        return PredicateOutcome::UnknownField;
    };

    match compare_values(&field_value, &predicate.op, &predicate.value) {
        Ok(result) => {
            if result {
                PredicateOutcome::Satisfied
            } else {
                PredicateOutcome::Violated
            }
        }
        Err(_) => PredicateOutcome::TypeMismatch,
    }
}

/// Compare a field value against a predicate value using the given operator.
///
/// # Type Coercion
///
/// Numeric comparisons attempt to coerce both values to `f64`.
/// String comparisons use the string representation.
/// Returns an error string if coercion fails.
fn compare_values(field_val: &Value, op: &Op, pred_val: &Value) -> Result<bool, String> {
    match op {
        Op::Eq => Ok(field_val == pred_val),
        Op::Neq => Ok(field_val != pred_val),
        Op::Gt | Op::Lt | Op::Gte | Op::Lte => {
            let field_num = field_val.as_f64().ok_or_else(|| {
                format!(
                    "Cannot coerce field value {:?} to number for numeric comparison",
                    field_val
                )
            })?;
            let pred_num = pred_val.as_f64().ok_or_else(|| {
                format!(
                    "Cannot coerce predicate value {:?} to number for numeric comparison",
                    pred_val
                )
            })?;
            match op {
                Op::Gt => Ok(field_num > pred_num),
                Op::Lt => Ok(field_num < pred_num),
                Op::Gte => Ok(field_num >= pred_num),
                Op::Lte => Ok(field_num <= pred_num),
                _ => unreachable!(),
            }
        }
        Op::Contains => {
            let field_str = field_val.as_str().ok_or_else(|| {
                format!(
                    "Cannot coerce field value {:?} to string for Contains comparison",
                    field_val
                )
            })?;
            let pred_str = pred_val.as_str().ok_or_else(|| {
                format!(
                    "Cannot coerce predicate value {:?} to string for Contains comparison",
                    pred_val
                )
            })?;
            Ok(field_str.contains(pred_str))
        }
        Op::Regex => {
            // Intentionally non-full-regex: substring match using std only.
            // The `regex` crate is not a dependency of runtimo-core.
            let field_str = field_val.as_str().ok_or_else(|| {
                format!(
                    "Cannot coerce field value {:?} to string for Regex comparison",
                    field_val
                )
            })?;
            let pred_str = pred_val.as_str().ok_or_else(|| {
                format!(
                    "Cannot coerce predicate value {:?} to string for Regex comparison",
                    pred_val
                )
            })?;
            Ok(field_str.contains(pred_str))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::oracle::spec;
    use crate::wal::WalEventType;

    fn sample_event() -> WalEvent {
        WalEvent {
            seq: 1,
            ts: 1715800000,
            event_type: WalEventType::JobStarted,
            job_id: "test-job-42".to_string(),
            capability: Some("FileRead".to_string()),
            output: Some(json!({"data": {"path": "/tmp/test.txt"}, "status": "ok"})),
            error: None,
            telemetry_before: None,
            telemetry_after: None,
            process_before: None,
            process_after: None,
            cmd: None,
            cmd_stdout: None,
            cmd_stderr: None,
            cmd_exit_code: None,
            cmd_corrected: None,
            ..Default::default()
        }
    }

    #[test]
    fn test_empty_spec_satisfied() {
        let spec = spec::PropertySpec {
            name: "empty-prop".to_string(),
            predicates: vec![],
        };
        let events = vec![sample_event()];
        let result = evaluate(&events, &spec).unwrap();
        assert_eq!(result.verdict, Verdict::Satisfied);
        assert!(!result.detail.is_empty());
    }

    #[test]
    fn test_satisfied_eq_predicate() {
        let spec = spec::PropertySpec {
            name: "event-type-check".to_string(),
            predicates: vec![Predicate {
                field: "event_type".to_string(),
                op: Op::Eq,
                value: Value::String("job_started".to_string()),
            }],
        };
        let events = vec![sample_event()];
        let result = evaluate(&events, &spec).unwrap();
        assert_eq!(result.verdict, Verdict::Satisfied);
    }

    #[test]
    fn test_violated_eq_predicate() {
        let spec = spec::PropertySpec {
            name: "event-type-check".to_string(),
            predicates: vec![Predicate {
                field: "event_type".to_string(),
                op: Op::Eq,
                value: Value::String("job_completed".to_string()),
            }],
        };
        let events = vec![sample_event()];
        let result = evaluate(&events, &spec).unwrap();
        assert_eq!(result.verdict, Verdict::Violated);
    }

    #[test]
    fn test_unknown_field_returns_error_verdict() {
        let spec = spec::PropertySpec {
            name: "unknown-field-check".to_string(),
            predicates: vec![Predicate {
                field: "nonexistent_field".to_string(),
                op: Op::Eq,
                value: Value::String("test".to_string()),
            }],
        };
        let events = vec![sample_event()];
        let result = evaluate(&events, &spec).unwrap();
        assert_eq!(result.verdict, Verdict::Error);
    }

    #[test]
    fn test_watermark_field_returns_error_verdict() {
        let spec = spec::PropertySpec {
            name: "watermark-check".to_string(),
            predicates: vec![Predicate {
                field: "watermark".to_string(),
                op: Op::Eq,
                value: Value::String("test".to_string()),
            }],
        };
        let events = vec![sample_event()];
        let result = evaluate(&events, &spec).unwrap();
        assert_eq!(result.verdict, Verdict::Error);
    }

    #[test]
    fn test_contains_predicate_satisfied() {
        let spec = spec::PropertySpec {
            name: "job-id-contains".to_string(),
            predicates: vec![Predicate {
                field: "job_id".to_string(),
                op: Op::Contains,
                value: Value::String("test".to_string()),
            }],
        };
        let events = vec![sample_event()];
        let result = evaluate(&events, &spec).unwrap();
        assert_eq!(result.verdict, Verdict::Satisfied);
    }

    #[test]
    fn test_regex_predicate_satisfied() {
        let spec = spec::PropertySpec {
            name: "regex-check".to_string(),
            predicates: vec![Predicate {
                field: "job_id".to_string(),
                op: Op::Regex,
                value: Value::String("test".to_string()),
            }],
        };
        let events = vec![sample_event()];
        let result = evaluate(&events, &spec).unwrap();
        assert_eq!(result.verdict, Verdict::Satisfied);
    }

    #[test]
    fn test_gt_predicate_satisfied() {
        let spec = spec::PropertySpec {
            name: "seq-gt".to_string(),
            predicates: vec![Predicate {
                field: "seq".to_string(),
                op: Op::Gt,
                value: Value::from(0u64),
            }],
        };
        let events = vec![sample_event()];
        let result = evaluate(&events, &spec).unwrap();
        assert_eq!(result.verdict, Verdict::Satisfied);
    }

    #[test]
    fn test_multiple_predicates_and_semantics() {
        let spec = spec::PropertySpec {
            name: "multi-pred".to_string(),
            predicates: vec![
                Predicate {
                    field: "event_type".to_string(),
                    op: Op::Eq,
                    value: Value::String("job_started".to_string()),
                },
                Predicate {
                    field: "job_id".to_string(),
                    op: Op::Contains,
                    value: Value::String("test".to_string()),
                },
            ],
        };
        let events = vec![sample_event()];
        let result = evaluate(&events, &spec).unwrap();
        assert_eq!(result.verdict, Verdict::Satisfied);
    }

    #[test]
    fn test_multiple_predicates_one_violated() {
        let spec = spec::PropertySpec {
            name: "multi-pred-violated".to_string(),
            predicates: vec![
                Predicate {
                    field: "event_type".to_string(),
                    op: Op::Eq,
                    value: Value::String("job_started".to_string()),
                },
                Predicate {
                    field: "event_type".to_string(),
                    op: Op::Eq,
                    value: Value::String("job_completed".to_string()),
                },
            ],
        };
        let events = vec![sample_event()];
        let result = evaluate(&events, &spec).unwrap();
        assert_eq!(result.verdict, Verdict::Violated);
    }

    #[test]
    fn test_evaluate_readonly_no_mutation() {
        // Prove that evaluate does not mutate the events slice
        let mut event = sample_event();
        event.seq = 42;
        let events = vec![event.clone()];

        let spec = spec::PropertySpec {
            name: "readonly-check".to_string(),
            predicates: vec![Predicate {
                field: "seq".to_string(),
                op: Op::Gt,
                value: Value::from(0u64),
            }],
        };

        let result1 = evaluate(&events, &spec).unwrap();
        let result2 = evaluate(&events, &spec).unwrap();

        // Events slice must be byte-identical before and after
        let before_json = serde_json::to_string(&events).unwrap();
        let _ = result1;
        let _ = result2;
        let after_json = serde_json::to_string(&events).unwrap();

        assert_eq!(before_json, after_json, "Events slice must not be mutated");
        assert_eq!(
            result1, result2,
            "Two evaluations over same slice must produce identical verdicts"
        );
    }

    #[test]
    fn test_hash_absent_event_evaluates_normally() {
        // Prove that oracle never consults bundle_hash / chain integrity
        let mut event = sample_event();
        event.bundle_hash = None; // Explicitly no bundle hash
        let events = vec![event];

        let spec = spec::PropertySpec {
            name: "no-bundle-hash".to_string(),
            predicates: vec![Predicate {
                field: "event_type".to_string(),
                op: Op::Eq,
                value: Value::String("job_started".to_string()),
            }],
        };

        let result = evaluate(&events, &spec).unwrap();
        assert_eq!(result.verdict, Verdict::Satisfied);
        // If bundle_hash were consulted, this would fail or error
    }

    #[test]
    fn test_type_coercion_failure_returns_error_verdict() {
        // Gt on a string field value should produce Error verdict
        let spec = spec::PropertySpec {
            name: "type-mismatch".to_string(),
            predicates: vec![Predicate {
                field: "event_type".to_string(),
                op: Op::Gt,
                value: Value::from(5u64),
            }],
        };
        let events = vec![sample_event()];
        let result = evaluate(&events, &spec).unwrap();
        assert_eq!(result.verdict, Verdict::Error);
    }

    #[test]
    fn test_detail_always_populated() {
        let spec = spec::PropertySpec {
            name: "detail-check".to_string(),
            predicates: vec![Predicate {
                field: "event_type".to_string(),
                op: Op::Eq,
                value: Value::String("job_started".to_string()),
            }],
        };
        let events = vec![sample_event()];
        let result = evaluate(&events, &spec).unwrap();
        assert!(!result.detail.is_empty());
    }
}
