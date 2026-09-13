# Runtime Contracts

Versioned runtime contracts — taxonomy + provenance, not acquisition.

## Module Overview

**Module**: `core/src/runtime/`

Each contract is designed for demonstrated consumers only. Fields without consumers are rejected.

## Contracts

| Contract | Writer | Reader | Serialization | Versioning |
|----------|--------|--------|---------------|------------|
| `ProviderStatus` | Provider Supervisor | WAL, CLI | serde JSON | semver |
| `RunProcessKey` | Executor | WAL, RuntimeFactV1 | serde JSON | semver |
| `RunManifestV1` | Executor | WAL, CLI | serde JSON | `schema_version` |
| `RuntimeLocator` | Providers | WAL, CLI | serde JSON | semver |
| `EvidenceFidelity` | Providers | RuntimeFactV1 | serde JSON | enum (stable) |
| `RuntimeFactV1` | Providers | WAL, CLI, Oracle | serde JSON | semver |

## Invariants

- All contracts are serializable with `serde` and comparable with `PartialEq + Eq`.
- Unknown fields are ignored during deserialization (forward-compat).
- `RunProcessKey` equality includes `process_start_time` — same PID with different start_time are distinct keys.
- `NOT OBSERVED != FALSE` — absence of evidence is not evidence of absence.
- PID is never used alone; always paired with `process_start_time`.
- No raw provider output types leak into `RuntimeLocator`.
- No `SymbolUID` inside raw adapters.
- No custom unwinder/parser/symbolizer is introduced.
- `first_seen <= last_seen` always holds in `RuntimeFactV1`.
- `run_id` and `revision` must not be empty in `RuntimeFactV1::new()`.

## Evidence Fidelity

| Variant | Description |
|---------|-------------|
| `ExactRuntime` | Evidence from exact runtime instrumentation (e.g., JFR class-load, EventPipe exception) |
| `KernelObserved` | Evidence from kernel-level observation (e.g., Tetragon exec) |
| `Sampled` | Evidence from sampling (e.g., OTel sampled traces) |
| `Derived` | Evidence derived from inference (not directly observed) |

## Provider Status

| Variant | Description |
|---------|-------------|
| `Unavailable` | Provider is not available (no fallback) |
| `Degraded { provider, version, mode, reason }` | Provider is available but degraded; `reason` carries the degradation reason |
| `Active { provider, version, mode }` | Provider is fully operational |
| `Failed { provider, version, mode, reason }` | Provider has failed; `reason` carries the failure reason |

## RunProcessKey

Key that uniquely identifies a running process, preventing PID-reuse ambiguity.

- `run_id`: Unique run identifier
- `pid`: Process ID (never used alone)
- `process_start_time`: From `/proc/pid/stat` field 22 (boot-time anchored)

## RuntimeFactV1

A single runtime fact with full provenance.

- `family`: RuntimeFactFamily enum
- `run_id`: Unique run identifier
- `process_key`: RunProcessKey (PID + start_time)
- `provider`: Provider name
- `artifact`: ArtifactRef
- `fidelity`: EvidenceFidelity
- `observations`: Observation count
- `first_seen`: First seen timestamp (Unix)
- `last_seen`: Last seen timestamp (Unix)
- `revision`: Git commit
- `extra`: Additional key-value pairs

## RuntimeFactFamily

| Variant | Provider | Fidelity | Status |
|---------|----------|----------|--------|
| `ObservedExec` | Tetragon | KernelObserved | IMPORT |
| `ObservedExit` | Tetragon | KernelObserved | IMPORT |
| `ObservedClassLoad` | JFR | ExactRuntime | IMPORT |
| `ObservedException` | JFR | ExactRuntime | IMPORT |
| `ObservedCall` | sema | Sampled | DEFERRED (OTel blocked) |

## Deferred Families

- `ObservedCall` from `sema` → `Sampled` is DEFERRED because OTel is blocked.
- `ObservedLibraryLoad`, `ObservedFileAccess`, `ObservedNetworkCall`, `ObservedProtocolCall`, `ObservedDatabaseCall`, `ObservedQueueOperation`, `ObservedAssemblyLoad` are NOT created — no demonstrated consumer.
