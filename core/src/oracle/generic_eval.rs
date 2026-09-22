//! Narrow generic select-then-quantify evaluator over a typed field resolver.
//!
//! Shared by WAL (`WalEvent`) and RuntimeFacts (`serde_json::Value`) evaluators.
//! Each source provides its own field resolution; the evaluator owns only the
//! comparator/selector/quantifier semantics.
//!
//! # Invariants
//! - Unknown field in selector → item filtered out (never Error).
//! - Unknown field in predicate → `Verdict::Error` with field name.
//! - Type mismatch → `Verdict::Error` with field name.
//! - Empty predicate set → `Verdict::Satisfied` (total-function behavior).
//! - Empty selection: `All` vacuously satisfied; `Exists`/`Count≥1` violated;
//!   `None` satisfied.
//!
//! # Type Coercion
//! - Numeric comparison: both values must coerce to f64.
//! - String comparison (Contains/Regex): both values must coerce to str.
//! - Eq/Neq: direct serde_json::Value equality.

use super::eval::compare_values;
use super::spec::{Op, PropertySpec, Quantifier};
use crate::oracle::eval::Verdict;
use serde_json::Value;
use std::fmt::Debug;

/// Result of evaluating a property spec over a generic item slice.
///
/// Mirrors `PropertyVerdict` but is generic over the item type so callers
/// can format source-specific details.
#[derive(Debug)]
#[allow(clippy::exhaustive_structs)]
pub struct GenericVerdict {
    /// Property name from the spec.
    pub name: String,
    /// The verdict (Satisfied, Violated, or Error).
    pub verdict: Verdict,
    /// Human-readable detail.
    pub detail: String,
    /// Number of items that passed the select filter.
    pub selected_count: usize,
    /// Number of selected items evaluated (always equals selected_count).
    pub evaluated_count: usize,
    /// Number of selected items matching all predicates.
    pub matched_count: usize,
}

/// Evaluate a property spec over a generic slice of items.
///
/// # Arguments
/// * `items` — Slice of items to evaluate (shared borrow only).
/// * `spec` — Property specification (select, predicates, quantifier).
/// * `resolve_field` — Typed field resolver: given an item and field path,
///   returns the field value or None if absent.
///
/// # Returns
/// A `GenericVerdict` with the result. Never returns `Err` — all failures
/// (unknown fields, type mismatches) produce `Verdict::Error` with detail.
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss,
    clippy::float_cmp
)]
pub fn evaluate_items<T>(
    items: &[T],
    spec: &PropertySpec,
    resolve_field: &dyn Fn(&T, &str) -> Option<Value>,
) -> GenericVerdict {
    // Empty predicate set → total-function satisfied.
    if spec.predicates.is_empty() {
        return GenericVerdict {
            name: spec.name.clone(),
            verdict: Verdict::Satisfied,
            detail: format!("Empty predicates over {} selected: satisfied", items.len()),
            selected_count: items.len(),
            evaluated_count: items.len(),
            matched_count: items.len(),
        };
    }

    // 1. Select: filter items where all select predicates match.
    // Missing field → filtered out (never Error in selector).
    let selected: Vec<&T> = items
        .iter()
        .filter(|item| {
            spec.select.iter().all(|p| {
                resolve_field(item, &p.field)
                    .and_then(|fv| compare_values(&fv, &p.op, &p.value).ok())
                    .unwrap_or(false)
            })
        })
        .collect();
    let selected_count = selected.len();

    // 2. Evaluate predicates on each selected item.
    // Missing field or type mismatch → Verdict::Error.
    let mut matched = 0usize;
    for item in &selected {
        let mut all_hold = true;
        for p in &spec.predicates {
            let Some(fv) = resolve_field(item, &p.field) else {
                return GenericVerdict {
                    name: spec.name.clone(),
                    verdict: Verdict::Error,
                    detail: format!(
                        "Unknown field '{}' in predicate; selector should have filtered unrelated items",
                        p.field
                    ),
                    selected_count,
                    evaluated_count: selected_count,
                    matched_count: matched,
                };
            };
            let ok = match compare_values(&fv, &p.op, &p.value) {
                Ok(b) => b,
                Err(e) => {
                    return GenericVerdict {
                        name: spec.name.clone(),
                        verdict: Verdict::Error,
                        detail: format!("Type mismatch on field '{}': {}", p.field, e),
                        selected_count,
                        evaluated_count: selected_count,
                        matched_count: matched,
                    };
                }
            };
            if !ok {
                all_hold = false;
                break;
            }
        }
        if all_hold {
            matched += 1;
        }
    }

    // 3. Quantify.
    let (verdict, detail) = match &spec.quantifier {
        Quantifier::All => {
            if matched == selected_count {
                (
                    Verdict::Satisfied,
                    format!("ALL: {matched}/{selected_count} satisfy"),
                )
            } else {
                (
                    Verdict::Violated,
                    format!("ALL violated: {matched}/{selected_count} satisfy"),
                )
            }
        }
        Quantifier::Exists => {
            if matched >= 1 {
                (
                    Verdict::Satisfied,
                    format!("EXISTS: {matched}/{selected_count} satisfy"),
                )
            } else {
                (
                    Verdict::Violated,
                    format!("EXISTS violated: 0/{selected_count} satisfy"),
                )
            }
        }
        Quantifier::None => {
            if matched == 0 {
                (
                    Verdict::Satisfied,
                    format!("NONE: 0/{selected_count} satisfy"),
                )
            } else {
                (
                    Verdict::Violated,
                    format!("NONE violated: {matched}/{selected_count} satisfy"),
                )
            }
        }
        Quantifier::Count { op, threshold } => {
            let a = matched as f64;
            let b = *threshold as f64;
            let ok = match op {
                Op::Gt => a > b,
                Op::Lt => a < b,
                Op::Gte => a >= b,
                Op::Lte => a <= b,
                Op::Eq => a == b,
                Op::Neq => a != b,
                _ => false,
            };
            if ok {
                (
                    Verdict::Satisfied,
                    format!("COUNT {matched} {op:?} {threshold} (empty predicates)"),
                )
            } else {
                (
                    Verdict::Violated,
                    format!("COUNT {matched} {op:?} {threshold} violated"),
                )
            }
        }
    };

    GenericVerdict {
        name: spec.name.clone(),
        verdict,
        detail,
        selected_count,
        evaluated_count: selected_count,
        matched_count: matched,
    }
}
