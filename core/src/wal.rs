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
/// event, calling `fsync` after each write for durability.
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
    file: std::fs::File,
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
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| crate::Error::WalError(e.to_string()))?;

        // Recover sequence from existing WAL content to ensure monotonic
        // ordering across process restarts.
        let seq = if path.exists() {
            if let Ok(content) = std::fs::read_to_string(path) {
                content
                    .lines()
                    .filter_map(|line| serde_json::from_str::<WalEvent>(line).ok())
                    .map(|e| e.seq)
                    .max()
                    .map(|max| max + 1)
                    .unwrap_or(0)
            } else {
                0
            }
        } else {
            0
        };

        Ok(Self { file, seq })
    }

    /// Appends an event to the WAL and calls `fsync`.
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
        writeln!(self.file, "{}", line).map_err(|e| crate::Error::WalError(e.to_string()))?;
        self.file
            .sync_all()
            .map_err(|e| crate::Error::WalError(e.to_string()))?;
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
    /// More efficient than [`load`] when only recent events are needed.
    /// Malformed lines are silently skipped.
    ///
    /// # Errors
    ///
    /// Returns [`Error::WalError`](crate::Error::WalError) if the file cannot
    /// be read.
    pub fn tail(path: &Path, n: usize) -> Result<Self> {
        use std::collections::VecDeque;
        use std::io::{BufRead, BufReader};
        let file = std::fs::File::open(path)
            .map_err(|e| crate::Error::WalError(e.to_string()))?;
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

        Ok(Self { events: window.into() })
    }
}

/// WAL cleanup and rotation utilities.
impl WalWriter {
    /// Cleans up WAL entries older than max_age_secs.
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

        // Read all events
        let content = std::fs::read_to_string(path)
            .map_err(|e| crate::Error::WalError(e.to_string()))?;
        
        let events: Vec<WalEvent> = content
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();

        // Filter out old events
        let retained: Vec<_> = events
            .into_iter()
            .filter(|e| e.ts >= cutoff)
            .collect();

        let total = content.lines().filter_map(|line| serde_json::from_str::<WalEvent>(line).ok()).count();
        let removed = total - retained.len();

        // Rewrite WAL: truncate first to prevent appending to stale data
        if removed > 0 {
            std::fs::write(path, "").map_err(|e| crate::Error::WalError(
                format!("truncate WAL before cleanup: {}", e)
            ))?;
            let mut new_wal = WalWriter::create(path)?;
            for event in retained {
                new_wal.append(event)?;
            }
        }

        Ok(removed)
    }
}
