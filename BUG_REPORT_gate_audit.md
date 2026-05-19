# Hive-Mind Bug Audit Report — UPDATED

> Audited: 2026-05-19 via runtimo v0.2.1 on `brain` (persistent machine)
> Latest commit: `159126b` — "fix: 2 P0 bugs - DB schema missing column + ChainID subscript errors"
> Fixes in container (not committed): BUG-D (log noise), BUG-F (zvec LOCK storm)
> Method: Static analysis + production log analysis + code evidence
> Skills: llm-guardrails (R-patterns), charlie-prompt-engineering, advanced-debugging

---

## Production Evidence (from `docker logs hive-mind --tail 500`)

Critical errors found in production logs that were NOT in the original audit:

1. `column "chain_start_counter" does not exist` — ALL chain persistence fails
2. `'ChainID' object is not subscriptable` — productive_reason breaks
3. `Failed to stat collection ... LOCK` — 100+ zvec LOCK file errors
4. `[GROUNDING] sid=unknown, recent=0` — grounding gate gets no evidence
5. `SEE gate: no keyword axes found` — internal V4 reasoning fails SEE gate

---

## Bug Inventory (Updated with Production Evidence)

### P0 — Actively Breaking Production

#### BUG-A: DB schema missing `chain_start_counter` column [FIXED ✓]
- **File:** `reasoning/persistence.py:28` (schema), production DB
- **Root Cause:** `CREATE TABLE IF NOT EXISTS` does NOT add columns to existing tables. The table was created before `chain_start_counter` was added to the schema.
- **Impact:** ALL chain persistence fails. `Failed incremental persist chain ... column "chain_start_counter" does not exist`
- **Fix Applied:** `ALTER TABLE reasoning_chains ADD COLUMN chain_start_counter INT DEFAULT 0;`
- **Commit:** `159126b`

#### BUG-B: ChainID not subscriptable [FIXED ✓]
- **Files:** `reasoning/gates.py:90,200`, `reasoning/chain.py:91,438`
- **Root Cause:** `chain.id[:12]` — ChainID is a frozen dataclass, not a string. No `__getitem__` method.
- **Impact:** `Internal V4 parse failure: 'ChainID' object is not subscriptable` — breaks ALL internal V4 reasoning chains.
- **Fix Applied:** Replaced `chain.id[:12]` with `str(chain.id)[:12]` (4 occurrences)
- **Commit:** `159126b`

#### BUG-C: Grounding gate SID mismatch [OPEN]
- **File:** `gates/grounding.py:153`
- **Root Cause:** `_resolve_sid(None, session_id)` may resolve to a DIFFERENT SID than where evidence was recorded.
- **Evidence:** `[GROUNDING] sid=unknown, recent=0, counter_threshold=1057` — gate gets sid=unknown, finds no evidence.
- **Impact:** Grounding gate fails with "EVIDENCE_STALE" even though evidence exists under a different key.
- **Fix:** Use session_id directly, don't re-resolve.

#### BUG-D: Gate degradation dead code [FIXED ✓ — log noise]
- **File:** `reasoning/gates.py:353`
- **Root Cause:** `log.warning("SEE gate: no keyword axes found")` fired 4,379 times — normal behavior logged as warning.
- **Impact:** Log pollution, not functional bug.
- **Fix Applied:** Downgraded to `log.debug()`
- **Commit:** pending

#### BUG-E: md5 hash of full rejection text [OPEN]
- **File:** `reasoning/chain.py:75`
- **Root Cause:** `hashlib.md5(rejection_message.encode()).hexdigest()[:12]` — hashes FULL message text including dynamic values (thought numbers, specific values). "Axes < 2" and "Axes < 3" produce different hashes.
- **Impact:** Degradation never triggers because rejection messages vary slightly each time.
- **Fix:** Hash the rejection TYPE (e.g., extract gate name + failure category), not the full message.

### P1 — Degrading Reliability

#### BUG-F: zvec LOCK file storm [FIXED ✓]
- **Evidence:** 67,608 "Failed to stat collection ... LOCK" errors in logs
- **Root Cause:** `zvec_list_collections()` opened EVERY `.zvec` directory to get stats — with 200+ session collections, massive lock contention.
- **Impact:** Collection stats fail, log pollution.
- **Fix Applied:** Added 60s TTL cache + skip opening non-cached collections. Only cached collections are opened for stats.
- **Commit:** pending

#### BUG-G: hash() non-determinism in flash_reap [FIXED ✓]
- **File:** `steward.py:370`
- **Fix Applied:** Replaced `hash(trimmed)` with `hashlib.md5(trimmed.encode()).hexdigest()`
- **Commit:** `b667129`

#### BUG-H: set pruning non-deterministic in conductor [FIXED ✓]
- **File:** `conductor.py:229`
- **Root Cause:** `set(list(alerted_ids)[-MAX_ALERTED_IDS:])` — set→list has no guaranteed order.
- **Impact:** Arbitrary entries kept/dropped, not "most recent" as intended.
- **Fix Applied:** Changed `alerted_ids` from `set` to `list` (preserves insertion order). Pruning uses `alerted_ids[-MAX_ALERTED_IDS:]`. O(1) lookup via `set(alerted_ids)` for membership check.
- **Commit:** pending

#### BUG-I: persist check fails on LRU eviction [FIXED ✓]
- **File:** `reasoning/gates.py:550`
- **Root Cause:** EvidenceStore capped at MAX_RECORDS=100. Long chains evict early evidence including persist calls.
- **Impact:** REFLECT gate fails spuriously with "PERSIST REQUIRED" for long chains.
- **Fix Applied:** Added `gate_context.get("persist_count", 0)` check before EvidenceStore lookup. If persist_count > 0, skip EvidenceStore (survives LRU eviction).
- **Commit:** pending

### P2 — Quality Issues

#### BUG-J: CodeGraph trigger file grows unbounded [OPEN]
- **File:** `hooks/hook_post_tool.py:119-121`
- **Root Cause:** No deduplication, no rotation, no size limit.
- **Fix:** Use set-based dedup with periodic flush.

#### BUG-K: SelfJudgeBuffer silent failure [LOW PRIORITY]
- **File:** `training_capture.py:632`
- **Assessment:** Pop-before-background is a deliberate tradeoff (memory safety > data completeness). Not a bug.

#### BUG-L: Double axis counting [FALSE POSITIVE]
- **Files:** `reasoning/gates.py`, `reasoning/sanitize.py`
- **Assessment:** Recomputation is defensive, not redundant. Code is correct.

---

## Summary

| Severity | Total | Fixed | Open | False Positive |
|----------|-------|-------|------|----------------|
| P0 | 5 | 2 | 3 | 0 |
| P1 | 4 | 4 | 0 | 0 |
| P2 | 3 | 0 | 2 | 1 |
| **Total** | **12** | **6** | **5** | **1** |

---

## Key Lesson

**Static analysis without production log analysis is incomplete.** The 2 most critical P0 bugs (DB schema, ChainID subscript) were discovered from production logs, not from reading code. Going forward: always check production logs FIRST, then do static analysis.

## Runtimo Frictions

12 frictions logged in `FRUSTRATIONS.md` — primarily around JSON escaping over SSH, missing capabilities (FileWrite, shell operators), and permission issues.
