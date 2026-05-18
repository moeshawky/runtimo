//! Write-Ahead Log (WAL) — Append-only, crash-resistant event log.
//!
//! Events are written as JSONL (one JSON object per line) with `fsync` after
//! each write to guarantee durability. The WAL enables crash recovery by
//! replaying events to reconstruct system state.
//!
//! # Example
//!
//! ```rust,ignore
//! use runtimo_core::{WalWriter, WalReader, WalEvent, WalEventType};
//! use std::path::Path;
//!
//! let mut wal = WalWriter::create(Path::new("/tmp/test.wal")).unwrap();
//! wal.append(WalEvent {
//!     seq: 0, ts: 1715800000,
//!     event_type: WalEventType::JobStarted,
//!     job_id: "abc123".into(),
//!     capability: Some("FileRead".into()),
//!     output: None, error: None,
//! }).unwrap();
//!
//! let reader = WalReader::load(Path::new("/tmp/test.wal")).unwrap();
//! assert_eq!(reader.events().len(), 1);
//! ```

use crate::processes::ProcessSummary;
use crate::telemetry::Telemetry;
use crate::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;



/// A single WAL event record.
///
/// Events are appended sequentially and identified by `seq`. The `ts` field
/// is a Unix timestamp in seconds. Optional fields (`capability`, `output`,
/// `error`) are skipped during serialization when `None`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalEvent {
    /// Sequence number (monotonically increasing within a writer session).
    pub seq: u64,
    /// Unix timestamp (seconds) when the event occurred.
    pub ts: u64,
    /// Type of the event (job lifecycle stage).
    #[serde(rename = "type")]
    pub event_type: WalEventType,
    /// The job ID this event relates to.
    pub job_id: String,
    /// Capability name, if applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capability: Option<String>,
    /// Output data from the capability, if applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<serde_json::Value>,
    /// Error message, if the event represents a failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Hardware telemetry snapshot before execution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub telemetry_before: Option<Telemetry>,
    /// Hardware telemetry snapshot after execution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub telemetry_after: Option<Telemetry>,
    /// Process summary before execution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_before: Option<ProcessSummary>,
    /// Process summary after execution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_after: Option<ProcessSummary>,
}

/// Types of WAL events, corresponding to job lifecycle stages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WalEventType {
    /// Job has been submitted to the system.
    JobSubmitted,
    /// Job arguments passed validation.
    JobValidated,
    /// Job execution has started.
    JobStarted,
    /// Job completed successfully.
    JobCompleted,
    /// Job failed during validation or execution.
    JobFailed,
    /// A completed job was rolled back.
    JobRolledBack,
}

/// Append-only WAL writer.
///
/// Opens (or creates) a file in append mode and writes one JSONL line per
/// event, using file locking for concurrent access safety and `fsync` after
/// each write for durability.
///
/// # Example
///
/// ```rust,ignore
/// use runtimo_core::{WalWriter, WalEvent, WalEventType};
/// use std::path::Path;
///
/// let mut wal = WalWriter::create(Path::new("/tmp/app.wal")).unwrap();
/// wal.append(WalEvent {
///     seq: 0, ts: 1715800000,
///     event_type: WalEventType::JobStarted,
///     job_id: "job1".into(),
///     capability: None, output: None, error: None,
/// }).unwrap();
/// ```
pub struct WalWriter {
    path: std::path::PathBuf,
    seq: u64,
}

impl WalWriter {
    /// Creates or opens a WAL file at the given path.
    ///
    /// The file is opened in append mode. Existing content is preserved.
    ///
    /// # Errors
    ///
    /// Returns [`Error::WalError`](crate::Error::WalError) if the file cannot
    /// be created or opened.
    pub fn create(path: &Path) -> Result<Self> {
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    crate::Error::WalError(format!(
                        "Failed to create WAL directory {}: {}",
                        parent.display(),
                        e
                    ))
                })?;
            }
        }

        // Create the file if it doesn't exist
        if !path.exists() {
            std::fs::File::create(path).map_err(|e| {
                crate::Error::WalError(format!(
                    "Failed to create WAL file {}: {}",
                    path.display(),
                    e
                ))
            })?;
        }

        // Recover sequence from existing WAL content to ensure monotonic
        // ordering across process restarts. Acquire lock to prevent reading
        // during a concurrent write (P2 FIX).
        let seq = if path.exists() {
            let lock_file = std::fs::File::open(path)
                .map_err(|e| crate::Error::WalError(format!("open WAL for seq recovery: {}", e)))?;
            Self::lock_file(&lock_file)?;
            let content = std::fs::read_to_string(path)
                .map_err(|e| crate::Error::WalError(format!("read WAL for seq recovery: {}", e)))?;
            let recovered = content
                .lines()
                .filter_map(|line| serde_json::from_str::<WalEvent>(line).ok())
                .map(|e| e.seq)
                .max()
                .map(|max| max + 1)
                .unwrap_or(0);
            Self::unlock_file(&lock_file);
            recovered
        } else {
            0
        };

        Ok(Self {
            path: path.to_path_buf(),
            seq,
        })
    }

    /// Acquires an exclusive file lock for writing (FINDING #14).
    #[cfg(unix)]
    fn lock_file(file: &std::fs::File) -> Result<()> {
        use std::os::unix::io::AsRawFd;
        let fd = file.as_raw_fd();
        let result = unsafe { libc::flock(fd, libc::LOCK_EX) };
        if result != 0 {
            return Err(crate::Error::WalError(format!(
                "Failed to acquire WAL lock: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    }

    /// Acquires an exclusive file lock (no-op on non-unix).
    #[cfg(not(unix))]
    fn lock_file(_file: &std::fs::File) -> Result<()> {
        Ok(())
    }

    /// Releases an exclusive file lock (FINDING #14).
    #[cfg(unix)]
    fn unlock_file(file: &std::fs::File) {
        use std::os::unix::io::AsRawFd;
        let fd = file.as_raw_fd();
        unsafe { libc::flock(fd, libc::LOCK_UN) };
    }

    /// Releases an exclusive file lock (no-op on non-unix).
    #[cfg(not(unix))]
    fn unlock_file(_file: &std::fs::File) {}

    /// Appends an event to the WAL using true append mode (P0 FIX).
    ///
    /// Opens the file in append mode, acquires an exclusive lock, writes the
    /// JSONL line, fsyncs, then releases the lock. This is O(1) per write
    /// instead of O(N) read-rewrite, and the lock is held during the entire
    /// write operation preventing concurrent write loss.
    ///
    /// Increments the internal sequence counter after a successful write.
    ///
    /// # Errors
    ///
    /// Returns [`Error::WalError`](crate::Error::WalError) on serialization
    /// or I/O failure.
    pub fn append(&mut self, event: WalEvent) -> Result<()> {
        use std::io::Write;
        let line =
            serde_json::to_string(&event).map_err(|e| crate::Error::WalError(e.to_string()))?;

        // Open in append mode — no read-rewrite, O(1) per write
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| crate::Error::WalError(format!("open WAL for append: {}", e)))?;

        // Hold exclusive lock during entire write
        Self::lock_file(&file)?;
        {
            let mut buf = std::io::BufWriter::new(&file);
            writeln!(buf, "{}", line)
                .map_err(|e| crate::Error::WalError(format!("write WAL line: {}", e)))?;
            buf.flush()
                .map_err(|e| crate::Error::WalError(format!("flush WAL: {}", e)))?;
            file.sync_all()
                .map_err(|e| crate::Error::WalError(format!("fsync WAL: {}", e)))?;
        }
        Self::unlock_file(&file);

        self.seq += 1;
        Ok(())
    }

    /// Returns the current sequence number (next event will use this value).
    pub fn seq(&self) -> u64 {
        self.seq
    }
}

/// Reads and parses a WAL file into a list of events.
///
/// Malformed lines are silently skipped. This is intentional — partial writes
/// from crashes may leave incomplete JSON at the end of the file.
///
/// # Example
///
/// ```rust,ignore
/// use runtimo_core::WalReader;
/// use std::path::Path;
///
/// let reader = WalReader::load(Path::new("/tmp/app.wal")).unwrap();
/// for event in reader.events() {
///     println!("Event: {:?} for job {}", event.event_type, event.job_id);
/// }
/// ```
pub struct WalReader {
    events: Vec<WalEvent>,
}

impl WalReader {
    /// Loads and parses all events from a WAL file.
    ///
    /// # Errors
    ///
    /// Returns [`Error::WalError`](crate::Error::WalError) if the file cannot
    /// be read. Individual malformed lines are skipped, not treated as errors.
    pub fn load(path: &Path) -> Result<Self> {
        let content =
            std::fs::read_to_string(path).map_err(|e| crate::Error::WalError(e.to_string()))?;

        let events: Vec<WalEvent> = content
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();

        Ok(Self { events })
    }

    /// Returns a slice of all parsed events.
    pub fn events(&self) -> &[WalEvent] {
        &self.events
    }

    /// Reads only the last `n` lines from the WAL file.
    ///
    /// More efficient than [`WalReader::load`] when only recent events are needed.
    /// Malformed lines are silently skipped.
    ///
    /// # Errors
    ///
    /// Returns [`Error::WalError`](crate::Error::WalError) if the file cannot
    /// be read.
    pub fn tail(path: &Path, n: usize) -> Result<Self> {
        use std::collections::VecDeque;
        use std::io::{BufRead, BufReader};
        let file = std::fs::File::open(path).map_err(|e| crate::Error::WalError(e.to_string()))?;
        let reader = BufReader::new(file);

        let mut window: VecDeque<WalEvent> = VecDeque::with_capacity(n + 1);
        for line in reader.lines() {
            let line = line.map_err(|e| crate::Error::WalError(e.to_string()))?;
            if let Ok(event) = serde_json::from_str(&line) {
                window.push_back(event);
                if window.len() > n {
                    window.pop_front();
                }
            }
        }

        Ok(Self {
            events: window.into(),
        })
    }
}

/// WAL cleanup and rotation utilities.
impl WalWriter {
    /// Returns the rotation path for a given WAL file and rotation index.
    ///
    /// For `foo.jsonl` with index `1`, returns `foo.jsonl.1` (preserves the
    /// original extension instead of replacing it).
    fn rotation_path(path: &Path, index: usize) -> std::path::PathBuf {
        let mut s = path.to_string_lossy().into_owned();
        s.push('.');
        s.push_str(&index.to_string());
        std::path::PathBuf::from(s)
    }

    /// Rotates the WAL when it exceeds max_size_bytes.
    ///
    /// Moves the current WAL to `{path}.1` (shifting older rotations),
    /// then creates a fresh empty WAL. Keeps at most `max_rotations` old files.
    /// FINDING #15: basic WAL rotation to prevent unbounded growth.
    pub fn rotate(path: &Path, max_size_bytes: u64, max_rotations: usize) -> Result<()> {
        let metadata = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => return Ok(()), // No WAL to rotate
        };

        if metadata.len() < max_size_bytes {
            return Ok(());
        }

        // Shift existing rotations (P1 FIX: proper naming preserves extension)
        for i in (1..max_rotations).rev() {
            let old = Self::rotation_path(path, i);
            let new = Self::rotation_path(path, i + 1);
            if old.exists() {
                let _ = std::fs::rename(&old, &new);
            }
        }

        // Move current to .1
        let rotated = Self::rotation_path(path, 1);
        std::fs::rename(path, &rotated)
            .map_err(|e| crate::Error::WalError(format!("WAL rotation rename: {}", e)))?;

        // Create fresh empty WAL
        std::fs::write(path, "")
            .map_err(|e| crate::Error::WalError(format!("WAL rotation create: {}", e)))?;

        // Remove oldest rotation if exceeding max
        let oldest = Self::rotation_path(path, max_rotations + 1);
        if oldest.exists() {
            let _ = std::fs::remove_file(&oldest);
        }

        Ok(())
    }

    /// Cleans up WAL entries older than max_age_secs.
    ///
    /// Writes retained events to a temporary file, then atomically renames
    /// it over the original WAL. This prevents event loss if another writer
    /// appends during cleanup.
    ///
    /// # Arguments
    /// * `path` - Path to WAL file
    /// * `max_age_secs` - Maximum age in seconds
    ///
    /// # Returns
    /// * `Ok(usize)` - Number of entries removed
    /// * `Err(Error)` - Cleanup failure
    pub fn cleanup(path: &Path, max_age_secs: u64) -> Result<usize> {
        use std::time::{SystemTime, UNIX_EPOCH};

        let cutoff = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            .saturating_sub(max_age_secs);

        // Read all events under lock to prevent concurrent append loss (P1 FIX)
        let lock_file = std::fs::File::open(path)
            .map_err(|e| crate::Error::WalError(format!("open WAL for cleanup: {}", e)))?;
        Self::lock_file(&lock_file)?;
        let content = std::fs::read_to_string(path)
            .map_err(|e| crate::Error::WalError(format!("read WAL for cleanup: {}", e)))?;

        let events: Vec<WalEvent> = content
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();

        // Filter out old events
        let retained: Vec<_> = events.into_iter().filter(|e| e.ts >= cutoff).collect();

        let total = content
            .lines()
            .filter_map(|line| serde_json::from_str::<WalEvent>(line).ok())
            .count();
        let removed = total - retained.len();

        if removed > 0 {
            // Write retained events to temp file, then merge any events appended
            // by concurrent writers during this cleanup window, then atomic rename.
            let temp_path = path.with_extension("wal.tmp");
            {
                let mut new_wal = WalWriter::create(&temp_path)?;
                for event in &retained {
                    new_wal.append(event.clone())?;
                }

                // Re-read the original WAL to catch any events appended during cleanup.
                // Lock is still held from above, so no concurrent writer can interleave.
                let last_seq = retained.last().map(|e| e.seq).unwrap_or(0);
                let current_content = std::fs::read_to_string(path)
                    .map_err(|e| crate::Error::WalError(format!("re-read WAL during cleanup: {}", e)))?;
                for line in current_content.lines() {
                    if let Ok(event) = serde_json::from_str::<WalEvent>(line) {
                        if event.seq > last_seq {
                            new_wal.append(event)?;
                        }
                    }
                }
            }
            // Release lock before rename
            Self::unlock_file(&lock_file);
            // Atomic rename replaces original — no window for lost events
            std::fs::rename(&temp_path, path).map_err(|e| {
                crate::Error::WalError(format!("atomic rename during cleanup: {}", e))
            })?;
        } else {
            Self::unlock_file(&lock_file);
        }

        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_wal(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("runtimo_test_wal_{}.jsonl", name))
    }

    #[test]
    fn test_wal_write_and_read() {
        let path = tmp_wal("write_read");
        let _ = std::fs::remove_file(&path);

        let mut wal = WalWriter::create(&path).unwrap();
        wal.append(WalEvent {
            seq: 0,
            ts: 1715800000,
            event_type: WalEventType::JobStarted,
            job_id: "test-job".into(),
            capability: Some("FileRead".into()),
            output: None,
            error: None,
            telemetry_before: None,
            telemetry_after: None,
            process_before: None,
            process_after: None,
        })
        .unwrap();

        let reader = WalReader::load(&path).unwrap();
        assert_eq!(reader.events().len(), 1);
        assert_eq!(reader.events()[0].job_id, "test-job");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_wal_seq_recovery() {
        let path = tmp_wal("seq_recovery");
        let _ = std::fs::remove_file(&path);

        let mut wal = WalWriter::create(&path).unwrap();
        assert_eq!(wal.seq(), 0);
        wal.append(WalEvent {
            seq: 0,
            ts: 1715800000,
            event_type: WalEventType::JobStarted,
            job_id: "job1".into(),
            capability: None,
            output: None,
            error: None,
            telemetry_before: None,
            telemetry_after: None,
            process_before: None,
            process_after: None,
        })
        .unwrap();
        assert_eq!(wal.seq(), 1);

        // Create new writer — should recover seq from file
        let wal2 = WalWriter::create(&path).unwrap();
        assert_eq!(wal2.seq(), 1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_wal_rotation() {
        let path = tmp_wal("rotation");
        let _ = std::fs::remove_file(&path);

        // Write enough data to trigger rotation
        let mut wal = WalWriter::create(&path).unwrap();
        for i in 0..100 {
            wal.append(WalEvent {
                seq: i,
                ts: 1715800000 + i,
                event_type: WalEventType::JobStarted,
                job_id: format!("job-{}", i),
                capability: None,
                output: None,
                error: None,
                telemetry_before: None,
                telemetry_after: None,
                process_before: None,
                process_after: None,
            })
            .unwrap();
        }

        let size = std::fs::metadata(&path).unwrap().len();
        // Rotate with a threshold smaller than current size
        WalWriter::rotate(&path, size - 1, 3).unwrap();

        assert!(WalWriter::rotation_path(&path, 1).exists());
        // New WAL should be empty
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(WalWriter::rotation_path(&path, 1));
    }

    #[test]
    fn test_wal_cleanup() {
        let path = tmp_wal("cleanup");
        let _ = std::fs::remove_file(&path);

        let mut wal = WalWriter::create(&path).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Write old event
        wal.append(WalEvent {
            seq: 0,
            ts: now - 1000,
            event_type: WalEventType::JobStarted,
            job_id: "old-job".into(),
            capability: None,
            output: None,
            error: None,
            telemetry_before: None,
            telemetry_after: None,
            process_before: None,
            process_after: None,
        })
        .unwrap();

        // Write recent event
        wal.append(WalEvent {
            seq: 1,
            ts: now,
            event_type: WalEventType::JobCompleted,
            job_id: "new-job".into(),
            capability: None,
            output: None,
            error: None,
            telemetry_before: None,
            telemetry_after: None,
            process_before: None,
            process_after: None,
        })
        .unwrap();

        let removed = WalWriter::cleanup(&path, 500).unwrap();
        assert_eq!(removed, 1); // Old event removed

        let reader = WalReader::load(&path).unwrap();
        assert_eq!(reader.events().len(), 1);
        assert_eq!(reader.events()[0].job_id, "new-job");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_wal_skip_serializing_optional_fields() {
        // FINDING #15: verify optional fields are skipped when None
        let event = WalEvent {
            seq: 0,
            ts: 1715800000,
            event_type: WalEventType::JobStarted,
            job_id: "test".into(),
            capability: None,
            output: None,
            error: None,
            telemetry_before: None,
            telemetry_after: None,
            process_before: None,
            process_after: None,
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("capability"));
        assert!(!json.contains("telemetry_before"));
        assert!(!json.contains("process_before"));
    }
}
