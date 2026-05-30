# Background Dispatch Stress Protocol

## System Under Test

`runtimo dispatch` → `runtimo-daemon` (Unix socket JSON-RPC) → `std::thread::spawn` → capability → WAL

## Causal Trace (SEE → MAP → CHALLENGE)

```
CLI send_rpc("dispatch") ──B3──→ Daemon handle_dispatch()
                                    ├─ Logs JobStarted to WAL (B4) ✓
                                    ├─ Inserts into BackgroundJobRegistry (B2) ✓
                                    └─ Spawns std::thread ──B1+B5──→ SYNTHETIC Registry
                                                                       └─ cap.execute() ← NAKED
                                                                          NO telemetry, NO WAL completion, NO guard
```

**Critical finding (CHALLENGE mode):** Dispatched jobs use `cap.execute()` directly instead of `execute_with_telemetry()`. This means: (1) no WAL JobCompleted/JobFailed event, (2) no resource guard, (3) no telemetry capture, (4) no spawned PID tracking, (5) synthetic registry creates a NEW BackupManager with potentially different path than main daemon. The thread exit is a black hole.

## Boundary Map (Expanded)

| Boundary | Type | Source | Sink | Authority | C1 | C2 | C3 | C4 | C5 | C6 |
|----------|------|--------|------|-----------|---|---|---|---|---|---|
| A | B3 (Message) | CLI `send_rpc()` | Daemon `handle_client()` | Same UID | YES | — | YES | YES | YES | — |
| B | B1+B5 (Call+Privilege) | `handle_dispatch()` | Synthetic `CapabilityRegistry` | **Re-created from scratch** | YES | **YES** | YES | — | — | — |
| C | B2 (Memory) | `BackgroundJobRegistry` | `handle_status()`/`handle_jobs()` | RwLock shared | YES | — | YES | YES | YES | YES |
| D | B4 (Persistence) | `cap.execute()` | WAL | **NOT USED by dispatch path** | — | — | — | — | — | — |
| E | B4 (Persistence) | `handle_dispatch()` | WAL `JobStarted` | WAL Mutex | — | — | — | YES | YES | — |
| F | B6 (Time) | Client poll loop | Daemon WAL reader | No authority check | — | — | — | YES | — | YES |
| G | B1+B1 (Call) | Background thread | `cap.execute()` | **No error propagation** | YES | — | YES | YES | — | — |

## Structural Findings (S-* from CAM Phase 0)

| Code | Finding | Boundary |
|------|---------|----------|
| S-MISSING-1 | No WAL JobCompleted/Failed logged for dispatched jobs | D |
| S-MISSING-2 | No resource guard check on dispatch path (cf. `execute_with_telemetry`) | G |
| S-MISSING-3 | No telemetry capture on dispatch path | G |
| S-MISSING-4 | No spawned PID tracking on dispatch path | G |
| S-PROMISE-1 | BackgroundJobRegistry records "running" status but thread never updates it to "completed" | C |
| S-CONTRACT-1 | Synthetic registry's BackupManager uses different path than main daemon's | B |
| S-UNREACHABLE-1 | `_wal_clone` in dispatch handler cloned but never read | — |
| G-COMP-1 | `execute_with_telemetry` provides 9 safety services; dispatch path provides 0 of them | G |

## Stress Vectors

### V1: Dispatch Storm (C5: Resource Contention + B3: Message)

**Goal**: Saturate the daemon with concurrent dispatches. Exhaust sockets, threads, or WAL mutex.

**Setup**:
```
# Start daemon in background
runtimo-daemon &

# Fire N concurrent dispatches
for i in $(seq 1 100); do
    runtimo dispatch -c ShellExec -a "{\"cmd\":\"sleep $((RANDOM % 5 + 1))\"}" &
done
wait
```

**CBP attack classes**: C5 (Resource Contention — WAL mutex, socket queue, thread pool), C4 (Ordering — dispatch order vs WAL write order)

**Expected invariants**:
- Every dispatch gets a unique job ID back (no dropped responses)
- Every job appears in `runtimo jobs` (no lost registrations)
- No daemon crash, panic, or deadlock
- No two jobs share the same job ID (P(collision) < 10⁻⁹ with 100 concurrent dispatches)

**Degenerate inputs**:
- Empty args: `"{}"`
- Malformed JSON: `"{bad"`
- Oversized args: 1MB+ JSON
- Missing capability field
- Nonexistent capability name
- Whitespace-only capability name
- Null bytes in args

---

### V2: Daemon Restart Recovery (C6: Temporal Drift + B4: Persistence)

**Goal**: Kill the daemon mid-execution. Restart. Verify WAL consistency.

**Setup**:
```
# Dispatch a long-running job
runtimo dispatch -c ShellExec -a '{"cmd":"sleep 60"}'
# Get job ID: abc123

# Wait 2 seconds, then kill daemon
sleep 2
kill -9 $(pgrep runtimo-daemon)

# Verify: job abc123 status should be "running" or "unknown" (never "completed")
runtimo status -j abc123

# Restart daemon
runtimo-daemon &

# Verify: job abc123 does NOT reappear as running (in-memory state lost)
# But WAL should have JobStarted event without matching JobCompleted
runtimo jobs
```

**CBP attack classes**: C6 (Temporal Drift — in-memory registry lost, WAL persists), C3 (Reclassification — "running" status becomes meaningless after restart), C1 (Contract Mismatch — daemon restart changes job semantics)

**Expected invariants**:
- WAL contains exactly one JobStarted per dispatched job
- No orphaned JobCompleted without matching JobStarted
- After restart, jobs that were running show "unknown" status
- Daemon binds to socket cleanly (no "address in use")
- Daemon cleans up stale socket file on start

**Stress variant**: Kill daemon with SIGKILL during WAL append (race window between `write()` and `fsync()`).

---

### V3: Registry vs WAL Disagreement (C3: Reclassification + B2: Memory + B4: Persistence)

**Goal**: Force disagreement between the in-memory BackgroundJobRegistry and the on-disk WAL.

**Setup**:
```
# Dispatch job A — daemon records it in registry + WAL
runtimo dispatch -c ShellExec -a '{"cmd":"echo A"}'  # Returns job_id: aaa
# Job A completes, WAL has JobStarted → JobCompleted

# Kill daemon, restart
kill -9 $(pgrep runtimo-daemon)
runtimo-daemon &

# Job A is now in WAL as "completed" but NOT in registry (lost on restart)

# Query status
runtimo status -j aaa  # Should read from WAL, show "completed"

# Query jobs
runtimo jobs  # Should include aaa from WAL
```

**CBP attack classes**: C3 (Reclassification — same job has different status in registry vs WAL), C4 (Ordering — WAL read races with registry read)

**Expected invariants**:
- `runtimo jobs` returns the UNION of registry jobs + WAL jobs (no duplicates)
- `runtimo status -j <id>` prefers registry (live) over WAL (historical)
- No panic when a job exists in WAL but not in registry

---

### V4: Socket Exhaustion (B3: Message + C5: Resource Contention)

**Goal**: Open many concurrent Unix socket connections. Verify daemon handles connection limits.

**Setup**:
```
# Open 1000 connections to daemon socket, send partial/invalid requests, don't close
for i in $(seq 1 1000); do
    echo '{"method":"run","params":{}}' | nc -U /home/$USER/.local/share/runtimo/runtimo.sock &
done
```

**CBP attack classes**: C5 (Resource Contention — file descriptors, tokio tasks), C4 (Ordering — incomplete requests interleaved)

**Expected invariants**:
- Daemon does not crash
- Legitimate requests still succeed
- Partial/invalid requests get error responses
- Connections without newlines don't block the event loop

---

### V5: WAL Corruption (B4: Persistence + C6: Temporal Drift)

**Goal**: Corrupt the WAL file between dispatches. Verify graceful degradation.

**Setup**:
```
# Dispatch N jobs to populate WAL
for i in $(seq 1 10); do
    runtimo dispatch -c ShellExec -a "{\"cmd\":\"echo $i\"}"
done

# Corrupt the WAL: truncate mid-event, inject garbage, remove newlines
WAL_PATH=~/.local/share/runtimo/wal.jsonl
dd if=/dev/urandom of=$WAL_PATH bs=1 count=100 seek=500 conv=notrunc
echo "garbage no json" >> $WAL_PATH

# Verify: daemon still starts and serves requests
runtimo-daemon &
runtimo dispatch -c ShellExec -a '{"cmd":"echo still works"}'
runtimo jobs  # Should show valid jobs, skip corrupted lines
```

**CBP attack classes**: C6 (Temporal Drift — corrupted historical data), C1 (Contract Mismatch — WalReader expects valid JSONL but gets garbage)

**Expected invariants**:
- Daemon starts successfully even with corrupted WAL
- `runtimo jobs` skips unparseable lines
- New jobs still write to WAL correctly
- WalReader::load() does not panic

---

### V6: Dispatch-Wait Race (C4: Ordering Dependency + B6: Time)

**Goal**: Fire dispatch and wait commands simultaneously. Verify status consistency.

**Setup**:
```
# Start dispatcher
while true; do
    runtimo dispatch -c ShellExec -a '{"cmd":"echo fast"}' | grep -oP '[a-f0-9]{32}'
    sleep 0.5
done > /tmp/jobs.txt &

# Start waiter
while read jid; do
    runtimo wait -j "$jid" --timeout 10
done < /tmp/jobs.txt &

# Let both run for 10 seconds
sleep 10
kill %1 %2
```

**CBP attack classes**: C4 (Ordering — wait might probe before dispatch returns), B6 (Time — WAL may not have JobStarted yet when status is polled), C5 (Resource — concurrent WAL reads and writes)

**Expected invariants**:
- Every job eventually reaches "completed" status
- Status never transitions: running → completed → running (monotonic)
- No "file not found" or "WAL locked" errors
- WAL events are sequential per job

---

### V7: Large Argument Propagation (B1: Function Call + B3: Message)

**Goal**: Send the largest possible arguments through dispatch. Test serialization boundaries.

**Setup**:
```
# 100KB content string
CONTENT=$(python3 -c "print('x' * 102400)")

# Via dispatch
runtimo dispatch -c FileWrite -a "{\"path\":\"/tmp/stress_large.txt\",\"content\":\"$CONTENT\"}"

# Verify written correctly
runtimo run -c FileRead -a '{"path":"/tmp/stress_large.txt"}'
wc -c /tmp/stress_large.txt
```

**CBP attack classes**: C1 (Contract Mismatch — 100KB exceeds socket buffer? JSON limits?), B1 (Function Call — spawned thread receives correct content)

**Expected invariants**:
- Full 100KB content round-trips correctly
- No truncation, no encoding corruption
- Binary-safe (null bytes, unicode, control chars)

---

### V8: Concurrent Status Queries (C5: Resource Contention + B2: Memory)

**Goal**: Hammer status/jobs RPC endpoints while dispatches are running.

**Setup**:
```
# Dispatcher: fire every 0.2 seconds
while true; do
    runtimo dispatch -c ShellExec -a '{"cmd":"sleep 2"}'
    sleep 0.2
done &

# Poller: query jobs list every 0.1 seconds
while true; do
    runtimo jobs --json >/dev/null 2>&1
    sleep 0.1
done &

# Status checker: query random job IDs
while true; do
    JIDS=$(runtimo jobs --json 2>/dev/null | jq -r '.jobs[].job_id' 2>/dev/null)
    for jid in $JIDS; do
        runtimo status -j "$jid" >/dev/null 2>&1
    done
    sleep 0.5
done &

# Run for 30 seconds
sleep 30
kill %1 %2 %3
```

**CBP attack classes**: C5 (Resource Contention — RwLock on JobRegistry, WAL Mutex, socket file descriptors), C4 (Ordering — registry update race with status query), B2 (Memory — RwLock read vs write contention)

**Expected invariants**:
- Daemon doesn't crash, panic, or deadlock
- All status queries return valid JSON
- Jobs list never contains duplicates
- No RwLock poison

---

### V9: Process Tree Leak (B1: Function Call + B5: Privilege)

**Goal**: Dispatch shell commands that spawn children. Verify cleanup.

**Setup**:
```
# Dispatch a command that spawns orphans
runtimo dispatch -c ShellExec -a '{"cmd":"(sleep 3600 &) ; echo done"}'

# Check process tree
sleep 5
ps aux | grep "sleep 3600" | grep -v grep
```

**Expected invariants**:
- Background `sleep 3600` process should NOT exist (should be reaped)
- No zombie processes from dispatched jobs
- `runtimo kill` capability can clean up strays

---

### V10: Daemon Kill During WAL Write (B4: Persistence + B6: Time)

**Goal**: Kill daemon in the middle of a WAL append. Check for torn writes.

**Setup**:
```
# Script that dispatches and immediately kills
for i in $(seq 1 20); do
    runtimo dispatch -c ShellExec -a '{"cmd":"sleep 0.1"}' &
    sleep 0.01
    kill -9 $(pgrep runtimo-daemon) 2>/dev/null
    sleep 0.1
    runtimo-daemon &
    sleep 0.2
done

# After chaos settles: verify WAL integrity
runtimo jobs
```

**Expected invariants**:
- WAL file is valid JSONL (every line parses)
- No torn writes (partial JSON objects)
- WAL sequence numbers don't regress
- Daemon recovers cleanly every time


---

### V11: Authority Confusion — Synthetic Registry (C2 + B5: Privilege)

**Goal**: Verify the background thread's synthetic CapabilityRegistry has identical security properties to the main daemon registry.

**Background**: `handle_dispatch()` spawns a thread that creates a fresh `CapabilityRegistry` from scratch. This means a different `BackupManager` instance. Two `BackupManager` instances sharing the same directory without coordination = authority confusion.

**Setup**:
```
# 1. Dispatch FileWrite via synchronous run (main registry)
echo "main" > /tmp/auth_main.txt
runtimo run -c FileWrite -a '{"path":"/tmp/auth_main.txt","content":"overwritten by main"}'

# 2. Dispatch FileWrite via background (synthetic registry)
echo "bg" > /tmp/auth_bg.txt
runtimo dispatch -c FileWrite -a '{"path":"/tmp/auth_bg.txt","content":"overwritten by bg"}'
sleep 2

# 3. Verify: both backups in same directory, both undo-able
ls -la ~/.local/share/runtimo/backups/
runtimo jobs
runtimo undo -j <job_id> --dry-run
```

**Expected invariants**:
- I13: Synthetic registry's FileWrite creates backups in same directory as main registry
- I14: Path validation enforced identically on both paths
- I15: Critical file denylist enforced identically

---

### V12: Completion Tracking — WAL Gap (G-COMP-1 + S-MISSING-1)

**Goal**: Document that dispatched jobs use `cap.execute()` directly, skipping WAL completion events.

**Setup**:
```
# Dispatch a fast job
runtimo dispatch -c ShellExec -a '{"cmd":"echo dispatched"}'
# Returns job_id: abc123

sleep 3

# Check WAL for this job
grep "abc123" ~/.local/share/runtimo/wal.jsonl
# BUG: ONLY JobStarted. NO JobCompleted.

# Compare with synchronous run
runtimo run -c ShellExec -a '{"cmd":"echo synchronous"}'
grep "<job_id>" ~/.local/share/runtimo/wal.jsonl
# OK: BOTH JobStarted AND JobCompleted
```

**Expected invariants**:
- I16: Dispatched jobs produce JobStarted in WAL
- I17: Dispatched jobs do NOT produce JobCompleted/JobFailed ← **DOCUMENTED BUG**
- I18: Synchronous runs produce BOTH events
- I19: After daemon restart, dispatched job status degrades to "unknown"

---

### V13: Thread Panic Silently Lost (G-ERR + B1)

**Goal**: What happens when a dispatched capability's thread panics? The background thread's `if let Ok(fw)` silently skips capabilities whose constructors fail. No error is logged, no status is updated.

**Setup**:
```
# Dispatch FileWrite to read-only backup location
chmod 000 ~/.local/share/runtimo/backups 2>/dev/null || true
runtimo dispatch -c FileWrite -a '{"path":"/tmp/panic_test.txt","content":"test"}'
sleep 2
chmod 755 ~/.local/share/runtimo/backups 2>/dev/null || true

# Synthetic registry's if let Ok(fw) silently skips FileWrite.
# Dispatch "succeeds" but capability never runs.
```

**Expected invariants**:
- I20: Thread panic/error does not crash daemon
- I21: Thread error leaves stale "running" entry in registry ← **DOCUMENTED BUG**
- I22: Synthetic registry errors are isolated to the thread

---

### V14: Idempotency — Duplicate Dispatch (C1)

**Goal**: Verify identical dispatch requests produce distinct jobs.

**Setup**:
```
REQUEST='{"method":"dispatch","params":{"capability":"ShellExec","args":{"cmd":"echo idem"}},"id":1}'
echo "$REQUEST" | nc -U ~/.local/share/runtimo/runtimo.sock
sleep 0.3
echo "$REQUEST" | nc -U ~/.local/share/runtimo/runtimo.sock
sleep 2
runtimo jobs
```

**Expected invariants**:
- I23: Identical requests produce different job IDs
- I24: Both jobs execute independently

---

### V15: Gradual Resource Leak (C5 + C6 + S-ENTROPY)

**Goal**: Verify no resource leak over many dispatches.

**Setup**:
```
# Dispatch 500 jobs
for i in $(seq 1 500); do
    runtimo dispatch -c ShellExec -a '{"cmd":"true"}'
    sleep 0.03
done

# Measure
ps -o pid,rss,nlwp -p $(pgrep runtimo-daemon)
wc -l ~/.local/share/runtimo/wal.jsonl
ls -lh ~/.local/share/runtimo/wal.jsonl
```

**Expected invariants**:
- I25: RSS does not grow linearly with dispatch count
- I26: Thread count returns to baseline after jobs complete
- I27: File descriptor count does not grow unbounded
- I28: WAL size grows linearly without duplication

---

### V16: Error Message Quality (G-ERR Quality)

**Goal**: Every error tells the user WHAT, WHERE, WHY, and HOW to fix.

**Setup**:
```
# 1. Daemon not running
kill $(pgrep runtimo-daemon) 2>/dev/null
runtimo dispatch -c ShellExec -a '{"cmd":"true"}'
# MUST say: "Is runtimo-daemon running?"

# 2. Nonexistent capability
runtimo-daemon &
runtimo dispatch -c NoSuchCap -a '{}'
# MUST name the capability that was requested

# 3. Invalid JSON
runtimo dispatch -c ShellExec -a '{bad'
# MUST include parse error detail

# 4. Stale socket
kill $(pgrep runtimo-daemon) 2>/dev/null
touch ~/.local/share/runtimo/runtimo.sock
runtimo dispatch -c ShellExec -a '{"cmd":"true"}'
# MUST NOT hang
```

**Expected invariants**:
- I29: Error message mentions `runtimo-daemon` by name
- I30: Error message names the requested capability
- I31: JSON parse error includes error detail
- I32: Stale socket does not cause hang

---

## Property-Based Invariants

These hold for ALL possible inputs, not just scenarios.

### P1: Job ID Uniqueness

```
Property: For any sequence of N concurrent dispatches,
          all returned job IDs are unique.
Falsification: Any two dispatches return same ID.
Generator: n in [1, 1000], arbitrary capability, arbitrary interleaving.
```

### P2: Status Monotonicity

```
Property: For any job J, observed status sequence is monotonic:
          unknown -> running -> completed.
          Never: running -> unknown, completed -> running.
Falsification: Any backward transition.
Generator: Random poll intervals, random job durations.
```

### P3: WAL Completeness

```
Property: For every dispatched job J, WAL contains exactly 1 JobStarted.
          (JobCompleted may be absent for dispatched jobs -- documented gap.)
Falsification: JobStarted missing, or duplicate JobStarted.
Generator: Arbitrary interleaved dispatch/run sequences.
```

### P4: Concurrent Dispatch Safety

```
Property: Under concurrent load, daemon never panics, deadlocks,
          or returns corrupted data.
Falsification: Any panic, timeout >30s, malformed JSON response.
Generator: Random interleaving of dispatch + status + jobs calls.
```

## Invariant Summary

| # | Invariant | Vectors | Priority |
|---|-----------|---------|----------|
| I1 | Unique job IDs | V1, P1 | P0 |
| I2 | Every job in jobs list | V1, V6 | P0 |
| I3 | Status monotonic | V6, V8, P2 | P0 |
| I4 | 1 JobStarted per job | V10, P3 | P0 |
| I5 | No resurrected jobs after restart | V2, V3 | P1 |
| I6 | Stale socket cleaned on start | V2 | P1 |
| I7 | Daemon survives WAL corruption | V5 | P1 |
| I8 | No RwLock poison | V4, V8 | P1 |
| I9 | No process leaks | V9 | P2 |
| I10 | Socket exhaustion survivable | V4 | P2 |
| I11 | Large args round-trip | V7 | P2 |
| I12 | Malformed JSON -> error, not crash | V1 | P1 |
| I13 | Synthetic registry = same backup dir | V11 | P0 |
| I14 | Path validation parity | V11 | P0 |
| I15 | Critical file parity | V11 | P0 |
| I16 | Dispatched -> JobStarted in WAL | V12 | P0 |
| I17 | Dispatched -> NO JobCompleted (BUG) | V12 | P0 |
| I18 | Synchronous -> BOTH events | V12 | P0 |
| I19 | Restart -> dispatched -> "unknown" | V12, V3 | P1 |
| I20 | Thread panic -> daemon survives | V13 | P1 |
| I21 | Thread error -> stale "running" (BUG) | V13 | P1 |
| I22 | Synthetic errors -> thread-isolated | V13 | P1 |
| I23 | Duplicate -> distinct IDs | V14 | P2 |
| I24 | Duplicate -> independent execution | V14 | P2 |
| I25 | No RSS leak (500 dispatches) | V15 | P2 |
| I26 | Thread count returns to baseline | V15 | P2 |
| I27 | No fd leak | V15 | P2 |
| I28 | WAL size linear | V15 | P2 |
| I29 | Error names daemon | V16 | P1 |
| I30 | Error names capability | V16 | P1 |
| I31 | JSON error detail | V16 | P1 |
| I32 | Stale socket no hang | V16 | P1 |

## Run Order

```
Phase 1 (Sanity):       V1 sequential, V16 error messages
Phase 2 (Structural):   V11 synthetic registry, V12 WAL gap, V13 thread panic
Phase 3 (WAL):          V5 corruption, V10 kill during write
Phase 4 (Concurrency):  V1 storm (100), V8 hammer
Phase 5 (Recovery):     V2 restart mid-job, V3 registry/WAL disagreement
Phase 6 (Resources):    V4 socket exhaust, V9 process leak, V15 gradual leak
Phase 7 (Data):         V7 large args, V14 idempotency
Phase 8 (Timing):       V6 race
Phase 9 (Properties):   P1-P4 for 1000+ random inputs
```

## Pass Criteria

- **P0 invariants (I1-I4, I13-I18)**: ALL must hold. 0 failures tolerated.
- **P1 invariants (I5-I8, I12, I19-I22, I29-I32)**: ALL must hold. 0 failures tolerated.
- **P2 invariants (I9-I11, I23-I28)**: At least 90% hold. Known gaps documented.
- **Properties (P1-P4)**: Pass for 1000+ random inputs each.
- **Global**: No daemon crash, panic, or hang in any phase.

## Known Gaps (Documented Bugs)

| Gap | Impact | CBP Classification |
|-----|--------|--------------------|
| I17 — No JobCompleted in WAL for dispatched jobs | Status degrades to "unknown" after restart | G-COMP-1 + S-MISSING-1 + C4 |
| I21 — Thread error leaves stale "running" | Status never updated, permanent "running" | G-ERR + C6 |
| S-PROMISE — BackgroundJobRegistry records "running" but thread never updates | Registry diverges from reality | C3 + C6 |
