# BUG-C Blast Radius Map: SID Mismatch in Grounding Gate

> Mapped: 2026-05-19 via codegraph-ferrari + direct grep analysis
> Severity: P0 (if triggered) / P2 (current impact: 8 occurrences)

---

## Root Cause

`_resolve_sid(None, session_id)` is called with `session_id` as the `explicit_sid` parameter.
If `session_id` is empty/None, the function falls through all 6 priorities and returns `"unknown"`.

**The mismatch scenario:**
1. `hook_pre_tool.py:45` records evidence under `sid = _resolve_sid(None, req.session_id)`
2. `grounding.py:153` reads evidence under `canonical_sid = _resolve_sid(None, session_id)`
3. If `req.session_id` is empty in either call → resolves to `"unknown"`
4. Evidence goes to `EvidenceStore("unknown")`, grounding reads from wrong store

---

## Data Flow

```
Claude Code hook → HTTP request (sessionId field)
    ↓
hook_pre_tool.py:45  →  _resolve_sid(None, req.session_id)  →  sid
    ↓
EvidenceStore.get(sid).record(...)  [evidence stored under sid]
    ↓
... reasoning chain starts ...
    ↓
chain.py:643  →  _gate_grounding(thoughts, session_id, chain_start_counter)
    ↓
grounding.py:153  →  _resolve_sid(None, session_id)  →  canonical_sid
    ↓
EvidenceStore.get_recent_records(canonical_sid, chain_start_counter)
    ↓
if canonical_sid != sid → EVIDENCE_STALE (no records found)
```

---

## All Call Sites (11 total)

| File | Line | Call | Risk |
|------|------|------|------|
| `hooks/hook_pre_tool.py` | 45 | `_resolve_sid(None, req.session_id)` | HIGH - records evidence |
| `hooks/hook_pre_tool.py` | 78 | `_resolve_sid(None, req.session_id)` | HIGH - SO#1 enforcement |
| `hooks/hook_post_tool.py` | 70 | `_resolve_sid(None, req.session_id)` | HIGH - records evidence |
| `hooks/hook_session_start.py` | 39 | `_resolve_sid(None, req.session_id)` | MED - session init |
| `hooks/hook_user_prompt.py` | 68 | `_resolve_sid(None, req.session_id)` | MED - context building |
| `hooks/hook_stop.py` | 59 | `_resolve_sid(None, req.session_id)` | LOW - stop processing |
| `hooks/hook_post_tool_failure.py` | 25 | `_resolve_sid(None, req.session_id)` | LOW - failure handling |
| `hooks/hook_subagent_stop.py` | 48 | `_resolve_sid(None, req.session_id)` | LOW - subagent stop |
| `hooks/helpers.py` | 42 | `_resolve_sid(None, session_id)` | MED - inbox check |
| `hooks/helpers.py` | 200 | `_resolve_sid(None, session_id)` | MED - complexity check |
| `gates/grounding.py` | 153 | `_resolve_sid(None, session_id)` | **CRITICAL** - gate check |

---

## Blast Radius

### Direct Impact (if sid="unknown")
- **EvidenceStore**: All evidence goes to "unknown" store instead of real session
- **Grounding gate**: Reads from wrong store → EVIDENCE_STALE → chain rejection
- **Steward state**: May be keyed under wrong SID
- **Training capture**: Timeline transcript keyed under wrong SID
- **Identity resolution**: Model resolution under wrong SID

### 2-hop Impact (who uses the wrong SID)
- `EvidenceStore.get_recent_records()` → returns empty → gate fails
- `StewardState` keyed under wrong SID → state isolation breach
- `session_buffer` keyed under wrong SID → training data lost
- `reasoning_buffer` keyed under wrong SID → chain persistence broken

### Cross-Session Contamination Risk
- If session A resolves to "unknown" and session B also resolves to "unknown"
- Both share the SAME EvidenceStore instance
- Session B's evidence appears in Session A's grounding check (false positive)
- Session A's evidence appears in Session B's grounding check (false positive)

---

## Why Only 8 Occurrences?

The `_resolve_sid` function has 6 fallback priorities:
1. `explicit_sid` param (if non-empty)
2. `ctx.session_id` (from MCP context)
3. `ctx.client_id` (from MCP context)
4. HTTP headers (X-Session-ID, Mcp-Session-Id, X-Hive-Session-ID)
5. Identity registry lookup
6. "unknown" (only if ALL above fail)

**Most hooks pass `req.session_id` which comes from the HTTP request** — this is usually populated.
The 8 occurrences of `sid=unknown` likely happened when:
- The HTTP request had an empty `sessionId` field
- No MCP context was available (None passed as ctx)
- No identity registry binding existed

---

## Fix Options

### Option A: Use session_id directly (no re-resolution)
```python
# grounding.py:153 - DON'T re-resolve, use session_id as-is
canonical_sid = session_id  # Already resolved by caller
```
**Pros:** Simple, eliminates mismatch entirely
**Cons:** Loses designation→session_id normalization (rarely needed)

### Option B: Pass ctx to grounding gate
```python
# grounding.py - pass MCP context for proper resolution
canonical_sid = await _resolve_sid(ctx, session_id)
```
**Pros:** Full resolution chain
**Cons:** Requires threading ctx through chain.py → grounding.py

### Option C: Log warning on "unknown" resolution
```python
# _resolve_sid - warn when falling to "unknown"
if raw_sid == "unknown":
    log.warning("_resolve_sid fell to 'unknown' for explicit_sid=%r", explicit_sid)
```
**Pros:** Diagnostic visibility
**Cons:** Doesn't fix the bug

**Recommended: Option A** — grounding gate receives session_id from chain.py which already resolved it properly. Re-resolving is redundant and introduces the mismatch risk.

---

## CodeGraph Advisory (from codegraph-ferrari)

CodeGraph blast radius for `_resolve_sid`:
- **729 entities impacted** at depth 3
- **20 cross-community** connections
- **413 functions, 177 methods, 77 classes** affected
- **Severity: CRITICAL**

Note: CodeGraph hasn't fully indexed hive-mind yet — these numbers are advisory only.
Direct grep analysis above is more accurate for this codebase.
