# Runtimo Field Stress Protocol

Three personas, one runtime: grandma's laptop, ephemeral ML, SSH admin.

## Personas & Failure Surfaces

| Persona | Connection | Constraint | Worst Case | Critical Capability |
|---------|-----------|------------|------------|---------------------|
| **Grandma's laptop** | Intermittent WiFi | No root, single user | Mid-write power loss corrupts config | FileWrite + Undo |
| **Ephemeral ML** | Spot preemption, 2min notice | GPU checkpointing, batch dispatch | Instance vanishes mid-training, WAL lost | Dispatch + Telemetry |
| **SSH Admin** | Flaky coffee-shop VPN | 50-server fleet, audit trail | Connection drops during `apt upgrade` | ShellExec + WAL |

## Phase 0: Sanity (always run first)

```
runtimo-daemon &
sleep 1
runtimo telemetry        # does it print without hanging?
runtimo processes        # zombies visible? PPID shown?
runtimo list             # all 6 capabilities?
```

Pass criterion: all three return in <5s, no crashes.

## Phase 1: Connectivity Chaos (grandma's laptop)

SSH drops. WiFi blips. The daemon must not leak state.

### V1: Client Disconnect During Write
```
runtimo-daemon &
sleep 1

echo '{"method":"run","params":{"capability":"FileWrite","args":{"path":"/tmp/v1.txt","content":"before"}},"id":1}' \
  | timeout 0.1 nc -U ~/.local/share/runtimo/runtimo.sock

# File must either be complete or absent — never partial
cat /tmp/v1.txt 2>/dev/null || echo "absent (ok)"
```
**Invariant:** After client disconnect, file is either intact or never created. No partial write.

### V2: Rapid Connect/Disconnect
```
for i in $(seq 1 50); do
  echo '{"method":"list","params":{},"id":1}' \
    | nc -w 1 -U ~/.local/share/runtimo/runtimo.sock 2>/dev/null &
done
wait
runtimo jobs | grep -c .
```
**Invariant:** Daemon survives 50 rapid-fire connections. No crash, no zombie sockets.

### V3: Mid-Operation SIGPIPE
```
# Start a long dispatch, kill the client mid-response
runtimo dispatch -c ShellExec -a '{"cmd":"sleep 30"}' &
sleep 0.5
kill -PIPE $!
sleep 1
runtimo jobs    # job must still be tracked
```
**Invariant:** Client death doesn't orphan the job. Job appears in `runtimo jobs`.

## Phase 2: Undo Under Fire (grandma's laptop)

The whole point of FileWrite's backup is: "I changed /etc/hosts and now DNS is broken, undo it."

### V4: Write → Write → Undo Chain
```
echo "original" > /tmp/v4.txt

# Write 1 (creates backup of "original")
runtimo run -c FileWrite -a '{"path":"/tmp/v4.txt","content":"version-1"}'
J1=$(runtimo logs -n 1 --json | jq -r '.events[0].job_id')

# Write 2 (creates backup of "version-1")
runtimo run -c FileWrite -a '{"path":"/tmp/v4.txt","content":"version-2"}'

# Undo write 1 only (restores "original")
runtimo undo -j $J1

# File must be "original"
grep -q "original" /tmp/v4.txt && echo "PASS: undo restored original" || echo "FAIL"
```
**Invariant:** Undo targets a specific job, not just "last write."

### V5: Undo After Daemon Restart
```
echo "before-restart" > /tmp/v5.txt
runtimo run -c FileWrite -a '{"path":"/tmp/v5.txt","content":"after-restart"}'
J5=$(runtimo logs -n 1 --json | jq -r '.events[0].job_id')

kill $(pgrep runtimo-daemon)
sleep 1
runtimo-daemon &
sleep 1

runtimo undo -j $J5
grep -q "before-restart" /tmp/v5.txt && echo "PASS" || echo "FAIL"
```
**Invariant:** Backups survive daemon restart. Undo works across process boundaries.

### V6: Undo With Missing Backup (graceful failure)
```
runtimo run -c FileWrite -a '{"path":"/tmp/v6.txt","content":"fresh"}' --dry-run
J6=$(runtimo logs -n 1 --json | jq -r '.events[0].job_id')
runtimo undo -j $J6 2>&1
```
**Invariant:** Undo of dry-run or nonexistent backup produces clear error, not crash.

## Phase 3: Ephemeral Machine (spot instance survival)

### V7: Dispatch Survives Daemon Kill -9
```
runtimo-daemon &
sleep 1

runtimo dispatch -c ShellExec -a '{"cmd":"sleep 10 && echo done > /tmp/v7.txt"}'
JID=$(runtimo jobs --json | jq -r '.result.jobs[0].job_id')

# Kill daemon HARD while job is running
kill -9 $(pgrep runtimo-daemon)
sleep 12  # let background thread finish

# Daemon gone, but check if the thread completed the work
ls -la /tmp/v7.txt 2>/dev/null && echo "PASS: work completed despite daemon kill" || echo "FAIL"

# Restart daemon and check WAL
runtimo-daemon &
sleep 1
runtimo status -j $JID
```
**Invariant:** Background thread survives daemon kill -9. WAL has JobCompleted on restart.

### V8: Checkpoint → Kill → Resume (ML workflow)
```
runtimo-daemon &
sleep 1

# Start training in background
runtimo dispatch -c ShellExec -a '{"cmd":"for i in $(seq 1 5); do echo checkpoint-$i >> /tmp/v8.ckpt; sleep 2; done"}'
J8=$(runtimo jobs --json | jq -r '.result.jobs[0].job_id')

# Wait for partial progress
sleep 7

# Simulate spot preemption: hard kill everything
kill -9 $(pgrep runtimo-daemon)
pkill -f "sleep 2" 2>/dev/null

sleep 2
runtimo-daemon &
sleep 1

# Verify WAL has the job record
runtimo logs -j $J8
cat /tmp/v8.ckpt 2>/dev/null | wc -l
```
**Invariant:** Partial progress is visible. Job is traceable in WAL after restart.

### V9: Telemetry Under GPU Load
```
runtimo telemetry    # baseline
runtimo telemetry    # cached (should be instant)
sleep 31
runtimo telemetry    # fresh (TTL expired)
```
**Invariant:** First call captures. Second call returns cached (<100ms). Third call recaptures after TTL.

## Phase 4: Fleet Admin (audit + concurrency)

### V10: WAL Integrity Under Concurrent Writes
```
runtimo-daemon &
sleep 1

# 20 concurrent FileWrites to different files
for i in $(seq 1 20); do
  runtimo run -c FileWrite -a "{\"path\":\"/tmp/v10-$i.txt\",\"content\":\"concurrent-$i\"}" &
done
wait

# Every file must exist with correct content
failures=0
for i in $(seq 1 20); do
  grep -q "concurrent-$i" /tmp/v10-$i.txt 2>/dev/null || failures=$((failures+1))
done
echo "Failures: $failures / 20"

# WAL must have 40 events (20 JobStarted + 20 JobCompleted)
EVENTS=$(runtimo logs -n 100 --json | jq '.result.events | length')
echo "WAL events: $EVENTS (expect 40+)"
```
**Invariant:** No lost writes under concurrency. WAL has matching event pairs.

### V11: WAL Growth is Bounded
```
wc -l ~/.local/share/runtimo/wal.jsonl 2>/dev/null
# After 100 operations, WAL should be kilobytes, not megabytes
ls -lh ~/.local/share/runtimo/wal.jsonl 2>/dev/null
```
**Invariant:** WAL size grows ~linearly with operation count. No explosion.

### V12: Dispatch Then Immediate Status
```
runtimo dispatch -c ShellExec -a '{"cmd":"sleep 3"}' 
J12=$(runtimo jobs --json | jq -r '.result.jobs[0].job_id')
sleep 1
runtimo status -j $J12    # must say "running"
sleep 3
runtimo status -j $J12    # must say "completed"
```
**Invariant:** Status transitions: running → completed. Never stuck at "running" forever.

### V13: Zombie Detection + Reaping
```
# Create a zombie by forking and immediately exiting the child
# (the parent intentionally doesn't waitpid)
python3 -c "
import os, time
pid = os.fork()
if pid == 0:
    os._exit(0)
else:
    time.sleep(60)
" &
ZPARENT=$!
sleep 1

runtimo zombies                           # must show the zombie
ZCOUNT=$(runtimo zombies 2>&1 | grep -c 'PPID:')
echo "Zombies detected: $ZCOUNT"

kill $ZPARENT 2>/dev/null
sleep 1
runtimo zombies                           # zombie should be gone
```
**Invariant:** Zombies are listed with PID + PPID + command. Reapable via parent kill.

## Phase 5: Edge Cases (field surprises)

### V14: Write to Full Disk
```
# Create a small tmpfs, fill it, try to write
mkdir -p /tmp/v14
mount -t tmpfs -o size=1M tmpfs /tmp/v14 2>/dev/null || true
dd if=/dev/zero of=/tmp/v14/filler bs=1M count=1 2>/dev/null
runtimo run -c FileWrite -a '{"path":"/tmp/v14/test.txt","content":"no-space"}' 2>&1
umount /tmp/v14 2>/dev/null
```
**Invariant:** Disk-full produces clear error message. No crash. No partial write.

### V15: Kill Nonexistent PID
```
runtimo run -c Kill -a '{"pid":999999,"signal":15}' 2>&1
```
**Invariant:** Clear error. No crash. WAL records the attempt.

### V16: Read Nonexistent File
```
runtimo run -c FileRead -a '{"path":"/tmp/never_created_xyz"}' 2>&1
```
**Invariant:** Clear error message. No panic.

### V17: ShellExec With Huge Output
```
runtimo run -c ShellExec -a '{"cmd":"yes | head -c 2000000"}' 2>&1 | wc -c
```
**Invariant:** Output is truncated to max. Command completes. No OOM.

### V18: Deeply Nested Path
```
DEEP=$(python3 -c "print('/tmp/' + '/'.join(['d']*50))")
runtimo run -c FileWrite -a "{\"path\":\"$DEEP/deep.txt\",\"content\":\"deep\"}"
ls -la "$DEEP/deep.txt" 2>/dev/null && echo "PASS: created" || echo "FAIL"
```
**Invariant:** Creates intermediate directories. Path validation doesn't reject deep nesting.

## Phase 6: Property-Based (run with proptest or manual sampling)

### P1: Job ID Uniqueness
Run 1000 dispatches. All job IDs are unique. (Already tested in integration.)

### P2: Backup Never Loses Original
For any sequence of N writes to the same file, the original content is recoverable via undo of the first write's job ID.

### P3: WAL Is Append-Only
After any sequence of operations, WAL event sequences are monotonically increasing. No gaps, no regressions.

### P4: Telemetry Is Idempotent
Two telemetry captures within 5 seconds return identical data (cached). Two captures 31 seconds apart may differ.

## Run Order

```
Phase 0: Sanity (always)
Phase 1: Connectivity (V1-V3)
Phase 2: Undo (V4-V6)
Phase 3: Ephemeral (V7-V9)
Phase 4: Fleet (V10-V13)
Phase 5: Edge (V14-V18)
Phase 6: Properties (P1-P4)
```

## Pass Criteria

| Tier | Vectors | Requirement |
|------|---------|-------------|
| **P0 (block release)** | V1-V6, V12 | All must pass. Zero failures. |
| **P1 (must fix)** | V7-V11, V13-V18 | All must pass. Known gaps documented. |
| **P2 (nice to have)** | P1-P4 | At least 3/4 pass. |

## Known Field Gaps

| Gap | Impact | Persona | Fix |
|-----|--------|---------|-----|
| Dispatch jobs lack WAL completion if daemon killed mid-write | Job status degrades to "unknown" | Ephemeral ML | Daemon writes JobCompleted on startup for unknown jobs |
| Background thread leaks if daemon killed -9 | Orphan thread runs without supervision | Ephemeral ML | Thread registers with process group that gets killed |
| No signal handling for graceful shutdown | Data loss on SIGTERM | SSH Admin | Signal handler flushes WAL before exit |
| Telemetry cache TTL is fixed at 30s | Stale data during rapid changes | All | Configurable TTL or force-refresh flag |
| Single daemon instance only | No multi-user isolation | SSH Admin (fleet) | Per-user socket paths or namespace support |

---

*Run against a live daemon. Not simulated. Field conditions only.*
