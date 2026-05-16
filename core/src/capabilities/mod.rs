//! Built-in capabilities.
//!
//! Ships with the Runtimo runtime:
//! - [`FileRead`] — Read file contents with path traversal protection
//! - [`FileWrite`] — Write file contents with backup-before-mutate for undo

pub mod file_read;
pub mod file_write;

pub use file_read::FileRead;
pub use file_write::FileWrite;
