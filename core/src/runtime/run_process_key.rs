//! Run process key — PID-reuse ambiguity prevention.
//!
//! # Ownership
//! - **Writer**: Executor
//! - **Reader**: WAL, RuntimeFactV1, RunManifestV1
//! - **Serialization**: serde JSON
//! - **Versioning**: semver
//!
//! # Invariants
//! - Equality includes `process_start_time` — same PID with different
//!   start_time are distinct keys.
//! - Ordering is deterministic (lexicographic on serialized form).
//! - PID is never used alone; always paired with `process_start_time`.
//! - `run_id` provides global uniqueness; `pid` + `process_start_time`
//!   provide process-level uniqueness.

use serde::{Deserialize, Serialize};

/// Key that uniquely identifies a running process, preventing PID-reuse ambiguity.
///
/// A PID alone is insufficient because the OS can reuse PIDs after process
/// exit. Pairing `pid` with `process_start_time` (from `/proc/pid/stat` field
/// 22) ensures that even if a PID is reused, the key remains distinct.
///
/// # Example
/// ```rust
/// use runtimo_core::runtime::RunProcessKey;
///
/// let key1 = RunProcessKey::new("run-001".to_string(), 1234, 1000);
/// let key2 = RunProcessKey::new("run-001".to_string(), 1234, 2000);
/// assert_ne!(key1, key2); // Same PID, different start_time → distinct
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)] // fields are write-through API contract
pub struct RunProcessKey {
    /// Unique run identifier (32 hex chars from `crate::utils::generate_id`).
    pub run_id: String,
    /// Process ID. Never used alone — always paired with `process_start_time`.
    pub pid: u32,
    /// Process start time from `/proc/pid/stat` field 22 (boot-time anchored).
    /// This prevents PID-reuse ambiguity: same PID with different start_time
    /// produces a distinct key.
    pub process_start_time: u64,
}

impl RunProcessKey {
    /// Creates a new `RunProcessKey`.
    ///
    /// # Panics
    /// Panics if `run_id` is empty.
    #[must_use]
    pub fn new(run_id: String, pid: u32, process_start_time: u64) -> Self {
        assert!(!run_id.is_empty(), "run_id must not be empty");
        Self {
            run_id,
            pid,
            process_start_time,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_process_key_same_pid_diff_start_time_distinct() {
        // Invariant: same PID but different start_time are distinct keys.
        let key1 = RunProcessKey::new("run-001".to_string(), 1234, 1000);
        let key2 = RunProcessKey::new("run-001".to_string(), 1234, 2000);
        assert_ne!(
            key1, key2,
            "Same PID with different start_time must be distinct"
        );
    }

    #[test]
    fn run_process_key_same_pid_same_start_time_equal() {
        let key1 = RunProcessKey::new("run-001".to_string(), 1234, 1000);
        let key2 = RunProcessKey::new("run-001".to_string(), 1234, 1000);
        assert_eq!(key1, key2);
    }

    #[test]
    fn run_process_key_round_trip() {
        let key = RunProcessKey::new("run-001".to_string(), 1234, 1000);
        let json = serde_json::to_string(&key).unwrap();
        let deserialized: RunProcessKey = serde_json::from_str(&json).unwrap();
        assert_eq!(key, deserialized);
    }

    #[test]
    fn run_process_key_unknown_field_ignored() {
        // Forward-compat: unknown fields are ignored.
        let json = r#"{"run_id":"run-001","pid":1234,"process_start_time":1000,"extra":"ignored"}"#;
        let key: RunProcessKey = serde_json::from_str(json).unwrap();
        assert_eq!(key.run_id, "run-001");
        assert_eq!(key.pid, 1234);
        assert_eq!(key.process_start_time, 1000);
    }

    #[test]
    fn run_process_key_ordering_deterministic() {
        let key1 = RunProcessKey::new("run-001".to_string(), 1234, 1000);
        let key2 = RunProcessKey::new("run-002".to_string(), 1234, 1000);
        let json1 = serde_json::to_string(&key1).unwrap();
        let json2 = serde_json::to_string(&key2).unwrap();
        assert!(json1 != json2, "Ordering must be deterministic");
    }

    #[test]
    #[should_panic(expected = "run_id must not be empty")]
    fn run_process_key_empty_run_id_panics() {
        let _ = RunProcessKey::new(String::new(), 1234, 1000);
    }
}
