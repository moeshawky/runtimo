# Tetragon + JFR Adapter Slice

## Summary

This slice implements the Tetragon (pinned v1.7.1, monitor-only) and JFR (JDK17 jcmd) provider adapters for the runtimo runtime. Raw artifacts are hashed in manifest+WAL, reduced to `RuntimeFactV1` facts, and verified with fixtures.

## Adapter Paths

| Adapter | Path | Provider |
|---------|------|----------|
| Tetragon | `core/src/adapters/tetragon.rs` | `tetragon` v1.7.1 |
| JFR | `core/src/adapters/jfr.rs` | `jfr` JDK17 |
| Reducer | `core/src/adapters/reducer.rs` | Both |
| Module | `core/src/adapters/mod.rs` | — |

## Fixture Paths

| Fixture | Path | Description |
|---------|------|-------------|
| `fixtures/mod.rs` | `core/tests/fixtures/mod.rs` | All fixture definitions |
| `adapters_integration.rs` | `core/tests/adapters_integration.rs` | Integration tests |

## Matrix Deltas

| Entry | Status | Description |
|-------|--------|-------------|
| `exec_exit_42` | `VERIFIED` | Child exec exit 42 + .so load |
| `jvm_class_exception` | `VERIFIED` | JVM class load + throw/catch |
| `degrade_binary_absent` | `DEGRADED` | Tetragon binary absent |
| `degrade_priv_denied` | `DEGRADED` | JFR permission denied |
| `pid_reuse_distinct` | `VERIFIED` | PID+start_time distinct |
| `no_observed_call` | `VERIFIED` | No ObservedCall regression |
| `rerun_no_regression` | `VERIFIED` | Re-run produces same results |

## Gate Outputs

- `cargo clippy --all-targets`: Must pass with zero new warnings
- `cargo test -p runtimo-core --lib`: Must pass
- `cargo test -p runtimo-core`: Must pass
- Artifact files exist + SHA match: Verified
- Matrix entries VERIFIED/PARTIAL/DEGRADED per fixture: Verified

## Prerequisites

See `docs/TETRAGON_JFR_PREREQS.md` for full prerequisites and fallback behavior.

## Key Invariants

1. **No enforcement**: Tetragon uses `--monitor` only; no enforcement policies.
2. **Raw custody**: Raw artifacts (JSON, .jfr) preserved as files; only SHA-256 hashes enter WAL.
3. **PID never alone**: `RunProcessKey` always includes `process_start_time`.
4. **No ObservedCall**: Per 006 pivot, `ObservedCall` shape is deferred.
5. **No SymbolUID**: Never appears in raw adapters.
6. **No custom syscall/parser**: No custom syscall, ancestry, loader, JVMTI, or parser introduced.
7. **Fidelity preserved**: `KernelObserved` for Tetragon, `ExactRuntime` for JFR — no flattening.
8. **Secrets redacted**: No secrets in artifact paths or data.
9. **Untrusted output never executed**: Raw artifacts are read, never executed.

## Ownership

| Component | Writer | Reader |
|-----------|--------|--------|
| `TetragonAdapter` | Tetragon provider | WAL, CLI |
| `JfrAdapter` | JFR provider | WAL, CLI |
| `ArtifactReducer` | Reducer | WAL, CLI, Oracle |
| `RuntimeFactV1` | Providers | WAL, CLI, Oracle |

## Deferred

- `ObservedCall` from `sema` → `Sampled` is DEFERRED (OTel blocked). Shape defined but not produced.
- OTel/OBI/EventPipe/symbolic are DEFERRED per the 006 pivot.
