//! Shared command execution helper.
//!
//! Provides `run_cmd` (legacy, returns empty string on failure) and
//! `run_cmd_result` (returns `Result` so callers can distinguish "command
//! failed" from "command returned empty string").
//!
//! # Security Warning
//!
//! This module uses `sh -c` to execute commands. **NEVER interpolate user input**
//! into command strings — only hardcoded, trusted commands should be used.
//! All commands in this codebase are static literals with no user data interpolation.
//!
//! Violating this rule causes shell injection attacks. Use [`std::process::Command`]
//! directly with `.arg()` for user-provided values.

use std::process::Command;

/// Run a shell command and return trimmed stdout, or an IO error if the
/// command fails to execute or exits with a non-zero status.
///
/// # Input
///
/// `cmd` — A shell command string (must be a hardcoded literal — see module docs).
///
/// # Output
///
/// `Ok(String)` — Trimmed stdout when the command succeeds (exit code 0).
///
/// # Errors
///
/// `Err(io::Error)` — When the process fails to spawn, or exits with a
/// non-zero status. The error message includes the failed command and
/// stderr output for debugging.
///
/// # Safety
///
/// **CRITICAL:** Only use with hardcoded, trusted command strings.
/// Never interpolate user input, file paths, or any external data into `cmd`.
/// This function uses `sh -c` which is vulnerable to shell injection if user data is included.
///
/// For user-provided values, use [`std::process::Command`] directly:
/// ```rust,ignore
/// std::process::Command::new("cat").arg(user_path).output()
/// ```
pub fn run_cmd_result(cmd: &str) -> std::io::Result<String> {
    // SECURITY: This is safe because all callers use hardcoded command literals.
    // The commands are: "cat /proc/cpuinfo | grep...", "free -h | grep...", etc.
    let output = Command::new("sh").arg("-c").arg(cmd).output()?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(std::io::Error::other(format!(
            "Command '{}' exited with {}: {}",
            cmd,
            output.status,
            stderr.trim(),
        )))
    }
}

/// Run a shell command and return trimmed stdout (legacy).
///
/// Returns an empty string on failure and logs a warning to stderr.
/// Prefer [`run_cmd_result`] for new code — it lets callers distinguish
/// "command failed" from "command returned empty string."
///
/// # Input
///
/// `cmd` — A shell command string (must be a hardcoded literal — see module docs).
///
/// # Output
///
/// Trimmed stdout on success, empty string on failure (with stderr warning).
///
/// # Safety
///
/// **CRITICAL:** Only use with hardcoded, trusted command strings.
/// Never interpolate user input, file paths, or any external data into `cmd`.
#[must_use]
pub fn run_cmd(cmd: &str) -> String {
    match run_cmd_result(cmd) {
        Ok(stdout) => stdout,
        Err(e) => {
            eprintln!("[runtimo] run_cmd failed — {} — cmd: {}", e, cmd);
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_cmd_echo() {
        let result = run_cmd("echo hello");
        assert_eq!(result, "hello");
    }

    #[test]
    fn test_run_cmd_echo_with_spaces() {
        let result = run_cmd("echo 'hello world'");
        assert!(result.contains("hello world"), "Got: {}", result);
    }

    #[test]
    fn test_run_cmd_empty_string() {
        // Empty command should return empty output (sh -c "" produces no output,
        // and run_cmd never panics — it returns empty string on any failure)
        let result = run_cmd("");
        assert_eq!(result, "", "Empty command should produce empty output");
    }

    #[test]
    fn test_run_cmd_nonexistent_command() {
        // Command that doesn't exist — sh returns error but run_cmd returns empty
        let result = run_cmd("nonexistent_command_xyz_123");
        // Should not panic, should return empty string (unwrap_or_default on error)
        // sh actually writes to stderr, not stdout, so output is empty
        assert_eq!(result, "");
    }

    #[test]
    fn test_run_cmd_exit_nonzero() {
        // `exit 1` makes sh exit with code 1 — stdout is empty
        let result = run_cmd("exit 1");
        // run_cmd returns empty string on non-zero exit because stdout is empty
        assert_eq!(result, "");
    }

    #[test]
    fn test_run_cmd_returns_trimmed_output() {
        // run_cmd trims whitespace from output
        let result = run_cmd("echo '  spaces  '");
        // echo preserves the spaces, but trim() removes them
        assert_eq!(result, "spaces");
    }
}
