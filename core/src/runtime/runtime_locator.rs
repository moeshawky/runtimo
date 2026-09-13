//! Runtime locator — language/runtime-neutral frame locator.
//!
//! # ADR: Enum with Tagged Variants (vs Struct with Optional Fields)
//!
//! **Chosen**: Enum with tagged variants (`#[serde(tag = "kind")]`).
//!
//! **Rejected**: Struct with optional fields.
//!
//! **Why**: Each language family has distinctly different fields. An
//! enum with tagged variants:
//! - Provides exhaustive pattern matching — no invalid combinations.
//! - Serializes cleanly with a `kind` discriminator.
//! - Prevents mixing fields from different language families.
//! - Better forward-compat: adding a new variant doesn't break
//!   existing deserialization of other variants.
//!
//! A struct with optional fields would allow invalid combinations
//! (e.g., a `Python` locator with `class` and `method` fields from
//! JVM), which violates the invariant that every locator is
//! serializable and comparable.
//!
//! # Ownership
//! - **Writer**: Providers (Tetragon, JFR, OTel)
//! - **Reader**: WAL, CLI, Oracle
//! - **Serialization**: serde JSON (tagged representation)
//! - **Versioning**: semver
//!
//! # Invariants
//! - Every locator is serializable and comparable.
//! - No Codegraph internals inside.
//! - No raw provider output types leak into `RuntimeLocator`.
//! - No `SymbolUID` inside raw adapters.

use serde::{Deserialize, Serialize};

/// Language/runtime-neutral locator for upstream frames/events.
///
/// Represents stack frames and events from different language
/// runtimes without putting provider-specific internals (like
/// `SymbolUID`) inside adapters.
///
/// # Variants
/// - `Native` — Native stack frames (`{ binary, build_id, address/offset, symbol, file, line, column }`)
/// - `Python` — Python stack frames (`{ module, qualname, file, line }`)
/// - `Jvm` — JVM stack frames (`{ class, method, descriptor, file, line }`)
/// - `DotNet` — .NET stack frames (`{ assembly, type_name, method, file, line }`)
/// - `JsTs` — JS/TS stack frames (`{ generated, original, function }`)
///
/// # Invariants
/// - Every locator is serializable and comparable.
/// - No Codegraph internals inside.
/// - No raw provider output types leak into this enum.
/// - No `SymbolUID` inside raw adapters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(clippy::exhaustive_enums)] // new variants are semver-breaking
pub enum RuntimeLocator {
    /// Native stack frame.
    Native {
        /// Binary path.
        binary: String,
        /// Build ID.
        build_id: String,
        /// Address or offset.
        address_offset: String,
        /// Symbol name.
        symbol: String,
        /// Source file path.
        file: String,
        /// Line number.
        line: u32,
        /// Column number.
        column: u32,
    },
    /// Python stack frame.
    Python {
        /// Module name.
        module: String,
        /// Qualified name.
        qualname: String,
        /// Source file path.
        file: String,
        /// Line number.
        line: u32,
    },
    /// JVM stack frame.
    Jvm {
        /// Class name.
        class: String,
        /// Method name.
        method: String,
        /// Method descriptor.
        descriptor: String,
        /// Source file path.
        file: String,
        /// Line number.
        line: u32,
    },
    /// .NET stack frame.
    DotNet {
        /// Assembly name.
        assembly: String,
        /// Type name.
        type_name: String,
        /// Method name.
        method: String,
        /// Source file path.
        file: String,
        /// Line number.
        line: u32,
    },
    /// JS/TS stack frame with source map support.
    JsTs {
        /// Generated frame location.
        generated: GeneratedLocation,
        /// Original frame location (after source map).
        original: GeneratedLocation,
        /// Function name.
        function: String,
    },
}

/// A generated/original location pair (used by `JsTs`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)]
pub struct GeneratedLocation {
    /// File path.
    pub file: String,
    /// Line number.
    pub line: u32,
    /// Column number.
    pub column: u32,
}

impl RuntimeLocator {
    /// Returns the file path of this locator, if available.
    #[must_use]
    pub fn file(&self) -> Option<&str> {
        match self {
            Self::Native { file, .. }
            | Self::Python { file, .. }
            | Self::Jvm { file, .. }
            | Self::DotNet { file, .. } => Some(file),
            Self::JsTs { generated, .. } => Some(&generated.file),
        }
    }

    /// Returns the line number of this locator, if available.
    #[must_use]
    pub fn line(&self) -> Option<u32> {
        match self {
            Self::Native { line, .. }
            | Self::Python { line, .. }
            | Self::Jvm { line, .. }
            | Self::DotNet { line, .. } => Some(*line),
            Self::JsTs { generated, .. } => Some(generated.line),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_locator_native_round_trip() {
        let locator = RuntimeLocator::Native {
            binary: "/usr/bin/rust".to_string(),
            build_id: "build-001".to_string(),
            address_offset: "0x1000".to_string(),
            symbol: "main".to_string(),
            file: "/src/main.rs".to_string(),
            line: 42,
            column: 1,
        };
        let json = serde_json::to_string(&locator).unwrap();
        let deserialized: RuntimeLocator = serde_json::from_str(&json).unwrap();
        assert_eq!(locator, deserialized);
    }

    #[test]
    fn runtime_locator_python_round_trip() {
        let locator = RuntimeLocator::Python {
            module: "runtimo".to_string(),
            qualname: "run".to_string(),
            file: "/src/runtimo.py".to_string(),
            line: 10,
        };
        let json = serde_json::to_string(&locator).unwrap();
        let deserialized: RuntimeLocator = serde_json::from_str(&json).unwrap();
        assert_eq!(locator, deserialized);
    }

    #[test]
    fn runtime_locator_js_ts_round_trip() {
        let locator = RuntimeLocator::JsTs {
            generated: GeneratedLocation {
                file: "/dist/bundle.js".to_string(),
                line: 1,
                column: 0,
            },
            original: GeneratedLocation {
                file: "/src/index.ts".to_string(),
                line: 5,
                column: 2,
            },
            function: "main".to_string(),
        };
        let json = serde_json::to_string(&locator).unwrap();
        let deserialized: RuntimeLocator = serde_json::from_str(&json).unwrap();
        assert_eq!(locator, deserialized);
    }

    #[test]
    fn runtime_locator_unknown_field_ignored() {
        // Forward-compat: unknown fields are ignored.
        let json = r#"{"kind":"native","binary":"/usr/bin/rust","build_id":"build-001","address_offset":"0x1000","symbol":"main","file":"/src/main.rs","line":42,"column":1,"unknown":"ignored"}"#;
        let locator: RuntimeLocator = serde_json::from_str(json).unwrap();
        assert!(matches!(locator, RuntimeLocator::Native { .. }));
    }

    #[test]
    fn runtime_locator_file_and_line() {
        let locator = RuntimeLocator::Python {
            module: "runtimo".to_string(),
            qualname: "run".to_string(),
            file: "/src/runtimo.py".to_string(),
            line: 10,
        };
        assert_eq!(locator.file(), Some("/src/runtimo.py"));
        assert_eq!(locator.line(), Some(10));
    }
}
