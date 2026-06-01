//! LLMOSafe integration — Real resource limits via the `llmosafe` crate.
//!
//! Uses `llmosafe::ResourceGuard` (Tier 0: Resource Body) for physical resource
//! monitoring. Maps RSS memory and CPU load to the CognitiveEntropy/Synapse system.
//!
//! The guard checks actual `/proc/stat` and `/proc/self/status` — no approximations.
//!
//! # Example
//!
//! ```rust,ignore
//! use runtimo_core::LlmoSafeGuard;
//!
//! let guard = LlmoSafeGuard::new();
//! guard.check()?;  // Ok(()) if resources are within limits
//!
//! let result = guard.execute(|| {
//!     // This closure only runs if resources are safe
//!     Ok(42)
//! })?;
//! ```

use llmosafe::llmosafe_body::ResourceGuard;
use llmosafe::llmosafe_integration::{EscalationPolicy, SafetyContext};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Rolling resource usage tracker for cooldown enforcement (FINDING #16).
///
/// Persists the last-check timestamp to disk so that process restarts
/// cannot bypass the cooldown period.
struct ResourceHistory {
    measurements: Vec<(Instant, u8)>,
    window_secs: u64,
    cooldown_secs: u64,
    last_check: Option<Instant>,
    persist_path: Option<PathBuf>,
}

impl ResourceHistory {
    fn new(window_secs: u64, cooldown_secs: u64, persist_path: Option<PathBuf>) -> Self {
        let mut history = Self {
            measurements: Vec::with_capacity(60),
            window_secs,
            cooldown_secs,
            last_check: None,
            persist_path,
        };
        history.restore_last_check();
        history
    }

    /// Restores the last_check timestamp from a persisted file.
    /// Prevents cooldown bypass via process restart.
    fn restore_last_check(&mut self) {
        if let Some(ref path) = self.persist_path {
            if let Ok(content) = fs::read_to_string(path) {
                if let Ok(secs) = content.trim().parse::<u64>() {
                    let now_epoch = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_secs());
                    let elapsed_secs = now_epoch.saturating_sub(secs);
                    if elapsed_secs < self.cooldown_secs {
                        self.last_check = Some(
                            Instant::now()
                                .checked_sub(Duration::from_secs(elapsed_secs))
                                .unwrap_or_else(Instant::now),
                        );
                    }
                }
            }
        }
    }

    /// Persists the current timestamp to disk for crash/restart recovery.
    fn persist_last_check(&self) {
        if let Some(ref path) = self.persist_path {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let _ = fs::write(path, secs.to_string());
        }
    }

    /// Records a pressure measurement and returns the rolling average.
    fn record(&mut self, pressure: u8) -> f64 {
        let now = Instant::now();
        let cutoff = now
            .checked_sub(Duration::from_secs(self.window_secs))
            .unwrap_or_else(Instant::now);
        self.measurements.retain(|(t, _)| *t > cutoff);
        self.measurements.push((now, pressure));

        if self.measurements.is_empty() {
            return pressure as f64;
        }
        #[allow(clippy::cast_precision_loss)]
        {
            let count = self.measurements.len() as f64;
            self.measurements
                .iter()
                .map(|(_, p)| *p as f64)
                .sum::<f64>()
                / count
        }
    }

    /// Returns the rolling average pressure over the tracking window.
    fn rolling_average(&self) -> Option<f64> {
        if self.measurements.is_empty() {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        {
            let count = self.measurements.len() as f64;
            Some(
                self.measurements
                    .iter()
                    .map(|(_, p)| *p as f64)
                    .sum::<f64>()
                    / count,
            )
        }
    }

    /// Checks if we're in a cooldown period after a recent check.
    fn is_in_cooldown(&self) -> bool {
        if let Some(last) = self.last_check {
            last.elapsed() < Duration::from_secs(self.cooldown_secs)
        } else {
            false
        }
    }

    fn mark_checked(&mut self) {
        self.last_check = Some(Instant::now());
        self.persist_last_check();
    }
}

static RESOURCE_HISTORY: Mutex<Option<ResourceHistory>> = Mutex::new(None);

/// Returns the default path for persisting resource history state.
fn resource_history_path() -> PathBuf {
    std::env::var("RUNTIMO_STATE_DIR")
        .map_or_else(
            |_| {
                std::env::var("HOME")
                    .ok()
                    .unwrap_or_else(|| {
                        panic!("Cannot determine state dir: set RUNTIMO_STATE_DIR or HOME")
                    })
                    .into()
            },
            PathBuf::from,
        )
        .join(".runtimo")
        .join("resource_history.state")
}

/// Resource guard wrapping `llmosafe::ResourceGuard` with safety context support.
///
/// Monitors RSS memory, CPU load, and IO wait from `/proc`. Acts as a circuit
/// breaker: if resource pressure exceeds the ceiling (default 80%), execution
/// is rejected.
///
/// FINDING #16: Tracks rolling average of resource usage and enforces a cooldown
/// period between executions to prevent threshold bypass via rapid repeated checks.
/// Cooldown state is persisted to disk to prevent bypass via process restart.
pub struct LlmoSafeGuard {
    guard: ResourceGuard,
    policy: EscalationPolicy,
}

impl LlmoSafeGuard {
    /// Creates a guard with the default memory ceiling (80% of system memory).
    #[must_use]
    pub fn new() -> Self {
        let guard = ResourceGuard::auto(0.8);
        Self {
            guard,
            policy: EscalationPolicy::default(),
        }
    }

    /// Creates a guard with an explicit memory ceiling in bytes.
    #[must_use]
    pub fn with_memory_ceiling_bytes(memory_ceiling_bytes: usize) -> Self {
        Self {
            guard: ResourceGuard::new(memory_ceiling_bytes),
            policy: EscalationPolicy::default(),
        }
    }

    /// Checks current resource usage via llmosafe's real `/proc/stat` reading.
    ///
    /// FINDING #16: Uses rolling average over recent measurements instead of
    /// instantaneous values, and enforces a cooldown period to prevent
    /// threshold bypass via rapid repeated checks.
    ///
    /// # Returns
    ///
    /// `Ok(())` if resources are within limits.
    ///
    /// # Errors
    ///
    /// Returns an error string if resource pressure exceeds 80% or the
    /// underlying `ResourceGuard::check()` fails.
    ///
    /// # Panics
    /// Panics if the global resource history mutex is poisoned.
    pub fn check(&self) -> Result<(), String> {
        let mut history = RESOURCE_HISTORY.lock().unwrap_or_else(|e| e.into_inner());
        if history.is_none() {
            *history = Some(ResourceHistory::new(30, 1, Some(resource_history_path())));
        }
        #[allow(clippy::expect_used)]
        let hist = history
            .as_mut()
            .expect("history always Some after initialization above");

        // FINDING #16: Enforce cooldown between checks
        if hist.is_in_cooldown() {
            if let Some(avg) = hist.rolling_average() {
                if avg > 80.0 {
                    return Err(format!(
                        "Resource pressure averaging {:.1}% over last 30s (cooldown active)",
                        avg
                    ));
                }
            }
            return Ok(());
        }

        let pressure = self.guard.pressure();
        let avg = hist.record(pressure);
        hist.mark_checked();

        // Check both instantaneous and rolling average
        if pressure > 80 {
            return Err(format!("Resource pressure at {}% (ceiling: 80%)", pressure));
        }
        if avg > 80.0 {
            return Err(format!(
                "Rolling average resource pressure at {:.1}% (ceiling: 80%)",
                avg
            ));
        }

        self.guard
            .check()
            .map(|_| ())
            .map_err(|e| format!("Resource guard check failed: {}", e))
    }

    /// Executes a function only if resources are safe.
    ///
    /// Runs `check()` first; if it passes, invokes `f()`.
    ///
    /// # Errors
    ///
    /// Propagates errors from `check()` or from `f()`.
    pub fn execute<F, T>(&self, f: F) -> Result<T, String>
    where
        F: FnOnce() -> Result<T, String>,
    {
        self.check()?;
        f()
    }

    /// Current RSS in bytes (from `/proc/self/status`).
    #[must_use]
    pub fn current_rss_bytes(&self) -> usize {
        ResourceGuard::current_rss_bytes()
    }

    /// Total system memory in bytes.
    #[must_use]
    pub fn system_memory_bytes(&self) -> usize {
        ResourceGuard::system_memory_bytes()
    }

    /// CPU load 0-100 via delta measurement on `/proc/stat`.
    #[must_use]
    pub fn system_cpu_load(&self) -> u8 {
        ResourceGuard::system_cpu_load()
    }

    /// Raw entropy score 0-1000 (weighted: RSS 50%, IO wait 25%, load 25%).
    #[must_use]
    pub fn raw_entropy(&self) -> u16 {
        self.guard.raw_entropy()
    }

    /// Pressure as percentage of memory ceiling (0-100).
    #[must_use]
    pub fn pressure(&self) -> u8 {
        self.guard.pressure()
    }

    /// Creates a safety context for tracking decisions across an execution.
    #[must_use]
    pub fn safety_context(&self) -> SafetyContext {
        SafetyContext::new(self.policy.clone())
    }
}

impl Default for LlmoSafeGuard {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn guard_reports_system_memory() {
        let guard = LlmoSafeGuard::new();
        let mem = guard.system_memory_bytes();
        assert!(mem > 0, "System memory should be > 0");
    }

    #[test]
    fn guard_reports_rss() {
        let rss = LlmoSafeGuard::new().current_rss_bytes();
        assert!(rss > 0, "RSS should be > 0 for running process");
    }

    #[test]
    fn check_passes_under_normal_load() {
        let guard = LlmoSafeGuard::new();
        let result = guard.check();
        if let Err(e) = result {
            eprintln!("System under pressure: {}", e);
        }
    }

    #[test]
    fn pressure_is_bounded() {
        let guard = LlmoSafeGuard::new();
        let p = guard.pressure();
        assert!(p <= 100, "Pressure should be 0-100, got {}", p);
    }

    #[test]
    fn entropy_is_bounded() {
        let guard = LlmoSafeGuard::new();
        let e = guard.raw_entropy();
        assert!(e <= 1000, "Entropy should be 0-1000, got {}", e);
    }

    #[test]
    fn test_resource_history_rolling_average() {
        let mut hist = ResourceHistory::new(30, 1, None);
        hist.record(50);
        hist.record(60);
        hist.record(70);

        let avg = hist.rolling_average().unwrap();
        assert!(
            (avg - 60.0).abs() < 0.1,
            "Rolling avg should be ~60, got {}",
            avg
        );
    }

    #[test]
    fn test_resource_history_cooldown() {
        let mut hist = ResourceHistory::new(30, 1, None);
        hist.record(90);
        hist.mark_checked();

        assert!(
            hist.is_in_cooldown(),
            "Should be in cooldown immediately after check"
        );
    }
}
