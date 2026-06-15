//! Capability trait and registry.
//!
//! The [`Capability`] trait is the core abstraction — every pluggable operation
//! (file read, file write, shell exec, etc.) implements this trait. The
//! [`CapabilityRegistry`] collects and dispatches to registered capabilities.

use crate::Result;
use serde_json::Value;
use std::path::PathBuf;

/// Context provided to capabilities during execution.
///
/// Carries execution metadata such as dry-run mode, the owning job ID,
/// and the working directory for relative path resolution.
///
/// Use [`Context::new`] to create instances — this ensures consistent
/// field initialization across CLI, executor, and daemon code paths.
#[allow(clippy::exhaustive_structs)] // fields are write-through API contract
pub struct Context {
    /// If true, the capability should not perform side effects.
    pub dry_run: bool,
    /// The job ID that owns this execution.
    pub job_id: String,
    /// Working directory for relative path resolution.
    pub working_dir: PathBuf,
}

impl Context {
    /// Creates a new execution context.
    ///
    /// Uses `std::env::current_dir()` as the default working directory.
    /// The caller should override `working_dir` if an explicit directory
    /// is known (e.g., from daemon dispatch parameters).
    #[must_use]
    pub fn new(dry_run: bool, job_id: String) -> Self {
        Self {
            dry_run,
            job_id,
            working_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
        }
    }

    /// Creates a new execution context with an explicit working directory.
    #[must_use]
    pub fn with_working_dir(dry_run: bool, job_id: String, working_dir: PathBuf) -> Self {
        Self {
            dry_run,
            job_id,
            working_dir,
        }
    }
}

/// Output from capability execution.
///
/// Returned by every [`Capability::execute`] call. The `data` field holds
/// structured JSON output, while `message` provides a human-readable summary.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[allow(clippy::exhaustive_structs)] // fields are write-through API contract
pub struct Output {
    /// Whether the capability completed successfully.
    pub success: bool,
    /// Structured output data (JSON).
    pub data: Value,
    /// Human-readable status or error message.
    pub message: Option<String>,
}

/// The Capability trait — all capabilities must implement this.
///
/// Each capability defines its name, argument schema, validation logic,
/// and execution behavior. The executor calls these methods in order:
/// `name()` → `schema()` → `validate()` → `execute()`.
///
/// # Example
///
/// ```rust
/// use runtimo_core::capability::{Capability, Context, Output};
/// use runtimo_core::Result;
/// use serde_json::Value;
///
/// struct Echo;
///
/// impl Capability for Echo {
///     fn name(&self) -> &'static str { "Echo" }
///     fn description(&self) -> &'static str { "Echo back arguments" }
///     fn schema(&self) -> Value { serde_json::json!({"type":"object"}) }
///     fn validate(&self, _args: &Value) -> Result<()> { Ok(()) }
///     fn execute(&self, args: &Value, _ctx: &Context) -> Result<Output> {
///         Ok(Output { success: true, data: args.clone(), message: None })
///     }
/// }
/// ```
pub trait Capability: Send + Sync {
    /// Returns the capability name (e.g., `"FileRead"`, `"FileWrite"`).
    ///
    /// This name is used for registry lookups and WAL event tagging.
    fn name(&self) -> &'static str;

    /// Returns a one-line human-readable description of what this capability does.
    ///
    /// Used by the CLI `list` command and `--help` output to help users
    /// discover available capabilities.
    fn description(&self) -> &'static str;

    /// Returns the JSON Schema for the capability's arguments.
    ///
    /// The schema is used by [`Capability::validate`] and by the CLI
    /// to generate `--help` output for each capability.
    fn schema(&self) -> Value;

    /// Validates the arguments against the schema.
    ///
    /// Implementations should deserialize `args` into their typed args struct
    /// and perform semantic checks (e.g., path traversal rejection).
    ///
    /// # Errors
    ///
    /// Returns [`Error::SchemaValidationFailed`](crate::Error::SchemaValidationFailed)
    /// if arguments are malformed or semantically invalid.
    fn validate(&self, args: &Value) -> Result<()>;

    /// Executes the capability with the given arguments and context.
    ///
    /// This is called after `validate()` passes. Implementations should
    /// perform the actual work and return an [`Output`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::ExecutionFailed`](crate::Error::ExecutionFailed)
    /// if the operation cannot be completed.
    fn execute(&self, args: &Value, ctx: &Context) -> Result<Output>;
}

/// Registry of available capabilities.
///
/// Stores capabilities by name and provides lookup, listing, and registration.
///
/// # Example
///
/// ```rust,ignore
/// use runtimo_core::{CapabilityRegistry, FileRead, FileWrite};
/// use std::path::PathBuf;
///
/// let mut registry = CapabilityRegistry::new();
/// registry.register(FileRead);
/// registry.register(FileWrite::new(PathBuf::from("/tmp/backups")).unwrap());
///
/// assert!(registry.get("FileRead").is_some());
/// let caps = registry.list();
/// assert_eq!(caps.len(), 2);
/// assert!(caps.contains(&"FileRead"));
/// assert!(caps.contains(&"FileWrite"));
/// ```
pub struct CapabilityRegistry {
    capabilities: std::collections::HashMap<String, Box<dyn Capability>>,
}

impl CapabilityRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            capabilities: std::collections::HashMap::new(),
        }
    }

    /// Registers a capability in the registry.
    ///
    /// The capability is stored under its [`Capability::name`]. If a capability
    /// with the same name already exists, it is replaced.
    pub fn register<C: Capability + 'static>(&mut self, capability: C) {
        let name = capability.name().to_string();
        self.capabilities.insert(name, Box::new(capability));
    }

    /// Looks up a capability by name (case-insensitive).
    ///
    /// Returns `None` if no capability with the given name is registered.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&dyn Capability> {
        if let Some(cap) = self.capabilities.get(name) {
            return Some(cap.as_ref());
        }
        let name_lower = name.to_lowercase();
        for (key, cap) in &self.capabilities {
            if key.to_lowercase() == name_lower {
                return Some(cap.as_ref());
            }
        }
        None
    }

    /// Returns the names of all registered capabilities.
    #[must_use]
    pub fn list(&self) -> Vec<&str> {
        self.capabilities.keys().map(|s| s.as_str()).collect()
    }
}

impl Default for CapabilityRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// A minimal test capability that echoes its name.
    struct TestCap {
        name: &'static str,
    }

    impl Capability for TestCap {
        fn name(&self) -> &'static str {
            self.name
        }
        fn description(&self) -> &'static str {
            "test capability"
        }
        fn schema(&self) -> Value {
            serde_json::json!({})
        }
        fn validate(&self, _args: &Value) -> crate::Result<()> {
            Ok(())
        }
        fn execute(&self, _args: &Value, _ctx: &Context) -> crate::Result<Output> {
            Ok(Output {
                success: true,
                data: serde_json::json!({}),
                message: None,
            })
        }
    }

    #[test]
    fn test_registry_register_and_get() {
        let mut reg = CapabilityRegistry::new();
        reg.register(TestCap { name: "Alpha" });

        let cap = reg.get("Alpha");
        assert!(cap.is_some(), "Should find registered capability");
        assert_eq!(cap.unwrap().name(), "Alpha");
    }

    #[test]
    fn test_registry_duplicate_name_replaces() {
        let mut reg = CapabilityRegistry::new();
        reg.register(TestCap { name: "Beta" });
        reg.register(TestCap { name: "Beta" }); // second registration replaces

        let cap = reg.get("Beta");
        assert!(
            cap.is_some(),
            "Should still find capability after duplicate registration"
        );
        assert_eq!(cap.unwrap().name(), "Beta");
    }

    #[test]
    fn test_registry_case_insensitive_lookup() {
        let mut reg = CapabilityRegistry::new();
        reg.register(TestCap { name: "ShellExec" });

        // Exact match
        assert!(reg.get("ShellExec").is_some());
        // Case-insensitive match
        assert!(
            reg.get("shellexec").is_some(),
            "Case-insensitive lookup should find ShellExec"
        );
        assert!(
            reg.get("SHELLEXEC").is_some(),
            "Uppercase lookup should find ShellExec"
        );
        assert!(
            reg.get("ShellExec").is_some(),
            "Exact-case lookup should find ShellExec"
        );
    }

    #[test]
    fn test_registry_unregistered_lookup_returns_none() {
        let mut reg = CapabilityRegistry::new();
        reg.register(TestCap { name: "Delta" });

        assert!(reg.get("NoSuchCap").is_none());
        assert!(reg.get("").is_none());
        // Unregistered even with case-insensitive match
        assert!(reg.get("gamma").is_none());
    }

    #[test]
    fn test_registry_list() {
        let mut reg = CapabilityRegistry::new();
        assert!(reg.list().is_empty());

        reg.register(TestCap { name: "A" });
        reg.register(TestCap { name: "B" });
        reg.register(TestCap { name: "C" });

        let list = reg.list();
        assert_eq!(list.len(), 3);
        assert!(list.contains(&"A"));
        assert!(list.contains(&"B"));
        assert!(list.contains(&"C"));
    }
}
