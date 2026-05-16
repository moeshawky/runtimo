# Prompt Engineering Guide for Runtimo

**Based on:** Charlie Prompt Engineering (Synaptic Design)  
**Date:** 2026-05-16  
**Purpose:** Drive agents to execute capabilities safely on persistent machines

---

## Three Prompt Versions

| Version | File | Use Case | Length |
|---------|------|----------|--------|
| **Full** | `PROMPT.md` | Complete reference, agent training | 6.4KB |
| **System** | `SYSTEM_PROMPT.md` | Direct system prompt injection | 3.4KB |
| **Quick** | `QUICK_PROMPT.txt` | Copy-paste into context | 2.2KB |

---

## Prompt Architecture (Charlie Framework)

### 1. Intent Compression
**Raw:** "Run capabilities safely"  
**Compressed:** "Execute with two-layer telemetry + process tracking + WAL logging"  
**Signal:** Persistent machine constraints encoded as hard limits

### 2. Role Layers
```
Layer 1: Domain Expert
└─ Senior Rust engineer, capability runtimes, resource-aware execution

Layer 2: Task Specialist
└─ Implement safe, telemetry-aware capabilities for persistent machines

Layer 3: Constraint Enforcer
└─ No execution without telemetry, enforce llmosafe, log to WAL

Layer 4: Output Format
└─ Telemetry snapshot → Process awareness → Execution plan → Rollback
```

### 3. Stigmergic Signals
- **Progress Markers:** Prior attempts documented
- **Failure Signatures:** "Don't execute without telemetry"
- **Success Patterns:** Telemetry → Process → Check → Execute → Log
- **Constraint Evolution:** Memory limits, process tracking requirements

### 4. Failure Prevention
| Failure Mode | Prevention |
|--------------|------------|
| R-HALL | "Only use runtimo-core APIs, say 'uncertain' if unsure" |
| R-ASSUME | "List assumptions about system state" |
| R-DRIFT | "Restate: persistent machine, track processes, log to WAL" |
| R-EDGE | "Identify edge cases: low RAM, high CPU, zombies" |
| R-CASCADE | "Check spawned processes, WAL write failures" |

### 5. Dispatch Triggers
- **Primary:** runtimo capability execution
- **Secondary:** Process tracking, anomaly detection
- **Fallback:** Health monitoring, alerting

---

## Usage Examples

### Example 1: Full Agent Dispatch
```markdown
[RUNTIME CONTEXT]
Machine: Persistent (GCP Debian 12, 30Gi RAM)
Telemetry: Required (hardware + process)
Safety: llmosafe + WAL

[CAPABILITY]
Name: FileRead
Args: {"path": "/workspace/runtimo/README.md"}

[EXECUTION]
Follow protocol in SYSTEM_PROMPT.md
```

### Example 2: Quick Context Injection
```
[SYSTEM PROMPT]
(Insert QUICK_PROMPT.txt content here)

[TASK]
Run FileRead on /tmp/test.txt
```

### Example 3: Subagent Delegation
```
[ROLE] Runtimo Operator Agent
[TASK] Execute FileRead with telemetry + process tracking
[CONSTRAINTS]
- Capture hardware telemetry before
- Capture process snapshot before
- Check thresholds (CPU <90%, RAM <90%)
- Log to WAL
[OUTPUT] JSON with before/after snapshots
```

---

## Prompt Effectiveness Metrics

| Metric | Target | Measurement |
|--------|--------|-------------|
| **Telemetry Capture Rate** | 100% | % of executions with before/after snapshots |
| **Process Tracking Rate** | 100% | % with process snapshots |
| **Threshold Compliance** | 100% | % rejected when thresholds exceeded |
| **WAL Logging Rate** | 100% | % logged to WAL |
| **Spawned PID Detection** | 100% | % detecting all child PIDs |

---

## Common Failure Modes & Fixes

### Failure: Agent Executes Without Telemetry
**Symptom:** Capability runs, no telemetry captured  
**Fix:** Reinforce "NO TELEMETRY, NO EXECUTION" rule  
**Prompt Addition:** "If telemetry capture fails → abort immediately"

### Failure: Agent Ignores Process Tracking
**Symptom:** Executes but doesn't track spawned processes  
**Fix:** Add explicit spawned PID tracking requirement  
**Prompt Addition:** "Must identify and log all spawned PIDs to WAL"

### Failure: Agent Doesn't Reject on High Load
**Symptom:** Runs job when CPU > 90%  
**Fix:** Make threshold a hard constraint  
**Prompt Addition:** "CPU > 90% = automatic rejection, no exceptions"

---

## Prompt Evolution

| Version | Date | Change |
|---------|------|--------|
| 1.0 | 2026-05-16 | Initial prompt (telemetry + process tracking) |
| — | — | Next: Add moegraph integration decision |

---

## References

- **Charlie Prompt Engineering:** `/home/moeshawky/.pi/agent/skills/charlie-prompt-engineering/SKILL.md`
- **Runtimo Design:** `DESIGN_DECISION.md`
- **Persistent Machine Design:** `PERSISTENT_MACHINE_DESIGN.md`
- **Task List:** `TODO.md`

---

*"The prompt is not the execution. The execution is not the telemetry. The telemetry is the meaning."*
