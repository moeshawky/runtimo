//! Oracle module — property specification and evaluation.
//!
//! The oracle provides a contract-first framework for defining and
//! evaluating properties against WAL events. It supports typed predicates
//! with AND semantics, field-path resolution from [`WalEvent`] records,
//! and a clear error contract that distinguishes infra failures from
//! property violations.
//!
//! # Module DNA
//! - **Owns**: Property specification parsing, predicate evaluation, error types
//! - **Depends**: [`crate::wal::WalEvent`] for event data, `serde_json` for value handling, `thiserror` for error types
//! - **Provides**: `OracleError`, `PropertySpec`, `Predicate`, `Op`, `PropertyVerdict`, `Verdict`, `evaluate`, `parse_spec`
//!
//! # Invariants
//! - `bundle_hash` is never read by the oracle — integrity is not its concern.
//! - Empty predicate sets always evaluate to `Satisfied` (total-function behavior).
//! - Unknown field paths produce `Verdict::Error`, not `Err`.
//! - All public items have complete DNA docstrings per the code-annotation-protocol.
//! - Zero new dependencies — uses only existing crate dependencies.

use thiserror::Error;

pub mod benchmark;
pub mod eval;
pub mod spec;

pub use benchmark::{run_benchmarks, BenchmarkReport};
pub use eval::{evaluate, PropertyVerdict, Verdict};
pub use spec::{parse_spec, Op, Predicate, PropertySpec};

/// Errors that can occur in the oracle module.
///
/// # Variants
///
/// - `ParseError`: Failed to parse a property specification string.
/// - `EvalError`: Catastrophic internal error during evaluation
///   (only returned for true misuse, not for property violations).
/// - `BundleError`: Bundle-related error (reserved for future use).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OracleError {
    /// Failed to parse a property specification.
    ///
    /// Contains the field name (if known) and a human-readable message.
    #[error("Parse error on field '{field}': {message}")]
    ParseError {
        /// The field path that caused the parse error, if identifiable.
        field: String,
        /// Human-readable description of the parse failure.
        message: String,
    },
    /// Catastrophic error during evaluation.
    ///
    /// This variant is only returned for true internal misuse — not for
    /// property violations or unknown field paths (those return
    /// `Verdict::Error` via `Ok`).
    #[error("Evaluation error on property '{property}': {message}")]
    EvalError {
        /// The property name being evaluated.
        property: String,
        /// Human-readable description of the evaluation failure.
        message: String,
    },
    /// Bundle-related error.
    ///
    /// Reserved for future use. The oracle never accesses bundle data
    /// directly — this variant exists for API completeness.
    #[error("Bundle error at path '{path}': {message}")]
    BundleError {
        /// The bundle path associated with the error.
        path: String,
        /// Human-readable description of the bundle error.
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_oracle_error_parse_error() {
        let err = OracleError::ParseError {
            field: "event_type".to_string(),
            message: "invalid field".to_string(),
        };
        assert!(format!("{}", err).contains("Parse error"));
    }

    #[test]
    fn test_oracle_error_eval_error() {
        let err = OracleError::EvalError {
            property: "test-prop".to_string(),
            message: "internal error".to_string(),
        };
        assert!(format!("{}", err).contains("Evaluation error"));
    }

    #[test]
    fn test_oracle_error_bundle_error() {
        let err = OracleError::BundleError {
            path: "/tmp/bundle".to_string(),
            message: "not found".to_string(),
        };
        assert!(format!("{}", err).contains("Bundle error"));
    }
}
