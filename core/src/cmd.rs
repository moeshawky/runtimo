//! Shared command execution helper.
//!
//! Provides `run_cmd_result` and `run_cmd`, both returning
//! `Result<String, CmdError>` so callers can distinguish "command not found"
//! from "command failed with exit code" from "I/O error spawning process".
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

/// Error type for shell command execution failures.
///
/// # Variants
/// - `NotFound`: The command executable was not found on the system PATH.
///   Contains the command string that was attempted.
/// - `Failed`: The command executed but exited with a non-zero status code.
///   Contains the exit code and captured stderr output.
/// - `Io`: The process failed to spawn (e.g., permission denied, fork failure).
///   Wraps `std::io::Error` for ergonomic `?` propagation.
///
/// # Invariants
/// - `NotFound` is distinct from `Failed`: a missing binary vs. a binary that
///   ran and returned an error.
/// - `Failed` always carries both the exit code and stderr — callers can
///   inspect either for retry decisions.
/// - `Io` wraps `std::io::Error` via `From` for ergonomic `?` propagation
///   from `Command::output()`.
///
/// # Errors
///
/// This enum IS the error type — no separate error channel exists.
/// [`run_cmd`] returns `Result<String, CmdError>`.
#[derive(Debug, thiserror::Error)]
#[allow(clippy::exhaustive_enums)] // error enums are intentionally exhaustive
pub enum CmdError {
    /// Command executable not found on the system PATH.
    ///
    /// The string value is the command that was attempted.
    #[error("command not found: {0}")]
    NotFound(String),

    /// Command executed but exited with a non-zero status code.
    ///
    /// `code` is the process exit code (always non-negative on Unix).
    /// `stderr` is the captured standard error output, trimmed.
    #[error("command failed with exit code {code}: {stderr}")]
    Failed {
        /// The non-zero exit code from the command.
        code: i32,
        /// The trimmed stderr output from the command.
        stderr: String,
    },

    /// Process failed to spawn.
    ///
    /// Wraps `std::io::Error` from `Command::output()`. Common causes:
    /// permission denied, resource limit reached, fork failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Run a shell command and return trimmed stdout, or a [`CmdError`] if the
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
/// Returns [`CmdError::NotFound`] if the command executable does not exist.
/// Returns [`CmdError::Failed`] if the command exits with a non-zero status code —
/// the variant carries both the exit code and captured stderr.
/// Returns [`CmdError::Io`] if the process fails to spawn (permission denied, fork failure).
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
pub fn run_cmd_result(cmd: &str) -> std::result::Result<String, CmdError> {
    // SECURITY: This is safe because all callers use hardcoded command literals.
    // The commands are: "cat /proc/cpuinfo | grep...", "free -h | grep...", etc.
    let output = Command::new("sh").arg("-c").arg(cmd).output()?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let code = output.status.code().unwrap_or(-1);
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(CmdError::Failed { code, stderr })
    }
}

/// Run a shell command and return trimmed stdout.
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
/// Returns [`CmdError`] if the command fails to execute or exits with a
/// non-zero status. Callers should handle the error (log, fallback, or propagate).
///
/// # Safety
///
/// **CRITICAL:** Only use with hardcoded, trusted command strings.
/// Never interpolate user input, file paths, or any external data into `cmd`.
pub fn run_cmd(cmd: &str) -> std::result::Result<String, CmdError> {
    run_cmd_result(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_cmd_echo() {
        let result = run_cmd("echo hello").unwrap();
        assert_eq!(result, "hello");
    }

    #[test]
    fn test_run_cmd_echo_with_spaces() {
        let result = run_cmd("echo 'hello world'").unwrap();
        assert!(result.contains("hello world"), "Got: {}", result);
    }

    #[test]
    fn test_run_cmd_empty_string() {
        // Empty command should succeed with empty output
        let result = run_cmd("").unwrap();
        assert_eq!(result, "", "Empty command should produce empty output");
    }

    #[test]
    fn test_run_cmd_nonexistent_command() {
        // Command that doesn't exist — sh exits with error
        let result = run_cmd("nonexistent_command_xyz_123");
        assert!(result.is_err(), "Nonexistent command should return Err");
        let err = result.unwrap_err();
        assert!(matches!(err, CmdError::Failed { .. }));
    }

    #[test]
    fn test_run_cmd_exit_nonzero() {
        // `exit 1` makes sh exit with code 1
        let result = run_cmd("exit 1");
        assert!(result.is_err(), "Non-zero exit should return Err");
        match result.unwrap_err() {
            CmdError::Failed { code, stderr: _ } => assert_eq!(code, 1),
            other => panic!("Expected CmdError::Failed, got: {:?}", other),
        }
    }

    #[test]
    fn test_run_cmd_returns_trimmed_output() {
        // run_cmd trims whitespace from output
        let result = run_cmd("echo '  spaces  '").unwrap();
        assert_eq!(result, "spaces");
    }

    #[test]
    fn test_cmd_error_display() {
        let err = CmdError::NotFound("gcc".into());
        assert!(err.to_string().contains("command not found"));

        let err = CmdError::Failed {
            code: 2,
            stderr: "no input files".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("exit code 2"));
        assert!(msg.contains("no input files"));

        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let err: CmdError = io_err.into();
        assert!(matches!(err, CmdError::Io(_)));
        assert!(err.to_string().contains("io error"));
    }

    #[test]
    fn test_cmd_error_debug_format() {
        let err = CmdError::NotFound("test".into());
        let debug = format!("{:?}", err);
        assert!(debug.contains("NotFound"));

        let err = CmdError::Failed {
            code: 1,
            stderr: "err".into(),
        };
        let debug = format!("{:?}", err);
        assert!(debug.contains("Failed"));
    }
}
