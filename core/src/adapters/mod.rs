//! Provider adapters — Tetragon + JFR with raw artifact custody.
//!
//! # Ownership
//! - **Tetragon** (`tetragon` binary, pinned v1.7.1): kernel-level exec/exit
//!   observation via `--monitor` mode only (no enforcement). Produces raw
//!   JSON artifacts hashed in manifest+WAL.
//! - **JFR** (`jcmd`, JDK17): external-attach class-load/exception
//!   observation. Produces `.jfr` artifacts hashed in manifest+WAL.
//! - **Reducer**: reduces raw provider artifacts into `RuntimeFactV1`
//!   with `RunProcessKey` (PID + start_time, never PID alone).
//!
//! # Invariants
//! - Raw artifacts (tetragon JSON, `.jfr`) are preserved as files;
//!   never forced into WAL volume. Only SHA-256 hashes enter WAL.
//! - `ProviderStatus::Degraded`/`Unavailable` surfaces when the
//!   provider binary is absent or access is denied — never silent.
//! - `SymbolUID` never appears inside raw adapters.
//! - No custom syscall/ancestry/loader/JVMTI/parser is introduced.
//! - `ObservedCall` shape is deferred (OTel blocked); no `ObservedCall`
//!   facts are produced by these adapters.
//! - PID is never used alone; always paired with `process_start_time`.
//! - Enforcement is never enabled in these adapters.

pub mod jfr;
pub mod reducer;
pub mod tetragon;

pub use jfr::JfrAdapter;
pub use jfr::JfrConfig;
pub use reducer::ArtifactReducer;
pub use tetragon::TetragonAdapter;
pub use tetragon::TetragonConfig;

use crate::runtime::ProviderStatus;
use std::path::PathBuf;

/// Result of attempting to start a provider adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::exhaustive_enums)] // new variants are semver-breaking
pub enum AdapterStartResult {
    /// Provider started successfully and is ready to observe.
    Started {
        /// Provider status (Active with version/mode).
        status: ProviderStatus,
        /// Path to the raw artifact directory.
        artifact_dir: PathBuf,
    },
    /// Provider is degraded but partially functional.
    Degraded {
        /// Provider status with degradation reason.
        status: ProviderStatus,
        /// Reason for degradation.
        reason: String,
    },
    /// Provider is unavailable (binary absent, permissions denied).
    Unavailable {
        /// Provider status with unavailability reason.
        status: ProviderStatus,
        /// Reason for unavailability.
        reason: String,
    },
}

/// Detects whether a provider binary is available on the system.
///
/// Returns the path to the binary if found, or `None` if absent.
/// This is used by both `TetragonAdapter` and `JfrAdapter` to
/// determine the initial provider status before attempting to start.
#[must_use]
pub fn detect_binary(binary_name: &str) -> Option<PathBuf> {
    // Check common paths first, then fall back to PATH lookup.
    let candidates = [
        PathBuf::from(format!("/usr/bin/{binary_name}")),
        PathBuf::from(format!("/usr/local/bin/{binary_name}")),
        PathBuf::from(binary_name.to_string()),
    ];
    for path in &candidates {
        if path.exists() {
            return Some(path.clone());
        }
    }
    // Fallback: use `which`-style detection via std::process::Command.
    // We avoid actually running the binary here — just check existence.
    std::process::Command::new("which")
        .arg(binary_name)
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let path = String::from_utf8(o.stdout).ok()?.trim().to_string();
                if PathBuf::from(&path).exists() {
                    return Some(PathBuf::from(path));
                }
            }
            None
        })
}

/// Computes the SHA-256 hash of a file.
///
/// Returns the hex-encoded SHA-256 string, or an error if the file
/// cannot be read. This is used for artifact integrity verification
/// and manifest entries.
///
/// # Errors
/// Returns an error if the file cannot be opened or read.
pub fn sha256_file(path: &PathBuf) -> Result<String, std::io::Error> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_binary_returns_none_for_absent() {
        // A binary that definitely doesn't exist should return None.
        let result = detect_binary("runtimo_nonexistent_binary_xyz");
        assert!(result.is_none(), "Absent binary should return None");
    }

    #[test]
    fn sha256_file_returns_64_char_hex() {
        // Create a temp file and hash it.
        let tmp = std::env::temp_dir().join("runtimo_adapter_test_sha.txt");
        std::fs::write(&tmp, "hello world").unwrap();
        let hash = sha256_file(&tmp).unwrap();
        assert_eq!(hash.len(), 64, "SHA-256 must be 64 hex chars");
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
        let _ = std::fs::remove_file(&tmp);
    }
}
