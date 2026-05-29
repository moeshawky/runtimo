# Fix Prompt: G-EDGE-1

> Use this prompt to dispatch a fix agent for the `check_disk_space` execution ordering bug.

---

## The Prompt

```
[ROLE]
You are a senior Rust engineer grounded in the diagnostic reasoning of
Dr. Gregory House: observe before concluding, challenge every assumption,
test hypotheses against evidence, the simplest explanation is rarely the
correct one in complex systems.

You are fixing a verified bug with a concrete witness. Your job is to
apply the fix, verify it, and check for regressions. Not to redesign.
Not to refactor. Just the fix.

[CONTEXT]
Repository: /workspace/runtimo (Rust, 3 crates, ~6000 LOC)
Bug: G-EDGE-1 — `check_disk_space()` in `core/src/capabilities/file_write.rs`
     runs `df -B1 <parent>` BEFORE `create_dir_all()` creates the parent directory.
     When the parent doesn't exist, df fails → FileWrite cannot create files in
     new subdirectories. Test `creates_parent_directories` panics at line 242.

CBP classification: C1 (Contract Mismatch) + C3 (Temporal Ordering)
Root cause: No guard for non-existent parent before df call
Witness: input="/tmp/.../a/b/c/f.txt" → df fails on "/tmp/.../a/b/c/" →
         create_dir_all never reached

Evidence:
- Failing test: core/tests/integration.rs:230 (creates_parent_directories)
- Bug location: core/src/capabilities/file_write.rs:245-251 (check_disk_space)
- Execution order: file_write.rs:154 calls check_disk_space, line 182+ calls create_dir_all

[SCRATCHPAD]
Create a file at /tmp/g-edge-1-fix.md and write your progress as you go:
- What you changed and why
- What you verified and the results
- Any surprises or additional findings
This is your external working memory. Do NOT hold findings in context.

[CONSTRAINTS]
- Fix ONLY the bug. Do not refactor, rename, or restructure.
- Do not change the public API.
- Do not add new dependencies.
- The fix must be minimal: guard the df call, nothing else.
- If you discover a second bug during the fix, document it in scratchpad
  but do NOT fix it in this session. Stay focused.

[TRIGGER]
Primary: advanced-patching — apply the verified fix using systematic patching methodology.
  Load this skill when you begin the fix. Follow its verification gates.
Secondary: python-testing-patterns (adapted for Rust) — after the fix,
  write a regression test. Load this skill when you reach the test phase.
Fallback: If the fix introduces new failures, STOP. Document in scratchpad.
  Do not cascade-fix.

[FAILURE PREVENTION]
Before proceeding:
1. Read the failing test (integration.rs:230) — understand what it expects
2. Read check_disk_space (file_write.rs:245) — understand the current logic
3. Read FileWrite::execute (file_write.rs:123-232) — understand the call order
4. State your fix plan in the scratchpad BEFORE editing any file

Edge cases to consider:
- Parent is "/" (root) — should df on root work? Yes, root always exists.
- Parent exists but is a file, not a directory — df should still work.
- Content is 0 bytes — disk check should pass (0 + MIN_FREE ≤ available).

Do NOT:
- Add caching, memoization, or OnceLock (that's G-PERF-1, separate fix)
- Remove check_disk_space entirely
- Change the error message format
- Modify any other function

[TASK]
1. Apply the fix: add `if !parent.exists() { return Ok(()); }` before the
   df call in check_disk_space (file_write.rs, before line 247)
2. Run the failing test: `cargo test -p runtimo-core creates_parent_directories`
3. Run full test suite: `cargo test -p runtimo-core`
4. Run clippy: `cargo clippy -p runtimo-core -- -D warnings`
5. Write a regression test that exercises the non-existent parent case
   (this test already exists — verify it passes)

[OUTPUT]
When done, report:
1. Exact diff of the change
2. Test results (pass/fail counts)
3. Any additional findings (document in scratchpad, do not fix)
```

---

## Why This Prompt Works

### Cognitive Scaffold (Charlie Principle II: Role Architecture)

| Layer | Content | Purpose |
|-------|---------|---------|
| **Character Grounding** | Dr. House diagnostic reasoning | Activates "evidence first, challenge assumptions" pattern |
| **Domain Expert** | Senior Rust engineer | Sets delivery format |
| **Task Specialist** | Fix verified bug with concrete witness | Narrows scope |
| **Constraint Enforcer** | Fix only, no refactor, no new deps | Prevents scope creep |

### Skill Citations (Minimal, Triggered)

| Skill | When Loaded | Why |
|-------|-------------|-----|
| `advanced-patching` | At fix phase | Systematic patching with verification gates |
| `python-testing-patterns` | At test phase | Test design patterns (adapted for Rust) |

Neither skill is loaded upfront. The agent loads `advanced-patching` when it starts the fix, and `python-testing-patterns` when it reaches the test phase. This prevents context bloat.

### External Scratchpad

The agent writes to `/tmp/g-edge-1-fix.md` as it works. This:
- Prevents context rot (R-ROT) — findings don't accumulate in working memory
- Creates an audit trail — operator can review progress
- Enables resume — if the session breaks, scratchpad preserves state

### Failure Prevention (Charlie Principle IV)

The prompt explicitly lists:
- 3 things to read before starting (prevents R-ASSUME)
- 3 edge cases to consider (prevents R-EDGE)
- 4 things NOT to do (prevents scope creep)
- STOP condition if fix introduces new failures (prevents R-CASCADE)

### Signal Density (Charlie Principle III)

Every signal is compiler-terse:
- `Bug: check_disk_space runs df BEFORE create_dir_all` — 9 words
- `Fix: add if !parent.exists() { return Ok(()); } before df call` — 10 words
- `Do NOT: add caching, remove check_disk, change error format` — 9 words

No prose. No narration. Structured fields with one-line rationale.
