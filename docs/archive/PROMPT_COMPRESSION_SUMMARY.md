# Prompt Compression Summary

**Date:** 2026-05-16  
**Task:** Compress `AGENTS.md` + `SYSTEM_PROMPT.md` into unified agent instructions  
**Result:** Single `AGENTS.md` with both implementation guide AND execution protocol

---

## Before Compression

| Document | Purpose | Length | Gap |
|----------|---------|--------|-----|
| `AGENTS.md` | Implementation guide for FileRead | 5.3KB | No execution protocol, no telemetry requirements |
| `SYSTEM_PROMPT.md` | Runtime execution rules | 3.4KB | No implementation details, no FileRead task |
| **Total** | | **8.7KB** | **Disconnected contexts** |

---

## After Compression

| Document | Purpose | Length | Coverage |
|----------|---------|--------|----------|
| **`AGENTS.md`** | Unified guide + protocol | 12KB | ✅ Role architecture ✅ Execution protocol ✅ Telemetry ✅ Process tracking ✅ FileRead task |
| `SYSTEM_PROMPT.md` | Kept for reference | 3.4KB | Deprecated (content merged) |
| `QUICK_PROMPT.txt` | Kept for copy-paste | 2.2KB | Quick injection only |
| **Total** | | **17.6KB** (but AGENTS.md is canonical) | **Single source of truth** |

---

## What Changed

### Added to AGENTS.md
1. **Core Identity** - Persistent machine runtime (from SYSTEM_PROMPT)
2. **Non-Negotiable Rules** - "No telemetry, no execution" (from SYSTEM_PROMPT)
3. **Execution Protocol** - Before/during/after steps (from SYSTEM_PROMPT)
4. **Two-Layer Telemetry** - Hardware + process tracking (new)
5. **Resource Guards** - CPU >90% = reject, etc. (from SYSTEM_PROMPT)
6. **Output Format** - JSON with telemetry + process snapshots (from SYSTEM_PROMPT)

### Kept from Original AGENTS.md
1. **Role Architecture** - Layer 1-4 structure
2. **Task: FileRead Implementation** - Step-by-step code
3. **Failure Mode Matrix** - R-HALL, R-ASSUME, etc.
4. **Verification Commands** - cargo test, CLI examples
5. **Stigmeric Signals** - Success patterns, memory references

### Compressed/Removed
1. **Removed duplicate SYSTEM_PROMPT.md** - Content merged into AGENTS.md
2. **Removed standalone execution protocol** - Now part of main task flow
3. **Consolidated output format** - Single JSON structure with all fields

---

## Charlie Framework Application

### 1. Intent Compression
**Before:** Two separate documents (implementation + execution)  
**After:** Single compressed signal: "Execute FileRead with telemetry + process tracking on persistent machines"

### 2. Role Architecture
**Layer 1:** Rust runtime engineer (capability security + telemetry)  
**Layer 2:** Task specialist (FileRead with mandatory telemetry)  
**Layer 3:** Constraint enforcer (no telemetry = no execution)  
**Layer 4:** Output format (JSON with before/after snapshots)

### 3. Stigmergic Signaling
- **Progress Marker:** "Scaffold complete → First capability (FileRead) next"
- **Failure Signature:** "Don't skip telemetry — blindness on persistent machines"
- **Constraint Evolution:** "Originally no telemetry, now MANDATORY (2026-05-16)"

### 4. Failure-Mode Prevention
| Failure | Prevention in AGENTS.md |
|---------|------------------------|
| R-HALL | "Only use std::fs, serde_json, runtimo-core" |
| R-ASSUME | "List assumptions about system state" |
| R-EDGE | "Handle: low RAM, high CPU, zombies" |
| R-CASCADE | "Check: WAL failure, telemetry failure, spawn explosion" |

### 5. Dispatch Triggers
- **Primary:** runtimo capability execution (FileRead)
- **Secondary:** Telemetry + process tracking
- **Fallback:** Health monitoring

---

## Usage

### For Implementation Tasks
```bash
# Agent reads AGENTS.md and implements FileRead
cat /workspace/runtimo/AGENTS.md
```

### For Direct Execution
```bash
# Quick prompt injection
cat /workspace/runtimo/QUICK_PROMPT.txt
```

### For Reference
```bash
# Full prompt engineering guide
cat /workspace/runtimo/PROMPT.md
```

---

## Verification

**Build:**
```bash
cd /workspace/runtimo && cargo check
# Expected: Finished dev profile
```

**Telemetry:**
```bash
./target/debug/moe telemetry
# Expected: Hardware snapshot (CPU, RAM, disk, services)
```

**Process Tracking:**
```bash
./target/debug/moe processes
# Expected: Process snapshot (ps aux style, zombies, top consumers)
```

**FileRead (when implemented):**
```bash
./target/debug/moe run FileRead --args '{"path":"/tmp/test.txt"}'
# Expected: JSON with content + telemetry_before + telemetry_after + process counts
```

---

## Next Steps

1. **Implement FileRead** following AGENTS.md task section
2. **Verify telemetry integration** (before/after snapshots in output)
3. **Verify process tracking** (spawned PIDs logged to WAL)
4. **Test resource guards** (reject if CPU/RAM thresholds exceeded)

---

**Summary:** Compressed 8.7KB × 2 disconnected docs → 12KB unified AGENTS.md with full implementation guide + execution protocol + telemetry requirements.

*"The prompt is not the execution. The execution is not the telemetry. The telemetry is the meaning."*
