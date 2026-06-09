//! Runtimo Daemon binary — thin wrapper around the daemon library.
//!
//! See [`runtimo_daemon::run`] for the full daemon logic.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    runtimo_daemon::run()
}
