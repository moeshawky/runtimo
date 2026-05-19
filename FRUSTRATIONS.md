# Runtimo Frictions & Issues Log

> Logged during SSH-based testing on `brain` (persistent machine, v0.2.1)
> Format: `#<id>` | Severity | Component | Description | Impact | Workaround | Status

---

## Issues

### #1 — ShellExec does not support shell operators
- **Severity:** P1
- **Component:** `core/src/capabilities/shell_exec.rs`
- **Description:** ShellExec uses direct `exec()` (Command::new), not `sh -c`. Shell operators (`&&`, `||`, `|`, `;`, `>`, `<`, `$VAR`, `$(...)`) are not parsed. Command `cd /path && ls` fails with "No such file or directory" because it looks for a literal binary named `cd /path && ls`.
- **Impact:** Cannot chain commands, use pipes, redirects, or variable expansion. Breaks most real-world usage patterns.
- **Workaround:** Use flags that accept paths directly (`git -C /path`), or run single commands only.
- **Status:** OPEN
- **Fix suggestion:** Add `shell: bool` field to ShellExecArgs. When true, wrap via `Command::new("sh").arg("-c").arg(cmd)`.

---

### #2 — FileRead JSON escaping is fragile over SSH
- **Severity:** P2
- **Component:** CLI argument parsing
- **Description:** Nested JSON escaping over SSH requires multiple levels of escaping (`\\\"`), making FileRead calls error-prone with "invalid JSON args" errors.
- **Workaround:** Use ShellExec for file reads instead.
- **Status:** OPEN

### #3 — No FileWrite capability for remote audit reports
- **Severity:** P2
- **Component:** Missing capability
- **Description:** Cannot write audit reports directly to the remote machine. Must use ShellExec with echo/heredoc.
- **Status:** OPEN

### #4 — git commit messages with spaces fail through runtimo JSON args
- **Severity:** P1
- **Component:** CLI argument parsing + ShellExec
- **Description:** `git commit -m "fix: replace hash() with hashlib.md5"` fails because runtimo parses the JSON args, then the shell splits on spaces. The commit message gets split into separate arguments: `error: pathspec 'replace' did not match any file(s)`. Quotes inside the JSON string are not preserved through the execution chain.
- **Impact:** Cannot commit changes with descriptive messages. Must use single-word messages or write commit message to a file first.
- **Workaround:** Use extremely short commit messages without special characters, or write message to temp file and use `git commit -F /tmp/msg`.
- **Status:** OPEN

### #5 — grep with special characters fails due to JSON escaping
- **Severity:** P2
- **Component:** ShellExec argument parsing
- **Description:** `grep -n "dict\[str, int\]" file.py` fails because backslashes in the JSON string are consumed by JSON parsing before reaching grep. The pattern becomes `dict[str, int]` (literal brackets) instead of the regex `dict\[str, int\]`.
- **Impact:** Cannot search for patterns containing backslashes, quotes, or other escape characters.
- **Workaround:** Use simpler grep patterns without escapes, or use Python for complex searches.
- **Status:** OPEN

### #6 — Python one-liners with quotes are impossible to pass through runtimo
- **Severity:** P1
- **Component:** ShellExec argument parsing
- **Description:** `python3 -c "import ast; ast.parse(open('file.py').read())"` fails because the nested quotes (`"`, `'`) conflict with JSON string escaping. Multiple levels of escaping (`\\\"`, `\\\\\"`) are required and still fail.
- **Impact:** Cannot run inline Python scripts for verification, syntax checking, or file manipulation. Must write scripts to files first.
- **Workaround:** Write Python scripts to `/tmp/` via `scp`, then execute via runtimo ShellExec.
- **Status:** OPEN

### #7 — sed -i with complex patterns fails
- **Severity:** P2
- **Component:** ShellExec argument parsing
- **Description:** `sed -i "s/^import json$/import hashlib\nimport json/" file.py` fails because the sed expression contains characters (`^`, `$`, `\n`) that interact badly with JSON string parsing and shell interpretation.
- **Impact:** Cannot perform in-place file edits with sed. Must use Python scripts instead.
- **Workaround:** Write Python edit scripts to `/tmp/` via `scp`, then execute.
- **Status:** OPEN

### #8 — No remote file write capability forces scp workaround
- **Severity:** P1
- **Component:** Missing capability
- **Description:** To fix BUG-4, had to write a Python script locally, `scp` it to brain's `/tmp/`, then execute via runtimo. This breaks the workflow: should be able to write files directly on the remote machine via runtimo.
- **Impact:** Every fix requires 3 steps (write local, scp, execute) instead of 1 (write remote).
- **Workaround:** Use `scp` for file creation, runtimo for execution.
- **Status:** OPEN

### #9 — pcache permission denied for non-root users
- **Severity:** P2
- **Component:** File permissions
- **Description:** `python3 -m py_compile file.py` fails with `[Errno 13] Permission denied: '__pycache__/file.cpython-313.pyc'` because the pcache directory is owned by root/docker and the runtimo user (admin) cannot write to it.
- **Impact:** Cannot verify Python syntax via py_compile. Must use AST parsing instead.
- **Workaround:** Use `python3 -c "import ast; ast.parse(open('file.py').read())"` (but see #6 — this also has escaping issues).
- **Status:** OPEN

### #10 — ShellExec JSON args require triple escaping over SSH
- **Severity:** P1
- **Component:** CLI argument parsing
- **Description:** The escaping chain is: local shell → SSH → runtimo JSON parser → ShellExec. Each layer consumes one level of escaping. To get a literal `"` to the command, you need `\\\"` at minimum. For complex commands with nested quotes, escaping becomes unreadable and error-prone.
- **Impact:** Most commands with quotes, backslashes, or special characters fail with "invalid JSON args" errors. Debugging escaping issues wastes more time than the actual fix.
- **Workaround:** Write scripts to files via scp, execute simple commands via runtimo.
- **Status:** OPEN

### #11 — grep with pipes and redirects fails
- **Severity:** P1
- **Component:** ShellExec
- **Description:** `grep pattern | head -20` fails because ShellExec doesn't support shell operators (see #1). Pipes, redirects, `&&`, `||`, `;` all fail. Also `grep -c pattern file 2>/dev/null || echo fallback` fails because the entire string is treated as a single command name.
- **Impact:** Cannot use common shell patterns for filtering, error suppression, or fallback logic.
- **Workaround:** Write scripts to files via scp, or use Python for equivalent logic.
- **Status:** OPEN

### #12 — docker exec with complex SQL fails through runtimo
- **Severity:** P1
- **Component:** ShellExec
- **Description:** `docker exec container psql -c "SELECT ..."` fails because the SQL query contains quotes, semicolons, and special characters that conflict with JSON escaping. Even simple queries with column names fail.
- **Impact:** Cannot query databases directly through runtimo. Must write Python scripts using asyncpg.
- **Workaround:** Write Python scripts that connect to DB directly via asyncpg.
- **Status:** OPEN

### #13 — No way to restart Docker container via runtimo
- **Severity:** P2
- **Component:** Missing capability
- **Description:** After applying DB schema migration or code fixes, the Docker container needs to be restarted to pick up changes. `docker restart hive-mind` requires shell operators and complex command chains.
- **Impact:** Must SSH directly to restart services, breaking the runtimo-only workflow.
- **Workaround:** Use raw SSH for container management.
- **Status:** OPEN

### #14 — ShellExec doesn't support pipes (|)
- **Severity:** P1
- **Component:** ShellExec
- **Description:** `docker logs hive-mind | grep -c "200 OK"` fails with "unknown shorthand flag" because the entire string including `|` is passed as a single argument to `docker` binary, not parsed by a shell.
- **Impact:** Cannot use any shell pipelines. Must write Python scripts to do filtering/counting.
- **Workaround:** Write Python scripts via scp, execute via ShellExec.
- **Status:** OPEN

### #15 — docker exec output truncated in ShellExec
- **Severity:** P2
- **Component:** ShellExec
- **Description:** `docker exec hive-mind cat /app/hive_mind/zvec_client.py` returned empty output (truncated at 5000 chars in script, but the actual file is ~20KB). Need to handle large file reads differently.
- **Impact:** Cannot read large files via docker exec in a single command.
- **Workaround:** Use `sed -n` to read specific line ranges, or write Python scripts to output sections.
- **Status:** OPEN
