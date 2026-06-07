//! JSON Schema validation for capability arguments.
//!
//! Provides basic type-checking validation against a JSON Schema object.
//! Currently supports simple `"type"` field matching. For full JSON Schema
//! validation, integrate the `jsonschema` crate.
//!
//! # Example
//!
//! ```rust,ignore
//! use runtimo_core::SchemaValidator;
//! use serde_json::json;
//!
//! let schema = json!({"type": "object"});
//! let args = json!({"path": "/tmp/test.txt"});
//!
//! assert!(SchemaValidator::validate(&args, &schema).is_ok());
//! ```

use crate::Result;
use serde_json::Value;

/// Validates JSON values against a simplified JSON Schema.
///
/// Currently performs basic type checking. Future versions may use the
/// `jsonschema` crate for full draft-07 validation.
#[allow(dead_code, clippy::exhaustive_structs)]
pub struct SchemaValidator {
    // Could use jsonschema crate for full JSON Schema validation
    // Reserved for future use in capability validation pipeline (S-DEAD-1)
}

#[allow(dead_code)]
impl SchemaValidator {
    /// Creates a new (stateless) schema validator.
    #[must_use]
    pub fn new() -> Self {
        Self {}
    }

    /// Validates arguments against a JSON Schema.
    ///
    /// # Arguments
    ///
    /// * `args` — The JSON value to validate
    /// * `schema` — A JSON Schema object (currently only `"type"` is checked)
    ///
    /// # Returns
    ///
    /// `Ok(())` if the value matches the schema.
    ///
    /// # Errors
    ///
    /// Returns [`Error::SchemaValidationFailed`](crate::Error::SchemaValidationFailed)
    /// if the value's type does not match the schema's `"type"` field.
    pub fn validate(args: &Value, schema: &Value) -> Result<()> {
        if let Some(expected_type) = schema.get("type").and_then(|t| t.as_str()) {
            let actual_type = if args.is_string() {
                "string"
            } else if args.is_number() {
                "number"
            } else if args.is_boolean() {
                "boolean"
            } else if args.is_array() {
                "array"
            } else if args.is_object() {
                "object"
            } else {
                "null"
            };

            if expected_type != actual_type {
                return Err(crate::Error::SchemaValidationFailed(format!(
                    "Expected type '{}', got '{}'",
                    expected_type, actual_type
                )));
            }
        }

        if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
            if let Some(obj) = args.as_object() {
                for key in required {
                    if let Some(key_str) = key.as_str() {
                        if !obj.contains_key(key_str) {
                            return Err(crate::Error::SchemaValidationFailed(format!(
                                "Missing required field: '{}'",
                                key_str
                            )));
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

impl Default for SchemaValidator {
    fn default() -> Self {
        Self::new()
    }
}
