# Hive-Mind Bug Audit Report

> Audited: 2026-05-19 via runtimo v0.2.1 on `brain` (persistent machine)
> Commit: `d603145` — "fix: 11 bugs + gate degradation protocol"
> Scope: All Python files in `/srv/hive_mind_py_in_docker/hive_mind/`
> Method: Read all critical files + diff analysis of latest commit

---

## P0 — Security / Data Corruption

### BUG-1: SQL Injection in `plm.py` (ValueDensityMetrics.aggregate_metrics)
- **File:** `hive_mind/plm.py:325-345`
- **Description:** `interval` variable interpolated directly into SQL via f-string: `INTERVAL '{interval}'`. While `interval` currently comes from a controlled set (`"1 day"`, `"7 days"`, `"30 days"`), the pattern is SQL injection. If `period` parameter ever accepts external input, this is exploitable.
- **Impact:** SQL injection via `period` parameter if exposed to user input.
- **Fix:** Use `make_interval()` parameterized query like conductor.py does, or validate `period` against a whitelist before interpolation.

### BUG-2: SQL Injection in `zvec_client.py` filter builder
- **File:** `hive_mind/zvec_client.py:458`
- **Description:** Filter values escaped with `.replace(chr(92), ...)` and `.replace(chr(39), ...)` but this is manual SQL escaping. The `chr()` obfuscation doesn't add security — it's equivalent to `\\` and `'`. More critically, the filter string is built as raw SQL: `f"{k} = '{v}'"`. If `k` (the key) contains SQL metacharacters, it bypasses escaping entirely since only `v` is escaped.
- **Impact:** SQL injection via filter key names. If any caller passes a malicious key, the filter expression is vulnerable.
- **Fix:** Use zvec's native filter API or parameterized queries. Validate keys against known schema field names.

### BUG-3: Cross-session contamination via `_last_sid` in `llm.py`
- **File:** `hive_mind/llm.py:216`
- **Description:** `internal_v4_reason` reads `_core._last_sid` to derive a session ID: `internal_sid = _core._last_sid or f"internal-{uuid.uuid4().hex[:8]}"`. The `tools/core.py` file explicitly documents that `_last_sid` was removed as a fallback source of truth because it caused cross-session contamination. Yet `llm.py` still reads it directly.
- **Impact:** Internal reasoning chains can hijack another session's identity, causing evidence to be recorded under the wrong session, grounding gate to see wrong evidence, and chain state to be corrupted.
- **Fix:** Always use `f"internal-{uuid.uuid4().hex[:8]}"` — never read `_last_sid`.

---

## P1 — Correctness / Reliability

### BUG-4: `_clear_degradation_history` prunes wrong entries (race condition)
- **File:** `hive_mind/reasoning/chain.py:95-107`
- **Description:** Prunes entries where `fp.first_seen < current_thoughts`. But `current_thoughts` is the thought count of the *current* chain, not the chain that created the fingerprint. A new chain with 1 thought will prune all fingerprints from prior chains (which had `first_seen` values from their own thought counts). This means degradation history is cleared prematurely on every new prompt.
- **Impact:** Gate degradation never actually works — it gets cleared before reaching DEGRADATION_THRESHOLD=3. The feature is dead code.
- **Fix:** Track `session_id` in `_RejectionFingerprint` and only prune entries belonging to the completing/stalling session.

### BUG-5: `conductor.py` alerted_ids pruning is non-deterministic
- **File:** `hive_mind/conductor.py:210`
- **Description:** `alerted_ids = set(list(alerted_ids)[-MAX_ALERTED_IDS:])` — converting a set to a list has no guaranteed order, so this does NOT keep the "most recent" entries. It keeps arbitrary entries.
- **Impact:** Old alerted IDs may persist while new ones are dropped, causing duplicate alerts for stale messages while missing new ones.
- **Fix:** Use an OrderedDict or list with timestamps instead of a bare set.

### BUG-6: `SelfJudgeBuffer.schedule_background_check` pops before background task
- **File:** `hive_mind/training_capture.py:629-632`
- **Description:** `first = self._first_results.pop(call_id, None)` happens BEFORE the background task. If the background `call_func` raises an exception, the first result is already gone and the comparison silently fails. The fix in the latest commit moved the pop before the background task, but this means exceptions in the background call lose the first result entirely.
- **Impact:** Disagreement detection silently fails when the second call raises — no logging, no training data captured.
- **Fix:** Pop inside the background task's try block, or log when the first result is lost.

### BUG-7: `steward.py flash_reap` uses non-deterministic `hash()`
- **File:** `hive_mind/steward.py:310`
- **Description:** `h = hash(trimmed)` — Python's built-in `hash()` is randomized per process (PYTHONHASHSEED). Across Docker restarts, the same text gets different hashes, so deduplication fails. Within a single process, hash collisions are likely for short strings.
- **Impact:** Context compaction is unreliable — either fails to deduplicate (wasting context window) or incorrectly drops unique lines (losing information).
- **Fix:** Use `hashlib.md5(trimmed.encode()).hexdigest()` for deterministic hashing.

### BUG-8: `hook_post_tool.py` codegraph trigger has no synchronization
- **File:** `hive_mind/hooks/hook_post_tool.py:117`
- **Description:** `with open(_CODEGRAPH_TRIGGER, "a") as f: f.write(...)` — concurrent tool calls can interleave writes to the same file. While append mode is atomic for small writes on Linux, there's no file locking or newline synchronization.
- **Impact:** Corrupted trigger file entries (e.g., `file1.py\nfile2.py\n` becoming `file1.py\nfile2.p\ny\n`).
- **Fix:** Use `os.open()` with `O_APPEND` and write atomic lines, or use `fcntl.flock()`.

---

## P2 — Quality / Maintainability

### BUG-9: `notifications.py` `_prune_alert_states` doesn't actually prune oldest
- **File:** `hive_mind/notifications.py:60-67`
- **Description:** Sorts by `last_alerted` ascending and removes first N entries. But after popping from `_alert_states`, the orphaned lock cleanup runs on a snapshot (`set(_alert_locks.keys()) - set(_alert_states.keys())`). If `_prune_alert_states` is called concurrently (via different alert keys), the orphan calculation can be stale.
- **Impact:** Minor — lock leaks possible under high concurrency.
- **Fix:** Hold `_alert_locks_guard` during the entire prune operation.

### BUG-10: `identity/project.py` has duplicate `slug_from_path` after latest commit
- **File:** `hive_mind/identity/project.py:51-62`
- **Description:** The latest commit claims to have removed the duplicate `slug_from_path` from `ProjectID`, but the diff shows it was removed from one location. However, `slug_from_path` still exists as a classmethod. The commit message says "Remove dead code (duplicate slug_from_path)" but the method is still present and used by `hook_user_prompt.py` via `ProjectID.slug_from_path(req.cwd)`.
- **Impact:** None — the method is needed. But the commit message is misleading about what was removed.
- **Fix:** Update commit message or clarify which duplicate was removed.

### BUG-11: `_gate_reflect` persist check uses wrong counter comparison
- **File:** `hive_mind/reasoning/gates.py:430-445`
- **Description:** Checks `EvidenceStore.get_recent_records(sid, chain_start_counter)` for persist tools. But `chain_start_counter` is the counter at chain creation, and the persist tool call happens during REFLECT — which is part of the same chain. The counter comparison `r.counter > chain_start_counter` should find these records, but if the EvidenceStore was evicted (LRU), the records are gone and the gate fails spuriously.
- **Impact:** REFLECT gate can fail with "PERSIST REQUIRED" even when persist was called, if EvidenceStore evicted the session.
- **Fix:** Persist tool calls should be tracked in the chain object itself, not in the ephemeral EvidenceStore.

### BUG-12: `grounding.py` `_MSG_NO_SESSION` error message is misleading
- **File:** `hive_mind/gates/grounding.py:100`
- **Description:** When `session_id` is None/empty, returns `_MSG_NO_SESSION` saying "no evidence of contact with reality" — but the actual issue is missing session ID, not missing evidence.
- **Impact:** Agent gets confusing error message, may try to run tools instead of fixing the session ID issue.
- **Fix:** Separate error messages: one for missing session ID, one for missing evidence.

---

## Summary

| Severity | Count | Description |
|----------|-------|-------------|
| P0 | 3 | SQL injection (plm.py, zvec_client.py), cross-session contamination (llm.py) |
| P1 | 5 | Race conditions, dead code, non-deterministic behavior |
| P2 | 4 | Quality issues, misleading errors, minor concurrency bugs |
| **Total** | **12** | |

---

## Runtimo Frictions (Tool Issues)

### #1 — ShellExec does not support shell operators
- **Severity:** P1
- **Component:** `core/src/capabilities/shell_exec.rs`
- **Description:** ShellExec uses direct `exec()` (Command::new), not `sh -c`. Shell operators (`&&`, `||`, `|`, `;`, `$VAR`, `>`, `<`, `$(...)`) are not parsed.
- **Workaround:** Use single commands or flags that accept paths directly.
- **Status:** OPEN

### #2 — FileRead JSON escaping is fragile over SSH
- **Severity:** P2
- **Component:** CLI argument parsing
- **Description:** Nested JSON escaping over SSH requires multiple levels of escaping (`\\\"`), making FileRead calls error-prone.
- **Workaround:** Use ShellExec for file reads instead.
- **Status:** OPEN

### #3 — No FileWrite capability for remote audit reports
- **Severity:** P2
- **Component:** Missing capability
- **Description:** Cannot write audit reports directly to the remote machine. Must use ShellExec with echo/heredoc.
- **Status:** OPEN
