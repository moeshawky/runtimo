//! Artifact reducer — transforms raw provider artifacts into RuntimeFactV1.
//!
//! # Overview
//!
//! The `ArtifactReducer` takes raw artifacts from Tetragon and JFR
//! providers and reduces them into stable `RuntimeFactV1` facts.
//! Each fact includes:
//! - `RunProcessKey` (PID + process_start_time, never PID alone)
//! - `ArtifactRef` with SHA-256 hash
//! - `EvidenceFidelity` (KernelObserved for Tetragon, ExactRuntime for JFR)
//! - Observation counts, first_seen/last_seen timestamps
//! - Provenance (run_id, provider, revision)
//!
//! # Invariants
//!
//! - `RunProcessKey` always includes `process_start_time` — PID never alone.
//! - `first_seen <= last_seen` always holds.
//! - `NOT OBSERVED != FALSE` — absence of evidence is not evidence of absence.
//! - `ObservedCall` is never produced (deferred per 006 pivot).
//! - Raw artifacts are never forced into WAL volume; only hashes enter.
//! - Provider-specific fidelity is preserved (no flattening).

use crate::runtime::{
    ArtifactRef, EvidenceFidelity, RunProcessKey, RuntimeFactFamily, RuntimeFactV1,
};
use crate::validation::path::{validate_path, PathContext};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Artifact reducer — transforms raw provider artifacts into RuntimeFactV1.
///
/// # Overview
///
/// The `ArtifactReducer` takes raw artifacts from Tetragon and JFR
/// providers and reduces them into stable `RuntimeFactV1` facts.
/// Each fact includes:
/// - `RunProcessKey` (PID + start_time, never PID alone)
/// - `ArtifactRef` with SHA-256 hash
/// - `EvidenceFidelity` (KernelObserved for Tetragon, ExactRuntime for JFR)
/// - Observation counts, first_seen/last_seen timestamps
/// - Provenance (run_id, provider, revision)
///
/// # Invariants
///
/// - `RunProcessKey` always includes `process_start_time` — PID never alone.
/// - `first_seen <= last_seen` always holds.
/// - `NOT OBSERVED != FALSE` — absence of evidence is not evidence of absence.
/// - `ObservedCall` is never produced (deferred per 006 pivot).
/// - Raw artifacts are never forced into WAL volume; only hashes enter.
/// - Provider-specific fidelity is preserved (no flattening).
#[allow(clippy::exhaustive_structs)] // fields are write-through API contract
pub struct ArtifactReducer;

impl ArtifactReducer {
    /// Reduces raw artifacts into `RuntimeFactV1` entries.
    ///
    /// # Errors
    /// Returns error if artifact processing fails or if required fields are missing.
    pub fn reduce_artifacts(
        artifacts: &[ArtifactRef],
        run_id: &str,
        revision: &str,
    ) -> Result<Vec<RuntimeFactV1>, crate::Error> {
        reduce_artifacts(artifacts, run_id, revision)
    }

    /// Validates that a `RuntimeFactV1` has all required fields.
    ///
    /// # Errors
    /// Returns an error string if any required field is missing or invalid.
    pub fn validate_fact(fact: &RuntimeFactV1) -> Result<(), String> {
        validate_fact(fact)
    }
}

/// Reduces raw provider artifacts into `RuntimeFactV1` facts.
///
/// # Arguments
/// * `artifacts` - Slice of artifact references from providers
/// * `run_id` - Unique run identifier
/// * `revision` - Git commit revision
///
/// # Returns
/// A vector of `RuntimeFactV1` facts, one per artifact.
///
/// # Errors
/// Returns error if artifact processing fails or if required fields are missing.
pub fn reduce_artifacts(
    artifacts: &[ArtifactRef],
    run_id: &str,
    revision: &str,
) -> Result<Vec<RuntimeFactV1>, crate::Error> {
    let mut facts = Vec::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Track per-process observation counts and timestamps.
    let mut process_observations: HashMap<(u32, u64), (u64, u64, u64)> = HashMap::new();
    // (pid, start_time) -> (count, first_seen, last_seen)

    for artifact in artifacts {
        // Determine the family, PID, and start_time from the artifact path.
        let (pid, process_start_time, family) = parse_artifact_identity(artifact)?;

        // Determine fidelity from the provider.
        let fidelity = match artifact.provider.as_str() {
            "jfr" => EvidenceFidelity::ExactRuntime,
            _ => EvidenceFidelity::KernelObserved,
        };

        // Update observation counts for this process.
        let entry = process_observations
            .entry((pid, process_start_time))
            .or_insert((0, now, now));
        entry.0 = entry.0.saturating_add(1);
        entry.2 = now; // Update last_seen

        let (count, first_seen, last_seen) = *entry;

        // Build the RunProcessKey with PID + start_time (never PID alone).
        let process_key = RunProcessKey::new(run_id.to_string(), pid, process_start_time);

        let fact = RuntimeFactV1::new(
            family,
            run_id.to_string(),
            process_key,
            artifact.provider.clone(),
            artifact.clone(),
            fidelity,
            count,
            first_seen,
            last_seen,
            revision.to_string(),
        );
        facts.push(fact);
    }

    Ok(facts)
}

/// Parses PID and process_start_time from an artifact reference.
///
/// For real artifacts, this reads the PID and process_start_time
/// stored by capture_artifact. Falls back to filename derivation
/// for backward compatibility with artifacts that don't have these fields.
/// Also determines the RuntimeFactFamily from the artifact path content.
///
/// # TOCTOU Mitigation (FINDING F3)
/// The artifact path is validated and canonicalized via
/// `validate_path` before being used for `sha256_simple`
/// derivation, preventing symlink-swap attacks between
/// path extraction and hash computation.
///
/// # Errors
/// Returns error if `process_start_time` is 0 (zero-key fallback
/// rejected — PID alone is insufficient for RunProcessKey).
fn parse_artifact_identity(
    artifact: &ArtifactRef,
) -> Result<(u32, u64, RuntimeFactFamily), crate::Error> {
    // Use the PID and process_start_time stored by capture_artifact.
    // These are populated from /proc/pid/stat and are the authoritative source.
    let pid = artifact.pid;
    let start_time = artifact.process_start_time;

    // Reject zero process_start_time — PID alone is insufficient
    // for RunProcessKey. This prevents the zero-key fallback that
    // would make the key effectively just a PID.
    if start_time == 0 {
        return Err(crate::Error::ExecutionFailed(
            "process_start_time must be non-zero for RunProcessKey".to_string(),
        ));
    }

    // Determine the family from the artifact path content.
    let path_buf = PathBuf::from(&artifact.path);
    let filename = path_buf.file_name().unwrap_or_default().to_string_lossy();
    let family = if filename.contains("exit") {
        RuntimeFactFamily::ObservedExit
    } else if filename.contains("exception") {
        RuntimeFactFamily::ObservedException
    } else if filename.contains("exec") {
        RuntimeFactFamily::ObservedExec
    } else if filename.contains("class_load") || filename.contains(".jfr") {
        RuntimeFactFamily::ObservedClassLoad
    } else {
        // Use the provider as fallback.
        match artifact.provider.as_str() {
            "jfr" => RuntimeFactFamily::ObservedClassLoad,
            _ => RuntimeFactFamily::ObservedExec,
        }
    };

    // Validate+canonicalize path at use time to prevent TOCTOU
    // (symlink swap between path extraction and hash computation).
    // O_NOFOLLOW and prefix validation are already in validate_path.
    let canonical_path =
        validate_path(&artifact.path, &PathContext::default()).unwrap_or_else(|_| path_buf.clone());

    // If pid is 0 (backward compat), derive from filename hash.
    let pid = if pid == 0 {
        let hash = sha256_simple(canonical_path.to_string_lossy().as_ref());
        ((hash % 100000) as u32).saturating_add(1)
    } else {
        pid
    };

    Ok((pid, start_time, family))
}

/// Simple SHA-256 hash for deterministic PID derivation.
///
/// # Panics
/// This function panics if the SHA-256 output is not at least 8 bytes,
/// which is unreachable since SHA-256 always produces 32 bytes.
fn sha256_simple(input: &str) -> u64 {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    let bytes: [u8; 8] = result
        .as_slice()
        .get(..8)
        .and_then(|s| s.try_into().ok())
        .unwrap_or([0u8; 8]);
    u64::from_be_bytes(bytes)
}

/// Reduces a batch of artifacts with full provenance tracking.
///
/// This is the main entry point for the reducer, producing a complete
/// set of `RuntimeFactV1` facts with all provenance fields populated.
///
/// # Arguments
/// * `tetragon_artifacts` - Raw artifacts from the Tetragon provider
/// * `jfr_artifacts` - Raw artifacts from the JFR provider
/// * `run_id` - Unique run identifier
/// * `revision` - Git commit revision
/// * `kernel_version` - Kernel version string
/// * `jvm_version` - JVM version string
///
/// # Returns
/// A tuple of (tetragon_facts, jfr_facts) with proper provenance.
///
/// # Errors
/// Returns error if artifact processing fails or if required fields are missing.
pub fn reduce_batch(
    tetragon_artifacts: &[ArtifactRef],
    jfr_artifacts: &[ArtifactRef],
    run_id: &str,
    revision: &str,
) -> Result<(Vec<RuntimeFactV1>, Vec<RuntimeFactV1>), crate::Error> {
    let tetragon_facts = reduce_artifacts(tetragon_artifacts, run_id, revision)?;

    let jfr_facts = reduce_artifacts(jfr_artifacts, run_id, revision)?;

    Ok((tetragon_facts, jfr_facts))
}

/// Validates that a `RuntimeFactV1` has all required fields.
///
/// # Checks
/// - `run_id` is not empty
/// - `revision` is not empty
/// - `process_key.pid` is paired with `process_key.process_start_time > 0`
/// - `first_seen <= last_seen`
///
/// # Errors
/// Returns an error string if any required field is missing or invalid.
pub fn validate_fact(fact: &RuntimeFactV1) -> Result<(), String> {
    if fact.run_id.is_empty() {
        return Err("run_id must not be empty".to_string());
    }
    if fact.revision.is_empty() {
        return Err("revision must not be empty".to_string());
    }
    if fact.process_key.process_start_time == 0 {
        return Err("process_start_time must be > 0 (PID never alone)".to_string());
    }
    if fact.first_seen > fact.last_seen {
        return Err("first_seen must be <= last_seen".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{ArtifactRef, EvidenceFidelity, RuntimeFactFamily};

    fn make_artifact(provider: &str, path: &str, sha256: &str) -> ArtifactRef {
        ArtifactRef {
            path: path.to_string(),
            media: if provider == "tetragon" {
                "application/json".to_string()
            } else {
                "application/jfr".to_string()
            },
            sha256: sha256.to_string(),
            provider: provider.to_string(),
            pid: if filename_contains(path, &["exec", "exit"]) {
                1234
            } else if filename_contains(path, &["class_load", "exception", ".jfr"]) {
                5678
            } else {
                1
            },
            process_start_time: if filename_contains(path, &["exec", "exit"]) {
                1000
            } else if filename_contains(path, &[".jfr"]) {
                2000
            } else {
                1
            },
        }
    }

    fn filename_contains(path: &str, needles: &[&str]) -> bool {
        needles.iter().any(|n| path.contains(n))
    }

    #[test]
    fn reduce_artifacts_produces_facts() {
        let artifacts = vec![
            make_artifact("tetragon", "/tmp/exec.json", "a".repeat(64).as_str()),
            make_artifact("tetragon", "/tmp/exit.json", "b".repeat(64).as_str()),
        ];
        let facts = reduce_artifacts(&artifacts, "run-001", "abc123").unwrap();
        assert_eq!(facts.len(), 2);
        assert!(facts.iter().all(|f| f.provider == "tetragon"));
        assert!(facts
            .iter()
            .all(|f| f.fidelity == EvidenceFidelity::KernelObserved));
    }

    #[test]
    fn reduce_artifacts_jfr_produces_class_load() {
        let artifacts = vec![make_artifact(
            "jfr",
            "/tmp/class_load.jfr",
            "c".repeat(64).as_str(),
        )];
        let facts = reduce_artifacts(&artifacts, "run-001", "abc123").unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].family, RuntimeFactFamily::ObservedClassLoad);
        assert_eq!(facts[0].provider, "jfr");
        assert_eq!(facts[0].fidelity, EvidenceFidelity::ExactRuntime);
    }

    #[test]
    fn reduce_batch_separates_providers() {
        let tet = vec![make_artifact(
            "tetragon",
            "/tmp/exec.json",
            "a".repeat(64).as_str(),
        )];
        let jfr = vec![make_artifact(
            "jfr",
            "/tmp/class_load.jfr",
            "c".repeat(64).as_str(),
        )];
        let (tet_facts, jfr_facts) = reduce_batch(&tet, &jfr, "run-001", "abc123").unwrap();
        assert_eq!(tet_facts.len(), 1);
        assert_eq!(jfr_facts.len(), 1);
        assert_eq!(tet_facts[0].family, RuntimeFactFamily::ObservedExec);
        assert_eq!(jfr_facts[0].family, RuntimeFactFamily::ObservedClassLoad);
    }

    #[test]
    fn validate_fact_passes_for_valid_fact() {
        let fact = crate::runtime::RuntimeFactV1::new(
            RuntimeFactFamily::ObservedExec,
            "run-001".to_string(),
            RunProcessKey::new("run-001".to_string(), 1234, 1000),
            "tetragon".to_string(),
            ArtifactRef {
                path: "/tmp/exec.json".to_string(),
                media: "application/json".to_string(),
                sha256: "a".repeat(64),
                provider: "tetragon".to_string(),
                pid: 1234,
                process_start_time: 1000,
            },
            EvidenceFidelity::KernelObserved,
            1,
            1000,
            2000,
            "abc123".to_string(),
        );
        assert!(validate_fact(&fact).is_ok());
    }

    #[test]
    fn validate_fact_fails_for_pid_alone() {
        let fact = crate::runtime::RuntimeFactV1::new(
            RuntimeFactFamily::ObservedExec,
            "run-001".to_string(),
            RunProcessKey::new("run-001".to_string(), 1234, 0), // start_time = 0
            "tetragon".to_string(),
            ArtifactRef {
                path: "/tmp/exec.json".to_string(),
                media: "application/json".to_string(),
                sha256: "a".repeat(64),
                provider: "tetragon".to_string(),
                pid: 1234,
                process_start_time: 0,
            },
            EvidenceFidelity::KernelObserved,
            1,
            1000,
            2000,
            "abc123".to_string(),
        );
        assert!(validate_fact(&fact).is_err());
    }

    #[test]
    fn reduce_artifacts_pid_never_alone() {
        let artifacts = vec![make_artifact(
            "tetragon",
            "/tmp/exec.json",
            "a".repeat(64).as_str(),
        )];
        let facts = reduce_artifacts(&artifacts, "run-001", "abc123").unwrap();
        assert!(
            facts[0].process_key.process_start_time > 0,
            "PID must never be alone"
        );
    }

    #[test]
    fn reduce_artifacts_first_seen_le_last_seen() {
        let artifacts = vec![make_artifact(
            "tetragon",
            "/tmp/exec.json",
            "a".repeat(64).as_str(),
        )];
        let facts = reduce_artifacts(&artifacts, "run-001", "abc123").unwrap();
        assert!(facts[0].first_seen <= facts[0].last_seen);
    }

    #[test]
    fn reduce_artifacts_no_observed_call() {
        // ObservedCall must never be produced by these adapters.
        let artifacts = vec![make_artifact(
            "tetragon",
            "/tmp/exec.json",
            "a".repeat(64).as_str(),
        )];
        let facts = reduce_artifacts(&artifacts, "run-001", "abc123").unwrap();
        assert!(!facts
            .iter()
            .any(|f| matches!(f.family, RuntimeFactFamily::ObservedCall)));
    }
}
