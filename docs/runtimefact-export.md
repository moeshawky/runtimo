# Runtime Fact Export — `runtime-facts-v1.jsonl` + `run-manifest-v1.json`

> Bridge module for versioned forward-compat export of runtime facts and run manifests.
> Activation: 009. No Codegraph import. RuntimeLocator→SymbolUID|unresolved.

## Overview

The `RuntimeFactExport` module produces two versioned artifacts from runtime facts:

| Artifact | Format | Path | Version |
|----------|--------|------|---------|
| `runtime-facts-v1.jsonl` | JSONL (one fact per line) | `{dir}/runtime-facts-v1.jsonl` | `v1` |
| `run-manifest-v1.json` | JSON (single manifest) | `{dir}/run-manifest-v1.json` | `schema_version: "1"` |

Both files are forward-compatible: unknown fields are ignored during deserialization.

## Export Paths

```rust
use runtimo_core::runtime::RuntimeFactExport;
use std::path::Path;

let dir = Path::new("/tmp/runtimo-exports");
let (facts_path, manifest_path) = RuntimeFactExport::export_paths(dir);
// facts_path: /tmp/runtimo-exports/runtime-facts-v1.jsonl
// manifest_path: /tmp/runtimo-exports/run-manifest-v1.json
```

## RuntimeLocator → SymbolUID | Unresolved

Each `RuntimeLocator` can be resolved to a `SymbolUID` via [`resolve_locator`].

### Resolution Outcomes

| Outcome | Meaning | When |
|---------|---------|------|
| `Unresolved(String)` | No real resolver wired; locator documented but not resolved | Default (no Codegraph resolver) |
| `Unresolved(String)` | Codegraph available but resolver not wired; `codegraph_available=true` does NOT fabricate | Even when `codegraph_available=true` |

### No Codegraph Import

The export works **without** Codegraph. When `codegraph_available` is `false` (the default),
`resolve_locator` returns `SymbolUidResolution::Unresolved` with a documented reason:

> "Codegraph unavailable — export works without Codegraph; locator documented but not resolved"

This ensures the export is useful without Codegraph while preserving the ability to resolve
when Codegraph is available.

### SymbolUID in Adapters

**`SymbolUID` never appears inside raw adapters.** This is enforced by:
- The adapter invariant: "No SymbolUID inside raw adapters"
- A grep test verifying no `SymbolUID` in `core/src/adapters/`

### Resolution Truth

`resolve_locator()` returns `SymbolUidResolution::Unresolved` always, even when `codegraph_available=true`. The old `codegraph_available=true` path that fabricated deterministic `file:line:kind` identity violated §56 evidence custody and has been removed. The only change when `codegraph_available=true` is the reason string. Fabrication is forbidden; `Resolved` only when a real resolver is wired.

## Observed* Distinct Calls

`Observed*` facts are kept **distinct** — never collapsed into a single entry.

| Family | Provider | Fidelity | Status |
|--------|----------|----------|--------|
| `ObservedExec` | Tetragon | `KernelObserved` | IMPORT |
| `ObservedExit` | Tetragon | `KernelObserved` | IMPORT |
| `ObservedClassLoad` | JFR | `ExactRuntime` | IMPORT |
| `ObservedException` | JFR | `ExactRuntime` | IMPORT |
| `ObservedCall` | sema | `Sampled` | DEFERRED (OTel blocked) |

Each `ObservedCall` fact is stored as a distinct entry. The `facts_are_distinct()` method
verifies no collapsing has occurred.

## Forward-Compat

All JSON/JSONL exports tolerate unknown fields during deserialization. This is achieved
through serde's default behavior (ignore unknown fields) and explicit test cases.

## Manifest No Secrets

`RunManifestV1` contains no raw environment variables or secrets. Environmental identity
is hashed or whitelisted. The manifest includes:

- `schema_version: "1"`
- `run_id`
- `repository` (identity, commit, tree_hash, dirty, diff_hash)
- `target` (executable, argv, cwd)
- `process` (pid + process_start_time — PID never alone)
- `environment` (kernel, arch, runtimes)
- `providers` (name, version, mode, config, binary_hash)
- `timing` (start_time, end_time)
- `artifacts` (path, media, sha256, provider)
- `completeness` (provider_status, watermark)

## Usage

```rust
use runtimo_core::runtime::{
    RuntimeFactExport, RuntimeFactV1, RuntimeFactFamily, RuntimeLocator,
    RunProcessKey, EvidenceFidelity, ArtifactRef,
};
use std::path::Path;

// Create export
let mut export = RuntimeFactExport::new("run-001".to_string());

// Add facts (kept distinct)
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
    },
    EvidenceFidelity::KernelObserved,
    1,
    1000,
    2000,
    "abc123".to_string(),
));

// Resolve a locator (returns Unresolved without Codegraph)
let locator = RuntimeLocator::Python {
    module: "runtimo".to_string(),
    qualname: "run".to_string(),
    file: "/src/runtimo.py".to_string(),
    line: 10,
};
let resolution = export.resolve_locator(&locator);
// resolution is SymbolUidResolution::Unresolved { .. }

// Export to directory
export.export_all(Path::new("/tmp/runtimo-exports"))?;
```

## Roundtrip Test

The export module includes roundtrip tests:
- `export_jsonl_roundtrip` — Facts exported to JSONL and read back match originals
- `export_manifest_roundtrip` — Manifest exported to JSON and read back matches originals
- `export_all_creates_both_files` — Both files are created in the directory

## Grep Verification

```bash
# Verify no SymbolUID in adapters
grep -r "SymbolUID" core/src/adapters/
# Expected: No matches (SymbolUID never appears in raw adapters)
```

## Versioning

- `runtime-facts-v1.jsonl` — Version `v1`, forward-compatible
- `run-manifest-v1.json` — `schema_version: "1"`, forward-compatible
- New versions are semver-breaking; existing data remains readable
