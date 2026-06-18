//! Built-in capabilities.
//!
//! - [`FileRead`] — Read file contents
//! - [`FileWrite`] — Write file contents with automatic backup
//! - [`ShellExec`] — Execute shell commands with audit logging
//! - [`Undo`] — Restore files from backup
//! - [`Kill`] — Kill runaway processes by PID
//! - [`GitExec`] — Git operations with state tracking and undo support

mod file_read;
mod file_write;
mod git_exec;
mod kill;
mod shell_exec;
mod undo;

pub use file_read::{FileRead, FileReadArgs};
pub use file_write::{FileWrite, FileWriteArgs};
pub use git_exec::{GitExec, GitExecArgs};
pub use kill::{Kill, KillArgs};
pub use shell_exec::{is_dangerous_command, is_network_command, network_enabled, ShellExec, ShellExecArgs};
pub use undo::{Undo, UndoArgs};
