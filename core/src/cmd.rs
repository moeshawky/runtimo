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
//! Violating this rule enables shell injection attacks. Use [`std::process::Command`]
//! directly with `.arg()` for user-provided values.

use std::process::Command;

/// Run a shell command and return trimmed stdout.
///
/// # Safety
///
/// **CRITICAL:** Only use with hardcoded, trusted command strings.
/// Never interpolate user input, file paths, or any external data into `cmd`.
/// This function uses `sh -c` which enables shell injection if user data is included.
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
