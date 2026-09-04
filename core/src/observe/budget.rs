//! Observe resource budget — reuses `ResourceHistory` pattern.
//!
//! Wraps the `ResourceHistory` sliding-window + cooldown logic from
//! `llmosafe.rs:57-164` with its own persist file `observe_budget.state`
//! next to `resource_history.state`. Reuse, don't reinvent.
//!
//! # Nexus survey
//! * nexus `store::store.rs` is memory-only — budget here is WAL-adjacent but
//!   persisted to disk for restart-restore.
//! * `llmosafe.rs` persist path `resource_history.state` is reused as sibling.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Rolling resource usage tracker for observe throttling.
///
/// Mirrors `ResourceHistory` in `llmosafe.rs` (window 30 s, cooldown 1 s)
/// but with independent storage at `observe_budget.state`.
///
/// # Invariants
/// * Persist file is `observe_budget.state` next to `resource_history.state`.
/// * Restart restores `last_check` if within cooldown window.
#[allow(clippy::exhaustive_structs)]
pub struct ObserveBudget {
    measurements: Vec<(Instant, u8)>,
    window_secs: u64,
    cooldown_secs: u64,
    last_check: Option<Instant>,
    persist_path: Option<PathBuf>,
}

impl ObserveBudget {
    /// Creates a budget tracker with window and cooldown.
    ///
    /// Persist path is derived from `RUNTIMO_STATE_DIR` or `HOME` + `.runtimo/observe_budget.state`.
    #[must_use]
    pub fn new(window_secs: u64, cooldown_secs: u64) -> Self {
        Self::new_with_path(window_secs, cooldown_secs, observe_budget_path())
    }

    /// Creates with explicit persist path (for tests).
    #[must_use]
    pub fn new_with_path(
        window_secs: u64,
        cooldown_secs: u64,
        persist_path: Option<PathBuf>,
    ) -> Self {
        let mut b = Self {
            measurements: Vec::with_capacity(60),
            window_secs,
            cooldown_secs,
            last_check: None,
            persist_path,
        };
        b.restore_last_check();
        b
    }

    /// Default budget (30 s window, 1 s cooldown).
    #[must_use]
    pub fn default_budget() -> Self {
        Self::new(30, 1)
    }

    /// Restores `last_check` from persisted epoch seconds if within cooldown.
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

    /// Persists current epoch seconds.
    fn persist_last_check(&self) {
        if let Some(ref path) = self.persist_path {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let _ = fs::write(path, secs.to_string());
        }
    }

    /// Records a pressure measurement and returns rolling average.
    pub fn record(&mut self, pressure: u8) -> f64 {
        let now = Instant::now();
        let cutoff = now
            .checked_sub(Duration::from_secs(self.window_secs))
            .unwrap_or_else(Instant::now);
        self.measurements.retain(|(t, _)| *t > cutoff);
        self.measurements.push((now, pressure));
        if self.measurements.is_empty() {
            return f64::from(pressure);
        }
        #[allow(clippy::cast_precision_loss)]
        {
            let count = self.measurements.len() as f64;
            self.measurements.iter().map(|(_, p)| f64::from(*p)).sum::<f64>() / count
        }
    }

    /// Rolling average over window, if any.
    #[must_use]
    pub fn rolling_average(&self) -> Option<f64> {
        if self.measurements.is_empty() {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        {
            let count = self.measurements.len() as f64;
            Some(
                self.measurements
                    .iter()
                    .map(|(_, p)| f64::from(*p))
                    .sum::<f64>()
                    / count,
            )
        }
    }

    /// Whether in cooldown since last check.
    #[must_use]
    pub fn is_in_cooldown(&self) -> bool {
        if let Some(last) = self.last_check {
            last.elapsed() < Duration::from_secs(self.cooldown_secs)
        } else {
            false
        }
    }

    /// Marks checked and persists.
    pub fn mark_checked(&mut self) {
        self.last_check = Some(Instant::now());
        self.persist_last_check();
    }

    /// Returns persist path, if any.
    #[must_use]
    pub fn persist_path(&self) -> Option<&PathBuf> {
        self.persist_path.as_ref()
    }

    /// Whether sampling should be suspended due to high pressure.
    ///
    /// Uses rolling average > 80 or instantaneous > 80, mirroring `LlmoSafeGuard::check`.
    #[must_use]
    pub fn should_suspend(&self, pressure: u8) -> bool {
        if pressure > 80 {
            return true;
        }
        if let Some(avg) = self.rolling_average() {
            if avg > 80.0 {
                return true;
            }
        }
        false
    }
}

/// Returns path for `observe_budget.state`.
///
/// Derived from `RUNTIMO_STATE_DIR` or `HOME` + `.runtimo/observe_budget.state`,
/// sibling to `resource_history.state` (`llmosafe.rs:182-189`). `None` if
/// neither env var is set — then state is in-memory only for this process.
#[must_use]
pub fn observe_budget_path() -> Option<PathBuf> {
    let base: Option<PathBuf> = std::env::var("RUNTIMO_STATE_DIR")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(PathBuf::from));
    base.map(|b| b.join(".runtimo").join("observe_budget.state"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn budget_restart_restore() {
        let _g = MUTEX.lock().unwrap();
        let dir = std::env::temp_dir().join("runtimo_test_budget_restore");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("observe_budget.state");
        let _ = std::fs::remove_file(&path);
        // First instance writes checkpoint.
        {
            let mut b = ObserveBudget::new_with_path(30, 60, Some(path.clone()));
            b.mark_checked();
            assert!(b.is_in_cooldown());
        }
        // Second instance should restore cooldown.
        {
            let b2 = ObserveBudget::new_with_path(30, 60, Some(path));
            assert!(
                b2.is_in_cooldown(),
                "restored budget should still be in cooldown"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn budget_rolling_average() {
        let mut b = ObserveBudget::new_with_path(30, 1, None);
        b.record(50);
        b.record(70);
        let avg = b.rolling_average().unwrap();
        assert!((avg - 60.0).abs() < 0.1);
    }

    #[test]
    fn budget_should_suspend_on_high_pressure() {
        let mut b = ObserveBudget::new_with_path(30, 1, None);
        assert!(b.should_suspend(90));
        assert!(!b.should_suspend(50));
        b.record(90);
        b.record(90);
        assert!(b.should_suspend(50)); // rolling avg >80
    }
}
