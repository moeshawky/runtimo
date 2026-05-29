# RUNTIMO v0.3.0 — Enhanced Code Audit Report

**Date:** 2026-05-29
**Repository:** `/workspace/runtimo`
**Version:** 0.3.0
**Scope:** Full codebase (3 crates, 24 source files, ~6,000 lines Rust)
**Skills:** `code-audit-mindset` + `advanced-debugging` + `hive-mind`
**Compilation:** ✅ `cargo check --all-targets` — 0 errors, 0 warnings
**Tests:** ⚠️ **30 passed / 1 FAILED**

---

## Verification Gates (G1–G5)

| Gate | Command | Result |
|------|---------|--------|
| G1 Evidence | All identifiers verified in source | ✅ All verified |
| G2 Compilation | `cargo check --all-targets` | ✅ Pass (exit 0) |
| G3 Tests | `cargo test -p runtimo-core` | ❌ **1 FAIL** |
| G4 Witness | Re-reviewed after break | ✅ All 7 findings confirmed |
| G5 Deacon | No pre-commit/CI detected | ⚠️ Not configured |

---

## Findings: 7 Total

### 🔴 G-EDGE-1 — `check_disk_space` fails when parent directory does not exist

> **Enhanced with AD Phase 1 root-cause trace + defense-in-depth analysis**

**Root-cause trace:**
```
Symptom: test creates_parent_directories PANICs at integration.rs:242
  "df command failed: .../a/b/c: No such file or directory"
    ↑
Immediate cause: check_disk_space() executes `df -B1 <parent>` where parent doesn't exist
    ↑
Why parent doesn't exist: check_disk_space() runs BEFORE create_dir_all() in FileWrite::execute
    ↑
Root trigger: No conditional check for parent.exists() before df call
    ↑
Source: The validation order is correct intent (check space before writing)
  but the implementation assumes the parent already exists
```

**Defense-in-depth analysis:**

| Layer | Check | Status |
|-------|-------|--------|
| 1 - Entry (FileWrite::execute) | Calls check_disk_space before write | ⚠️ Doesn't handle non-existent parent |
| 2 - Business (check_disk_space) | Calls df on parent path | ❌ Fails when parent missing |
| 3 - Environment (df command) | Cannot report space for non-existent path | ❌ Fundamental limitation |
| 4 - Debug | No debug logging on df failure | ⚠️ Error message indistinguishable from "no space" |

**Compound cascade (AD llm-failure-modes.md):**
- P-4 (Happy-Path Only): Existing tests only cover existing-parent case
- Minimal-Patch Bias: The fix is one line — but **scope matches** (the problem IS that simple)
- Semantic Trap: Falsification condition verified — skipping check when parent doesn't exist is safe

**Falsification condition:** If `!parent.exists()` → `return Ok(())`. Three remaining cases (no space / has space / df error) all handled correctly.

| Field | Value |
|-------|-------|
| **Code** | `G-EDGE-1` |
| **Trigger Scenario** | User calls `FileWrite` on a deep path where parent directories don't exist. `check_disk_space()` runs `df` on non-existent parent → returns `ExecutionFailed` → `create_dir_all` never reached |
| **Impact** | FileWrite cannot create files in new subdirectories. Test panics. |
| **Recommendation** | Add `if !parent.exists() { return Ok(()); }` before the df call. Add debug logging to distinguish "no space" from "path doesn't exist" errors. Add test for non-existent parent case. |
| **Location** | `core/src/capabilities/file_write.rs:245` |
| **Test location** | `core/tests/integration.rs:230` |

---

### 🔴 G-PERF-1 — Config file read on every path validation

> **Enhanced with AD Phase 1 trace**

**Trace:** `validate_path()` → `get_allowed_prefixes(ctx)` → `RuntimoConfig::get_allowed_prefixes()` → `Self::load()` → reads `~/.config/runtimo/config.toml` from disk, parses TOML.

This happens **once per `validate_path()` call**. For a session processing 100 files, that's 100 disk reads and 100 TOML parses of the same static config.

| Field | Value |
|-------|-------|
| **Code** | `G-PERF-1` |
| **Trigger Scenario** | High-volume file operations (100+ files) cause 100 config loads from disk |
| **Impact** | Unbounded performance regression as file operations scale. On NFS or slow disk, adds latency to every operation. |
| **Recommendation** | Use `std::sync::OnceLock` or `lazy_static` to cache config at first load. Load config once per process, not once per operation. |
| **Location** | `core/src/validation/path.rs:74`, `core/src/config.rs:107` |

---

### 🟡 S-DEAD-1 — `SchemaValidator` publicly exported but never consumed

| Field | Value |
|-------|-------|
| **Code** | `S-DEAD-1` |
| **Procedure** | `grep -rn SchemaValidator --include="*.rs"` (no target/) — only definition + doc examples, **zero external consumers** |
| **Evidence** | `lib.rs:85` re-exports it; cli, daemon, tests, examples never instantiate it |
| **Trigger Scenario** | Full implementation exists with type checking and required-fields validation — it's real code that was just never wired into any pipeline |
| **Impact** | Dead public API. Misleading for users expecting JSON Schema validation integration. |
| **Recommendation** | Either remove `pub use schema::SchemaValidator` from lib.rs, or wire it into the capability validation pipeline |
| **Location** | `core/src/schema.rs:27`, `core/src/lib.rs:85` |

---

### 🟡 S-DEAD-2 — `HealthAlert` enum publicly re-exported but never used externally

| Field | Value |
|-------|-------|
| **Code** | `S-DEAD-2` |
| **Procedure** | `grep -rn HealthAlert --include="*.rs"` (no target/) — only used internally in `monitor.rs` |
| **Evidence** | `lib.rs:83` re-exports it; cli and daemon have zero imports |
| **Trigger Scenario** | HealthMonitor tracks alerts internally; the enum is surfaced in public re-export but no consumer uses it |
| **Impact** | Clutters public API with an unusable type |
| **Recommendation** | Remove from public re-export, or expose via `HealthMonitor::alerts() -> Vec<HealthAlert>` in public API |
| **Location** | `core/src/monitor.rs:82`, `core/src/lib.rs:83` |

---

### 🟡 S-DEAD-3 — `run_git` dead convenience wrapper

> **Found via AD methodology: check `#[allow(dead_code)]` in wired modules**

| Field | Value |
|-------|-------|
| **Code** | `S-DEAD-3` |
| **Procedure** | `grep -n "allow(dead_code)" --include="*.rs" -r . | grep -v target/` → found `git_exec.rs:166` |
| **Evidence** | `fn run_git()` marked `#[allow(dead_code)]` — calls `run_git_with_timeout` with hardcoded 300s timeout. 0 external call sites. All real callers use `run_git_with_timeout` directly. |
| **Trigger Scenario** | A convenience wrapper exists for backwards-compatible 5-minute timeout git calls, but no code uses it — all callers specify their own timeout |
| **Impact** | Dead code with a misleading signature. The `#[allow(dead_code)]` suppresses the compiler warning, hiding the issue |
| **Recommendation** | Remove the wrapper function, or document why it's kept. If intentionally reserved for future use, add a comment explaining the design intent. |
| **Location** | `core/src/capabilities/git_exec.rs:166-168` |

---

### 🟡 S-CONTRACT-1 — No `rust-version`/MSRV declaration

| Field | Value |
|-------|-------|
| **Code** | `S-CONTRACT-1` |
| **Procedure** | `grep -rn "rust-version|MSRV" --include="*.toml"` — zero results across all 3 crates |
| **Evidence** | No `rust-version` field in any Cargo.toml |
| **Trigger Scenario** | Users can't discover minimum supported Rust version. Silent breakage on old toolchains. |
| **Recommendation** | Add `rust-version = "1.70"` (or appropriate) to `[package]` in workspace Cargo.toml. Add CI MSRV test job. |
| **Location** | `Cargo.toml`, all member crate Cargo.toml files |

---

### 🟡 S-CONTRACT-2 — No semver compatibility tests

| Field | Value |
|-------|-------|
| **Code** | `S-CONTRACT-2` |
| **Procedure** | `grep -rn "semver|cargo-semver"` across all config files — zero results |
| **Evidence** | No CI workflow tests backward compatibility. Version is 0.3.0 with no automated compat guarantees |
| **Trigger Scenario** | Future change to public struct field or function signature breaks downstream without CI catching it |
| **Recommendation** | Add `cargo-semver-checks` to CI. Test public API stability on every PR. |
| **Location** | Workspace (no CI workflow found) |

---

## Compound Cascade Analysis

| Pattern | Findings | Diagnosis | Action |
|---------|----------|-----------|--------|
| S-ENTROPY + S-DEAD | S-DEAD-1, S-DEAD-2, S-DEAD-3 | 3 dead exports — structural entropy accumulated in public API | Audit all `pub use` in lib.rs; remove or wire each |
| S-CONTRACT-1 + S-CONTRACT-2 | S-CONTRACT-1, S-CONTRACT-2 | Compound contract failure — implied version contracts with no verification | Add MSRV declaration AND semver CI in same PR |
| G-EDGE-1 (isolated) | G-EDGE-1 | Single boundary failure — no cascade | Fix with `if !parent.exists()` guard + debug logging |
| G-PERF-1 (isolated) | G-PERF-1 | Single performance regression | Add config caching via `OnceLock` |

**No cross-boundary cascade** (G-HALL+G-SEC+G-SEM) detected. Code is internally consistent.

---

## LLM Failure Scan

| Check | Result | Evidence |
|-------|--------|----------|
| Minimal-Patch Bias | ✅ Pass | No AI patches reviewed (audit scope) |
| Template Fitting | ✅ Pass | Consistent crate conventions throughout |
| Semantic Trap | ✅ Pass | All conditionals traced with boundary values |
| Plausible-but-Vulnerable | ✅ Pass | ShellExec uses `arg()` not shell interpolation; path validation prevents traversal |
| Stylistic Fingerprint | ✅ Pass | snake_case, consistent error handling, matching patterns |
| **AI-generated likelihood** | **LOW** | No AI signatures detected |

---

## Prior Audit Cross-Reference (Hive-Mind)

A prior audit session (2026-05-29 pre-release) found **2 CRITICAL, 10 HIGH, 11 MEDIUM** findings for runtimo. Cross-referencing:

| Prior Finding | Status |
|---------------|--------|
| C-1: `confirmation_salt` default | **Not found in current codebase** — fixed or different crate |
| Execution bugs (GitExec/Kill/ShellExec) | **Resolved** — current audit finds no G-SEC or G-SEM in these capabilities |
| Current findings | **Residual structural and performance issues** that remained after prior fixes |

---

## Hive-Mind Insights Stored

| Insight ID | Pattern |
|------------|---------|
| `ins-531d6708ca28` | Resource checks that run BEFORE the operation creating their preconditions will fail on first-use. Add `if !precondition_exists { return Ok(()) }` guard. |
| `ins-f9384efcf19c` | `#[allow(dead_code)]` convenience wrappers within wired modules are S-DEAD — check if the function is called anywhere, not just if it's public. |
| `ins-3108e5b1fd5b` | Static config loaded per-call causes N disk reads for N operations. Use `OnceLock` or `lazy_static` to cache. |

---

## Audit Opinion

| Metric | Value |
|--------|-------|
| **Overall** | `FAIL` |
| **Summary** | "1 test-failing execution bug, 1 performance regression, 3 dead exports, 2 missing contract declarations — structural entropy and execution gaps block clean opinion" |
| **CBP Verified** | ✅ All 7 findings confirmed via CBP v2 7-phase boundary analysis. No cascades, no concealed bugs. |
| **Critical blockers** | 0 |
| **Requires fixes** | 2 (G-EDGE-1, G-PERF-1) |
| **Style notes** | 3 (S-DEAD) |
| **Contract gaps** | 2 (S-CONTRACT) |

**Immediate action:** Fix `G-EDGE-1` — `check_disk_space()` must handle non-existent parent directories. This causes a **failing test**. Second priority: `G-PERF-1` config caching.

**Design debt:** Remove or wire `SchemaValidator`, `HealthAlert`, and `run_git` (S-DEAD findings). Add `rust-version` declaration and semver compatibility CI (S-CONTRACT findings). These are not blocking but represent accumulated structural entropy.

---

## Files in Scope

```
core/src/{lib.rs, capability.rs, config.rs, executor.rs, job.rs, session.rs,
         backup.rs, schema.rs, wal.rs, telemetry.rs, monitor.rs, processes.rs,
         llmosafe.rs, cmd.rs}
core/src/capabilities/{file_read.rs, file_write.rs, shell_exec.rs, git_exec.rs,
                       kill.rs, undo.rs, mod.rs}
core/src/validation/path.rs
cli/src/main.rs
daemon/src/main.rs
```

---

**Verification:**
- ✅ All identifiers verified in source files
- ✅ Compilation: `cargo check --all-targets` → exit 0
- ✅ Test evidence: `cargo test -p runtimo-core` → 30 passed, 1 FAILED (G-EDGE-1)
- ✅ Findings re-reviewed after break — all 7 confirmed
- ✅ Insights stored in hive-mind for future sessions
- ✅ Lessons captured: CAM Phase 0 → AD Phase 1 trace required for full coverage; `#[allow(dead_code)]` check in wired modules needed in addition to pub declaration scan

---

## CBP v2 Verification (Compounded Bug Protocol)

**Date:** 2026-05-29
**Protocol:** CBP v2 — Compounded Bug Protocol
**Skill:** `compounded-bug-protocol` v2 (all 5 reference files read)
**Purpose:** Verify the 7 audit findings against CBP's 7-phase boundary analysis. Check for compound cascades, missed findings, and classification accuracy.

---

### Phase 0: Boundary Map

#### Components

| Component | Module | Trust Domain |
|-----------|--------|-------------|
| FileWrite::execute | `capabilities/file_write.rs` | Internal |
| validate_path | `validation/path.rs` | Internal |
| check_disk_space | `capabilities/file_write.rs` | Internal → subprocess |
| RuntimoConfig::load | `config.rs` | Internal → disk |
| df command | OS subprocess | Kernel authority |
| create_dir_all | std::fs | Internal → filesystem |
| lib.rs pub re-exports | `lib.rs` | Crate boundary |

#### Boundary Map

```
User args ──[Function Call]──→ FileWrite::execute
                                    │
                                    ├─[Function Call]──→ validate_path
                                    │                        │
                                    │                        └─[Function Call]──→ RuntimoConfig::get_allowed_prefixes
                                    │                                                  │
                                    │                                                  └─[Persistence]──→ config.toml (disk)
                                    │
                                    ├─[Function Call]──→ check_disk_space
                                    │                        │
                                    │                        └─[Privilege + Function Call]──→ df -B1 <parent>
                                    │
                                    ├─[Function Call]──→ create_dir_all
                                    │
                                    └─[Function Call]──→ std::fs::write
```

| # | Boundary | Type | Direction | Data Format | Authority Context |
|---|----------|------|-----------|-------------|-------------------|
| B1 | FileWrite::execute → validate_path | Function Call | Internal | PathContext { require_exists: false } | Same process |
| B2 | FileWrite::execute → check_disk_space | Function Call | Internal | (&Path, usize) | Same process |
| B3 | validate_path → RuntimoConfig::get_allowed_prefixes → Self::load | Function Call chain | Internal | &PathContext → Vec<String> | Same process |
| B4 | RuntimoConfig::load → filesystem | Persistence | Internal→Disk | TOML file read | Filesystem access |
| B5 | check_disk_space → df subprocess | Privilege + Function Call | Internal→OS | PathBuf arg → stdout text | Subprocess with ambient authority |
| B6 | FileWrite::execute → create_dir_all | Function Call | Internal | &Path | Same process |
| B7 | lib.rs pub re-exports → external | Function Call | Crate→External | Type re-exports | Public API contract |

**Gate:** All 7 boundaries classified. B5 is compound (Function Call + Privilege — spawns subprocess).

---

### Phase 1: Attack Surface Mapping

#### B1: FileWrite::execute → validate_path

- **C1 — Contract:** Source sends `PathContext { require_exists: false, require_file: false }`. Sink validates path format, null bytes, prefix. **Contract correct.**
- **C2 — Authority:** Same privilege. No.
- **C3 — Temporal:** No concurrent writers to path state. No.
- **C4 — Side Channel:** No secret-dependent behavior. No.
- **C5 — Capability:** No property stripped. No.
- **C6 — Protocol:** No version negotiation. No.
- **C7 — Semantic Loss:** `PathContext` carries all needed info (allow non-existent, allow directories). **No loss.**

**Result: CLEAN. No flags.**

#### B2: FileWrite::execute → check_disk_space

- **C1 — Contract:** Source sends `(&Path, content_size: usize)`. Sink's **implicit** contract: `path.parent()` must exist on disk (so `df` can stat it). **Source does not guarantee this.** → **FLAG C1.**
- **C2 — Authority:** Same privilege. No.
- **C3 — Temporal:** B2 runs at T1. B6 (`create_dir_all`) runs at T2. Parent doesn't exist at T1. **State changes between validate and use — but here the "use" is df, which requires the parent to exist, and the creation happens AFTER.** → **FLAG C3.**
- **C4 — Side Channel:** No. No.
- **C5 — Capability:** No. No.
- **C6 — Protocol:** No. No.
- **C7 — Semantic Loss:** Caller has `path.parent()` which is the target for df. All info transmitted. No.

**Result: FLAGGED C1 + C3. This is G-EDGE-1.**

**CHALLENGE mode — "What if this boundary is actually safe?"**
Counter-position: `check_disk_space` is a BEST-EFFORT pre-check. If the parent doesn't exist, it can't check space, so returning an error is a safe failure mode. The real question is: does the caller EXPECT this behavior? The caller (`FileWrite::execute`) intends to CREATE the parent via `create_dir_all` later. The disk check was supposed to be a GUARD before writing, not a blocker before creation. The contract mismatch is real — the caller assumes "check space, then create dir + write," but the callee assumes "parent already exists."

#### B3: validate_path → RuntimoConfig::get_allowed_prefixes → Self::load

- **C1 — Contract:** Format correct. No.
- **C2 — Authority:** No. No.
- **C3 — Temporal:** No. Config file is static during execution. No.
- **C4 — Side Channel:** No. No.
- **C5 — Capability:** No. No.
- **C6 — Protocol:** No. No.
- **C7 — Semantic Loss:** `get_allowed_prefixes()` returns all sources (defaults + env + config). **Complete.**

**Result: CLEAN. No flags.**

#### B4: RuntimoConfig::load → filesystem

- **C1 — Contract:** TOML parser validates format. `unwrap_or_default()` on parse failure = graceful degradation. **Contract correct.**
- **C2 — Authority:** Same user. No.
- **C3 — Temporal:** Config file could be modified between calls, but this is expected behavior (config reload). No race condition. No.
- **C4 — Side Channel:** No. No.
- **C5 — Capability:** No. No.
- **C6 — Protocol:** No. No.
- **C7 — Semantic Loss:** All config fields present in struct. No.

**Result: CLEAN. No flags.** However, this boundary has a **performance** issue (no caching) — correctly classified as G-PERF-1 in the audit, not a CBP boundary violation.

#### B5: check_disk_space → df subprocess

- **C1 — Contract:** `df -B1 <parent>` — df expects an existing path. Non-existent path → error output. **Contract violation when parent missing.** → **FLAG C1** (same finding as B2, different angle).
- **C2 — Authority:** Subprocess inherits process authority. `df` runs with same permissions. No re-authentication needed. No.
- **C3 — Temporal:** df is synchronous. No. No.
- **C4 — Side Channel:** df output reveals disk layout. Not attacker-controllable. No.
- **C5 — Capability:** df is read-only. No property stripped. No.
- **C6 — Protocol:** No version. No.

**Result: FLAGGED C1 (same root cause as B2).**

#### B6: FileWrite::execute → create_dir_all

- **C1 — Contract:** `create_dir_all` creates nested dirs. No validation needed — it's a filesystem operation. No.
- **C2 — Authority:** Same user. No.
- **C3 — Temporal:** No concurrent writers assumed. No.
- **C4 — Side Channel:** No. No.
- **C5 — Capability:** No. No.
- **C6 — Protocol:** No. No.

**Result: CLEAN. No flags.**

#### B7: lib.rs pub re-exports → external consumers

- **C1 — Contract:** Public API promises `SchemaValidator`, `HealthAlert` are usable types. They are defined but never wired into any execution path. **Contract mismatch: exported types are dead.** → **FLAG C1** (low severity).
- **C2 — Authority:** Same crate. No.
- **C3 — Temporal:** No. No.
- **C4 — Side Channel:** No. No.
- **C5 — Capability:** No. No.
- **C6 — Protocol:** No. No.
- **C7 — Semantic Loss:** `run_git` wrapper exists but 0 callers use it — all call `run_git_with_timeout` directly. **Semantic contract is stale.**

**Result: FLAGGED C1 (dead exports — structural, not runtime).**

---

### Phase 2: Concrete Witness — G-EDGE-1

```
Input: target = "/tmp/runtimo_test_.../a/b/c/f.txt", content = "deep"

Step 1: FileWrite::execute(args, ctx)
        → validate_path("/tmp/.../a/b/c/f.txt", PathContext { require_exists: false })
        → Path is non-null, non-empty, prefix OK
        → OK

Step 2: is_critical_file(&path) → false

Step 3: check_disk_space(&path, 4)
        → parent = path.parent() = "/tmp/.../a/b/c/"
        → Command::new("df").arg("-B1").arg("/tmp/.../a/b/c/")
        → Parent does NOT exist on disk
        → df: stderr = "df: /tmp/.../a/b/c: No such file or directory"
        → Returns Err("df command failed: ...")

Step 4: FileWrite::execute returns ExecutionFailed

Step 5: create_dir_all("/tmp/.../a/b/c/") NEVER REACHED

Step 6: Test panics at .unwrap() on line 242
```

**Witness confirmed.** Specific input → specific state → specific error at `file_write.rs:245-251`.

---

### Phase 3: Convergent Coverage — G-EDGE-1

**Trace A (forward):** `FileWrite::execute` → `validate_path` (OK) → `check_disk_space` (FAIL at line 251) → never reaches `create_dir_all` (line 182+).

**Trace B (backward):** Starting from the error `"df command failed: df: /tmp/.../a/b/c: No such file or directory"`:
1. `df` failed because `/tmp/.../a/b/c/` doesn't exist on disk
2. `check_disk_space` was called with this non-existent parent path
3. `FileWrite::execute` called `check_disk_space` BEFORE `create_dir_all`
4. The test target requires creating 3 nested directories (`a/b/c/`)
5. Root cause: **no `if !parent.exists() { return Ok(()); }` guard** before df call

**Both traces converge on the same root cause:** The execution ordering in `FileWrite::execute` runs `check_disk_space` (line 154) before `create_dir_all` (line 182), and `check_disk_space` has no guard for non-existent parents.

**Convergent finding confirmed.**

---

### Phase 4: Compound Cascade Detection

#### Finding Pair Analysis

| Finding Pair | Cascade? | Type | Analysis |
|-------------|----------|------|----------|
| G-EDGE-1 ↔ G-PERF-1 | **No** | Isolated | Different boundaries (B2 vs B4), different components, no interaction |
| G-EDGE-1 ↔ S-DEAD-* | **No** | Isolated | Dead exports are in lib.rs/schema.rs/monitor.rs — unrelated to file_write execution |
| G-PERF-1 ↔ S-DEAD-* | **No** | Isolated | Config loading and dead exports have no code path interaction |
| S-DEAD-1 ↔ S-DEAD-2 ↔ S-DEAD-3 | **Weak parallel** | Parallel (same class) | 3 independent dead exports at B7. Same root cause (structural entropy) but no interaction |
| S-CONTRACT-1 ↔ S-CONTRACT-2 | **Weak parallel** | Parallel | Both governance gaps. No code interaction |

#### Concealed Bug Check

**Does G-EDGE-1 conceal another bug?** The `check_disk_space` failure returns `ExecutionFailed` at line 155. This causes an early return that skips:
- Lines 158-170: append size check
- Line 172+: dry_run check
- Line 182+: create_dir_all
- Lines 190+: backup creation
- Lines 200+: actual file write

**Are any of these skipped paths hiding additional bugs?** No — the skipped paths are normal operations that would succeed if reached. No concealed bugs detected.

**Does G-PERF-1 conceal anything?** Config loading per-call adds latency but doesn't suppress errors or mask state corruption. No concealment.

#### Dual-Decision Check

**Are there two components implementing the same decision with different rules?** No. Path validation is centralized in `validate_path`. Disk space checking is a single function. No dual-decision divergence.

**Verdict: No compound cascades. No concealed bugs. No dual-decision divergence.** The audit's conclusion of "no cross-boundary cascade" is confirmed.

---

### Phase 5: Classification

#### CBP-Verified Findings

| Finding | CBP Categories | Boundary | Severity | Priority | Audit Correct? |
|---------|---------------|----------|----------|----------|----------------|
| **G-EDGE-1** | C1 (Contract) + C3 (Temporal ordering) | B2, B5 | **HIGH** | **1** (blocks test + user-facing bug) | ✅ Yes |
| **G-PERF-1** | Performance (not a CBP boundary violation) | B4 | **MEDIUM** | **2** (scaling issue) | ✅ Yes |
| **S-DEAD-1** | C1 (Contract — dead public API) | B7 | **LOW** | **4** (cleanup) | ✅ Yes |
| **S-DEAD-2** | C1 (Contract — dead public API) | B7 | **LOW** | **4** (cleanup) | ✅ Yes |
| **S-DEAD-3** | C1 (Contract — dead public API) | B7 | **LOW** | **4** (cleanup) | ✅ Yes |
| **S-CONTRACT-1** | Governance (outside CBP scope) | N/A | **LOW** | **5** (process) | ✅ Yes |
| **S-CONTRACT-2** | Governance (outside CBP scope) | N/A | **LOW** | **5** (process) | ✅ Yes |

#### CBP Fix Priority (from cascade-patterns.md)

1. **Dual-decision:** None found.
2. **Authority + contract compound:** None found.
3. **TOCTOU at privilege boundary:** None found.
4. **Protocol downgrade:** None found.
5. **Silent capability downgrade:** None found.
6. **Dead contract:** S-DEAD-1/2/3 — dead exports are dead contracts at the public API boundary.

**CBP overall priority:** G-EDGE-1 is the only finding requiring immediate code fix.

---

### Phase 6: Verification Gate

| Gate | Status | Evidence |
|------|--------|----------|
| All 7 boundaries mapped | ✅ | B1–B7 classified with type, direction, data format, authority |
| G-EDGE-1: all 7 categories screened | ✅ | C1+C3 flagged at B2/B5, others cleared with reasoning |
| G-EDGE-1: concrete witness | ✅ | Specific input → specific error at `file_write.rs:245-251` |
| G-EDGE-1: convergent coverage | ✅ | Forward + backward traces agree on root cause |
| G-EDGE-1: confirmed by live test | ✅ | `creates_parent_directories` PANICs with `"df command failed"` |
| G-PERF-1: confirmed | ✅ | `config.rs:65` → `Self::load()` on every call, no caching |
| Dead exports: grep confirmed | ✅ | SchemaValidator: 0 consumers. HealthAlert: 0 external. run_git: 0 callers |
| No hidden cascades | ✅ | All finding pairs checked, none compound |
| No concealed bugs | ✅ | Early return from G-EDGE-1 skips normal paths, no bugs hidden |
| No dual-decision | ✅ | Path validation centralized in `validate_path` |
| No missed CBP findings | ✅ | No additional boundary violations beyond the 7 |

---

### CBP Verdict

**All 7 audit findings verified correct under CBP v2 analysis.**

- **G-EDGE-1** is the only runtime bug — a C1+C3 compound at the `FileWrite::execute` → `check_disk_space` boundary. The fix (add `if !parent.exists() { return Ok(()); }` before `df` call) is correct and addresses the root cause.
- **G-PERF-1** is a real performance regression but NOT a CBP boundary violation. Correctly classified.
- **3 dead exports** are low-severity C1 violations at the public API boundary. Structural entropy.
- **2 contract gaps** are governance issues, outside CBP scope.
- **No compound cascades** exist — findings are isolated.
- **No concealed bugs** — G-EDGE-1's early return doesn't mask other failures.
- **No additional findings** — CBP screening found no missed boundary violations beyond the 7.

**Final audit opinion: CONFIRMED `FAIL`. 2 requires fixes, 5 style/governance. Codebase is sound with structural entropy and one execution edge case.**