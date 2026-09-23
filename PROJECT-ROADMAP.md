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

## Post-v0.10.1 Carry-Forward

| Priority | Item | Why retained | Current state | Definition of done |
|----------|------|-------------|---------------|-------------------|
| P1 | sampler/load-sensitive self-test (`observe_fixture_b_integration`, `observe_self_test_run_exits_zero`) | Deterministic aggregation vs liveness; constrained+CI agree; prod timing not weakened | Known same-known failures in container; sampler yields 0 samples at 50Hz | Deterministic aggregation agrees with CI; prod timing not weakened; future debt not v0.10.1 blocker |
| P1 | session/logging policy | 4 findings (`cli/src/main.rs:2567`, `:838`, `core/src/executor.rs:912`, `:917`); classify identifiers, threat model/log boundary, redact/retain, preserve rationale in private engineering decision system | Identified; classification pending | Identifiers classified; threat model/log boundary defined; redact/retain policy documented |
| P1 | config/test isolation residual | EnvGuard improved but file mutation/read audit needed; inventory `XDG_CONFIG_HOME`/`HOME`/`RUNTIMO_*/config` files; readers-vs-writers; explicit config over global locking; builtin-default tests cannot consume operator config | EnvGuard RAII exists; audit incomplete | File mutation/read audit complete; readers-vs-writers documented; builtin-default tests isolated from operator config |
| P2 | malformed sample-rate fallback | `RUNTIMO_OBSERVE_SAMPLE_HZ=bad`; precedence+fallback from source; deterministic regression; docs/source/tests agree | Precedence documented; malformed env fallthrough verified | Deterministic regression test added; docs/source/tests agree on fallback behavior |
| P2 | retention contract | Provisional 7d; writer/reader/cleanup consumer; stable public key; configured value changes behavior; no vaporware | Provisional; `core/src/config.rs` | Consumer analysis complete; stable public key defined; configured value behavior documented |
| P2 | `max_bundle_bytes` residual | Cross-ref TODO.md; enumerate occurrences; live vs dead vs stale; consumer analysis before remove; compile/tests prove no phantom | Field exists but not wired to rotation | Occurrences enumerated; consumer analysis complete; compile/tests prove no phantom usage |
| P2 | oracle `--properties` e2e | Real CLI path: parsing, evidence-source selection, selector/operator/quantifier, verdict, exit, malformed-spec; impl presence ≠ complete | `--properties` flag exists; e2e path unverified | Full e2e path verified end-to-end; malformed-spec handled; exit code correct |
| P2 | destructive-test containment invariant | "Test proving rejection of irreversible op must itself be incapable of performing it if rejection fails"; fixtures cannot reach real side effects; blocklist-off stays safe; failed assert = test fail never host mutation; CI proves containment | Invariant stated; CI proves containment | All destructive tests verified incapable of host mutation if rejection fails |
| P3 | dep warning hygiene | Non-blocking; enumerate dup versions; unavoidable splits vs drift; consolidate only justified; no cosmetic churn | Warnings exist; consolidation pending | Duplicate versions enumerated; consolidation justified; no cosmetic churn |

## Release-process invariants learned from v0.10.1

1. Self-reported PASS ≠ evidence — a test passing on one machine does not prove it passes everywhere; ambient conditions (container scheduler, load) can mask failures.
2. Ambient config ≠ default evidence — a test passing with ambient environment variables does not prove it passes with clean defaults; validate with explicit config roots.
3. Clean config root for default validation — default behavior must be verified with a clean config root, not inherited ambient state.
4. Rejection tests fail safely — a test proving rejection of an irreversible operation must itself be incapable of performing that operation if the rejection fails.
5. Freeze immutable SHA — once a version tag is published, the SHA must be frozen and never moved or rewritten.
6. Exact-SHA CI before tag — CI must run against the exact SHA that will be tagged, not a later commit.
7. Never move/rewrite published tag — published tags are immutable history; never rebase, amend, or force-push to change what a tag points to.
8. Annotation consistency ≠ semantic correctness — annotations can be consistent with each other while still being semantically wrong; structural analysis augments but never replaces runtime/testing.
9. Structural analysis augments runtime/testing evidence; it does not replace it.
10. Historical changelog sections at tag boundaries — changelog sections must respect immutable tag boundaries; post-tag work must not be attributed to a prior tag.
11. Never fabricate SymbolUID/evidence identities — identity must come only from an authoritative resolver; unknown/unavailable/not-observed must not silently become safe/false.
12. Unknown/unavailable/not-observed must not silently become safe/false — absence of evidence is not evidence of safety; unknown states must remain explicitly unknown.
