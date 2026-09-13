# Tetragon + JFR Adapter Prerequisites

## Overview

This document describes the prerequisites for the Tetragon (v1.7.1 monitor-only) and JFR (JDK17 jcmd) provider adapters.

## System Requirements

### Tetragon Adapter

| Requirement | Value |
|-------------|-------|
| Binary | `tetragon` |
| Pinned Version | 1.7.1 |
| Mode | `--monitor` (observation-only, no enforcement) |
| Kernel | ≥ 5.10 |
| Architecture | x86_64, aarch64 |

### JFR Adapter

| Requirement | Value |
|-------------|-------|
| Binary | `jcmd` |
| Pinned JDK | 17 |
| Mode | External attach (no in-process instrumentation) |
| Architecture | x86_64, aarch64 |

## Installation

### Tetragon

```bash
# Install Tetragon v1.7.1
# See: https://github.com/aquasecurity/tetragon/releases
# The adapter detects the binary via `which tetragon` or common paths.
# If not found, the adapter reports ProviderStatus::Unavailable.
```

### JFR

```bash
# JDK 17 with jcmd
# The adapter detects jcmd via `which jcmd` or common paths.
# If not found, the adapter reports ProviderStatus::Unavailable.
```

## Fallback Behavior

When the provider binary is absent:

1. **`TetragonAdapter::start()`** returns `AdapterStartResult::Unavailable`
2. **`JfrAdapter::start()`** returns `AdapterStartResult::Unavailable`
3. The reducer still produces `RuntimeFactV1` facts from any available artifacts
4. Tests pass with `MatrixEntry::Degraded` for the affected provider

### Degraded Paths

| Condition | Status | Test Result |
|-----------|--------|-------------|
| Binary absent | `Unavailable` | `MatrixEntry::Degraded` |
| Permission denied | `Degraded` | `MatrixEntry::Degraded` |
| Version mismatch | `Degraded` | `MatrixEntry::Degraded` |
| Binary present and active | `Active` | `MatrixEntry::Verified` |

## Artifact Custody

### Raw Artifacts

- **Tetragon**: Raw JSON artifacts are preserved as files in the artifact directory.
- **JFR**: Raw `.jfr` artifacts are preserved as files in the artifact directory.
- **Never forced into WAL volume**: Only SHA-256 hashes enter the manifest and WAL.

### SHA-256 Hashing

Each artifact is hashed using SHA-256. The hash is stored in:
- `ArtifactRef.sha256`
- `RunManifestV1.artifacts`
- WAL events (as hash references)

### File Preservation

Raw artifacts are never deleted by the adapter. They remain in the artifact directory for audit and verification.

## Configuration

### Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `RUNTIMO_WAL_PATH` | WAL file path | `data_dir()/wal.jsonl` |
| `RUNTIMO_DAL` | Design Assurance Level | `A` |
| `RUNTIMO_STATE_DIR` | State directory | `~/.local/share/runtimo` |

### Adapter Configuration

Both adapters use `Default` configuration:
- **Tetragon**: `TetragonConfig::default()` — pinned v1.7.1, monitor mode, no enforcement
- **JFR**: `JfrConfig::default()` — JDK 17, exact mode, external attach

## Testing

### Running Tests

```bash
# Run all adapter tests
cargo test -p runtimo-core --lib -- adapters

# Run integration tests
cargo test -p runtimo-core --test adapters_integration

# Run fixture tests
cargo test -p runtimo-core --test integration fixtures

# Run all tests
cargo test -p runtimo-core
```

### Test Matrix

| Fixture | Expected | Description |
|---------|----------|-------------|
| `exec_exit_42` | `Verified` | Child exec exit 42 + .so load |
| `jvm_class_exception` | `Verified` | JVM class load + throw/catch |
| `degrade_binary_absent` | `Degraded` | Binary absent → Unavailable |
| `degrade_priv_denied` | `Degraded` | Permission denied → Degraded |
| `pid_reuse_distinct` | `Verified` | PID+start_time distinct keys |
| `no_observed_call` | `Verified` | No ObservedCall facts |
| `rerun_no_regression` | `Verified` | Same results on re-run |

## Security

### Secrets Redaction

- No secrets are captured in artifact paths or data.
- All artifact data is serialized as JSON with no sensitive fields.
- The `secrets_redacted` test verifies no secret patterns appear.

### Untrusted Output

- Raw provider output is never executed.
- The adapter only reads artifacts; it never executes them.
- `untrusted_output_not_executed` test verifies this invariant.

### Enforcement Never Enabled

- Tetragon adapter uses `--monitor` mode only.
- No enforcement policies are loaded or applied.
- `TetragonConfig::enforcement_enabled` is always `false`.

## ObservedCall Deferral

Per the 006 pivot, `ObservedCall` from `sema` → `Sampled` is DEFERRED because OTel is blocked. The shape is defined but not yet produced by a provider. Neither Tetragon nor JFR adapters produce `ObservedCall` facts.

## Ownership

| Contract | Writer | Reader |
|----------|--------|--------|
| `TetragonAdapter` | Tetragon provider | WAL, CLI |
| `JfrAdapter` | JFR provider | WAL, CLI |
| `ArtifactReducer` | Reducer | WAL, CLI, Oracle |
| `RuntimeFactV1` | Providers | WAL, CLI, Oracle |
