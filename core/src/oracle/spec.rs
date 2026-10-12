//! Property specification parsing and validation.
//!
//! Defines the `PropertySpec`, `Predicate`, and `Op` types that form the
//! contract for property-based evaluation. The `parse_spec` function
//! validates field paths and op/type compatibility at parse time, ensuring
//! that only well-formed specifications reach the evaluator.
//!
//! # Module DNA
//! - **Owns**: Predicate definition, field path validation, op/type compatibility checks
//! - **Depends**: `serde_json::Value` for predicate values, `WalEvent` for field resolution context
//! - **Provides**: `PropertySpec`, `Predicate`, `Op`, `parse_spec`
//!
//! # Invariants
//! - Empty `predicates` vector means total-function satisfied (load-bearing behavior).
//! - Field paths are validated at parse time; unknown paths are deferred to eval-time Error verdicts.
//! - Op/type incompatibility is caught at parse time, not eval time.

use serde_json::Value;
use thiserror::Error;

use crate::wal::WalEvent;

/// The set of comparison operators supported by predicates.
///
/// Each variant defines how a predicate's `value` is compared against
/// a field extracted from a [`WalEvent`]. The `Regex` variant uses
/// intentionally non-full-regex substring matching (see module docstring).
///
/// # Inputs
/// Wire strings: `"Eq"`, `"Neq"`, `"Gt"`, `"Lt"`, `"Gte"`, `"Lte"`,
/// `"Contains"`, `"Regex"` (exact case). Struct-form JSON deserializes
/// through [`Op::parse`], the single canonical authority — the shorthand
/// string form additionally accepts symbolic aliases (`==`, `!=`, `>`,
/// `<`, `>=`, `<=`) and lowercase `contains`/`regex`, then delegates
/// canonical spellings back to [`Op::parse`].
///
/// # Outputs
/// The parsed [`Op`] variant; deserialization fails on unknown spellings.
///
/// # Errors
/// Deserialization returns an unknown-variant error naming the expected
/// canonical spellings when the input is not recognized.
///
/// # Invariants
/// Canonical spelling table exists exactly once (in [`Op::parse`]);
/// struct-form serde and shorthand-form canonical branch both delegate
/// to it, so the two paths cannot diverge.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Op {
    /// Equality: field value equals `value`.
    Eq,
    /// Inequality: field value differs from `value`.
    Neq,
    /// Greater than: field value is strictly greater than `value`.
    Gt,
    /// Less than: field value is strictly less than `value`.
    Lt,
    /// Greater than or equal: field value is greater than or equal to `value`.
    Gte,
    /// Less than or equal: field value is less than or equal to `value`.
    Lte,
    /// Contains: the field value (as a string) contains `value` as a substring.
    Contains,
    /// Regex: the field value (as a string) contains `value` as a substring.
    ///
    /// # Implementation Note
    ///
    /// The `regex` crate is **not** a dependency of `runtimo-core`.
    /// This variant intentionally implements **substring matching** using
    /// only `std` — the pattern string must appear anywhere within the
    /// field value. This is **not** full regular-expression matching.
    /// Clients requiring true regex should add the `regex` crate as a
    /// dependency. This design decision is documented here and in the
    /// `parse_spec` docstring.
    Regex,
}

impl Op {
    /// Parses a string to an `Op` variant.
    ///
    /// Single canonical authority for operator spellings. Accepts exactly
    /// `"Eq"`, `"Neq"`, `"Gt"`, `"Lt"`, `"Gte"`, `"Lte"`, `"Contains"`,
    /// `"Regex"` (exact case). Both the serde struct-form path and the
    /// shorthand canonical branch delegate here.
    ///
    /// # Inputs
    /// `s`: operator spelling to resolve (borrowed, not retained).
    ///
    /// # Returns
    /// `Some(Op)` if the string matches a known canonical variant,
    /// `None` otherwise (symbolic aliases and lowercase spellings are
    /// resolved by the shorthand layer, not here).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Eq" => Some(Self::Eq),
            "Neq" => Some(Self::Neq),
            "Gt" => Some(Self::Gt),
            "Lt" => Some(Self::Lt),
            "Gte" => Some(Self::Gte),
            "Lte" => Some(Self::Lte),
            "Contains" => Some(Self::Contains),
            "Regex" => Some(Self::Regex),
            _ => None,
        }
    }

    /// Returns the wire string representation of this operator.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Eq => "Eq",
            Self::Neq => "Neq",
            Self::Gt => "Gt",
            Self::Lt => "Lt",
            Self::Gte => "Gte",
            Self::Lte => "Lte",
            Self::Contains => "Contains",
            Self::Regex => "Regex",
        }
    }
}

impl<'de> serde::Deserialize<'de> for Op {
    /// Deserializes a struct-form operator spelling via [`Op::parse`].
    ///
    /// # Inputs
    /// A JSON string holding one canonical spelling (`"Eq"`, `"Neq"`,
    /// `"Gt"`, `"Lt"`, `"Gte"`, `"Lte"`, `"Contains"`, `"Regex"`).
    ///
    /// # Outputs
    /// The resolved [`Op`] variant.
    ///
    /// # Errors
    /// Unknown-variant error on any non-canonical spelling (symbolic
    /// aliases such as `"=="` are shorthand-only and rejected here).
    ///
    /// # Invariants
    /// Delegates to [`Op::parse`]; the canonical table is not duplicated.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::parse(&s).ok_or_else(|| {
            serde::de::Error::unknown_variant(
                &s,
                &["Eq", "Neq", "Gt", "Lt", "Gte", "Lte", "Contains", "Regex"],
            )
        })
    }
}

/// A single predicate that tests a field path against an operator and value.
///
/// # Fields
/// - `field`: field path from the union vocabulary: WAL/event fields
///   (`"event_type"`, `"job_id"`, `"seq"`, `"capability"`, `"error"`,
///   `"output.<key>"`, `"watermark"`, `safety.*`), Runtime Facts v1 fields
///   (`"family"`, `"run_id"`, `"provider"`, `"fidelity"`, `"observations"`,
///   `"first_seen"`, `"last_seen"`, `"revision"`, `"process.pid"`,
///   `"process.process_start_time"`, `"extra.<key>"`).
/// - `op`: the comparison operator (canonical spellings via [`Op::parse`]).
/// - `value`: the value to compare against. Op/type compatibility is
///   validated at parse time.
///
/// # Deserialization
/// Accepts either the canonical struct form
/// (`{"field": .., "op": .., "value": ..}`) or the string shorthand
/// `"field op value"` (e.g. `"extra.drop_count == 0"`). Shorthand values
/// are parsed as JSON when possible, otherwise kept as strings, so `"0"`
/// becomes `Number(0)` while `"job started"` stays `String`. Shorthand
/// requires at least three whitespace-separated tokens; a two-token input
/// such as `"field Eq"` is rejected (a missing value is never coerced to
/// `""`). Struct-form operators accept canonical spellings only;
/// shorthand additionally accepts `==`, `!=`, `>`, `<`, `>=`, `<=` and
/// lowercase `contains`/`regex`.
///
/// # Source scoping
/// [`parse_spec`] validates against the union vocabulary, so a WAL-only
/// field (e.g. `"event_type"`) parses on a facts spec and vice versa.
/// Per-source evaluators resolve only their own vocabulary: unknown fields
/// at eval time yield `Verdict::Error` (WAL/generic paths), and the facts
/// CLI maps source-mismatched predicate fields to `Error`/exit 2 rather
/// than silent `Satisfied`.
///
/// # Invariants
/// - Three-token minimum on shorthand; empty value never implied.
/// - Canonical operator spellings resolve through [`Op::parse`] only.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Predicate {
    /// The field path to extract from a [`WalEvent`].
    pub field: String,
    /// The comparison operator.
    pub op: Op,
    /// The value to compare against.
    pub value: Value,
}

impl<'de> serde::Deserialize<'de> for Predicate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct PredicateStruct {
            field: String,
            op: Op,
            value: Value,
        }

        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum PredicateHelper {
            Struct(PredicateStruct),
            Shorthand(String),
        }

        match PredicateHelper::deserialize(deserializer)? {
            PredicateHelper::Struct(s) => Ok(Self {
                field: s.field,
                op: s.op,
                value: s.value,
            }),
            PredicateHelper::Shorthand(raw) => {
                let parts: Vec<&str> = raw.split_whitespace().collect();
                let [field, op, rest @ ..] = parts.as_slice() else {
                    return Err(serde::de::Error::custom(format!(
                        "Invalid shorthand predicate: '{raw}'. Expected 'field op value'"
                    )));
                };
                if rest.is_empty() {
                    return Err(serde::de::Error::custom(format!(
                        "Invalid shorthand predicate: '{raw}'. Expected 'field op value' (missing value)"
                    )));
                }
                let field = (*field).to_string();
                let op_ref: &str = op;
                // Canonical spellings resolve through the single authority
                // (Op::parse); symbolic aliases and lowercase
                // contains/regex are shorthand-only fallbacks.
                let op = Op::parse(op_ref)
                    .or(match op_ref {
                        "==" => Some(Op::Eq),
                        "!=" => Some(Op::Neq),
                        ">" => Some(Op::Gt),
                        "<" => Some(Op::Lt),
                        ">=" => Some(Op::Gte),
                        "<=" => Some(Op::Lte),
                        "contains" => Some(Op::Contains),
                        "regex" => Some(Op::Regex),
                        _ => None,
                    })
                    .ok_or_else(|| {
                        serde::de::Error::custom(format!(
                            "Unknown operator in shorthand '{op_ref}'"
                        ))
                    })?;
                let val_str = rest.join(" ");
                let value = match serde_json::from_str(&val_str) {
                    Ok(v) => v,
                    Err(_) => Value::String(val_str),
                };
                Ok(Self { field, op, value })
            }
        }
    }
}

/// Quantifier over the selected record set (§45-46).
///
/// Legacy specs (no `quantifier`) mean `All` (every event satisfies all
/// predicates — the v1 universal semantics, preserved).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Quantifier {
    /// Every selected record satisfies all predicates. Empty selection is
    /// vacuously `Satisfied` (legacy spirit) but reported via
    /// `selected_count: 0` — zero matches are never hidden.
    #[serde(alias = "ALL")]
    #[default]
    All,
    /// At least one selected record satisfies all predicates.
    #[serde(alias = "EXISTS")]
    Exists,
    /// No selected record satisfies all predicates.
    #[serde(alias = "NONE")]
    None,
    /// The number of satisfying records compared against `threshold`
    /// via `op` (numeric ops only: `Gte`/`Gt`/`Eq`/`Lte`/`Lt`).
    Count {
        /// Numeric comparison operator.
        op: Op,
        /// Threshold count.
        threshold: u64,
    },
}

/// A property specification consisting of a name and a set of predicates.
///
/// Predicates are combined with **AND semantics** within each candidate
/// record. An **empty predicate set** means **always satisfied**
/// (total-function behavior, load-bearing).
///
/// v2 (additive, back-compatible): optional `select` (pre-filter, ANDed),
/// optional `quantifier` (`all`|`exists`|`none`|`count`), optional
/// `version`. Legacy JSON without these fields parses as
/// `select: []`, `quantifier: All` — identical legacy meaning.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[non_exhaustive]
pub struct PropertySpec {
    /// The name of the property being specified.
    pub name: String,
    /// The predicates that must all hold (AND semantics).
    ///
    /// **Invariant**: An empty vector means the property is satisfied
    /// for all inputs (total-function behavior). This is intentional
    /// and load-bearing — do not change it to return `Violated`.
    #[serde(default)]
    pub predicates: Vec<Predicate>,
    /// Pre-filter: only records satisfying ALL select predicates are
    /// evaluated. Unrelated events (e.g. `JobStarted` lacking `safety.*`)
    /// are removed before predicate evaluation — never `UnknownField`.
    #[serde(default)]
    pub select: Vec<Predicate>,
    /// Quantifier over the selected set. Default `All` (legacy).
    #[serde(default)]
    pub quantifier: Quantifier,
    /// Explicit spec version (`1` legacy, `2` select/quantifier).
    /// `None` = legacy v1 shape.
    #[serde(default)]
    pub version: Option<u32>,
}

/// Errors that can occur during specification parsing.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ParseSpecError {
    /// The input string could not be parsed as a valid specification.
    #[error("Parse error: {0}")]
    InvalidFormat(String),
    /// The field path is not recognized or is malformed.
    #[error("Invalid field path '{field}': {message}")]
    InvalidFieldPath { field: String, message: String },
    /// The operator is incompatible with the field's type.
    #[error("Op/type mismatch on field '{field}': {message}")]
    OpTypeMismatch { field: String, message: String },
}

/// Internal helper struct for parsing the JSON wrapper format.
#[derive(serde::Deserialize)]
struct SpecWrapper {
    name: String,
    #[serde(default)]
    predicates: Vec<Predicate>,
    #[serde(default)]
    select: Vec<Predicate>,
    #[serde(default)]
    quantifier: Option<Quantifier>,
    #[serde(default)]
    version: Option<u32>,
}

/// Parse a specification string into a [`PropertySpec`].
///
/// # Inputs
/// A JSON object string with `name` (string), `predicates` (array of
/// struct-form `{"field","op","value"}` objects or shorthand strings
/// `"field op value"`), plus optional `select`, `quantifier`, `version`
/// (v2 additive; legacy shapes parse with `select: []`,
/// `quantifier: All`).
///
/// # Outputs
/// The validated [`PropertySpec`]; predicates preserve input order.
///
/// # Validation
/// 1. **Field path syntax**: union vocabulary — WAL/event fields
///    (`"event_type"`, `"job_id"`, `"seq"`, `"capability"`, `"error"`,
///    `"output.<key>"`, `"watermark"`, `safety.*`), Runtime Facts v1 fields
///    (`"family"`, `"run_id"`, `"provider"`, `"fidelity"`,
///    `"observations"`, `"first_seen"`, `"last_seen"`, `"revision"`,
///    `"process.pid"`, `"process.process_start_time"`, `"extra.<key>"`).
///    Shorthand values parse as JSON when possible, else strings (`"0"`
///    yields `Number(0)`; `"job started"` stays `String`).
/// 2. **Op/type compatibility**: numeric ops (`Gt`, `Lt`, `Gte`, `Lte`)
///    require numeric values; `Contains`/`Regex` accept string-compatible
///    values; `Eq`/`Neq` accept any JSON type.
/// 3. **Source scoping caveat**: validation is union-scoped, not
///    source-scoped — a WAL-only field parses on a facts spec and vice
///    versa. Per-source evaluators reject mismatched predicate fields at
///    eval time (`Verdict::Error`), and the facts CLI maps them to
///    `Error`/exit 2.
///
/// # Errors
///
/// Returns [`ParseSpecError::InvalidFormat`] if the string is not valid JSON
/// or cannot be deserialized into the expected structure (including
/// malformed shorthand: fewer than three tokens, missing value, or
/// unknown operator).
/// Returns [`ParseSpecError::InvalidFieldPath`] if the field path is malformed.
/// Returns [`ParseSpecError::OpTypeMismatch`] if the operator is incompatible
/// with the field's type.
///
/// # Invariants
/// - Empty `predicates` parses successfully (total-function satisfied).
/// - `process.pid` spans the on-disk `process_key.pid` alias; both the
///   dotted spec spelling and the storage key resolve to the same value
///   (see [`validate_field_path`] and the facts resolver).
///
/// # Example
///
/// ```rust,ignore
/// let spec = parse_spec(r#"{"name":"check","predicates":[{"field":"event_type","op":"Eq","value":"job_started"}]}"#)?;
/// assert_eq!(spec.predicates.len(), 1);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn parse_spec(input: &str) -> Result<PropertySpec, ParseSpecError> {
    let wrapper: SpecWrapper = serde_json::from_str(input)
        .map_err(|e| ParseSpecError::InvalidFormat(format!("JSON parse error: {e}")))?;

    for pred in wrapper.predicates.iter().chain(wrapper.select.iter()) {
        validate_field_path(&pred.field)?;
        validate_op_type(&pred.field, &pred.op, &pred.value)?;
    }
    if let Some(Quantifier::Count { op, .. }) = &wrapper.quantifier {
        match op {
            Op::Gt | Op::Lt | Op::Gte | Op::Lte | Op::Eq | Op::Neq => {}
            Op::Contains | Op::Regex => {
                return Err(ParseSpecError::OpTypeMismatch {
                    field: "quantifier.count.op".to_string(),
                    message: "Count op must be numeric (Gt/Lt/Gte/Lte/Eq/Neq)".to_string(),
                });
            }
        }
    }

    Ok(PropertySpec {
        name: wrapper.name,
        predicates: wrapper.predicates,
        select: wrapper.select,
        quantifier: wrapper.quantifier.unwrap_or_default(),
        version: wrapper.version,
    })
}

/// Validate that a field path follows a known pattern.
///
/// # Inputs
/// `field`: candidate dotted path borrowed from a parsed predicate.
///
/// # Outputs
/// `Ok(())` when the path is in the union vocabulary; otherwise
/// [`ParseSpecError::InvalidFieldPath`].
///
/// Supported union vocabulary: `"event_type"`, `"job_id"`, `"seq"`,
/// `"capability"`, `"error"`, `"output.<key>"` (non-empty key),
/// `"watermark"`, `safety.*`
/// (`safety.semantic_policy|dal|llmosafe_status|runtimo_disposition|
/// input_class|analysis_kind|provenance_consistent|no_evidence|
/// stages_executed|oov_ratio|detection_flags|body_pressure|schema_version|field_id`),
/// Runtime Facts v1 top-level fields (`family|run_id|provider|fidelity|
/// observations|first_seen|last_seen|revision`), `process.*`
/// (`process.pid|process.process_start_time`), and `extra.<key>`
/// (non-empty key).
///
/// # Errors
/// [`ParseSpecError::InvalidFieldPath`] naming the rejected path and the
/// supported patterns (including on empty `output.`/`extra.` keys).
///
/// # Invariants
/// - Union-scoped: acceptance does not imply the field resolves on every
///   source; per-source evaluators report mismatches as `Verdict::Error`.
/// - Storage alias: spec spelling `process.pid` (and
///   `process.process_start_time`) resolves the on-disk RuntimeFact
///   `process_key.pid` (resp. `process_key.process_start_time`); the
///   `process_key.*` spelling is storage layout, not spec vocabulary.
fn validate_field_path(field: &str) -> Result<(), ParseSpecError> {
    // Exact matches: WAL fields, Safety fields, and Runtime Facts top-level fields
    match field {
        "event_type"
        | "job_id"
        | "seq"
        | "capability"
        | "error"
        | "watermark"
        | "safety.semantic_policy"
        | "safety.dal"
        | "safety.llmosafe_status"
        | "safety.runtimo_disposition"
        | "safety.input_class"
        | "safety.analysis_kind"
        | "safety.provenance_consistent"
        | "safety.no_evidence"
        | "safety.stages_executed"
        | "safety.oov_ratio"
        | "safety.detection_flags"
        | "safety.body_pressure"
        | "safety.schema_version"
        | "safety.field_id"
        // Runtime Facts v1 vocabulary (matches `runtimo oracle schema`)
        | "family"
        | "run_id"
        | "provider"
        | "fidelity"
        | "observations"
        | "first_seen"
        | "last_seen"
        | "revision"
        | "process.pid"
        | "process.process_start_time" => return Ok(()),
        _ => {}
    }

    // Check output.<key> pattern (WAL / Observe)
    if let Some(key) = field.strip_prefix("output.") {
        if !key.is_empty() {
            return Ok(());
        }
    }

    // Check extra.<key> pattern (Runtime Facts)
    if let Some(key) = field.strip_prefix("extra.") {
        if !key.is_empty() {
            return Ok(());
        }
    }

    Err(ParseSpecError::InvalidFieldPath {
        field: field.to_string(),
        message: format!(
            "Field path '{field}' is not a recognized path. \
             Supported: event_type, job_id, seq, capability, error, output.<key>, watermark, safety.*, family, extra.<key>, process.*"
        ),
    })
}

/// Validate that the operator is compatible with the value's type.
///
/// Numeric ops (Gt, Lt, Gte, Lte) require numeric values.
/// Contains and Regex require string-compatible values.
fn validate_op_type(field: &str, op: &Op, value: &Value) -> Result<(), ParseSpecError> {
    match op {
        Op::Gt | Op::Lt | Op::Gte | Op::Lte => {
            // Numeric ops require numeric values
            if !value.is_number() {
                return Err(ParseSpecError::OpTypeMismatch {
                    field: field.to_string(),
                    message: format!("Operator {:?} requires a numeric value, got {}", op, value),
                });
            }
        }
        Op::Contains | Op::Regex => {
            // Contains/Regex require string-compatible values
            if !value.is_string() && !value.is_null() {
                // Allow arrays/objects for Contains (check if they contain)
                // But for type safety, we accept any JSON value
                // The actual type coercion happens at eval time
            }
        }
        Op::Eq | Op::Neq => {
            // Eq/Neq work with any type
        }
    }
    Ok(())
}

/// Extract a field value from a [`WalEvent`] by field path.
///
/// # Inputs
/// `event`: borrowed WAL event to read; `field`: dotted path from the
/// union vocabulary.
///
/// # Returns
/// - `Some(Value)` if the field exists and can be extracted
/// - `None` if the field path is unknown on WAL records or the field
///   is absent on this event (callers map `None` in predicate position
///   to `Verdict::Error`)
///
/// # Field Resolution
///
/// - `"event_type"` → the event type as a string (via [`crate::wal::WalEventType::as_str`])
/// - `"job_id"` → the job ID string
/// - `"seq"` → the sequence number as a number
/// - `"capability"` → capability name (absent → `None`, not empty string)
/// - `"error"` → error string (absent → `None`)
/// - `"output.<key>"` → the nested key from the output JSON value
/// - `"watermark"` → always `None` (bundle metadata, not a record field;
///   eval returns Error — §50 conflation fix)
/// - `"safety.*"` → typed safety record fields when `event.safety` is
///   present; `None` when the event has no safety record (selector
///   filtering removes these before predicate eval — §47)
/// - `"bundle_hash"` → **never accessed** (integrity invariant)
///
/// # Source scoping
/// WAL-only resolver: Runtime Facts fields (`family`, `extra.<key>`,
/// `process.*`, …) always yield `None` here even though
/// [`parse_spec`] accepts them (union validation). Facts records use the
/// separate CLI `fact_field` resolver over `RuntimeFactV1`, which
/// likewise yields `None` for WAL-only fields. Source-mismatched
/// predicate fields therefore surface as `Verdict::Error`, never silent
/// `Satisfied`.
#[must_use]
pub fn extract_field(event: &WalEvent, field: &str) -> Option<Value> {
    match field {
        "event_type" => Some(Value::String(event.event_type.as_str().to_string())),
        "job_id" => Some(Value::String(event.job_id.clone())),
        "seq" => Some(Value::from(event.seq)),
        "capability" => event.capability.clone().map(Value::String),
        "error" => event.error.clone().map(Value::String),
        "watermark" => None,
        "safety.semantic_policy" => event
            .safety
            .as_ref()
            .map(|s| Value::String(s.semantic_policy.clone())),
        "safety.dal" => event.safety.as_ref().map(|s| Value::String(s.dal.clone())),
        "safety.llmosafe_status" => event
            .safety
            .as_ref()
            .map(|s| Value::String(s.llmosafe_status.clone())),
        "safety.runtimo_disposition" => event
            .safety
            .as_ref()
            .map(|s| Value::String(s.runtimo_disposition.as_str().to_string())),
        "safety.input_class" => event
            .safety
            .as_ref()
            .map(|s| Value::String(s.input_class.as_str().to_string())),
        "safety.analysis_kind" => event
            .safety
            .as_ref()
            .map(|s| Value::String(s.analysis_kind.as_str().to_string())),
        "safety.provenance_consistent" => event
            .safety
            .as_ref()
            .and_then(|s| s.provenance_consistent.map(Value::Bool)),
        "safety.no_evidence" => event
            .safety
            .as_ref()
            .and_then(|s| s.no_evidence.map(Value::Bool)),
        "safety.stages_executed" => event
            .safety
            .as_ref()
            .map(|s| Value::from(s.stages_executed)),
        "safety.oov_ratio" => event
            .safety
            .as_ref()
            .and_then(|s| s.oov_ratio.map(Value::from)),
        "safety.detection_flags" => event
            .safety
            .as_ref()
            .and_then(|s| s.detection_flags.map(Value::from)),
        "safety.body_pressure" => event
            .safety
            .as_ref()
            .and_then(|s| s.body_pressure.map(Value::from)),
        "safety.schema_version" => event.safety.as_ref().map(|s| Value::from(s.schema_version)),
        "safety.field_id" => event
            .safety
            .as_ref()
            .map(|s| Value::String(s.field_id.clone())),
        _ if field.starts_with("output.") => {
            let path = &field["output.".len()..];
            event.output.as_ref().and_then(|o| {
                let mut current = o;
                for part in path.split('.') {
                    match current.get(part) {
                        Some(v) => current = v,
                        None => return None,
                    }
                }
                Some(current.clone())
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::WalEventType;
    use serde_json::json;

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
    fn test_parse_valid_spec() {
        let input = r#"{"name":"test-prop","predicates":[{"field":"event_type","op":"Eq","value":"job_started"}]}"#;
        let spec = parse_spec(input).unwrap();
        assert_eq!(spec.predicates.len(), 1);
        assert_eq!(spec.predicates[0].field, "event_type");
        assert_eq!(spec.predicates[0].op, Op::Eq);
        assert_eq!(spec.predicates[0].value, json!("job_started"));
        assert_eq!(spec.name, "test-prop");
    }

    #[test]
    fn test_parse_empty_predicates() {
        let input = r#"{"name":"empty-prop","predicates":[]}"#;
        let spec = parse_spec(input).unwrap();
        assert!(spec.predicates.is_empty());
    }

    #[test]
    fn test_parse_invalid_field_path() {
        let input =
            r#"{"name":"test","predicates":[{"field":"unknown_field","op":"Eq","value":"test"}]}"#;
        let result = parse_spec(input);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_output_field_path() {
        let input = r#"{"name":"test","predicates":[{"field":"output.data.path","op":"Eq","value":"/tmp/test.txt"}]}"#;
        let spec = parse_spec(input).unwrap();
        assert_eq!(spec.predicates[0].field, "output.data.path");
    }

    #[test]
    fn test_parse_invalid_op_name() {
        let input =
            r#"{"name":"test","predicates":[{"field":"event_type","op":"Bogus","value":"test"}]}"#;
        let result = parse_spec(input);
        assert!(result.is_err());
    }

    #[test]
    fn test_op_parse() {
        assert!(matches!(Op::parse("Eq"), Some(Op::Eq)));
        assert!(matches!(Op::parse("Regex"), Some(Op::Regex)));
        assert!(Op::parse("Bogus").is_none());
    }

    #[test]
    fn test_extract_event_type() {
        let event = sample_event();
        let val = extract_field(&event, "event_type").unwrap();
        assert_eq!(val, Value::String("job_started".to_string()));
    }

    #[test]
    fn test_extract_job_id() {
        let event = sample_event();
        let val = extract_field(&event, "job_id").unwrap();
        assert_eq!(val, Value::String("test-job-42".to_string()));
    }

    #[test]
    fn test_extract_seq() {
        let event = sample_event();
        let val = extract_field(&event, "seq").unwrap();
        assert_eq!(val, Value::from(1u64));
    }

    #[test]
    fn test_extract_output_nested() {
        let event = sample_event();
        let val = extract_field(&event, "output.data.path").unwrap();
        assert_eq!(val, Value::String("/tmp/test.txt".to_string()));
    }

    #[test]
    fn test_extract_watermark_returns_none() {
        let event = sample_event();
        let val = extract_field(&event, "watermark");
        assert!(val.is_none());
    }

    #[test]
    fn test_extract_unknown_field_returns_none() {
        let event = sample_event();
        let val = extract_field(&event, "nonexistent");
        assert!(val.is_none());
    }

    #[test]
    fn test_validate_field_path_exact_matches() {
        assert!(validate_field_path("event_type").is_ok());
        assert!(validate_field_path("job_id").is_ok());
        assert!(validate_field_path("seq").is_ok());
        assert!(validate_field_path("watermark").is_ok());
    }

    #[test]
    fn test_validate_field_path_output_prefix() {
        assert!(validate_field_path("output.data").is_ok());
        assert!(validate_field_path("output.").is_err());
    }

    #[test]
    fn test_validate_field_path_invalid() {
        assert!(validate_field_path("unknown").is_err());
    }

    #[test]
    fn test_shorthand_token_boundaries() {
        // 0-token and 1-token inputs are rejected (missing field/op/value).
        for raw in ["", "family"] {
            let input = format!(
                "{{\"name\":\"t\",\"predicates\":[{}]}}",
                serde_json::to_string(&raw).unwrap()
            );
            assert!(parse_spec(&input).is_err(), "raw {raw:?} must be rejected");
        }
        // 2-token input (missing value) is rejected — never coerced to Eq "".
        let two = r#"{"name":"t","predicates":["family Eq"]}"#;
        let err = parse_spec(two).unwrap_err();
        assert!(
            err.to_string().contains("missing value"),
            "2-token shorthand must name the missing value, got: {err}"
        );
        // 3-token shorthand parses; value keeps JSON-vs-string coercion.
        let three = r#"{"name":"t","predicates":["family == f1"]}"#;
        let spec = parse_spec(three).unwrap();
        assert_eq!(spec.predicates[0].field, "family");
        assert_eq!(spec.predicates[0].op, Op::Eq);
        assert_eq!(spec.predicates[0].value, json!("f1"));
        // Multi-word string values rejoin across tokens.
        let multi = r#"{"name":"t","predicates":["family == job started"]}"#;
        let spec = parse_spec(multi).unwrap();
        assert_eq!(spec.predicates[0].value, json!("job started"));
    }

    #[test]
    fn test_shorthand_value_coercion() {
        // "0" coerces to Number; non-JSON text stays String (E1).
        let num = r#"{"name":"t","predicates":["extra.drop_count == 0"]}"#;
        let spec = parse_spec(num).unwrap();
        assert_eq!(spec.predicates[0].value, json!(0));
        let text = r#"{"name":"t","predicates":["family == job started"]}"#;
        let spec = parse_spec(text).unwrap();
        assert_eq!(spec.predicates[0].value, json!("job started"));
    }

    #[test]
    fn test_extra_key_admission_and_empty_reject() {
        // extra.<key> admitted with non-empty key; empty key rejected (E2).
        assert!(validate_field_path("extra.drop_count").is_ok());
        assert!(validate_field_path("extra.").is_err());
        assert!(validate_field_path("output.").is_err());
        let ok = r#"{"name":"t","predicates":[{"field":"extra.typo_xyz","op":"Eq","value":0}]}"#;
        assert!(parse_spec(ok).is_ok());
        let empty = r#"{"name":"t","predicates":[{"field":"extra.","op":"Eq","value":0}]}"#;
        assert!(parse_spec(empty).is_err());
    }

    #[test]
    fn test_union_validation_per_source_accept() {
        // Union validator admits both WAL-only and facts-only fields;
        // per-source resolvers scope them at eval time (RC-A).
        for field in [
            "event_type",
            "output.data.path",
            "safety.dal",
            "family",
            "extra.drop_count",
            "process.pid",
            "process.process_start_time",
        ] {
            let input = format!(
                r#"{{"name":"t","predicates":[{{"field":"{field}","op":"Eq","value":"x"}}]}}"#
            );
            assert!(parse_spec(&input).is_ok(), "union must admit {field}");
        }
        // WAL resolver stays WAL-scoped: facts-only fields yield None here
        // (callers map predicate-position None to Verdict::Error).
        let event = sample_event();
        assert!(extract_field(&event, "family").is_none());
        assert!(extract_field(&event, "extra.drop_count").is_none());
        assert!(extract_field(&event, "process.pid").is_none());
    }

    #[test]
    fn test_op_canonical_authority() {
        // Op::parse is the single canonical authority: struct-form serde
        // accepts canonical spellings and rejects shorthand-only aliases.
        for spelling in ["Eq", "Neq", "Gt", "Lt", "Gte", "Lte", "Contains", "Regex"] {
            assert!(Op::parse(spelling).is_some());
        }
        assert!(Op::parse("==").is_none());
        assert!(Op::parse("contains").is_none());
        // Shorthand layer accepts symbols + lowercase via its own fallback.
        let sym = r#"{"name":"t","predicates":["extra.drop_count == 0"]}"#;
        assert_eq!(parse_spec(sym).unwrap().predicates[0].op, Op::Eq);
        let lower = r#"{"name":"t","predicates":[{"field":"family","op":"Contains","value":"f"}]}"#;
        assert!(parse_spec(lower).is_ok());
    }
}
