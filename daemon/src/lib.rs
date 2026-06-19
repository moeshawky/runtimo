//! Runtimo Daemon library.
//!
//! Provides [`run`] — the daemon's main event loop. Called by both the
//! standalone `runtimo-daemon` binary and the `runtimo` CLI `--daemon` mode.
//!
//! Internal modules: `config` (paths), `auth` (peer auth), `dispatch` (RPC + jobs),
//! `engine` (state + event loop), `rpc` (JSON-RPC types), `jobs` (background jobs).
//! Not part of public API.

mod auth;
mod config;
mod dispatch;
mod engine;
mod jobs;
mod rpc;

pub use engine::run;
