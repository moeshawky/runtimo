//! Shared command execution helper.
//!
//! Provides a single `run_cmd` function used by telemetry and process modules
//! to avoid duplication. Returns only stdout (stderr is logged separately).
//!
//! # Security Warning
//!
//! This function uses `sh -c` to execute commands. **NEVER interpolate user input**
//! into command strings — only hardcoded, trusted commands should be used.
//! All commands in this codebase are static literals with no user data interpolation.
//!
//! Violating this rule causes shell injection attacks. Use [`std::process::Command`]
//! directly with `.arg()` for user-provided values.

use std::process::Command;

/// Run a shell command and return trimmed stdout.
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
///
/// Returns an empty string on failure. Stderr is discarded — callers should
/// not mix error output with data.
#[must_use]
pub fn run_cmd(cmd: &str) -> String {
    // SECURITY: This is safe because all callers use hardcoded command literals.
    // The commands are: "cat /proc/cpuinfo | grep...", "free -h | grep...", etc.
    // None of them interpolate user input.
    let output = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .map(|out| out.stdout)
        .unwrap_or_default();

    String::from_utf8_lossy(&output).trim().to_string()
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
