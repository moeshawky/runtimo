//! Capability trait, typed capability trait, and registry.
//!
//! The [`Capability`] trait is the core abstraction — every pluggable operation
//! (file read, file write, shell exec, etc.) implements this trait. The
//! [`CapabilityRegistry`] collects and dispatches to registered capabilities.
//!
//! The [`TypedCapability`] trait provides compile-time type safety for capability
//! arguments. A blanket impl bridges [`TypedCapability`] to [`Capability`], so
//! the executor's `&dyn Capability` dynamic dispatch continues to work while
//! direct callers get typed args.

use crate::telemetry::Telemetry;
use crate::Result;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::path::PathBuf;

/// Error type for capability execution failures.
///
/// # Variants
/// - `InvalidArgs`: Deserialization or validation failed. Message describes
///   the specific field error from serde or manual validation.
/// - `PermissionDenied`: Path or operation rejected by security policy
///   (e.g., path traversal outside allowed prefix).
/// - `NotFound`: Target file or resource does not exist at the specified path.
/// - `Io`: Underlying I/O operation failed (read, write, create, delete).
/// - `Git`: Git command returned an error or produced invalid output.
/// - `Internal`: Unexpected internal failure that should not occur in normal
///   operation. Indicates a bug in the capability implementation.
///
/// # Invariants
/// - Every error carries a human-readable message suitable for logging.
/// - Callers can match on variant to decide retry/abort/skip behavior.
/// - `Io` wraps `std::io::Error` via `From` for ergonomic `?` propagation.
///
/// # Errors
///
/// This enum IS the error type — no separate error channel exists.
/// Capabilities return `Result<Output, CapabilityError>`.
#[derive(Debug, thiserror::Error)]
#[allow(clippy::exhaustive_enums)] // error enums are intentionally exhaustive
pub enum CapabilityError {
    /// Deserialization or field validation failed.
    ///
    /// Contains a description of the specific validation failure
    /// (e.g., "missing field `file_path`", "invalid type: string, expected PathBuf").
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),

    /// Path or operation rejected by security policy.
    ///
    /// Triggered when a path traversal check fails or an operation
    /// targets a location outside allowed prefixes.
    #[error("blocked: {0}")]
    PermissionDenied(String),

    /// Target file or resource does not exist.
    ///
    /// The message identifies the missing resource (typically its path).
    #[error("file not found: {0}")]
    NotFound(String),

    /// Underlying I/O operation failed.
    ///
    /// Wraps `std::io::Error` for ergonomic `?` propagation from
    /// filesystem operations (read, write, create, rename, delete).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Git command returned an error or produced invalid output.
    ///
    /// Contains the git error message or stderr output.
    #[error("git error: {0}")]
    Git(String),

    /// Unexpected internal failure.
    ///
    /// Should not occur in normal operation. Indicates a bug in the
    /// capability implementation or an invariant violation.
    #[error("internal error: {0}")]
    Internal(String),
}

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
/// Returned by every [`Capability::execute`] call. Provides a standardized
/// envelope across all capabilities — `status` distinguishes ok/error,
/// `output` carries human-readable text, `data` holds structured JSON,
/// and `error` contains the failure message when status is `"error"`.
///
/// # Invariants
///
/// - `status` is always `"ok"` or `"error"`.
/// - When `status == "error"`, `error` is `Some`.
/// - When `status == "ok"`, `error` is `None`.
/// - `data` is capability-specific structured output (may be `None`).
/// - `backup_path` is set when a file was backed up before mutation.
/// - `artifacts` lists paths of files created or modified by the capability.
///
/// # Constructors
///
/// Use [`Output::ok`] for successful executions and [`Output::error`] for
/// failures. Do not construct `Output` directly — the constructors enforce
/// the invariants above.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[allow(clippy::exhaustive_structs)] // fields are write-through API contract
pub struct Output {
    /// Execution status: `"ok"` or `"error"`.
    pub status: String,
    /// Human-readable result message.
    pub output: String,
    /// Capability-specific structured data (JSON). `None` when the
    /// capability produces no structured output.
    pub data: Option<Value>,
    /// Path to a backup file created before mutation. `None` when no
    /// backup was created (e.g., read-only operations, new files).
    pub backup_path: Option<PathBuf>,
    /// Error message when `status == "error"`. `None` when `status == "ok"`.
    pub error: Option<String>,
    /// Wall-clock execution duration in milliseconds.
    pub duration_ms: u64,
    /// Telemetry delta captured around the execution (before/after).
    pub telemetry_delta: Telemetry,
    /// Paths of files created or modified by this capability execution.
    pub artifacts: Vec<PathBuf>,
}

impl Output {
    /// Creates a successful output with the given human-readable message.
    ///
    /// Sets `status` to `"ok"`, `error` to `None`, and all other fields
    /// to sensible defaults. Callers should populate `data`, `backup_path`,
    /// and `artifacts` after construction as needed.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use runtimo_core::capability::Output;
    ///
    /// let out = Output::ok("Read 42 bytes".into());
    /// assert_eq!(out.status, "ok");
    /// assert!(out.error.is_none());
    /// ```
    #[must_use]
    pub fn ok(output: String) -> Self {
        Self {
            status: "ok".to_string(),
            output,
            data: None,
            backup_path: None,
            error: None,
            duration_ms: 0,
            telemetry_delta: Telemetry::capture_lightweight(),
            artifacts: Vec::new(),
        }
    }

    /// Creates an error output with the given human-readable message and
    /// error detail.
    ///
    /// Sets `status` to `"error"` and `error` to `Some(error)`. The
    /// `output` field carries a caller-facing summary; `error` carries
    /// the machine-parseable failure reason.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use runtimo_core::capability::Output;
    ///
    /// let out = Output::error("failed".into(), "file not found".into());
    /// assert_eq!(out.status, "error");
    /// assert_eq!(out.error.as_deref(), Some("file not found"));
    /// ```
    #[must_use]
    pub fn error(output: String, error: String) -> Self {
        Self {
            status: "error".to_string(),
            output,
            data: None,
            backup_path: None,
            error: Some(error),
            duration_ms: 0,
            telemetry_delta: Telemetry::capture_lightweight(),
            artifacts: Vec::new(),
        }
    }

    /// Serializes the output to a JSON [`Value`].
    ///
    /// All fields are included — this is the canonical JSON representation
    /// written to the WAL and returned to clients.
    #[must_use]
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
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
///         Ok(Output::ok(format!("echo: {}", args)))
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

/// Typed capability trait — compile-time safe arguments for capabilities.
///
/// [`TypedCapability`] provides type-safe argument handling via an associated
/// `Args` type. Each capability defines its own args struct (e.g.,
/// [`FileReadArgs`](crate::capabilities::FileReadArgs)) and
/// implements this trait.
///
/// # Blanket impl bridge
///
/// A blanket `impl<T: TypedCapability> Capability for T` bridges this trait
/// to the untyped [`Capability`] trait. This means:
///
/// - The executor's `&dyn Capability` dynamic dispatch **still works** —
///   it calls the blanket impl which deserializes `Value` into `Self::Args`.
/// - Direct callers can use `TypedCapability` methods with compile-time
///   type safety — no `Value` deserialization needed.
///
/// Both paths coexist. The blanket impl is the bridge.
///
/// # Examples
///
/// ```rust,ignore
/// use runtimo_core::capability::{TypedCapability, Context, Output, CapabilityError};
/// use runtimo_core::capabilities::file_read::FileReadArgs;
/// use runtimo_core::capabilities::FileRead;
///
/// let cap = FileRead;
/// let args = FileReadArgs { path: "/etc/hostname".into(), max_bytes: None };
/// // Direct typed call — no Value involved:
/// // let output = cap.execute(args, &ctx)?;
/// ```
pub trait TypedCapability: Send + Sync {
    /// The typed arguments struct for this capability.
    ///
    /// Must implement `DeserializeOwned` so the blanket impl can convert
    /// from `serde_json::Value`.
    type Args: DeserializeOwned + Send + Sync;

    /// Returns the capability name (e.g., `"FileRead"`).
    fn name(&self) -> &'static str;

    /// Returns a one-line description of what this capability does.
    fn description(&self) -> &'static str;

    /// Returns the JSON Schema for the capability's arguments.
    fn schema(&self) -> Value;

    /// Executes the capability with typed arguments.
    ///
    /// # Errors
    ///
    /// Returns [`CapabilityError`] if deserialization, validation, or
    /// execution fails.
    fn execute(
        &self,
        args: Self::Args,
        ctx: &Context,
    ) -> std::result::Result<Output, CapabilityError>;

    /// Dry-run execution — same as [`execute`](TypedCapability::execute)
    /// but the capability should skip side effects.
    ///
    /// Default implementation calls `execute` — capabilities that support
    /// dry-run should override this.
    ///
    /// # Errors
    ///
    /// Returns [`CapabilityError`] if the dry-run simulation fails.
    fn dry_run(
        &self,
        args: Self::Args,
        ctx: &Context,
    ) -> std::result::Result<Output, CapabilityError> {
        self.execute(args, ctx)
    }
}

/// Blanket implementation bridging [`TypedCapability`] to [`Capability`].
///
/// This is the critical bridge that allows the executor's `&dyn Capability`
/// dynamic dispatch to work with typed capabilities. When the executor calls
/// `Capability::execute(&dyn Capability, &Value, &Context)`, this impl:
///
/// 1. Deserializes the `Value` into `T::Args` (one deserialization).
/// 2. Calls `TypedCapability::execute` with the typed args.
/// 3. Maps `CapabilityError` to `crate::Error`.
///
/// The `validate` method always returns `Ok(())` because deserialization
/// **is** validation — `serde_json::from_value` rejects malformed input.
///
/// # Why this exists
///
/// Without this blanket impl, `TypedCapability` would be unusable by the
/// executor (which stores `Box<dyn Capability>`). With it, both paths
/// coexist:
///
/// - **Dynamic dispatch** (executor): `cap.execute(&value, &ctx)` — goes
///   through this blanket impl.
/// - **Static dispatch** (direct callers): `cap.execute(typed_args, &ctx)` —
///   calls `TypedCapability::execute` directly.
impl<T: TypedCapability> Capability for T {
    fn name(&self) -> &'static str {
        TypedCapability::name(self)
    }

    fn description(&self) -> &'static str {
        TypedCapability::description(self)
    }

    fn schema(&self) -> Value {
        TypedCapability::schema(self)
    }

    fn validate(&self, _args: &Value) -> Result<()> {
        // Deserialization IS validation. The blanket impl's execute method
        // deserializes Value into Self::Args, which rejects malformed input.
        // No separate validate step needed.
        Ok(())
    }

    fn execute(&self, args: &Value, ctx: &Context) -> Result<Output> {
        let typed_args: T::Args = serde_json::from_value(args.clone())
            .map_err(|e| crate::Error::SchemaValidationFailed(e.to_string()))?;
        TypedCapability::execute(self, typed_args, ctx)
            .map_err(|e| crate::Error::ExecutionFailed(e.to_string()))
    }
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
            Ok(Output::ok("test completed".into()))
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

    #[test]
    fn test_capability_error_display() {
        let err = CapabilityError::InvalidArgs("missing field `path`".into());
        assert!(err.to_string().contains("invalid arguments"));

        let err = CapabilityError::PermissionDenied("/etc/shadow".into());
        assert!(err.to_string().contains("blocked"));

        let err = CapabilityError::NotFound("/tmp/nonexistent.txt".into());
        assert!(err.to_string().contains("file not found"));

        let err = CapabilityError::Git("fatal: not a repository".into());
        assert!(err.to_string().contains("git error"));

        let err = CapabilityError::Internal("unexpected state".into());
        assert!(err.to_string().contains("internal error"));
    }

    #[test]
    fn test_capability_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let cap_err: CapabilityError = io_err.into();
        assert!(matches!(cap_err, CapabilityError::Io(_)));
        assert!(cap_err.to_string().contains("io error"));
    }

    #[test]
    fn test_capability_error_debug_format() {
        let err = CapabilityError::InvalidArgs("test".into());
        let debug = format!("{:?}", err);
        assert!(debug.contains("InvalidArgs"));
    }

    // ── Output constructors ──────────────────────────────────────────

    #[test]
    fn test_output_ok_constructor() {
        let out = Output::ok("done".into());
        assert_eq!(out.status, "ok");
        assert_eq!(out.output, "done");
        assert!(out.error.is_none());
        assert!(out.data.is_none());
        assert!(out.backup_path.is_none());
        assert!(out.artifacts.is_empty());
    }

    #[test]
    fn test_output_error_constructor() {
        let out = Output::error("failed".into(), "not found".into());
        assert_eq!(out.status, "error");
        assert_eq!(out.output, "failed");
        assert_eq!(out.error.as_deref(), Some("not found"));
    }

    #[test]
    fn test_output_to_json() {
        let out = Output::ok("test".into());
        let json = out.to_json();
        assert_eq!(json["status"], "ok");
        assert_eq!(json["output"], "test");
        assert!(json["error"].is_null());
    }

    // ── TypedCapability blanket impl ─────────────────────────────────

    /// A test TypedCapability implementation.
    struct TypedTestCap;

    impl TypedCapability for TypedTestCap {
        type Args = serde_json::Value;

        fn name(&self) -> &'static str {
            "TypedTest"
        }

        fn description(&self) -> &'static str {
            "typed test capability"
        }

        fn schema(&self) -> Value {
            serde_json::json!({"type": "object"})
        }

        fn execute(
            &self,
            args: Self::Args,
            _ctx: &Context,
        ) -> std::result::Result<Output, CapabilityError> {
            Ok(Output::ok(format!("typed: {}", args)))
        }
    }

    #[test]
    fn test_typed_capability_blanket_impl_bridge() {
        // Register a TypedCapability as a Capability via blanket impl
        let mut reg = CapabilityRegistry::new();
        reg.register(TypedTestCap);

        // Dynamic dispatch through &dyn Capability
        let cap = reg.get("TypedTest").unwrap();
        assert_eq!(cap.name(), "TypedTest");

        let result = cap.execute(
            &serde_json::json!("hello"),
            &Context::new(false, "test".into()),
        );
        assert!(result.is_ok());
        let output = result.unwrap();
        assert_eq!(output.status, "ok");
    }

    #[test]
    fn test_typed_capability_validate_always_ok() {
        // The blanket impl's validate() always returns Ok — deserialization
        // is validation, happening in execute().
        let cap = TypedTestCap;
        let result = Capability::validate(&cap, &serde_json::json!({}));
        assert!(result.is_ok());
    }
}
