//! Runtimo Daemon library.
//!
//! Provides [`run`] — the daemon's main event loop. Called by both the
//! standalone `runtimo-daemon` binary and the `runtimo` CLI `--daemon` mode.

mod engine;

pub use engine::run;
