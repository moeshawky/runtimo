//! Runtimo daemon binary — bundled with the CLI crate.
//!
//! `cargo install runtimo-cli` installs both `runtimo` and `runtimo-daemon`.
//! The daemon binary delegates to [`runtimo_daemon::run`].

fn main() -> Result<(), Box<dyn std::error::Error>> {
    runtimo_daemon::run()
}
