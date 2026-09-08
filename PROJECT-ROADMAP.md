# Runtimo Project Roadmap

Intent: capability execution engine with telemetry, WAL, backup/undo, resource guards.

## Breaks (fix → proof)

| # | Break | Fix | Proof |
|---|-------|-----|-------|
| 1 | Stale README file:line refs | Remove line numbers, keep module refs | README: no `:NNN` refs in Observe section |
| 2 | `max_bundle_bytes` vaporware | Removed from config; CHANGELOG notes it | grep `max_bundle_bytes` → only CHANGELOG |
| 3 | fail-closed verify | `verify_bundle` returns `hash_ok=false` on read/parse failure | bundle.rs:511, self-test #4 |
| 4 | Immutable restart | Restart truncates file, seq restarts at 0 | bundle.rs:127,152,173; test `bundle_restart_truncates_fresh_chain` |
| 5 | Started/Suspended states | `ObserveStarted` on creation, `ObserveSuspended` on pressure | supervisor.rs:183,269,324; test `supervisor_emits_started_and_suspended` |
| 6 | suspend_ms wiring | `pressure_suspend_ms` → `cooldown_secs` → `ObserveBudget` | config.rs:117, supervisor.rs:207 |
| 7 | Single-drain | `gated_tick` fans sampler + `audit.drain()` → `bundle.append` | supervisor.rs:289 |
| 8 | DAL-A Halt never kills target | Watermark `INCOMPLETE` only; target exits naturally | supervisor.rs:306-338; test `supervisor_target_never_signalled_on_failure` |

## Remaining work

- `--burst` P2B file-watch (deferred, polling fallback)
- Daemon `observe_burst` RPC (`-32601` deferred)
- `RUNTIMO_OBSERVE_SAMPLE_HZ=bad` env fallthrough verification (unverified)
- 7d retention config key name provisional (`core/src/config.rs:109`)

## Scope-creep defenses

- No in-target code injection (P1A out-of-process only)
- No `ptrace` stop >1 ms
- No `LD_PRELOAD`
- Collector `Halt` never signals target
- Bounded channels drop newest + `TRUNCATED` marker, never silent zeros
- Secrets redacted at every WAL boundary
