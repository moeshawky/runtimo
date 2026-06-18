//! Runtimo Daemon library.
//!
//! Provides [`run`] — the daemon's main event loop. Called by both the
//! standalone `runtimo-daemon` binary and the `runtimo` CLI `--daemon` mode.
//!
//! Internal modules: `config` (paths), `auth` (peer auth), `dispatch` (RPC + jobs),
//! `engine` (state + event loop). Not part of public API.

mod config;
mod auth;
mod dispatch;
mod engine;

pub use engine::run;
