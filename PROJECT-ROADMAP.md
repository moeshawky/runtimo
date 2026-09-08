# Runtimo Project Roadmap

Intent: capability execution engine with telemetry, WAL, backup/undo, resource guards.

## Breaks (fix → proof)

| # | Break | Fix | Proof |
|---|-------|-----|-------|
| 1 | Stale README file:line refs | Remove line numbers, keep module refs | README: no `:NNN` refs in Observe section |
| 2 | `max_bundle_bytes` vaporware | Removed from config; CHANGELOG notes it | grep `max_bundle_bytes` → only CHANGELOG |
| 3 | fail-closed verify | `verify_bundle` returns `hash_ok=false` on read/parse failure | bundle.rs, self-test #4 |
| 4 | Immutable restart | Restart truncates file, seq restarts at 0 | bundle.rs, test `bundle_restart_truncates_fresh_chain` |
| 5 | Started/Suspended states | `ObserveStarted` on creation, `ObserveSuspended` on pressure | supervisor.rs, test `supervisor_emits_started_and_suspended` |
| 6 | suspend_ms wiring | `pressure_suspend_ms` → `cooldown_secs` → `ObserveBudget` | config.rs, supervisor.rs |
| 7 | Single-drain | `gated_tick` fans sampler + `audit.drain()` → `bundle.append` | supervisor.rs |
| 8 | DAL-A Halt never kills target | Watermark `INCOMPLETE` only; target exits naturally | supervisor.rs, test `supervisor_target_never_signalled_on_failure` |

## Remaining work

- `--burst` P2B file-watch (deferred, polling fallback)
- Daemon `observe_burst` RPC (`-32601` deferred)
- `RUNTIMO_OBSERVE_SAMPLE_HZ=bad` env fallthrough verification (unverified)
- 7d retention config key name provisional (`core/src/config.rs`)

## v1.0.0 — Python bindings

**Goal:** Python bindings for the capability runtime.
**Scope-creep defenses still apply** — no in-target code injection, no `ptrace`, no `LD_PRELOAD`, collector `Halt` never signals target, bounded channels drop newest + `TRUNCATED` marker, secrets redacted at every WAL boundary.
**Acceptance sketch:**
- Importable module (`import runtimo` or similar)
- Capability parity for FileRead / FileWrite / ShellExec minimum
- Docs (usage + safety model)

## Scope-creep defenses

- No in-target code injection (P1A out-of-process only)
- No `ptrace` stop >1 ms
- No `LD_PRELOAD`
- Collector `Halt` never signals target
- Bounded channels drop newest + `TRUNCATED` marker, never silent zeros
- Secrets redacted at every WAL boundary
