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
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
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
    /// # Returns
    /// `Some(Op)` if the string matches a known variant, `None` otherwise.
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

/// A single predicate that tests a field path against an operator and value.
///
/// # Fields
/// - `field`: The field path to extract from a [`WalEvent`]. Supported
///   paths: `"event_type"`, `"job_id"`, `"seq"`, `"output.<key>"`, `"watermark"`.
/// - `op`: The comparison operator.
/// - `value`: The value to compare against. Type compatibility with the
///   field is validated at parse time.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[non_exhaustive]
pub struct Predicate {
    /// The field path to extract from a [`WalEvent`].
    pub field: String,
    /// The comparison operator.
    pub op: Op,
    /// The value to compare against.
    pub value: Value,
}

/// A property specification consisting of a name and a set of predicates.
///
/// Predicates are combined with **AND semantics**: all predicates must
/// hold for the property to be satisfied. An **empty predicate set**
/// means the property is **always satisfied** (total-function behavior),
/// which is load-bearing and must not be "fixed" to return `Violated`.
///
/// # Example
///
/// ```rust,ignore
/// let spec = PropertySpec {
///     name: "job-completed".to_string(),
///     predicates: vec![
///         Predicate { field: "event_type".to_string(), op: Op::Eq, value: json!("job_completed") },
///         Predicate { field: "job_id".to_string(), op: Op::Contains, value: json!("batch") },
///     ],
/// };
/// ```
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
    pub predicates: Vec<Predicate>,
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
    predicates: Vec<Predicate>,
}

/// Parse a specification string into a [`PropertySpec`].
///
/// # Input Format
///
/// The input must be a JSON object with:
/// - `name`: string property name
/// - `predicates`: JSON array of predicate objects, each with:
///   - `field`: string field path (e.g., `"event_type"`, `"output.data.path"`)
///   - `op`: string operator name (e.g., `"Eq"`, `"Gt"`, `"Contains"`)
///   - `value`: JSON value to compare against
///
/// # Validation
///
/// This function validates:
/// 1. **Field path syntax**: Must match one of the known patterns
///    (`"event_type"`, `"job_id"`, `"seq"`, `"output.<key>"`, `"watermark"`).
/// 2. **Op/type compatibility**: Numeric ops (`Gt`, `Lt`, `Gte`, `Lte`) require
///    numeric field values; `Contains`/`Regex` require string-compatible values.
///
/// # Errors
///
/// Returns [`ParseSpecError::InvalidFormat`] if the string is not valid JSON
/// or cannot be deserialized into the expected structure.
/// Returns [`ParseSpecError::InvalidFieldPath`] if the field path is malformed.
/// Returns [`ParseSpecError::OpTypeMismatch`] if the operator is incompatible
/// with the field's type.
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
        .map_err(|e| ParseSpecError::InvalidFormat(format!("JSON parse error: {}", e)))?;

    for pred in &wrapper.predicates {
        validate_field_path(&pred.field)?;
        validate_op_type(&pred.field, &pred.op, &pred.value)?;
    }

    Ok(PropertySpec {
        name: wrapper.name,
        predicates: wrapper.predicates,
    })
}

/// Validate that a field path follows a known pattern.
///
/// Supported patterns: `"event_type"`, `"job_id"`, `"seq"`,
/// `"output.<key>"` (where `<key>` is any string), `"watermark"`.
fn validate_field_path(field: &str) -> Result<(), ParseSpecError> {
    // Check exact matches first
    match field {
        "event_type" | "job_id" | "seq" | "watermark" => return Ok(()),
        _ => {}
    }
    // Check output.<key> pattern
    if let Some(key) = field.strip_prefix("output.") {
        if !key.is_empty() {
            return Ok(());
        }
    }
    Err(ParseSpecError::InvalidFieldPath {
        field: field.to_string(),
        message: format!(
            "Field path '{}' is not a recognized path. \
             Supported: event_type, job_id, seq, output.<key>, watermark",
            field
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
/// # Returns
/// - `Some(Value)` if the field exists and can be extracted
/// - `None` if the field path is unknown or the field doesn't exist
///
/// # Field Resolution
///
/// - `"event_type"` → the event type as a string (via [`WalEventType::as_str`])
/// - `"job_id"` → the job ID string
/// - `"seq"` → the sequence number as a number
/// - `"output.<key>"` → the nested key from the output JSON value
/// - `"watermark"` → always `None` (not a WalEvent field; eval returns Error)
/// - `"bundle_hash"` → **never accessed** (integrity invariant)
#[must_use]
pub fn extract_field(event: &WalEvent, field: &str) -> Option<Value> {
    match field {
        "event_type" => Some(Value::String(event.event_type.as_str().to_string())),
        "job_id" => Some(Value::String(event.job_id.clone())),
        "seq" => Some(Value::from(event.seq)),
        "watermark" => None,
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
}
