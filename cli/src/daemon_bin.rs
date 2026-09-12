//! Runtimo daemon binary — bundled with the CLI crate.
//!
//! Part of the single-program runtimo suite: `runtimo-core` + `runtimo-daemon` + `runtimo-cli`
//! are one program at one version. `cargo install runtimo-cli` installs both `runtimo` and
//! `runtimo-daemon` binaries. The `runtimo-daemon` package is the library; the
//! `runtimo-daemon` binary delegates to [`runtimo_daemon::run`].
//!
//! The `runtimo-daemon` lib vs bin are distinguished by the binary name —
//! the lib is `runtimo_daemon`, the bin is `runtimo-daemon`.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    runtimo_daemon::run()
}
