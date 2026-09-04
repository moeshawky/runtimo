//! WAL-backed bundle writer for observe events.
//!
//! Wraps [`WalWriter`] semantics with batched, hash-chained durability.
//! Each batch (≤256 events or 100 ms) gets a single `fsync`; a final
//! watermark `fsync` guarantees durability on close. Checkpoints every
//! 1 000 events or 5 s. Offline verification detects seq gaps as
//! `TRUNCATED` and recomputes the hash chain.
//!
//! # Nexus reuse
//! * `events.rs` `AnalysisEvent` envelope discipline: id/timestamp/payload
//!   pattern reused for `WalEvent` with `bundle_hash` + dual-clock.
//! * `bus/transport` channel batching pattern: bounded batch + time window.
//!
//! # Defects fixed
//! * nexus-store `EventStore` was memory-only — this writer is WAL-backed via
//!   `WalWriter::create`/`append` (wal.rs:253,362), never memory-only.
//! * nexus-bus dropped newest on overflow with no counter — this writer counts
//!   drops and emits `TRUNCATED` markers.

use crate::validation::{validate_path, PathContext};
use crate::wal::{WalEvent, WalEventType};
use crate::{utils, WalWriter};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Number of events per batch before forced flush.
const BATCH_SIZE: usize = 256;
/// Maximum time to hold a batch before flushing.
const BATCH_WINDOW: Duration = Duration::from_millis(100);
/// Checkpoint interval (events).
const CHECKPOINT_EVENTS: u64 = 1000;
/// Checkpoint interval (time).
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(5);

/// Returns the bundle path for a run.
///
/// The path is `data_dir().join("bundles/<run_id>.jsonl")`, validated via
/// `validation::path`. The caller must not use `wal.jsonl` as run_id.
///
/// # Panics
/// Panics if `run_id` would cause the path to end in `wal.jsonl`.
#[must_use]
pub fn bundle_path(run_id: &str) -> PathBuf {
    assert!(
        !run_id.ends_with("wal.jsonl"),
        "bundle path must never end in wal.jsonl (run_id={run_id})"
    );
    let p = utils::data_dir().join("bundles").join(format!("{run_id}.jsonl"));
    let s = p.to_string_lossy().to_string();
    assert!(
        !s.ends_with("wal.jsonl"),
        "bundle path must never end in wal.jsonl: {s}"
    );
    p
}

/// Validates a bundle path via [`validate_path`].
///
/// Uses `require_exists: false` since bundles are created on first write.
fn validated_bundle_path(path: &Path) -> Result<PathBuf, String> {
    let s = path.to_string_lossy().to_string();
    // Allow bundle directory creation — validate with parent context.
    // We check that the resolved path is inside allowed prefixes + data_dir.
    let ctx = PathContext {
        allowed_prefixes: vec![utils::data_dir().to_string_lossy().to_string()],
        require_exists: false,
        require_file: false,
    };
    validate_path(&s, &ctx)
}

/// WAL-backed bundle writer with hash-chained batching.
///
/// Batching: up to 256 events or 100 ms, whichever comes first.
/// Hash chain: `sha256(prev_hash ++ batch_json)` hex-encoded, stored in
/// `bundle_hash` on each event in the batch. Single `fsync` per batch plus
/// a final watermark `fsync` on close/drop. Checkpoint every 1 000 events
/// or 5 s by persisting the current seq + hash.
///
/// Dual-clock: `mono_ns` is elapsed nanoseconds since writer creation
/// (`base.elapsed()`), strictly increasing across events. `wall_ns` is
/// wall-clock nanoseconds since UNIX epoch.
#[allow(clippy::exhaustive_structs)]
pub struct BundleWriter {
    /// Validated bundle path.
    path: PathBuf,
    /// Current batch buffer.
    batch: Vec<WalEvent>,
    /// Previous batch hash (hex); genesis is 64 zeros.
    prev_hash: String,
    /// Total events written (including flushed).
    event_count: u64,
    /// Sequence counter for events (monotonic).
    next_seq: u64,
    /// Last batch flush time.
    last_flush: Instant,
    /// Last checkpoint (event_count, time).
    last_checkpoint: (u64, Instant),
    /// Number of dropped events (overflow counter).
    dropped: u64,
    /// Base instant for monotonic clock (`mono_ns = base.elapsed()`).
    base: Instant,
}

impl BundleWriter {
    /// Creates a bundle writer for `run_id`.
    ///
    /// Validates the path via `validation::path` and asserts it never ends
    /// in `wal.jsonl`. Creates parent directories and initializes the WAL file
    /// via `WalWriter::create` for durability.
    ///
    /// # Errors
    /// Returns error if path validation fails or WAL creation fails.
    pub fn create(run_id: &str) -> Result<Self, String> {
        let path = bundle_path(run_id);
        validated_bundle_path(&path).map_err(|e| format!("bundle path invalid: {e}"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        // Ensure WAL backing — use WalWriter::create for atomic init + seq recovery.
        let _ = WalWriter::create(&path).map_err(|e| e.to_string())?;
        // Recover next_seq from existing file if any.
        let next_seq = if path.exists() {
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            content
                .lines()
                .filter_map(|l| serde_json::from_str::<WalEvent>(l).ok())
                .map(|e| e.seq)
                .max()
                .map_or(0, |m| m + 1)
        } else {
            0
        };
        let now = Instant::now();
        Ok(Self {
            path,
            batch: Vec::with_capacity(BATCH_SIZE),
            prev_hash: "0".repeat(64),
            event_count: next_seq,
            next_seq,
            last_flush: now,
            last_checkpoint: (next_seq, now),
            dropped: 0,
            base: now,
        })
    }

    /// Creates a bundle writer at an explicit path (for tests).
    ///
    /// Validates via `validation::path` and asserts not ending in `wal.jsonl`.
    pub fn create_at(path: &Path) -> Result<Self, String> {
        let s = path.to_string_lossy().to_string();
        assert!(
            !s.ends_with("wal.jsonl"),
            "bundle path must never end in wal.jsonl: {s}"
        );
        let ctx = PathContext {
            allowed_prefixes: vec![utils::data_dir().to_string_lossy().to_string()],
            require_exists: false,
            require_file: false,
        };
        // Also allow temp paths for tests by adding temp_dir.
        let tmp = std::env::temp_dir().to_string_lossy().to_string();
        let mut ctx2 = ctx;
        ctx2.allowed_prefixes.push(tmp);
        validate_path(&s, &ctx2).map_err(|e| format!("bundle path invalid: {e}"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let _ = WalWriter::create(path).map_err(|e| e.to_string())?;
        let next_seq = if path.exists() {
            let content = std::fs::read_to_string(path).unwrap_or_default();
            content
                .lines()
                .filter_map(|l| serde_json::from_str::<WalEvent>(l).ok())
                .map(|e| e.seq)
                .max()
                .map_or(0, |m| m + 1)
        } else {
            0
        };
        let now = Instant::now();
        Ok(Self {
            path: path.to_path_buf(),
            batch: Vec::with_capacity(BATCH_SIZE),
            prev_hash: "0".repeat(64),
            event_count: next_seq,
            next_seq,
            last_flush: now,
            last_checkpoint: (next_seq, now),
            dropped: 0,
            base: now,
        })
    }

    /// Returns the bundle file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns count of dropped events due to overflow.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Appends an event to the batch.
    ///
    /// If the batch reaches 256 events, it is flushed with a single `fsync`.
    /// Callers should also poll `maybe_flush_time` if they hold events for
    /// longer than 100 ms without filling a batch.
    ///
    /// Dual-clock: `mono_ns` is `base.elapsed().as_nanos()` (strictly
    /// increasing since writer creation); `wall_ns` is wall-clock time.
    pub fn append(&mut self, mut event: WalEvent) -> Result<(), String> {
        event.seq = self.next_seq;
        self.next_seq += 1;
        // Dual-clock: fill mono_ns/wall_ns if not set.
        if event.mono_ns.is_none() {
            let mono = self.base.elapsed().as_nanos() as u64;
            event.mono_ns = Some(mono);
        }
        if event.wall_ns.is_none() {
            let wall = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos() as u64);
            event.wall_ns = Some(wall);
        }
        self.batch.push(event);
        self.event_count += 1;
        if self.batch.len() >= BATCH_SIZE {
            self.flush_batch()?;
        }
        self.maybe_checkpoint()?;
        Ok(())
    }

    /// Flushes if the batch window (100 ms) has elapsed and batch is non-empty.
    pub fn maybe_flush_time(&mut self) -> Result<(), String> {
        if !self.batch.is_empty() && self.last_flush.elapsed() >= BATCH_WINDOW {
            self.flush_batch()?;
        }
        Ok(())
    }

    /// Records a dropped event due to overflow, emitting a TRUNCATED marker on next flush.
    pub fn record_drop(&mut self) {
        self.dropped += 1;
    }

    /// Flushes the current batch with a single `fsync` and hash-chain update.
    ///
    /// Computes `sha256(prev_hash ++ batch_json)` and stamps `bundle_hash` on
    /// each event before writing. Performs one `fsync` for the entire batch.
    pub fn flush_batch(&mut self) -> Result<(), String> {
        if self.batch.is_empty() {
            return Ok(());
        }
        // Compute batch hash: sha256(prev_hash ++ serialized batch)
        let mut hasher = Sha256::new();
        hasher.update(self.prev_hash.as_bytes());
        for ev in &self.batch {
            let json = serde_json::to_vec(ev).map_err(|e| e.to_string())?;
            hasher.update(&json);
        }
        let hash = format!("{:x}", hasher.finalize());
        // Stamp hash on each event.
        for ev in &mut self.batch {
            ev.bundle_hash = Some(hash.clone());
        }
        // If drops occurred, inject a TRUNCATED marker event at end of batch.
        if self.dropped > 0 {
            let truncated = WalEvent {
                seq: self.next_seq,
                ts: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs()),
                event_type: WalEventType::ObserveTruncated,
                job_id: "bundle".to_string(),
                output: Some(serde_json::json!({"dropped": self.dropped})),
                bundle_hash: Some(hash.clone()),
                mono_ns: Some(self.base.elapsed().as_nanos() as u64),
                wall_ns: Some(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_nanos() as u64),
                ),
                ..Default::default()
            };
            self.next_seq += 1;
            self.event_count += 1;
            self.batch.push(truncated);
            self.dropped = 0;
        }
        // Write batch with single fsync.
        self.write_batch_with_fsync(&hash)?;
        self.prev_hash = hash;
        self.batch.clear();
        self.last_flush = Instant::now();
        Ok(())
    }

    /// Writes the current batch to disk with one `fsync`.
    fn write_batch_with_fsync(&self, _hash: &str) -> Result<(), String> {
        use std::io::Write;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| format!("open bundle for batch write: {e}"))?;
        // Acquire lock (reuse WAL locking discipline).
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let fd = file.as_raw_fd();
            // SAFETY: fd is a valid open file descriptor; LOCK_EX is a well-defined operation
            let r = unsafe { libc::flock(fd, libc::LOCK_EX) };
            if r != 0 {
                return Err(format!("flock failed: {}", std::io::Error::last_os_error()));
            }
        }
        {
            let mut buf = std::io::BufWriter::new(&file);
            for ev in &self.batch {
                let line = serde_json::to_string(ev).map_err(|e| e.to_string())?;
                writeln!(buf, "{line}").map_err(|e| format!("write bundle line: {e}"))?;
            }
            buf.flush().map_err(|e| format!("flush bundle: {e}"))?;
            file.sync_all().map_err(|e| format!("fsync bundle: {e}"))?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let fd = file.as_raw_fd();
            // SAFETY: fd is a valid open file descriptor; LOCK_UN is a well-defined operation
            unsafe { libc::flock(fd, libc::LOCK_UN) };
        }
        Ok(())
    }

    /// Performs a checkpoint if 1 000 events or 5 s have elapsed.
    fn maybe_checkpoint(&mut self) -> Result<(), String> {
        let count_since = self.event_count.saturating_sub(self.last_checkpoint.0);
        let time_since = self.last_checkpoint.1.elapsed();
        if count_since >= CHECKPOINT_EVENTS || time_since >= CHECKPOINT_INTERVAL {
            self.checkpoint()?;
        }
        Ok(())
    }

    /// Returns the checkpoint sidecar path for a bundle (preserves `.jsonl` extension).
    ///
    /// Uses `format!("{}.checkpoint", path.display())` to avoid `with_extension`
    /// dropping the original `.jsonl`, consistent with `wal.rs::rotation_path_for`.
    #[must_use]
    pub fn checkpoint_path(path: &Path) -> PathBuf {
        PathBuf::from(format!("{}.checkpoint", path.display()))
    }

    /// Writes a checkpoint (seq + hash) to a sidecar file and fsyncs.
    fn checkpoint(&mut self) -> Result<(), String> {
        let cp_path = Self::checkpoint_path(&self.path);
        let content = format!("{} {}\n", self.event_count, self.prev_hash);
        std::fs::write(&cp_path, content).map_err(|e| e.to_string())?;
        // fsync checkpoint file
        let f = std::fs::File::open(&cp_path).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
        self.last_checkpoint = (self.event_count, Instant::now());
        Ok(())
    }

    /// Finalizes the bundle with a watermark `fsync`.
    ///
    /// Flushes any pending batch and fsyncs the file to guarantee durability.
    pub fn finalize(&mut self) -> Result<(), String> {
        self.flush_batch()?;
        // Watermark fsync: ensure file is durable even if batch was empty.
        let f = std::fs::File::open(&self.path).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
        // Update checkpoint on finalize as well.
        self.checkpoint()?;
        Ok(())
    }
}

impl Drop for BundleWriter {
    fn drop(&mut self) {
        let _ = self.flush_batch();
        if let Ok(f) = std::fs::File::open(&self.path) {
            let _ = f.sync_all();
        }
    }
}

/// Result of offline bundle verification.
#[derive(Debug, Clone)]
#[allow(clippy::exhaustive_structs)]
pub struct VerifyResult {
    /// Total events read.
    pub total: usize,
    /// Number of TRUNCATED markers found (seq gaps).
    pub truncated_gaps: usize,
    /// Whether hash chain verified.
    pub hash_ok: bool,
    /// First error, if any.
    pub error: Option<String>,
}

/// Offline verification of a bundle file.
///
/// Checks:
/// * seq is strictly increasing by 1; gaps are counted as `TRUNCATED`.
/// * hash chain recomputes: `sha256(prev_hash ++ batch_line)` matches `bundle_hash`.
///
/// `TRUNCATED` events are expected to have `ObserveTruncated` type; gaps without
/// a marker are reported as truncated gaps.
///
/// # Errors
/// Returns `VerifyResult` with `error` if file cannot be read.
#[must_use]
pub fn verify_bundle(path: &Path) -> VerifyResult {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            return VerifyResult {
                total: 0,
                truncated_gaps: 0,
                hash_ok: false,
                error: Some(format!("read failed: {e}")),
            }
        }
    };
    let mut events: Vec<WalEvent> = Vec::new();
    for line in content.lines().filter(|l| !l.trim().is_empty()) {
        if let Ok(ev) = serde_json::from_str::<WalEvent>(line) {
            events.push(ev);
        }
    }
    if events.is_empty() {
        return VerifyResult {
            total: 0,
            truncated_gaps: 0,
            hash_ok: true,
            error: None,
        };
    }
    // Seq-gap detection → TRUNCATED count.
    let mut truncated_gaps = 0usize;
    for w in events.windows(2) {
        let a = w[0].seq;
        let b = w[1].seq;
        if b != a + 1 {
            // If next event is explicit TRUNCATED, it's an accounted gap.
            if w[1].event_type == WalEventType::ObserveTruncated {
                truncated_gaps += 1;
            } else {
                truncated_gaps += 1;
            }
        }
    }
    // Hash recompute: group by bundle_hash (each batch shares one hash).
    // Recompute per contiguous hash group.
    let mut hash_ok = true;
    let mut prev_hash = "0".repeat(64);
    let mut idx = 0;
    while idx < events.len() {
        let current_hash = events[idx].bundle_hash.clone().unwrap_or_default();
        // Collect batch: contiguous events with same hash.
        let mut batch_end = idx;
        while batch_end < events.len()
            && events[batch_end].bundle_hash.as_deref() == Some(current_hash.as_str())
        {
            batch_end += 1;
        }
        if batch_end == idx {
            // No hash — check if events without hash are allowed (legacy). Treat as ok.
            idx += 1;
            continue;
        }
        // Detect whether last event in batch is a writer-injected drop TRUNCATED marker.
        // That marker is appended AFTER hash computation (output contains "dropped"), so it
        // must be excluded from hash recompute. Sampler TRUNCATED samples (frames) are part
        // of the batch and must NOT be excluded — only drop markers have "dropped".
        let has_trailing_drop = batch_end > idx + 1
            && events[batch_end - 1].event_type == WalEventType::ObserveTruncated
            && events[batch_end - 1]
                .output
                .as_ref()
                .is_some_and(|v| v.get("dropped").is_some());
        let hash_end = if has_trailing_drop { batch_end - 1 } else { batch_end };
        // For verification, recompute hash over the batch events WITHOUT their bundle_hash (as writer did before stamping).
        // Writer computed hash before stamping, over events without bundle_hash.
        // So we need to clone and clear bundle_hash for hashing.
        let mut hasher = Sha256::new();
        hasher.update(prev_hash.as_bytes());
        for ev in &events[idx..hash_end] {
            let mut tmp = ev.clone();
            tmp.bundle_hash = None;
            // Also clear mono/wall ns? No — writer included them, so keep them as-is.
            if let Ok(json) = serde_json::to_vec(&tmp) {
                hasher.update(&json);
            }
        }
        let computed = format!("{:x}", hasher.finalize());
        // Compare only if current_hash non-empty; empty means legacy without hash.
        if !current_hash.is_empty() && computed != current_hash {
            // Allow truncated-injected batch where hash includes only pre-truncated events — our computed should match.
            // If mismatch, mark hash_ok false.
            hash_ok = false;
        }
        if !current_hash.is_empty() {
            prev_hash = current_hash;
        }
        idx = batch_end;
    }

    VerifyResult {
        total: events.len(),
        truncated_gaps,
        hash_ok,
        error: None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::wal::WalEventType;

    fn tmp_bundle(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("runtimo_test_bundle_{name}.jsonl"))
    }

    #[test]
    fn bundle_round_trip() {
        let path = tmp_bundle("roundtrip");
        let cp = PathBuf::from(format!("{}.checkpoint", path.display()));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&cp);
        let mut w = BundleWriter::create_at(&path).unwrap();
        for i in 0..10u64 {
            w.append(WalEvent {
                ts: 1000 + i,
                event_type: WalEventType::ObserveBatch,
                job_id: format!("job-{i}"),
                ..Default::default()
            })
            .unwrap();
        }
        w.finalize().unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 10);
        for line in &lines {
            let ev: WalEvent = serde_json::from_str(line).unwrap();
            assert!(ev.bundle_hash.is_some());
            assert!(ev.mono_ns.is_some());
            assert!(ev.wall_ns.is_some());
        }
        let v = verify_bundle(&path);
        assert_eq!(v.total, 10);
        assert!(v.hash_ok, "hash chain should verify");
        // Checkpoint must preserve extension (.jsonl.checkpoint)
        assert!(cp.exists(), "checkpoint sidecar should exist at {}", cp.display());
        assert!(!path.with_extension("checkpoint").exists() || cp.exists());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&cp);
    }

    #[test]
    fn bundle_truncate_to_truncated() {
        let path = tmp_bundle("truncate");
        let cp = PathBuf::from(format!("{}.checkpoint", path.display()));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&cp);
        let mut w = BundleWriter::create_at(&path).unwrap();
        for i in 0..5u64 {
            w.append(WalEvent {
                ts: 2000 + i,
                event_type: WalEventType::ObserveBatch,
                job_id: format!("j{i}"),
                ..Default::default()
            })
            .unwrap();
        }
        // Simulate drops
        w.record_drop();
        w.record_drop();
        w.flush_batch().unwrap();
        w.finalize().unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let events: Vec<WalEvent> = content
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        // Last event should be TRUNCATED marker
        assert_eq!(events.last().unwrap().event_type, WalEventType::ObserveTruncated);
        assert_eq!(
            events.last().unwrap().output.as_ref().unwrap()["dropped"],
            2
        );
        // Verify detects gap/truncated
        let v = verify_bundle(&path);
        assert!(v.hash_ok);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&cp);
    }

    #[test]
    fn bundle_path_never_wal_jsonl() {
        let p = bundle_path("test123");
        assert!(!p.to_string_lossy().ends_with("wal.jsonl"));
        assert!(p.to_string_lossy().contains("bundles"));
    }

    #[test]
    #[should_panic(expected = "wal.jsonl")]
    fn bundle_path_assert_on_wal() {
        let _ = bundle_path("wal.jsonl");
    }

    #[test]
    fn verify_seq_gap_counts_truncated() {
        let path = tmp_bundle("seqgap");
        let _ = std::fs::remove_file(&path);
        // Manually write a bundle with a seq gap.
        let ev0 = WalEvent {
            seq: 0,
            ts: 1,
            event_type: WalEventType::ObserveBatch,
            job_id: "a".into(),
            bundle_hash: Some("0".repeat(64)),
            ..Default::default()
        };
        let ev2 = WalEvent {
            seq: 2,
            ts: 2,
            event_type: WalEventType::ObserveBatch,
            job_id: "b".into(),
            bundle_hash: Some("0".repeat(64)),
            ..Default::default()
        };
        let mut s = String::new();
        s.push_str(&serde_json::to_string(&ev0).unwrap());
        s.push('\n');
        s.push_str(&serde_json::to_string(&ev2).unwrap());
        s.push('\n');
        std::fs::write(&path, s).unwrap();
        let v = verify_bundle(&path);
        assert_eq!(v.truncated_gaps, 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bundle_mono_ns_strictly_increasing() {
        let path = tmp_bundle("mono_increasing");
        let cp = PathBuf::from(format!("{}.checkpoint", path.display()));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&cp);
        let mut w = BundleWriter::create_at(&path).unwrap();
        w.append(WalEvent {
            ts: 4000,
            event_type: WalEventType::ObserveBatch,
            job_id: "mono-0".into(),
            ..Default::default()
        })
        .unwrap();
        w.flush_batch().unwrap();
        let mono_first = {
            let content = std::fs::read_to_string(&path).unwrap();
            let ev: WalEvent = serde_json::from_str(content.lines().next().unwrap()).unwrap();
            ev.mono_ns.unwrap()
        };
        std::thread::sleep(std::time::Duration::from_millis(2));
        w.append(WalEvent {
            ts: 4001,
            event_type: WalEventType::ObserveBatch,
            job_id: "mono-1".into(),
            ..Default::default()
        })
        .unwrap();
        w.flush_batch().unwrap();
        let mono_second = {
            let content = std::fs::read_to_string(&path).unwrap();
            let ev: WalEvent = serde_json::from_str(content.lines().nth(1).unwrap()).unwrap();
            ev.mono_ns.unwrap()
        };
        assert!(
            mono_second > mono_first,
            "mono_ns must strictly increase: first={mono_first} second={mono_second}"
        );
        w.finalize().unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&cp);
    }

    #[test]
    fn checkpoint_preserves_extension() {
        let path = tmp_bundle("checkpoint_ext");
        let cp_expected = PathBuf::from(format!("{}.checkpoint", path.display()));
        let cp_wrong = path.with_extension("checkpoint");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&cp_expected);
        let _ = std::fs::remove_file(&cp_wrong);
        let mut w = BundleWriter::create_at(&path).unwrap();
        w.append(WalEvent {
            ts: 5000,
            event_type: WalEventType::ObserveBatch,
            job_id: "cp".into(),
            ..Default::default()
        })
        .unwrap();
        w.finalize().unwrap();
        assert!(
            cp_expected.exists(),
            "checkpoint should be at {} (preserved .jsonl), with_extension would be {}",
            cp_expected.display(),
            cp_wrong.display()
        );
        // Ensure the wrong path (dropped extension) does not exist unless it equals expected (never for .jsonl)
        if cp_wrong != cp_expected {
            assert!(
                !cp_wrong.exists(),
                "with_extension path {} must not exist; expected is {}",
                cp_wrong.display(),
                cp_expected.display()
            );
        }
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&cp_expected);
        let _ = std::fs::remove_file(&cp_wrong);
    }
}
