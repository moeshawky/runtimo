//! Runtime fact V1 — stable Codegraph-facing evidence vocabulary.
//!
//! # Justified Families
//!
//! Only families with a demonstrated provider are created:
//!
//! | Family | Provider | Fidelity | Status |
//! |--------|----------|----------|--------|
//! | `ObservedExec` | Tetragon | `KernelObserved` | IMPORT |
//! | `ObservedExit` | Tetragon | `KernelObserved` | IMPORT |
//! | `ObservedClassLoad` | JFR | `ExactRuntime` | IMPORT |
//! | `ObservedException` | JFR | `ExactRuntime` | IMPORT |
//! | `ObservedCall` | sema | `Sampled` | DEFERRED (OTel blocked) |
//!
//! # Deferred Families
//!
//! - `ObservedCall` from `sema` → `Sampled` is DEFERRED because OTel
//!   is blocked. Shape is defined but not yet produced by a provider.
//! - `ObservedLibraryLoad`, `ObservedFileAccess`, `ObservedNetworkCall`,
//!   `ObservedProtocolCall`, `ObservedDatabaseCall`, `ObservedQueueOperation`,
//!   `ObservedAssemblyLoad` are NOT created — no demonstrated consumer.
//!
//! # Ownership
//! - **Writer**: Providers (Tetragon, JFR)
//! - **Reader**: WAL, CLI, Oracle
//! - **Serialization**: serde JSON
//! - **Versioning**: semver
//!
//! # Provenance
//!
//! Every fact retains provenance fields:
//! - `run_id` — unique run identifier
//! - `process_key` — `RunProcessKey` (PID + start_time, never PID alone)
//! - `provider` — provider name
//! - `artifact` — artifact reference
//! - `fidelity` — `EvidenceFidelity`
//! - `observations` — observation count
//! - `first_seen` — first seen timestamp
//! - `last_seen` — last seen timestamp
//! - `revision` — git commit
//!
//! # Global Invariant
//! - `NOT OBSERVED != FALSE` — absence of evidence is not evidence of absence.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::runtime::EvidenceFidelity;
use crate::runtime::RunProcessKey;

/// Reference to an artifact produced by a provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)]
pub struct ArtifactRef {
    /// Artifact path.
    pub path: String,
    /// Media type.
    pub media: String,
    /// SHA-256 hash.
    pub sha256: String,
    /// Provider that produced this artifact.
    pub provider: String,
    /// Process ID captured from /proc/pid/stat.
    pub pid: u32,
    /// Process start time from /proc/pid/stat field 22.
    pub process_start_time: u64,
}

/// A single runtime fact with full provenance.
///
/// # Justified Families
/// Only `ObservedExec`, `ObservedExit`, `ObservedClassLoad`, and
/// `ObservedException` are created — each has a demonstrated provider.
///
/// # Invariants
/// - `process_key` always includes `process_start_time` (PID never alone).
/// - `fidelity` determines the evidence quality.
/// - `observations` counts the number of observations.
/// - `first_seen <= last_seen` always holds.
/// - `NOT OBSERVED != FALSE` — absence is not evidence of absence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)] // fields are write-through API contract
pub struct RuntimeFactV1 {
    /// Fact family name (e.g., "ObservedExec", "ObservedExit").
    pub family: RuntimeFactFamily,
    /// Unique run identifier.
    pub run_id: String,
    /// Process key (PID + start_time, never PID alone).
    pub process_key: RunProcessKey,
    /// Provider name.
    pub provider: String,
    /// Artifact reference.
    pub artifact: ArtifactRef,
    /// Evidence fidelity classification.
    pub fidelity: EvidenceFidelity,
    /// Observation count.
    pub observations: u64,
    /// First seen timestamp (Unix timestamp).
    pub first_seen: u64,
    /// Last seen timestamp (Unix timestamp).
    pub last_seen: u64,
    /// Git commit revision.
    pub revision: String,
    /// Additional observations as key-value pairs.
    pub extra: HashMap<String, serde_json::Value>,
}

/// Runtime fact family — the evidence vocabulary.
///
/// Only justified families are created. See `RuntimeFactV1` docs
/// for the provider mapping and status.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::exhaustive_enums)] // new families are semver-breaking
pub enum RuntimeFactFamily {
    /// Process execution observed by Tetragon (KernelObserved).
    ObservedExec,
    /// Process exit observed by Tetragon (KernelObserved).
    ObservedExit,
    /// Class load observed by JFR (ExactRuntime).
    ObservedClassLoad,
    /// Exception observed by JFR (ExactRuntime).
    ObservedException,
    /// Call observed by sema (Sampled; DEFERRED — OTel blocked).
    ///
    /// Shape defined but not yet produced by a provider.
    ObservedCall,
}

impl RuntimeFactV1 {
    /// Creates a new `RuntimeFactV1`.
    ///
    /// # Panics
    /// Panics if `run_id` is empty or `revision` is empty.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        family: RuntimeFactFamily,
        run_id: String,
        process_key: RunProcessKey,
        provider: String,
        artifact: ArtifactRef,
        fidelity: EvidenceFidelity,
        observations: u64,
        first_seen: u64,
        last_seen: u64,
        revision: String,
    ) -> Self {
        assert!(!run_id.is_empty(), "run_id must not be empty");
        assert!(!revision.is_empty(), "revision must not be empty");
        assert!(first_seen <= last_seen, "first_seen must be <= last_seen");
        Self {
            family,
            run_id,
            process_key,
            provider,
            artifact,
            fidelity,
            observations,
            first_seen,
            last_seen,
            revision,
            extra: HashMap::new(),
        }
    }

    /// Returns `true` if this fact represents directly observed evidence.
    #[must_use]
    pub fn is_observed(&self) -> bool {
        self.fidelity.is_observed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_process_key() -> RunProcessKey {
        RunProcessKey::new("run-001".to_string(), 1234, 1000)
    }

    fn make_artifact() -> ArtifactRef {
        ArtifactRef {
            path: "/tmp/bundle.jsonl".to_string(),
            media: "application/json".to_string(),
            sha256: "a".repeat(64),
            provider: "tetragon".to_string(),
            pid: 1234,
            process_start_time: 1000,
        }
    }

    #[test]
    fn runtime_fact_round_trip() {
        let fact = RuntimeFactV1::new(
            RuntimeFactFamily::ObservedExec,
            "run-001".to_string(),
            make_process_key(),
            "tetragon".to_string(),
            make_artifact(),
            EvidenceFidelity::KernelObserved,
            5,
            1000,
            2000,
            "abc123".to_string(),
        );
        let json = serde_json::to_string(&fact).unwrap();
        let deserialized: RuntimeFactV1 = serde_json::from_str(&json).unwrap();
        assert_eq!(fact, deserialized);
    }

    #[test]
    fn runtime_fact_unknown_field_ignored() {
        // Forward-compat: unknown fields are ignored.
        let json = r#"{"family":"observed_exec","run_id":"run-001","process_key":{"run_id":"run-001","pid":1234,"process_start_time":1000},"provider":"tetragon","artifact":{"path":"/tmp/bundle.jsonl","media":"application/json","sha256":"a","provider":"tetragon","pid":1234,"process_start_time":1000},"fidelity":"kernel_observed","observations":5,"first_seen":1000,"last_seen":2000,"revision":"abc123","extra":{}}"#;
        let fact: RuntimeFactV1 = serde_json::from_str(json).unwrap();
        assert_eq!(fact.family, RuntimeFactFamily::ObservedExec);
    }

    #[test]
    fn runtime_fact_first_seen_le_last_seen() {
        let fact = RuntimeFactV1::new(
            RuntimeFactFamily::ObservedExec,
            "run-001".to_string(),
            make_process_key(),
            "tetragon".to_string(),
            make_artifact(),
            EvidenceFidelity::KernelObserved,
            1,
            1000,
            2000,
            "abc123".to_string(),
        );
        assert!(fact.first_seen <= fact.last_seen);
    }

    #[test]
    #[should_panic(expected = "first_seen must be <= last_seen")]
    fn runtime_fact_first_seen_gt_last_seen_panics() {
        let _ = RuntimeFactV1::new(
            RuntimeFactFamily::ObservedExec,
            "run-001".to_string(),
            make_process_key(),
            "tetragon".to_string(),
            make_artifact(),
            EvidenceFidelity::KernelObserved,
            1,
            2000,
            1000,
            "abc123".to_string(),
        );
    }

    #[test]
    fn runtime_fact_is_observed() {
        let fact = RuntimeFactV1::new(
            RuntimeFactFamily::ObservedExec,
            "run-001".to_string(),
            make_process_key(),
            "tetragon".to_string(),
            make_artifact(),
            EvidenceFidelity::KernelObserved,
            1,
            1000,
            2000,
            "abc123".to_string(),
        );
        assert!(fact.is_observed());
    }

    #[test]
    fn runtime_fact_provenance_fields() {
        let fact = RuntimeFactV1::new(
            RuntimeFactFamily::ObservedClassLoad,
            "run-001".to_string(),
            make_process_key(),
            "jfr".to_string(),
            make_artifact(),
            EvidenceFidelity::ExactRuntime,
            3,
            1000,
            3000,
            "def456".to_string(),
        );
        assert_eq!(fact.run_id, "run-001");
        assert_eq!(fact.provider, "jfr");
        assert_eq!(fact.fidelity, EvidenceFidelity::ExactRuntime);
        assert_eq!(fact.observations, 3);
    }

    #[test]
    fn runtime_fact_pid_never_alone() {
        // Invariant: process_key always has process_start_time.
        let fact = RuntimeFactV1::new(
            RuntimeFactFamily::ObservedExec,
            "run-001".to_string(),
            RunProcessKey::new("run-001".to_string(), 1234, 1000),
            "tetragon".to_string(),
            make_artifact(),
            EvidenceFidelity::KernelObserved,
            1,
            1000,
            2000,
            "abc123".to_string(),
        );
        assert!(
            fact.process_key.process_start_time > 0,
            "PID must never be alone; process_start_time must be present"
        );
    }
}
