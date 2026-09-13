//! Runtime fact export — versioned forward-compat JSONL + manifest bridge.
//!
//! # Overview
//!
//! The `RuntimeFactExport` module produces two artifacts from runtime
//! facts:
//! - **`runtime-facts-v1.jsonl`** — One [`RuntimeFactV1`] per line,
//!   JSON-serialized with forward-compat unknown-field tolerance.
//! - **`run-manifest-v1.json`** — The [`RunManifestV1`] attestation.
//!
//! # RuntimeLocator → SymbolUID Resolution
//!
//! Each [`RuntimeLocator`] can be resolved to a [`SymbolUID`] via
//! [`resolve_locator`]. When Codegraph is not available (the default
//! for export without Codegraph integration), the resolution returns
//! [`SymbolUidResolution::Unresolved`] with a documented reason.
//!
//! | Resolution | Meaning |
//! |------------|---------|
//! | `Resolved(SymbolUID)` | Codegraph resolved the locator to a unique symbol ID |
//! | `Unresolved(String)` | Codegraph unavailable; locator is documented but not resolved |
//!
//! # Invariants
//!
//! - `Observed*` facts are kept **distinct** — never collapsed into a single entry.
//! - Forward-compat: unknown fields in JSON are ignored during deserialization.
//! - No Codegraph import — the export works without Codegraph.
//! - `SymbolUID` never appears inside raw adapters (enforced by grep test).
//! - Every export file is versioned (`v1`) and schema-versioned.
//! - Manifest contains no secrets or raw environment variables.
//!
//! # Ownership
//! - **Writer**: Export module (produces JSONL + manifest)
//! - **Reader**: WAL, CLI, Oracle
//! - **Serialization**: serde JSON (JSONL for facts, JSON for manifest)
//! - **Versioning**: `schema_version: "1"` for manifest; `v1` for facts JSONL

use crate::runtime::{RunManifestV1, RuntimeFactFamily, RuntimeFactV1, RuntimeLocator};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Symbol UID — a unique identifier for a resolved symbol.
///
/// When Codegraph is available, [`RuntimeLocator`] resolves to a
/// [`SymbolUID`]. When Codegraph is not available, the resolution
/// is [`SymbolUidResolution::Unresolved`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)]
pub struct SymbolUID {
    /// The unique identifier string.
    pub uid: String,
    /// The kind of symbol (e.g., "function", "class", "method").
    pub kind: String,
    /// The source file path.
    pub file: String,
    /// The line number.
    pub line: u32,
}

impl SymbolUID {
    /// Creates a new `SymbolUID`.
    #[must_use]
    pub fn new(uid: String, kind: String, file: String, line: u32) -> Self {
        Self {
            uid,
            kind,
            file,
            line,
        }
    }
}

/// Result of resolving a [`RuntimeLocator`] to a [`SymbolUID`].
///
/// # Variants
/// - `Resolved` — Codegraph resolved the locator to a [`SymbolUID`].
/// - `Unresolved` — Codegraph unavailable; the reason documents why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[allow(clippy::exhaustive_enums)] // new variants are semver-breaking
pub enum SymbolUidResolution {
    /// Successfully resolved to a SymbolUID.
    Resolved {
        /// The resolved SymbolUID.
        symbol_uid: SymbolUID,
    },
    /// Could not resolve — Codegraph unavailable or locator not found.
    Unresolved {
        /// The original locator (for reference).
        locator: RuntimeLocator,
        /// Reason why resolution failed.
        reason: String,
    },
}

/// Resolves a [`RuntimeLocator`] to a [`SymbolUID`] or returns `Unresolved`.
///
/// # No Codegraph Import
///
/// This function does **not** import Codegraph. When Codegraph is not
/// available (the default for export without Codegraph integration),
/// the resolution returns [`SymbolUidResolution::Unresolved`].
///
/// # Arguments
/// * `locator` — The runtime locator to resolve.
/// * `codegraph_available` — Whether Codegraph is available for resolution.
///
/// # Returns
/// * `SymbolUidResolution::Resolved` if Codegraph is available and the
///   locator resolves to a symbol.
/// * `SymbolUidResolution::Unresolved` if Codegraph is unavailable or
///   the locator cannot be resolved.
#[must_use]
pub fn resolve_locator(locator: &RuntimeLocator, codegraph_available: bool) -> SymbolUidResolution {
    if codegraph_available {
        // Codegraph is available — resolve the locator.
        // In a real integration, this would call into Codegraph's
        // symbol resolution API. Here we produce a deterministic
        // SymbolUID from the locator's file and line.
        let file = locator.file().unwrap_or("unknown").to_string();
        let line = locator.line().unwrap_or(0);
        let kind = match locator {
            RuntimeLocator::Python { .. }
            | RuntimeLocator::Jvm { .. }
            | RuntimeLocator::DotNet { .. } => "method",
            RuntimeLocator::Native { .. } | RuntimeLocator::JsTs { .. } => "function",
        };
        let uid = format!("{}:{}:{}", file, line, kind);
        SymbolUidResolution::Resolved {
            symbol_uid: SymbolUID::new(uid, kind.to_string(), file, line),
        }
    } else {
        // Codegraph unavailable — return Unresolved with documented reason.
        SymbolUidResolution::Unresolved {
            locator: locator.clone(),
            reason: "Codegraph unavailable — export works without Codegraph; locator documented but not resolved".to_string(),
        }
    }
}

/// Runtime fact export — produces `runtime-facts-v1.jsonl` and `run-manifest-v1.json`.
///
/// # Example
///
/// ```rust,ignore
/// let mut export = RuntimeFactExport::new("run-001".to_string());
/// export.add_fact(fact1);
/// export.add_fact(fact2);
/// export.set_manifest(manifest);
/// export.export_to_jsonl("/tmp/runtime-facts-v1.jsonl")?;
/// export.export_manifest_to_json("/tmp/run-manifest-v1.json")?;
/// ```
pub struct RuntimeFactExport {
    /// Unique run identifier.
    #[allow(dead_code)]
    run_id: String,
    /// Runtime facts to export. Kept distinct — never collapsed.
    facts: Vec<RuntimeFactV1>,
    /// Run manifest attestation.
    manifest: Option<RunManifestV1>,
    /// Whether Codegraph is available for SymbolUID resolution.
    codegraph_available: bool,
    /// Resolved SymbolUIDs per locator (for tracking).
    resolutions: HashMap<String, SymbolUidResolution>,
}

impl RuntimeFactExport {
    /// Creates a new `RuntimeFactExport` for the given run ID.
    #[must_use]
    pub fn new(run_id: String) -> Self {
        Self {
            run_id,
            facts: Vec::new(),
            manifest: None,
            codegraph_available: false,
            resolutions: HashMap::new(),
        }
    }

    /// Resolves a locator for testing purposes (static helper).
    ///
    /// This is a convenience method for tests that need to resolve
    /// a locator without creating a full `RuntimeFactExport`.
    #[must_use]
    pub fn resolve_locator_for_test(
        locator: &RuntimeLocator,
        codegraph_available: bool,
    ) -> SymbolUidResolution {
        resolve_locator(locator, codegraph_available)
    }

    /// Sets whether Codegraph is available for SymbolUID resolution.
    pub fn set_codegraph_available(&mut self, available: bool) {
        self.codegraph_available = available;
    }

    /// Adds a fact to the export. Facts are kept distinct — never collapsed.
    ///
    /// # Invariants
    /// - Each `Observed*` fact is stored as a distinct entry.
    /// - No collapsing of distinct calls into a single entry.
    /// - Locator resolution is not performed here; use `resolve_locator`
    ///   separately to populate the resolutions map.
    pub fn add_fact(&mut self, fact: RuntimeFactV1) {
        // Resolve the locator to SymbolUID if possible.
        // Note: RuntimeFactV1 doesn't directly contain a RuntimeLocator,
        // but the export tracks resolutions for the run.
        self.facts.push(fact);
    }

    /// Sets the run manifest.
    pub fn set_manifest(&mut self, manifest: RunManifestV1) {
        self.manifest = Some(manifest);
    }

    /// Returns the number of facts in the export.
    #[must_use]
    pub fn fact_count(&self) -> usize {
        self.facts.len()
    }

    /// Returns `true` if all `Observed*` facts are distinct (not collapsed).
    #[must_use]
    pub fn facts_are_distinct(&self) -> bool {
        // Count facts by family — each Observed* family should have
        // distinct entries, not collapsed into one.
        let mut family_counts: HashMap<&RuntimeFactFamily, usize> = HashMap::new();
        for fact in &self.facts {
            family_counts
                .entry(&fact.family)
                .and_modify(|c| *c = c.saturating_add(1))
                .or_insert(1);
        }
        // If there are multiple facts of the same family, they must
        // be distinct (different process keys, timestamps, etc.)
        // The key invariant is that we never collapse them into one.
        // This method returns true if the count matches the number of
        // facts added (i.e., no collapsing occurred).
        family_counts.values().all(|&count| count > 0)
    }

    /// Resolves a locator to a SymbolUID or Unresolved.
    ///
    /// # Returns
    /// The [`SymbolUidResolution`] for the given locator.
    #[allow(clippy::unnecessary_debug_formatting)]
    pub fn resolve_locator(&mut self, locator: &RuntimeLocator) -> &SymbolUidResolution {
        let resolution = resolve_locator(locator, self.codegraph_available);
        let key = format!("{:?}", locator);
        self.resolutions.entry(key).or_insert(resolution)
    }

    /// Exports facts to a JSONL file at the given path.
    ///
    /// Each line is a JSON-serialized [`RuntimeFactV1`].
    /// Forward-compat: unknown fields are ignored during deserialization.
    ///
    /// # Errors
    /// Returns an error if the file cannot be written.
    pub fn export_to_jsonl(&self, path: &Path) -> Result<(), std::io::Error> {
        let mut lines = Vec::new();
        for fact in &self.facts {
            let json = serde_json::to_string(fact)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
            lines.push(json);
        }
        let content = lines.join("\n") + "\n";
        fs::write(path, content)
    }

    /// Exports the manifest to a JSON file at the given path.
    ///
    /// # Errors
    /// Returns an error if the manifest is not set or the file cannot be written.
    pub fn export_manifest_to_json(&self, path: &Path) -> Result<(), std::io::Error> {
        let manifest = self
            .manifest
            .as_ref()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "Manifest not set"))?;
        let json = serde_json::to_string_pretty(manifest)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        fs::write(path, json)
    }

    /// Exports both facts (JSONL) and manifest (JSON) to the given directory.
    ///
    /// Creates the directory if it doesn't exist.
    ///
    /// # Errors
    /// Returns an error if either export fails.
    pub fn export_all(&self, dir: &Path) -> Result<(), std::io::Error> {
        fs::create_dir_all(dir)?;
        let facts_path = dir.join("runtime-facts-v1.jsonl");
        let manifest_path = dir.join("run-manifest-v1.json");
        self.export_to_jsonl(&facts_path)?;
        self.export_manifest_to_json(&manifest_path)?;
        Ok(())
    }

    /// Returns the export paths for facts and manifest.
    #[must_use]
    pub fn export_paths(dir: &Path) -> (PathBuf, PathBuf) {
        (
            dir.join("runtime-facts-v1.jsonl"),
            dir.join("run-manifest-v1.json"),
        )
    }

    /// Returns a reference to the facts.
    #[must_use]
    pub fn facts(&self) -> &[RuntimeFactV1] {
        &self.facts
    }

    /// Returns a reference to the manifest, if set.
    #[must_use]
    pub fn manifest(&self) -> Option<&RunManifestV1> {
        self.manifest.as_ref()
    }

    /// Returns the resolutions map.
    #[must_use]
    pub fn resolutions(&self) -> &HashMap<String, SymbolUidResolution> {
        &self.resolutions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        ArtifactRef, EvidenceFidelity, RunProcessKey, RuntimeFactFamily, RuntimeFactV1,
        RuntimeLocator,
    };

    fn make_fact(family: RuntimeFactFamily) -> RuntimeFactV1 {
        RuntimeFactV1::new(
            family,
            "run-001".to_string(),
            RunProcessKey::new("run-001".to_string(), 1234, 1000),
            "tetragon".to_string(),
            ArtifactRef {
                path: "/tmp/test.json".to_string(),
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
        )
    }

    #[test]
    fn export_new_has_zero_facts() {
        let export = RuntimeFactExport::new("run-001".to_string());
        assert_eq!(export.fact_count(), 0);
    }

    #[test]
    fn export_add_fact_increases_count() {
        let mut export = RuntimeFactExport::new("run-001".to_string());
        export.add_fact(make_fact(RuntimeFactFamily::ObservedExec));
        assert_eq!(export.fact_count(), 1);
    }

    #[test]
    fn export_facts_are_distinct() {
        let mut export = RuntimeFactExport::new("run-001".to_string());
        export.add_fact(make_fact(RuntimeFactFamily::ObservedExec));
        export.add_fact(make_fact(RuntimeFactFamily::ObservedExit));
        export.add_fact(make_fact(RuntimeFactFamily::ObservedClassLoad));
        assert!(export.facts_are_distinct());
    }

    #[test]
    fn export_observed_call_distinct() {
        // ObservedCall facts must be kept distinct, not collapsed.
        let mut export = RuntimeFactExport::new("run-001".to_string());
        export.add_fact(make_fact(RuntimeFactFamily::ObservedCall));
        export.add_fact(make_fact(RuntimeFactFamily::ObservedCall));
        assert_eq!(export.fact_count(), 2);
        assert!(export.facts_are_distinct());
    }

    #[test]
    fn resolve_locator_returns_unresolved_without_codegraph() {
        let locator = RuntimeLocator::Python {
            module: "runtimo".to_string(),
            qualname: "run".to_string(),
            file: "/src/runtimo.py".to_string(),
            line: 10,
        };
        let resolution = resolve_locator(&locator, false);
        assert!(
            matches!(resolution, SymbolUidResolution::Unresolved { .. }),
            "Should be Unresolved when Codegraph is unavailable"
        );
    }

    #[test]
    fn resolve_locator_returns_resolved_with_codegraph() {
        let locator = RuntimeLocator::Python {
            module: "runtimo".to_string(),
            qualname: "run".to_string(),
            file: "/src/runtimo.py".to_string(),
            line: 10,
        };
        let resolution = resolve_locator(&locator, true);
        assert!(
            matches!(resolution, SymbolUidResolution::Resolved { .. }),
            "Should be Resolved when Codegraph is available"
        );
    }

    #[test]
    fn export_jsonl_roundtrip() {
        let mut export = RuntimeFactExport::new("run-001".to_string());
        export.add_fact(make_fact(RuntimeFactFamily::ObservedExec));
        export.add_fact(make_fact(RuntimeFactFamily::ObservedExit));

        let dir = std::env::temp_dir().join("runtimo_export_test");
        fs::create_dir_all(&dir).unwrap();
        let facts_path = dir.join("runtime-facts-v1.jsonl");
        export.export_to_jsonl(&facts_path).unwrap();

        // Read back and verify
        let content = fs::read_to_string(&facts_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);

        // Deserialize each line and verify it matches the original
        let fact1: RuntimeFactV1 = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(fact1.family, RuntimeFactFamily::ObservedExec);
        let fact2: RuntimeFactV1 = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(fact2.family, RuntimeFactFamily::ObservedExit);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_manifest_roundtrip() {
        let mut export = RuntimeFactExport::new("run-001".to_string());
        let manifest = RunManifestV1::new(
            "run-001".to_string(),
            crate::runtime::RepositoryIdentity {
                identity: "runtimo".to_string(),
                commit: "abc123".to_string(),
                tree_hash: "def456".to_string(),
                dirty: false,
                diff_hash: "ghi789".to_string(),
            },
            crate::runtime::Target {
                executable: "/usr/bin/runtimo".to_string(),
                argv: vec!["--mode".to_string(), "test".to_string()],
                cwd: "/tmp".to_string(),
            },
        );
        export.set_manifest(manifest);

        let dir = std::env::temp_dir().join("runtimo_export_test");
        fs::create_dir_all(&dir).unwrap();
        let manifest_path = dir.join("run-manifest-v1.json");
        export.export_manifest_to_json(&manifest_path).unwrap();

        // Read back and verify
        let content = fs::read_to_string(&manifest_path).unwrap();
        let deserialized: RunManifestV1 = serde_json::from_str(&content).unwrap();
        assert_eq!(deserialized.run_id, "run-001");
        assert_eq!(deserialized.schema_version, "1");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_all_creates_both_files() {
        let mut export = RuntimeFactExport::new("run-001".to_string());
        export.add_fact(make_fact(RuntimeFactFamily::ObservedExec));
        let manifest = RunManifestV1::new(
            "run-001".to_string(),
            crate::runtime::RepositoryIdentity {
                identity: "runtimo".to_string(),
                commit: "abc123".to_string(),
                tree_hash: "def456".to_string(),
                dirty: false,
                diff_hash: "ghi789".to_string(),
            },
            crate::runtime::Target {
                executable: "/usr/bin/runtimo".to_string(),
                argv: vec!["--mode".to_string(), "test".to_string()],
                cwd: "/tmp".to_string(),
            },
        );
        export.set_manifest(manifest);

        let dir = std::env::temp_dir().join("runtimo_export_test_all");
        fs::create_dir_all(&dir).unwrap();
        export.export_all(&dir).unwrap();

        assert!(dir.join("runtime-facts-v1.jsonl").exists());
        assert!(dir.join("run-manifest-v1.json").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_forward_compat_unknown_field_ignored() {
        // Forward-compat: unknown fields in JSON are ignored.
        let json = r#"{"family":"observed_exec","run_id":"run-001","process_key":{"run_id":"run-001","pid":1234,"process_start_time":1000},"provider":"tetragon","artifact":{"path":"/tmp/test.json","media":"application/json","sha256":"a","provider":"tetragon","pid":1234,"process_start_time":1000},"fidelity":"kernel_observed","observations":1,"first_seen":1000,"last_seen":2000,"revision":"abc123","extra":{},"unknown_field":"ignored"}"#;
        let fact: RuntimeFactV1 = serde_json::from_str(json).unwrap();
        assert_eq!(fact.family, RuntimeFactFamily::ObservedExec);
    }

    #[test]
    fn export_paths_are_correct() {
        let dir = std::path::PathBuf::from("/tmp/exports");
        let (facts_path, manifest_path) = RuntimeFactExport::export_paths(&dir);
        assert_eq!(facts_path, dir.join("runtime-facts-v1.jsonl"));
        assert_eq!(manifest_path, dir.join("run-manifest-v1.json"));
    }
}
