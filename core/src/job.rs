//! Job lifecycle management.
//!
//! Provides [`Job`], [`JobId`], and [`JobState`] for tracking capability
//! executions through their lifecycle: `Pending → Validating → Validated →
//! Executing → Completed` (or `Failed`, with optional `RolledBack`).

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Unique identifier for a job.
///
/// Generated from the current timestamp in nanoseconds, formatted as hex.
///
/// # Example
///
/// ```rust
/// use runtimo_core::JobId;
///
/// let id = JobId::new();
/// assert!(!id.as_str().is_empty());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)]
pub struct JobId(String);

impl JobId {
    /// Creates a new job ID from 16 random bytes (32 hex chars).
    #[must_use] 
    pub fn new() -> Self {
        Self(crate::utils::generate_id())
    }

    /// Returns the job ID as a string slice.
    #[must_use] 
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for JobId {
    fn default() -> Self {
        Self::new()
    }
}

/// States in the job lifecycle.
///
/// Valid transitions:
/// ```text
/// Pending → Validating → Validated → Executing → Completed → RolledBack
///                     ↘ Failed      ↘ Failed
/// ```
#[allow(clippy::exhaustive_enums)] // new states are breaking changes regardless
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Job has been created but not yet processed.
    Pending,
    /// Job arguments are being validated.
    Validating,
    /// Arguments passed validation.
    Validated,
    /// Capability is currently executing.
    Executing,
    /// Capability completed successfully.
    Completed,
    /// Job failed during validation or execution.
    Failed,
    /// A completed job was rolled back (undo).
    RolledBack,
}

/// A tracked unit of work in the Runtimo runtime.
///
/// Jobs carry a capability name, serialized arguments, current state,
/// timestamps, and optional output or error information.
///
/// # Example
///
/// ```rust
/// use runtimo_core::{Job, JobState};
/// use serde_json::json;
///
/// let mut job = Job::new("FileRead".into(), json!({"path": "/tmp/test.txt"}), false);
/// assert_eq!(job.state, JobState::Pending);
///
/// job.transition_to(JobState::Validating).unwrap();
/// assert_eq!(job.state, JobState::Validating);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)]
pub struct Job {
    /// Unique job identifier.
    pub id: JobId,
    /// Name of the capability to execute.
    pub capability: String,
    /// Serialized capability arguments.
    pub args: serde_json::Value,
    /// Current state in the job lifecycle.
    pub state: JobState,
    /// Unix timestamp (seconds) when the job was created.
    pub created_at: u64,
    /// Unix timestamp (seconds) of the last state change.
    pub updated_at: u64,
    /// Output data from successful execution (JSON).
    pub output: Option<serde_json::Value>,
    /// Error message if the job failed.
    pub error: Option<String>,
    /// Whether this job is a dry run.
    pub dry_run: bool,
}

impl Job {
    /// Creates a new job in the `Pending` state.
    ///
    /// # Arguments
    ///
    /// * `capability` — Name of the capability to execute
    /// * `args` — Serialized capability arguments
    /// * `dry_run` — If true, skip side effects
    #[must_use] 
    pub fn new(capability: String, args: serde_json::Value, dry_run: bool) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            id: JobId::new(),
            capability,
            args,
            state: JobState::Pending,
            created_at: now,
            updated_at: now,
            output: None,
            error: None,
            dry_run,
        }
    }

    /// Attempts to transition the job to a new state.
    ///
    /// Only valid transitions are allowed (see [`JobState`] for the state machine).
    /// On success, updates `updated_at` to the current time.
    ///
    /// # Design Note
    ///
    /// The state machine is expressed as a `matches!` macro for performance on this
    /// hot path. A `const fn valid_transitions()` or a lookup table would be more
    /// extensible but adds indirection. The explicit tuple match is O(1) and
    /// optimizes to a jump table. If clippy suggests `match_like_matches_macro`,
    /// this is intentional — the macro is already the most compact form.
    ///
    /// # Errors
    ///
    /// Returns an error string describing the invalid transition.
    #[allow(clippy::match_like_matches_macro, clippy::unnested_or_patterns)]
    pub fn transition_to(&mut self, new_state: JobState) -> Result<(), String> {
        let valid = matches!(
            (self.state, new_state),
            (JobState::Pending, JobState::Validating)
                | (JobState::Validating, JobState::Validated)
                | (JobState::Validating, JobState::Failed)
                | (JobState::Validated, JobState::Executing)
                | (JobState::Executing, JobState::Completed)
                | (JobState::Executing, JobState::Failed)
                | (JobState::Completed, JobState::RolledBack)
        );

        if valid {
            self.state = new_state;
            self.updated_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            Ok(())
        } else {
            Err(format!(
                "Invalid state transition: {:?} -> {:?}",
                self.state, new_state
            ))
        }
    }
}
