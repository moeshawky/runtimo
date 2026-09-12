//! Runtimo Daemon library.
//!
//! Part of the single-program runtimo suite: `runtimo-core` + `runtimo-daemon` + `runtimo-cli`
//! are one program at one version. `cargo install runtimo-cli` installs both `runtimo` and
//! `runtimo-daemon` binaries. The `runtimo-daemon` package is the library; the
//! `runtimo-daemon` binary delegates to [`run`].
//!
//! The `runtimo-daemon` lib vs bin are distinguished by the binary name —
//! the lib is `runtimo_daemon`, the bin is `runtimo-daemon`.
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
