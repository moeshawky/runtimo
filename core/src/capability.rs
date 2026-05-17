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
pub struct Context {
    /// If true, the capability should not perform side effects.
    pub dry_run: bool,
    /// The job ID that owns this execution.
    pub job_id: String,
    /// Working directory for relative path resolution.
    pub working_dir: PathBuf,
}

/// Output from capability execution.
///
/// Returned by every [`Capability::execute`] call. The `data` field holds
/// structured JSON output, while `message` provides a human-readable summary.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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

    /// Looks up a capability by name.
    ///
    /// Returns `None` if no capability with the given name is registered.
    pub fn get(&self, name: &str) -> Option<&dyn Capability> {
        self.capabilities.get(name).map(|c| c.as_ref())
    }

    /// Returns the names of all registered capabilities.
    pub fn list(&self) -> Vec<&str> {
        self.capabilities.keys().map(|s| s.as_str()).collect()
    }
}

impl Default for CapabilityRegistry {
    fn default() -> Self {
        Self::new()
    }
}
