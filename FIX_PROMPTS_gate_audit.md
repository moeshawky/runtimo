# Hive-Mind Bug Fix Dispatch Prompts

> Generated: 2026-05-19 via charlie prompt engineering
> Source: `BUG_REPORT_gate_audit.md` — 9 bugs, 3 severity levels
> Target: Subagent execution on `brain` box via SSH + runtimo

---

## BUG-1: Gate degradation dead code

```
[ROLE]
You are a senior Python concurrency specialist fixing a reasoning gate degradation system in a production AI agent backend.

[CONTEXT]
- File: /srv/hive_mind_py_in_docker/hive_mind/reasoning/chain.py
- Lines: 55-107 (_RejectionFingerprint, _check_degradation, _clear_degradation_history)
- Lines: 425-470 (_handle_rejection)
- Commit: d603145 — "fix: 11 bugs + gate degradation protocol" (this commit INTRODUCED the bugs)

[INTENT]
Fix the gate degradation system so it correctly detects 3 identical rejections WITHIN THE SAME CHAIN and bypasses the gate, WITHOUT cross-chain contamination.

[FAILURE SIGNATURES]
- BUG 1a (cross-chain contamination): _rejection_history is a global dict keyed only by md5 hash. Chain A gets 2 rejections, Chain B gets 1 more → degradation fires for Chain B incorrectly.
- BUG 1b (broken pruning): _clear_degradation_history prunes entries where fp.first_seen < current_thoughts. current_thoughts is the NEW chain's count, not the fingerprint's chain. A new chain with 6 thoughts prunes a fingerprint from a prior chain with first_seen=5.

[CONSTRAINTS]
- HARD: Do NOT change the public API of _check_degradation or _handle_rejection
- HARD: Do NOT add new dependencies
- HARD: Must preserve the degradation behavior: after 3 identical rejections in the same chain, bypass the gate with a warning
- SOFT: Keep the fix under 40 lines
- SOFT: Add a test that verifies cross-chain isolation

[SKILLS TO LOAD]
1. advanced-debugging — for root cause analysis before fixing
2. llm-guardrails — screen fix against R-STATE, R-CASCADE, R-SEM patterns

[FAILURE PREVENTION]
Before proceeding:
1. R-STATE check: Ensure rejection history is keyed by (session_id, message_type) not just message hash
2. R-EDGE check: What happens when a chain has 0 thoughts? What if first_seen=0?
3. R-SEM check: Verify the fix actually isolates chains — trace through a scenario with 3 chains
4. R-ASSUME check: List assumptions about chain lifecycle (when are chains created/destroyed?)

[TASK]
1. Read chain.py lines 55-110 and 425-470 completely
2. Read _handle_rejection to understand all call sites of _clear_degradation_history
3. Fix _RejectionFingerprint to include session_id in the key
4. Fix _clear_degradation_history to only prune entries belonging to the given session_id
5. Add test: create 3 chains, trigger same rejection in each, verify degradation only fires within a single chain
6. Verify: run existing tests pass, new test passes

[OUTPUT]
1. The fixed code (full functions, not just diffs)
2. Test code
3. Verification command output
```

---

## BUG-2: Conductor set pruning non-deterministic

```
[ROLE]
You are a senior Python engineer fixing a non-deterministic data structure bug in a production health monitoring system.

[CONTEXT]
- File: /srv/hive_mind_py_in_docker/hive_mind/conductor.py
- Line: 229 — `alerted_ids = set(list(alerted_ids)[-MAX_ALERTED_IDS:])`
- MAX_ALERTED_IDS = 500
- This runs every 60 seconds in the conductor cycle

[INTENT]
Replace non-deterministic set pruning with ordered LRU eviction that actually keeps the most recently seen message IDs.

[FAILURE SIGNATURE]
- R-SEM: Code comment says "Keep only the most recent MAX_ALERTED_IDS entries" but set→list has no guaranteed order. The last 500 elements of an arbitrarily-ordered list are NOT the most recent.
- Under sustained load (>500 stale messages), the conductor may keep alerting about old messages while new ones are randomly dropped.

[CONSTRAINTS]
- HARD: Do NOT change the alerted_mail_ids memory block format (it's a JSON list of strings)
- HARD: Do NOT add new dependencies
- HARD: Must maintain bounded growth (max 500 entries)
- SOFT: Minimize changes — prefer a 2-line fix

[SKILLS TO LOAD]
1. advanced-debugging — for root cause verification
2. llm-guardrails — screen against R-SEM pattern

[FAILURE PREVENTION]
1. R-SEM check: Verify the fix actually preserves insertion order
2. R-EDGE check: What if the persisted block is corrupted? What if it contains non-string entries?
3. R-PERF check: Ensure the fix doesn't add O(n log n) overhead to a 60-second cycle

[TASK]
1. Read conductor.py lines 180-235
2. Replace the set-based pruning with a list-based approach that preserves insertion order
3. The simplest fix: use a list instead of a set for alerted_ids, and append new IDs to the end. When pruning, keep the last 500 entries: `alerted_ids = alerted_ids[-MAX_ALERTED_IDS:]`
4. Verify the JSON serialization still works (list serializes fine)
5. Check that `m["id"] not in alerted_ids` lookup is still efficient (consider using a set for lookup + list for order if needed)

[OUTPUT]
1. The fixed code
2. Verification: explain why the fix preserves insertion order
```

---

## BUG-3: SelfJudgeBuffer silent failure

```
[ROLE]
You are a senior Python async engineer fixing a race condition in a training data capture system.

[CONTEXT]
- File: /srv/hive_mind_py_in_docker/hive_mind/training_capture.py
- Lines: 620-660 (SelfJudgeBuffer.schedule_background_check)
- The system double-calls LLM functions to detect disagreements for training data

[INTENT]
Fix the race condition where the first result is popped from _first_results BEFORE the background task runs, causing silent data loss when the background call raises an exception.

[FAILURE SIGNATURE]
- R-ERR: No recovery plan when background call fails — first result is already consumed
- R-STATE: Dict state mutated (pop) before async operation that depends on it completes
- If call_func raises, the first result is gone AND no disagreement is recorded. The log says "Double-call background failed" but the training data opportunity is lost.

[CONSTRAINTS]
- HARD: Do NOT add latency to the first call (background must remain async)
- HARD: Do NOT add new dependencies
- HARD: Must preserve the 2-second delay for rate limiting
- SOFT: Keep the fix under 20 lines

[SKILLS TO LOAD]
1. advanced-debugging — for async race condition analysis
2. llm-guardrails — screen against R-ERR, R-STATE patterns

[FAILURE PREVENTION]
1. R-ERR check: What happens if the background task is cancelled? Is the first result restored?
2. R-EDGE check: What if call_func returns None? What if first_result is an empty string?
3. R-STATE check: Ensure _first_results dict is consistent after any exit path

[TASK]
1. Read training_capture.py lines 600-680
2. Move the pop INSIDE the background task's try block
3. If the background call raises, restore the first result to _first_results for potential retry
4. Alternatively: don't pop at all — let the background task pop after successful comparison
5. Add logging when first result is lost due to exception

[OUTPUT]
1. The fixed schedule_background_check method
2. Explanation of the new error handling path
```

---

## BUG-4: flash_reap uses non-deterministic hash()

```
[ROLE]
You are a senior Python engineer fixing a non-deterministic hashing bug in a context compaction system.

[CONTEXT]
- File: /srv/hive_mind_py_in_docker/hive_mind/steward.py
- Line: 597 — `h = hash(trimmed)` in flash_reap()
- flash_reap is a verbatim context reaper that deduplicates repeated lines
- Called by get_virtual_history() and post-tool output compaction

[INTENT]
Replace Python's non-deterministic hash() with a deterministic hash function so flash_reap produces consistent results across process restarts.

[FAILURE SIGNATURE]
- R-EDGE: Python's hash() is randomized per process (PYTHONHASHSEED). Across Docker restarts, same text gets different hashes. Short strings have non-trivial collision rates within a process.
- Unique lines are incorrectly dropped due to hash collisions. Behavior is non-deterministic across restarts.

[CONSTRAINTS]
- HARD: Do NOT change flash_reap's signature or return type
- HARD: Do NOT add new dependencies (hashlib is stdlib, OK to use)
- HARD: Must preserve the threshold-based deduplication logic
- SOFT: The fix should be a single-line change

[SKILLS TO LOAD]
1. advanced-debugging — for hash collision analysis
2. llm-guardrails — screen against R-EDGE pattern

[FAILURE PREVENTION]
1. R-EDGE check: Verify hashlib.md5 produces consistent output for the same input across restarts
2. R-PERF check: Ensure md5 doesn't add significant overhead to a function called on every tool output
3. R-SEM check: Verify the fix doesn't change the deduplication behavior (same lines should still be deduplicated)

[TASK]
1. Read steward.py lines 585-615 (flash_reap function)
2. Replace `h = hash(trimmed)` with `h = hashlib.md5(trimmed.encode()).hexdigest()`
3. Add `import hashlib` at the top of the file if not present
4. Verify no other uses of hash() in steward.py that need the same fix

[OUTPUT]
1. The fixed flash_reap function
2. Verification: explain why md5 is deterministic while hash() is not
```

---

## BUG-5: CodeGraph trigger file grows unbounded

```
[ROLE]
You are a senior Python engineer fixing an unbounded file growth bug in a file indexing trigger system.

[CONTEXT]
- File: /srv/hive_mind_py_in_docker/hive_mind/hooks/hook_post_tool.py
- Lines: 119-121 — `with open(_CODEGRAPH_TRIGGER, "a") as f: f.write(f"{file_path}\n")`
- _CODEGRAPH_TRIGGER = "/app/.codegraph/pending_updates.txt"
- Called every time an edit tool (Edit, Write, MultiEdit) succeeds

[INTENT]
Add deduplication and size limiting to the CodeGraph trigger file so it doesn't grow unbounded with duplicate entries.

[FAILURE SIGNATURE]
- R-PERF: No complexity constraint on the trigger file. Same file edited 100 times = 100 entries. CodeGraph re-indexes the same file repeatedly.
- No rotation, no deduplication, no size limit.

[CONSTRAINTS]
- HARD: Do NOT change the trigger file format (one file path per line)
- HARD: Do NOT add new dependencies
- HARD: Must remain append-only for atomicity (CodeGraph may read while we write)
- SOFT: Keep the fix under 15 lines

[SKILLS TO LOAD]
1. advanced-debugging — for file I/O analysis
2. llm-guardrails — screen against R-PERF pattern

[FAILURE PREVENTION]
1. R-PERF check: Ensure the deduplication doesn't require reading the entire file on every write
2. R-EDGE check: What if the trigger file is deleted between reads? What if two processes write simultaneously?
3. R-SEC check: Ensure file_path doesn't contain path traversal characters

[TASK]
1. Read hook_post_tool.py lines 100-130
2. The simplest fix: before appending, read the file, check if the path is already present, and only append if it's new. Then truncate if the file exceeds a reasonable size (e.g., 1000 lines).
3. Alternative: use a set-based approach with periodic flush — maintain an in-memory set of pending files, flush to disk periodically.
4. Choose the simplest approach that works and implement it.

[OUTPUT]
1. The fixed code
2. Explanation of the deduplication strategy
```

---

## BUG-6: Grounding gate SID resolution mismatch

```
[ROLE]
You are a senior Python engineer fixing a session identity mismatch bug in a reasoning gate system.

[CONTEXT]
- File: /srv/hive_mind_py_in_docker/hive_mind/gates/grounding.py
- Line: 153 — `canonical_sid = await _resolve_sid(None, session_id)`
- Evidence is recorded in hook_post_tool under the HTTP request SID
- Grounding gate queries EvidenceStore under a re-resolved SID
- These can differ, causing the gate to find no evidence

[INTENT]
Fix the grounding gate to use the session_id passed by the caller directly, without re-resolving it through _resolve_sid.

[FAILURE SIGNATURE]
- R-CASCADE: Local decision (resolve SID one way) conflicts with global state (evidence recorded under different SID)
- R-HALL: _resolve_sid may return a different SID than what the caller intended, especially for internal reasoning chains
- llm.py internal_v4_reason uses `_core._last_sid` which is explicitly documented as NOT a fallback source of truth

[CONSTRAINTS]
- HARD: Do NOT change the EvidenceStore API
- HARD: Do NOT change hook_post_tool's evidence recording
- HARD: The gate must still work for both HTTP-request-initiated chains and internal chains
- SOFT: Minimize changes to grounding.py

[SKILLS TO LOAD]
1. advanced-debugging — for session identity tracing
2. llm-guardrails — screen against R-CASCADE, R-HALL patterns

[FAILURE PREVENTION]
1. R-CASCADE check: Verify the fix doesn't break SID resolution for other callers of _gate_grounding
2. R-HALL check: Ensure the session_id passed to _gate_grounding is always the canonical one
3. R-CTX check: Read all callers of _gate_grounding to understand the SID flow

[TASK]
1. Read grounding.py completely (it's small — ~170 lines)
2. Read chain.py to find where _gate_grounding is called (search for "_gate_grounding")
3. The fix: remove the _resolve_sid call in _gate_grounding and use the session_id parameter directly
4. If session_id is None or empty, fail with a clear error message about missing session ID (not "no evidence of contact with reality")
5. Verify: trace the SID flow from hook_post_tool → chain → grounding gate

[OUTPUT]
1. The fixed _gate_grounding function
2. SID flow diagram: hook_post_tool → chain.py → grounding.py
```

---

## BUG-7: _gate_reflect persist check fails on LRU eviction

```
[ROLE]
You are a senior Python engineer fixing a spurious gate failure caused by ephemeral state eviction.

[CONTEXT]
- File: /srv/hive_mind_py_in_docker/hive_mind/reasoning/gates.py
- Lines: 430-445 (_gate_reflect persist check)
- EvidenceStore is capped at MAX_RECORDS=100 per session, MAX_SESSIONS=1000 globally
- Long chains (>100 tool calls) or busy servers evict evidence, causing REFLECT gate to fail

[INTENT]
Fix the REFLECT gate's persist check to not depend on the ephemeral EvidenceStore. Track persist tool calls in the Chain object itself.

[FAILURE SIGNATURE]
- R-EDGE: Boundary case — chains with >100 tool calls have early evidence evicted
- R-ASSUME: Assumes EvidenceStore retains all evidence for the duration of a chain. This assumption is violated by LRU eviction.
- REFLECT gate fails with "PERSIST REQUIRED" even when persist was called.

[CONSTRAINTS]
- HARD: Do NOT change the EvidenceStore API or capacity limits
- HARD: Do NOT change the Chain dataclass structure significantly (add one field max)
- HARD: The gate must still verify that a persist tool was called
- SOFT: Keep the fix under 30 lines

[SKILLS TO LOAD]
1. advanced-debugging — for state lifecycle analysis
2. llm-guardrails — screen against R-EDGE, R-ASSUME patterns

[FAILURE PREVENTION]
1. R-EDGE check: What if the Chain object is reconstructed from DB? Is the persist flag preserved?
2. R-STATE check: Ensure the persist tracking survives chain serialization/deserialization
3. R-SEM check: Verify the fix doesn't weaken the gate — it should still require actual persist calls

[TASK]
1. Read gates.py lines 400-460 (_gate_reflect function)
2. Read chain.py to find the Chain dataclass definition
3. Add a `persist_calls_made: bool = False` field to the Chain dataclass
4. In hook_post_tool.py, when a persist tool (memory_store, insight, etc.) succeeds, set the flag on the current chain
5. In _gate_reflect, check the Chain's flag instead of querying EvidenceStore
6. Alternative simpler fix: In _gate_reflect, wrap the EvidenceStore check in a try/except and if it returns empty, check the chain's thought history for persist tool mentions (text-based fallback)

[OUTPUT]
1. The fixed _gate_reflect function
2. Any changes to Chain dataclass or hook_post_tool
3. Explanation of why the fix survives LRU eviction
```

---

## BUG-8: md5 hash of full rejection text misses semantic equivalence

```
[ROLE]
You are a senior Python engineer fixing a semantic matching bug in a reasoning gate degradation system.

[CONTEXT]
- File: /srv/hive_mind_py_in_docker/hive_mind/reasoning/chain.py
- Line: 75 — `h = hashlib.md5(rejection_message.encode()).hexdigest()[:12]`
- Rejection messages contain dynamic data (thought numbers, specific values)
- "Axes < 2" and "Axes < 3" are semantically the same failure but produce different hashes

[INTENT]
Fix the degradation system to detect semantically equivalent rejections, not just textually identical ones.

[FAILURE SIGNATURE]
- R-SEM: The code hashes the full message text, but the INTENT is to detect semantically equivalent rejections. Dynamic values in rejection messages cause the same logical failure to produce different hashes.
- Degradation never triggers because rejection messages vary slightly each time.

[CONSTRAINTS]
- HARD: Do NOT change the rejection message format (used elsewhere for display)
- HARD: Do NOT add new dependencies
- HARD: Must still distinguish different TYPES of rejections (missing axes vs missing blind spot vs missing approach)
- SOFT: Keep the fix under 20 lines

[SKILLS TO LOAD]
1. advanced-debugging — for semantic analysis
2. llm-guardrails — screen against R-SEM pattern

[FAILURE PREVENTION]
1. R-SEM check: Verify the fix groups semantically equivalent rejections together
2. R-EDGE check: What if the rejection message has no recognizable pattern? What if it's empty?
3. R-PERF check: Ensure the normalization doesn't add significant overhead

[TASK]
1. Read chain.py lines 55-110 (degradation system)
2. Read gates.py to understand rejection message formats for each gate
3. Create a _normalize_rejection_message() function that extracts the rejection TYPE:
   - Strip numbers: "Axes < 2" → "Axes < N"
   - Strip thought numbers: "thought 5" → "thought N"
   - Extract gate name from message prefix
4. Hash the normalized message, not the raw message
5. Test: verify that "Axes < 2" and "Axes < 3" produce the same hash

[OUTPUT]
1. The _normalize_rejection_message function
2. The updated _check_degradation function
3. Test cases showing semantic equivalence detection
```

---

## BUG-9: Double axis counting — sanitize and gate compute independently

```
[ROLE]
You are a senior Python engineer fixing a state inconsistency bug in a reasoning gate validation system.

[CONTEXT]
- Files: /srv/hive_mind_py_in_docker/hive_mind/reasoning/gates.py (lines 240-260), /srv/hive_mind_py_in_docker/hive_mind/reasoning/sanitize.py (lines 130-145)
- sanitize_for_gate computes axes_count via _count_axes → delegates to gates.py
- _gate_see ALSO computes axes via _count_axes_with_cache
- These two computations can disagree

[INTENT]
Make _gate_see use the axes_count from sanitize's gate_context as the single source of truth, removing the redundant recomputation.

[FAILURE SIGNATURE]
- R-STATE: Prior computation (axes_count in ctx) is not trusted; the gate re-computes it. The two computations can diverge.
- Gate may pass when sanitize said it should fail, or vice versa.

[CONSTRAINTS]
- HARD: Do NOT change the sanitize_for_gate API
- HARD: Do NOT change the _gate_see signature
- HARD: The gate must still validate axes correctly
- SOFT: Remove the redundant computation path entirely

[SKILLS TO LOAD]
1. advanced-debugging — for state consistency analysis
2. llm-guardrails — screen against R-STATE pattern

[FAILURE PREVENTION]
1. R-STATE check: Verify there is exactly ONE computation path for axes_count
2. R-CTX check: Read all callers of _gate_see to ensure they always pass gate_context with axes_count
3. R-EDGE check: What if gate_context is None? What if axes_count is missing from ctx?

[TASK]
1. Read gates.py _gate_see function (lines 220-280)
2. Read sanitize.py sanitize_for_gate function
3. In _gate_see, remove the recomputation path: always use ctx.get("axes_count", 0)
4. If axes_count is 0 and ctx is not None, that means sanitize found 0 axes — the gate should fail with a clear message
5. Remove _count_axes_with_cache if it's no longer used anywhere else (grep for callers first)

[OUTPUT]
1. The fixed _gate_see function
2. List of any removed dead code
3. Verification: grep showing single source of truth for axes_count
```

---

## Execution Order

Fix in this order to minimize interference:

1. **BUG-4** (flash_reap hash) — single-line fix, isolated
2. **BUG-2** (conductor set pruning) — 2-line fix, isolated
3. **BUG-5** (CodeGraph trigger) — isolated file I/O fix
4. **BUG-9** (double axis counting) — gates.py only, no chain.py changes
5. **BUG-8** (md5 semantic hash) — chain.py only, no gates.py changes
6. **BUG-1** (gate degradation) — chain.py, depends on BUG-8 being fixed first
7. **BUG-6** (grounding SID) — grounding.py only, isolated
8. **BUG-7** (persist check LRU) — gates.py + chain.py, depends on BUG-9
9. **BUG-3** (SelfJudgeBuffer) — training_capture.py only, isolated
