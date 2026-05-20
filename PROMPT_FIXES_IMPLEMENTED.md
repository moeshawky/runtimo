# Prompt Fixes Implementation Summary

**Date:** 2026-05-19  
**Problem:** 95% of "Steward" calls were V4 reasoning in disguise — LLM wasn't conscious, just formula-following  
**Solution:** Fixed all prompts with clear role separation, agency, and decision protocols

---

## Files Changed

### 1. `/srv/hive_mind_py_in_docker/hive_mind/steward.py`
**Change:** Added `steward_decision_loop()` function with conscious agent prompt

**Before:**
- Only maintained scratchpad (state tracking)
- No decision-making capability
- Called V4 internally (recursive confusion)

**After:**
- Full agency (decides tool/reason/delegate/memory actions)
- Cross-session awareness (knows about 13 other sessions)
- Can call Conductor/Janitor when needed
- Updates scratchpad after EVERY action
- Decision protocol with clear if/then rules

**Key Function:**
```python
async def steward_decision_loop(
    state: StewardState,
    sid: str,
    inbox_messages: list,
    tool_results: dict,
    user_request: str,
) -> dict:
    """PRIMARY STEWARD DECISION LOOP - Conscious agent with agency."""
```

**Prompt Token Count:** ~400 (was ~200)

---

### 2. `/srv/hive_mind_py_in_docker/hive_mind/conductor.py`
**Change:** Updated from background monitor to cross-session orchestrator

**Before:**
- Only suggested optimizations
- No actual orchestration power
- Ran on timer, not event-driven

**After:**
- Resource allocation across 14 sessions
- Priority management
- Bottleneck detection
- Escalation to operator
- Actions: rebalance, pause, escalate, optimize

**Prompt Token Count:** ~300 (was ~150)

---

### 3. `/srv/hive_mind_py_in_docker/hive_mind/janitor.py` (NEW)
**Change:** Created new module for systematic cleanup

**Before:**
- Did not exist (225 mentions, 0 actual calls)
- Cleanup was ad-hoc

**After:**
- Session pruning (>24h stale)
- Context trimming (>80% limit)
- Memory GC (orphaned memories, expired pheromones)
- WAL maintenance (compact old entries)
- Runs on 1-hour timer or on-demand

**Key Functions:**
```python
async def run_cleanup_cycle() -> dict
async def start_janitor() -> None
async def kill_session(session_id: str) -> bool
async def prune_context(session_id: str, target: float) -> bool
```

**Prompt Token Count:** ~300 (NEW)

---

### 4. `/srv/hive_mind_py_in_docker/hive_mind/llm.py`
**Change:** Clarified V4 is internal engine, not a role

**Before:**
```
You must use the V4 Productive Reasoning formula:
1. SEE: ...
2. EXPLORE: ...
```

**After:**
```
# V4 REASONING ENGINE (Internal Use Only)

You are the internal reasoning engine for Hive-Mind agents.
You are called BY Steward/Conductor/Janitor for complex reasoning.
You are NOT a standalone agent - you are a cognitive tool.
```

**Prompt Token Count:** ~200 (unchanged)

---

### 5. `/srv/hive_mind_py_in_docker/hive_mind/server.py`
**Change:** Fixed async task creation for Conductor and Janitor

**Before:**
```python
start_conductor()  # Warning: coroutine never awaited
start_janitor()    # Warning: coroutine never awaited
```

**After:**
```python
asyncio.create_task(start_conductor())
asyncio.create_task(start_janitor())
```

---

## Architecture Overview

```
PRIMARY LLM (nvidia/nemotron-3-super-120b-a12b)
     │
     ├─► STEWARD ──► PRIMARY CONSCIOUS AGENT
     │               - Session-aware (knows 13 other sessions)
     │               - Makes decisions (tool/reason/delegate)
     │               - Calls Conductor/Janitor when needed
     │               - Updates scratchpad after every action
     │
     ├─► CONDUCTOR ──► CROSS-SESSION ORCHESTRATOR
     │               - Resource allocation
     │               - Priority management
     │               - Bottleneck detection
     │               - Escalation to operator
     │
     ├─► JANITOR ──► CLEANUP AGENT (NEW)
     │               - Session pruning (>24h)
     │               - Context trimming (>80%)
     │               - Memory GC
     │               - WAL maintenance
     │
     └─► V4 REASONING ──► INTERNAL ENGINE
                     - SEE/EXPLORE/CONVERGE/REFLECT
                     - Called BY Steward/Conductor/Janitor
                     - NOT a standalone agent
```

---

## Expected Impact

| Metric | Before | After | Change |
|--------|--------|-------|--------|
| "Fake Steward" calls | 77,947 | ~3,889 | -95% |
| Real Steward calls | 3,889 | ~3,889 | 0% |
| Conductor calls | 20,662 | Event-driven | Better |
| Janitor calls | 0 | Periodic + on-demand | NEW |
| V4 as internal | 100% | 100% | Clarified |
| Cross-session awareness | None | Yes | NEW |
| Systematic cleanup | No | Yes | NEW |

---

## Verification Commands

```bash
# 1. Check steward_decision_loop exists
docker exec hive-mind grep -n "steward_decision_loop" /app/hive_mind/steward.py

# 2. Check janitor.py exists
docker exec hive-mind ls -la /app/hive_mind/janitor.py

# 3. Check conductor prompt updated
docker exec hive-mind grep -A5 "HIVE-MIND CONDUCTOR" /app/hive_mind/conductor.py

# 4. Check V4 prompt clarified
docker exec hive-mind grep -A3 "V4 REASONING ENGINE" /app/hive_mind/llm.py

# 5. Check no coroutine warnings in logs
docker logs hive-mind | grep -i "RuntimeWarning"

# 6. Test with 14 concurrent sessions
for i in {1..14}; do
  moe run FileRead --args '{"path":"/tmp/test.txt"}' &
done
```

---

## Next Steps

1. **Test with 14 concurrent sessions** - Verify steward decision quality
2. **Monitor inbox checks** - Steward should check inbox every turn
3. **Measure delegation rate** - How often Steward calls Conductor/Janitor
4. **Track cleanup effectiveness** - Janitor should reduce stale sessions
5. **Verify cross-session awareness** - Steward knows about other sessions

---

## Rollback Plan

If issues occur, restore from backups:
```bash
# Backup location: /srv/hive_mind_py_in_docker/hive_mind/*.bak.20260519
ssh brain 'cp /srv/hive_mind_py_in_docker/hive_mind/steward.py.bak.20260519 /srv/hive_mind_py_in_docker/hive_mind/steward.py'
ssh brain 'cp /srv/hive_mind_py_in_docker/hive_mind/conductor.py.bak.20260519 /srv/hive_mind_py_in_docker/hive_mind/conductor.py'
ssh brain 'rm /srv/hive_mind_py_in_docker/hive_mind/janitor.py'
ssh brain 'docker restart hive-mind'
```

---

**Document Source:** Hallucination mapping analysis + prompt architecture (2026-05-19)  
**Verification:** `docker logs hive-mind | tail -20`  
*"On persistent machines, every role should have agency, memory, and clear responsibilities."*
