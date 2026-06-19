//! Background job registry for the daemon dispatch system.
//!
//! Thread-safe registry of in-flight and recently-completed background jobs.

use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};

/// Maximum concurrent background jobs across all dispatch calls.
pub const MAX_CONCURRENT_JOBS: u32 = 16;

/// A tracked background job dispatched via the `dispatch` RPC method.
#[derive(Debug, Clone, Serialize)]
pub struct BackgroundJob {
    /// Unique job identifier.
    pub job_id: String,
    /// Capability name being executed.
    pub capability: String,
    /// Current status: "running", "completed", or "failed".
    pub status: String,
    /// Unix timestamp when the job was dispatched.
    pub started_at: u64,
    /// Unix timestamp when the job finished (absent while running).
    pub finished_at: Option<u64>,
    /// Error message if the job failed.
    pub result: Option<String>,
}

/// Thread-safe registry of in-flight and recently-completed background jobs.
///
/// Uses an atomic counter to enforce `MAX_CONCURRENT_JOBS` and a `std::sync::RwLock`-backed
/// map for status queries. Synchronous methods allow use from both async and
/// blocking (spawn_blocking) contexts without nested runtimes.
pub struct BackgroundJobRegistry {
    /// Job map keyed by job ID.
    jobs: std::sync::RwLock<HashMap<String, BackgroundJob>>,
    /// Count of currently-running background jobs.
    running: AtomicU32,
}

impl BackgroundJobRegistry {
    /// Creates a new empty background job registry.
    pub fn new() -> Self {
        Self {
            jobs: std::sync::RwLock::new(HashMap::new()),
            running: AtomicU32::new(0),
        }
    }

    /// Attempts to reserve a concurrency slot for a new background job.
    ///
    /// Returns `true` if a slot was reserved (current running jobs < MAX_CONCURRENT_JOBS),
    /// `false` if the limit has been reached.
    pub fn try_reserve(&self) -> bool {
        #[allow(clippy::arithmetic_side_effects)] // n+1 only when n < MAX, bounded
        self.running
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                if n < MAX_CONCURRENT_JOBS {
                    Some(n + 1)
                } else {
                    None
                }
            })
            .is_ok()
    }

    /// Releases a concurrency slot after a background job completes.
    pub fn release(&self) {
        self.running.fetch_sub(1, Ordering::SeqCst);
    }

    /// Inserts a new background job into the registry.
    ///
    /// The job is stored with its initial "running" status.
    pub fn insert(&self, job: BackgroundJob) {
        self.jobs
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(job.job_id.clone(), job);
    }

    /// Retrieves a background job by its ID.
    ///
    /// Returns `None` if no job with the given ID exists.
    pub fn get(&self, job_id: &str) -> Option<BackgroundJob> {
        self.jobs
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(job_id)
            .cloned()
    }

    /// Updates the status and result of a background job.
    ///
    /// Sets the job's status, finished timestamp, and optional error message.
    /// No-op if the job ID is not found.
    pub fn update(&self, job_id: &str, status: &str, result: Option<String>, finished_at: u64) {
        let mut jobs = self.jobs.write().unwrap_or_else(|e| e.into_inner());
        if let Some(job) = jobs.get_mut(job_id) {
            job.status = status.to_string();
            job.finished_at = Some(finished_at);
            job.result = result;
        }
    }

    /// Lists recent background jobs, newest first.
    ///
    /// Returns up to `limit` jobs sorted by start time (descending).
    pub fn list(&self, limit: usize) -> Vec<BackgroundJob> {
        let jobs = self.jobs.read().unwrap_or_else(|e| e.into_inner());
        let mut v: Vec<_> = jobs.values().cloned().collect();
        v.sort_by_key(|j| j.started_at);
        v.reverse();
        v.truncate(limit);
        v
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_background_job_registry_new_is_empty() {
        let reg = BackgroundJobRegistry::new();
        assert!(reg.list(10).is_empty());
    }

    #[test]
    fn test_background_job_registry_try_reserve() {
        let reg = BackgroundJobRegistry::new();
        for _ in 0..MAX_CONCURRENT_JOBS {
            assert!(reg.try_reserve());
        }
        assert!(!reg.try_reserve());
    }

    #[test]
    fn test_background_job_registry_release_allows_re_reserve() {
        let reg = BackgroundJobRegistry::new();
        for _ in 0..MAX_CONCURRENT_JOBS {
            assert!(reg.try_reserve());
        }
        reg.release();
        assert!(reg.try_reserve());
    }

    #[test]
    fn test_background_job_registry_insert_and_get() {
        let reg = BackgroundJobRegistry::new();
        let job = BackgroundJob {
            job_id: "test-123".into(),
            capability: "FileRead".into(),
            status: "running".into(),
            started_at: 1000,
            finished_at: None,
            result: None,
        };
        reg.insert(job);
        let retrieved = reg.get("test-123").unwrap();
        assert_eq!(retrieved.job_id, "test-123");
        assert_eq!(retrieved.capability, "FileRead");
    }

    #[test]
    fn test_background_job_registry_update() {
        let reg = BackgroundJobRegistry::new();
        let job = BackgroundJob {
            job_id: "test-123".into(),
            capability: "FileRead".into(),
            status: "running".into(),
            started_at: 1000,
            finished_at: None,
            result: None,
        };
        reg.insert(job);
        reg.update("test-123", "completed", Some("success".into()), 2000);
        let retrieved = reg.get("test-123").unwrap();
        assert_eq!(retrieved.status, "completed");
        assert_eq!(retrieved.finished_at, Some(2000));
        assert_eq!(retrieved.result, Some("success".into()));
    }

    #[test]
    fn test_background_job_registry_list() {
        let reg = BackgroundJobRegistry::new();
        for i in 0..5 {
            let job = BackgroundJob {
                job_id: format!("job-{}", i),
                capability: "FileRead".into(),
                status: "completed".into(),
                started_at: 1000 + i,
                finished_at: Some(2000 + i),
                result: None,
            };
            reg.insert(job);
        }
        let jobs = reg.list(3);
        assert_eq!(jobs.len(), 3);
        // Newest first
        assert_eq!(jobs.first().unwrap().job_id, "job-4");
        assert_eq!(jobs.get(1).unwrap().job_id, "job-3");
        assert_eq!(jobs.get(2).unwrap().job_id, "job-2");
    }
}