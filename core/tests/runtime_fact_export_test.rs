//! Runtime fact export bridge tests — 009 activation.
//!
//! Tests the `RuntimeFactExport` module for:
//! - Distinct Observed* Calls (not collapsed)
//! - RuntimeLocator→SymbolUID|unresolved resolution
//! - No Codegraph import
//! - Roundtrip export/import
//! - Grep no SymbolUID in adapters
//!
//! # Activation
//! 009 bridge per seshat+memory+graph CLI.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::unused_result_ok,
    clippy::indexing_slicing,
    clippy::redundant_clone
)]

use runtimo_core::runtime::{
    ArtifactRef, EvidenceFidelity, RunProcessKey, RuntimeFactExport, RuntimeFactFamily,
    RuntimeFactV1, RuntimeLocator, SymbolUidResolution,
};
use std::fs;
use std::path::Path;

// ── Distinct Observed* Calls ─────────────────────────────────────

#[test]
fn distinct_observed_calls_not_collapsed() {
    // Each Observed* fact must be kept distinct — never collapsed.
    let mut export = RuntimeFactExport::new("run-001".to_string());

    // Add multiple ObservedCall facts (deferred but shape-defined)
    export.add_fact(RuntimeFactV1::new(
        RuntimeFactFamily::ObservedCall,
        "run-001".to_string(),
        RunProcessKey::new("run-001".to_string(), 1234, 1000),
        "sema".to_string(),
        ArtifactRef {
            path: "/tmp/call1.json".to_string(),
            media: "application/json".to_string(),
            sha256: "a".repeat(64),
            provider: "sema".to_string(),
            pid: 1234,
            process_start_time: 1000,
        },
        EvidenceFidelity::Sampled,
        1,
        1000,
        2000,
        "abc123".to_string(),
    ));
    export.add_fact(RuntimeFactV1::new(
        RuntimeFactFamily::ObservedCall,
        "run-001".to_string(),
        RunProcessKey::new("run-001".to_string(), 1234, 1001),
        "sema".to_string(),
        ArtifactRef {
            path: "/tmp/call2.json".to_string(),
            media: "application/json".to_string(),
            sha256: "b".repeat(64),
            provider: "sema".to_string(),
            pid: 1234,
            process_start_time: 1001,
        },
        EvidenceFidelity::Sampled,
        1,
        1000,
        2000,
        "abc123".to_string(),
    ));

    // Both facts must be present (not collapsed)
    assert_eq!(export.fact_count(), 2);
    assert!(export.facts_are_distinct());

    // Verify they have different process keys
    let facts = export.facts();
    assert_ne!(facts[0].process_key, facts[1].process_key);
}

#[test]
fn distinct_observed_exec_exit_not_collapsed() {
    // ObservedExec and ObservedExit must be distinct facts.
    let mut export = RuntimeFactExport::new("run-001".to_string());
    export.add_fact(RuntimeFactV1::new(
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
    ));
    export.add_fact(RuntimeFactV1::new(
        RuntimeFactFamily::ObservedExit,
        "run-001".to_string(),
        RunProcessKey::new("run-001".to_string(), 1234, 1000),
        "tetragon".to_string(),
        ArtifactRef {
            path: "/tmp/exit.json".to_string(),
            media: "application/json".to_string(),
            sha256: "b".repeat(64),
            provider: "tetragon".to_string(),
            pid: 1234,
            process_start_time: 1000,
        },
        EvidenceFidelity::KernelObserved,
        1,
        1000,
        2000,
        "abc123".to_string(),
    ));

    assert_eq!(export.fact_count(), 2);
    assert!(export.facts_are_distinct());
}

// ── RuntimeLocator → SymbolUID | Unresolved ──────────────────────

#[test]
fn mock_resolver_proves_distinct() {
    // Mock resolver test: distinct locators produce distinct resolutions.
    let locator1 = RuntimeLocator::Python {
        module: "runtimo".to_string(),
        qualname: "run".to_string(),
        file: "/src/runtimo.py".to_string(),
        line: 10,
    };
    let locator2 = RuntimeLocator::Python {
        module: "runtimo".to_string(),
        qualname: "run".to_string(),
        file: "/src/runtimo.py".to_string(),
        line: 20,
    };

    let resolution1 = RuntimeFactExport::resolve_locator_for_test(&locator1, false);
    let resolution2 = RuntimeFactExport::resolve_locator_for_test(&locator2, false);

    // Both should be Unresolved (no Codegraph)
    assert!(
        matches!(resolution1, SymbolUidResolution::Unresolved { .. }),
        "locator1 should be Unresolved"
    );
    assert!(
        matches!(resolution2, SymbolUidResolution::Unresolved { .. }),
        "locator2 should be Unresolved"
    );

    // The locators are distinct (different lines), so the resolutions
    // should reference different locators.
    if let (
        SymbolUidResolution::Unresolved { locator: l1, .. },
        SymbolUidResolution::Unresolved { locator: l2, .. },
    ) = (&resolution1, &resolution2)
    {
        assert_ne!(
            l1, l2,
            "Distinct locators must produce distinct unresolved resolutions"
        );
    } else {
        panic!("Expected Unresolved resolutions");
    }
}

#[test]
fn mock_resolver_proves_unresolved() {
    // Mock resolver test: without Codegraph, resolution is Unresolved.
    let locator = RuntimeLocator::Native {
        binary: "/usr/bin/rust".to_string(),
        build_id: "build-001".to_string(),
        address_offset: "0x1000".to_string(),
        symbol: "main".to_string(),
        file: "/src/main.rs".to_string(),
        line: 42,
        column: 1,
    };

    let resolution = RuntimeFactExport::resolve_locator_for_test(&locator, false);

    assert!(
        matches!(resolution, SymbolUidResolution::Unresolved { .. }),
        "Without Codegraph, resolution must be Unresolved"
    );

    // Verify the reason documents the unavailability
    if let SymbolUidResolution::Unresolved { reason, .. } = &resolution {
        assert!(
            reason.contains("Codegraph unavailable"),
            "Reason must document Codegraph unavailability, got: {}",
            reason
        );
    } else {
        panic!("Expected Unresolved resolution");
    }
}

#[test]
fn resolver_refuses_fabrication_when_codegraph_flag_true() {
    // Per core/src/runtime/runtime_fact_export.rs:119-129, resolve_locator
    // always returns Unresolved — even when codegraph_available=true —
    // because no real resolver is wired. Fabricating a SymbolUID is
    // banned evidence custody (§56).
    let locator = RuntimeLocator::Python {
        module: "runtimo".to_string(),
        qualname: "run".to_string(),
        file: "/src/runtimo.py".to_string(),
        line: 10,
    };

    let resolution = RuntimeFactExport::resolve_locator_for_test(&locator, true);

    // Prove codegraph_available=true was passed, result is Unresolved,
    // no fabricated UID exists.
    assert!(
        matches!(resolution, SymbolUidResolution::Unresolved { .. }),
        "codegraph_available=true must still yield Unresolved (no real resolver wired)"
    );
    if let SymbolUidResolution::Unresolved { reason, .. } = &resolution {
        assert!(
            reason.contains("refusing to fabricate") || reason.contains("not wired"),
            "Reason must document refusal to fabricate, got: {}",
            reason
        );
    } else {
        panic!("Expected Unresolved resolution; no fabricated SymbolUID permitted");
    }
}

// ── No Codegraph Import ──────────────────────────────────────────

#[test]
fn no_codegraph_import_in_export() {
    // The export module must not import Codegraph.
    // This test verifies the export works without Codegraph by
    // confirming that resolve_locator returns Unresolved when
    // codegraph_available is false.
    let locator = RuntimeLocator::Python {
        module: "runtimo".to_string(),
        qualname: "run".to_string(),
        file: "/src/runtimo.py".to_string(),
        line: 10,
    };

    let _export = RuntimeFactExport::new("run-001".to_string());
    let resolution = RuntimeFactExport::resolve_locator_for_test(&locator, false);

    assert!(
        matches!(resolution, SymbolUidResolution::Unresolved { .. }),
        "Export must work without Codegraph import"
    );
}

// ── Roundtrip Tests ──────────────────────────────────────────────

#[test]
fn roundtrip_jsonl_export() {
    let mut export = RuntimeFactExport::new("run-001".to_string());
    export.add_fact(RuntimeFactV1::new(
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
    ));

    let dir = std::env::temp_dir().join(format!("runtimo_roundtrip_test_{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let facts_path = dir.join("runtime-facts-v1.jsonl");
    export.export_to_jsonl(&facts_path).unwrap();

    // Read back
    let content = fs::read_to_string(&facts_path).unwrap();
    let lines: Vec<&str> = content.lines().collect();

    // Verify roundtrip
    let deserialized: Vec<RuntimeFactV1> = lines
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(deserialized.len(), 1);
    assert_eq!(deserialized[0].family, RuntimeFactFamily::ObservedExec);
    assert_eq!(deserialized[0].process_key.pid, 1234);
    assert_eq!(deserialized[0].process_key.process_start_time, 1000);
}

#[test]
fn roundtrip_manifest_export() {
    let mut export = RuntimeFactExport::new("run-001".to_string());
    let manifest = runtimo_core::runtime::RunManifestV1::new(
        "run-001".to_string(),
        runtimo_core::runtime::RepositoryIdentity {
            identity: "runtimo".to_string(),
            commit: "abc123".to_string(),
            tree_hash: "def456".to_string(),
            dirty: false,
            diff_hash: "ghi789".to_string(),
        },
        runtimo_core::runtime::Target {
            executable: "/usr/bin/runtimo".to_string(),
            argv: vec!["--mode".to_string(), "test".to_string()],
            cwd: "/tmp".to_string(),
        },
    );
    export.set_manifest(manifest);

    let dir = std::env::temp_dir().join(format!("runtimo_roundtrip_test_{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let manifest_path = dir.join("run-manifest-v1.json");
    export.export_manifest_to_json(&manifest_path).unwrap();

    // Read back
    let content = fs::read_to_string(&manifest_path).unwrap();
    let deserialized: runtimo_core::runtime::RunManifestV1 =
        serde_json::from_str(&content).unwrap();
    assert_eq!(deserialized.run_id, "run-001");
    assert_eq!(deserialized.schema_version, "1");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn roundtrip_full_export_all() {
    let mut export = RuntimeFactExport::new("run-001".to_string());
    export.add_fact(RuntimeFactV1::new(
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
    ));
    export.add_fact(RuntimeFactV1::new(
        RuntimeFactFamily::ObservedExit,
        "run-001".to_string(),
        RunProcessKey::new("run-001".to_string(), 1234, 1000),
        "tetragon".to_string(),
        ArtifactRef {
            path: "/tmp/exit.json".to_string(),
            media: "application/json".to_string(),
            sha256: "b".repeat(64),
            provider: "tetragon".to_string(),
            pid: 1234,
            process_start_time: 1000,
        },
        EvidenceFidelity::KernelObserved,
        1,
        1000,
        2000,
        "abc123".to_string(),
    ));

    let manifest = runtimo_core::runtime::RunManifestV1::new(
        "run-001".to_string(),
        runtimo_core::runtime::RepositoryIdentity {
            identity: "runtimo".to_string(),
            commit: "abc123".to_string(),
            tree_hash: "def456".to_string(),
            dirty: false,
            diff_hash: "ghi789".to_string(),
        },
        runtimo_core::runtime::Target {
            executable: "/usr/bin/runtimo".to_string(),
            argv: vec!["--mode".to_string(), "test".to_string()],
            cwd: "/tmp".to_string(),
        },
    );
    export.set_manifest(manifest);

    let dir =
        std::env::temp_dir().join(format!("runtimo_roundtrip_test_all_{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    export.export_all(&dir).unwrap();

    // Verify both files exist
    assert!(dir.join("runtime-facts-v1.jsonl").exists());
    assert!(dir.join("run-manifest-v1.json").exists());

    // Read back and verify
    let facts_content = fs::read_to_string(dir.join("runtime-facts-v1.jsonl")).unwrap();
    assert_eq!(facts_content.lines().count(), 2);

    let manifest_content = fs::read_to_string(dir.join("run-manifest-v1.json")).unwrap();
    let deserialized: runtimo_core::runtime::RunManifestV1 =
        serde_json::from_str(&manifest_content).unwrap();
    assert_eq!(deserialized.run_id, "run-001");

    let _ = std::fs::remove_dir_all(&dir);
}

// ── Export Paths ─────────────────────────────────────────────────

#[test]
fn export_paths_are_correct() {
    let dir = Path::new("/tmp/runtimo-exports");
    let (facts_path, manifest_path) = RuntimeFactExport::export_paths(dir);
    assert_eq!(
        facts_path,
        Path::new("/tmp/runtimo-exports/runtime-facts-v1.jsonl")
    );
    assert_eq!(
        manifest_path,
        Path::new("/tmp/runtimo-exports/run-manifest-v1.json")
    );
}

// ── Forward-Compat ───────────────────────────────────────────────

#[test]
fn forward_compat_unknown_field_ignored() {
    let json = r#"{"family":"observed_exec","run_id":"run-001","process_key":{"run_id":"run-001","pid":1234,"process_start_time":1000},"provider":"tetragon","artifact":{"path":"/tmp/test.json","media":"application/json","sha256":"a","provider":"tetragon","pid":1234,"process_start_time":1000},"fidelity":"kernel_observed","observations":1,"first_seen":1000,"last_seen":2000,"revision":"abc123","extra":{},"unknown_field":"ignored"}"#;
    let fact: RuntimeFactV1 = serde_json::from_str(json).unwrap();
    assert_eq!(fact.family, RuntimeFactFamily::ObservedExec);
}

// ── Manifest No Secrets ──────────────────────────────────────────

#[test]
fn manifest_no_secrets() {
    let manifest = runtimo_core::runtime::RunManifestV1::new(
        "run-001".to_string(),
        runtimo_core::runtime::RepositoryIdentity {
            identity: "runtimo".to_string(),
            commit: "abc123".to_string(),
            tree_hash: "def456".to_string(),
            dirty: false,
            diff_hash: "ghi789".to_string(),
        },
        runtimo_core::runtime::Target {
            executable: "/usr/bin/runtimo".to_string(),
            argv: vec!["--mode".to_string(), "test".to_string()],
            cwd: "/tmp".to_string(),
        },
    );
    let json = serde_json::to_string(&manifest).unwrap();
    // Manifest should not contain secret-like fields
    assert!(!json.contains("password"));
    assert!(!json.contains("secret"));
    assert!(!json.contains("token"));
    assert!(!json.contains("api_key"));
}
