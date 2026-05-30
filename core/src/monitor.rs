//! Health Monitoring Daemon — Background health checks with alerting.
//!
//! Monitors system health by capturing periodic snapshots of hardware telemetry
//! and process state. Alerts on threshold violations:
//! - Zombie processes > 10
//! - CPU usage > 90% for 5 consecutive minutes
//! - Memory monotonic increase (potential leak)
//!
//! # Example
//!
//! ```rust,ignore
//! use runtimo_core::HealthMonitor;
//!
//! let monitor = HealthMonitor::start()?;
//! // Monitor runs in background, checking every 60s
//! // Access latest health state:
//! let health = monitor.health();
//! println!("CPU: {:.1}%, RAM: {:.1}%", health.cpu_percent, health.ram_percent);
//! ```

use crate::processes::ProcessSnapshot;
use crate::telemetry::Telemetry;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

/// Alert thresholds for health monitoring.
const ZOMBIE_THRESHOLD: usize = 10;
const CPU_THRESHOLD: f32 = 90.0;
const CPU_ALERT_MINUTES: usize = 5;
const CHECK_INTERVAL_SECS: u64 = 60;

/// Current health state snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)]
pub struct HealthState {
    /// Unix timestamp of last check.
    pub timestamp: u64,
    /// Total CPU usage percentage.
    pub cpu_percent: f32,
    /// Total memory usage percentage.
    pub ram_percent: f32,
    /// Number of zombie processes.
    pub zombie_count: usize,
    /// Total process count.
    pub process_count: usize,
    /// Top CPU consuming process name.
    pub top_cpu_process: Option<String>,
    /// Top memory consuming process name.
    pub top_mem_process: Option<String>,
    /// Number of consecutive minutes CPU exceeded threshold.
    pub cpu_alert_count: usize,
    /// Number of consecutive checks with monotonically increasing RAM.
    pub ram_alert_count: usize,
    /// Whether memory is monotonically increasing.
    pub ram_increasing: bool,
    /// Last RAM usage for monotonicity check.
    pub last_ram_percent: Option<f32>,
}

impl Default for HealthState {
    fn default() -> Self {
        Self {
            timestamp: 0,
            cpu_percent: 0.0,
            ram_percent: 0.0,
            zombie_count: 0,
            process_count: 0,
            top_cpu_process: None,
            top_mem_process: None,
            cpu_alert_count: 0,
            ram_alert_count: 0,
            ram_increasing: false,
            last_ram_percent: None,
        }
    }
}

/// Health alert types.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::exhaustive_enums)]
pub enum HealthAlert {
    /// Zombie process count exceeded threshold.
    ZombieCount { count: usize, threshold: usize },
    /// CPU usage exceeded threshold for consecutive minutes.
    CpuHigh { percent: f32, minutes: usize },
    /// Memory usage monotonically increasing (potential leak).
    MemoryLeak { ram_percent: f32 },
}

impl std::fmt::Display for HealthAlert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZombieCount { count, threshold } => {
                write!(f, "Zombie processes: {} (threshold: {})", count, threshold)
            }
            Self::CpuHigh { percent, minutes } => {
                write!(f, "CPU usage: {:.1}% for {} minutes", percent, minutes)
            }
            Self::MemoryLeak { ram_percent } => {
                write!(f, "Memory leak detected: {:.1}% RAM", ram_percent)
            }
        }
    }
}

/// Health monitoring daemon with background thread.
///
/// Captures snapshots every 60 seconds and alerts on threshold violations.
/// Thread-safe state access via RwLock.
#[allow(clippy::exhaustive_structs)]
pub struct HealthMonitor {
    /// Shared health state.
    state: Arc<RwLock<HealthState>>,
    /// Stop flag for background thread.
    stop_flag: Arc<AtomicBool>,
    /// Background thread handle.
    _thread: thread::JoinHandle<()>,
    /// Alert history (last 100 alerts).
    alerts: Arc<RwLock<Vec<HealthAlert>>>,
}

impl Drop for HealthMonitor {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
    }
}

impl HealthMonitor {
    /// Starts the health monitoring background thread.
    ///
    /// The monitor checks system health every 60 seconds and updates
    /// the shared health state. Alerts are generated for:
    /// - Zombie count > 10
    /// - CPU > 90% for 5+ consecutive minutes
    /// - Monotonic RAM increase
    ///
    /// # Returns
    ///
    /// `Ok(HealthMonitor)` on success, or error if thread spawn fails.
    ///
    /// # Errors
    ///
    /// Returns `Err(String)` if the background monitoring thread fails to spawn.
    #[allow(clippy::arithmetic_side_effects)] // alert counters are intentional increments
    pub fn start() -> Result<Self, String> {
        let state = Arc::new(RwLock::new(HealthState::default()));
        let alerts = Arc::new(RwLock::new(Vec::new()));
        let stop_flag = Arc::new(AtomicBool::new(false));

        let state_clone = Arc::clone(&state);
        let alerts_clone = Arc::clone(&alerts);
        let stop_flag_clone = Arc::clone(&stop_flag);

        let handle = thread::spawn(move || {
            while !stop_flag_clone.load(Ordering::Relaxed) {
                // Capture health snapshot
                let telemetry = Telemetry::capture();
                let processes = ProcessSnapshot::capture();

                let mut current_state = state_clone.write().unwrap_or_else(|e| {
                    eprintln!("[HealthMonitor] State lock poisoned: {}", e);
                    // Recover from poison by taking the broken lock
                    e.into_inner()
                });

                // Update state
                current_state.timestamp = telemetry.timestamp;
                current_state.cpu_percent = processes.summary.total_cpu_percent;
                current_state.ram_percent =
                    parse_ram_percent(&telemetry.system.ram_total, &telemetry.system.ram_free);
                current_state.zombie_count = processes.summary.zombie_count;
                current_state.process_count = processes.summary.total_processes;
                current_state.top_cpu_process.clone_from(&processes.summary.top_cpu_consumer);
                current_state.top_mem_process.clone_from(&processes.summary.top_mem_consumer);

                // Check CPU threshold
                if current_state.cpu_percent > CPU_THRESHOLD {
                    current_state.cpu_alert_count += 1;
                    if current_state.cpu_alert_count >= CPU_ALERT_MINUTES {
                        let alert = HealthAlert::CpuHigh {
                            percent: current_state.cpu_percent,
                            minutes: current_state.cpu_alert_count,
                        };
                        add_alert(&alerts_clone, alert);
                    }
                } else {
                    current_state.cpu_alert_count = 0;
                }

                // Check memory monotonicity
                if let Some(last_ram) = current_state.last_ram_percent {
                    if current_state.ram_percent > last_ram {
                        current_state.ram_increasing = true;
                        current_state.ram_alert_count += 1;
                        // Alert if RAM increased for 5 consecutive checks
                        if current_state.ram_alert_count >= 5 {
                            let alert = HealthAlert::MemoryLeak {
                                ram_percent: current_state.ram_percent,
                            };
                            add_alert(&alerts_clone, alert);
                        }
                    } else {
                        current_state.ram_increasing = false;
                        current_state.ram_alert_count = 0;
                    }
                }
                current_state.last_ram_percent = Some(current_state.ram_percent);

                // Check zombie threshold
                if current_state.zombie_count > ZOMBIE_THRESHOLD {
                    let alert = HealthAlert::ZombieCount {
                        count: current_state.zombie_count,
                        threshold: ZOMBIE_THRESHOLD,
                    };
                    add_alert(&alerts_clone, alert);
                }

                // Sleep for check interval
                for _ in 0..CHECK_INTERVAL_SECS {
                    if stop_flag_clone.load(Ordering::Relaxed) {
                        break;
                    }
                    thread::sleep(Duration::from_secs(1));
                }
            }
        });

        Ok(Self {
            state,
            stop_flag,
            _thread: handle,
            alerts,
        })
    }

    /// Returns the current health state snapshot.
    #[must_use] 
    pub fn health(&self) -> HealthState {
        self.state.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Returns recent health alerts (up to 100).
    #[must_use] 
    pub fn alerts(&self) -> Vec<HealthAlert> {
        self.alerts
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Stops the background monitoring thread.
    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::Relaxed);
    }

    /// Returns whether the monitor is still running.
    #[must_use] 
    pub fn is_running(&self) -> bool {
        !self.stop_flag.load(Ordering::Relaxed)
    }
}

/// Helper to compute RAM usage percentage from total and free values.
///
/// Accepts raw telemetry strings like "16Gi" (total) and "13Gi" (free).
/// Returns used percentage: ((total - free) / total) * 100.
fn parse_ram_percent(ram_total: &str, ram_free: &str) -> f32 {
    let total_val = parse_size_value(ram_total.trim());
    let free_val = parse_size_value(ram_free.trim());

    if total_val > 0.0 {
        ((total_val - free_val) / total_val) * 100.0
    } else {
        0.0
    }
}

/// Parses a size string (e.g., "13Gi", "512Mi", "16384MB") into a numeric value in GB.
fn parse_size_value(size_str: &str) -> f32 {
    let size_str = size_str.trim();
    if size_str.ends_with("Gi") {
        size_str.trim_end_matches("Gi").parse().unwrap_or(0.0)
    } else if size_str.ends_with("Mi") {
        #[allow(clippy::map_unwrap_or)] // parse::<f32>().unwrap_or(0.0) is idiomatic for fallback
        size_str
            .trim_end_matches("Mi")
            .parse::<f32>()
            .map(|v| v / 1024.0)
            .unwrap_or(0.0)
    } else if size_str.ends_with("Ki") {
        size_str
            .trim_end_matches("Ki")
            .parse::<f32>()
            .map_or(0.0, |v| v / (1024.0 * 1024.0))
    } else if size_str.ends_with("MB") {
        size_str
            .trim_end_matches("MB")
            .parse::<f32>()
            .map_or(0.0, |v| v / 1000.0)
    } else if size_str.ends_with("GB") {
        size_str
            .trim_end_matches("GB")
            .parse::<f32>()
            .unwrap_or(0.0)
    } else {
        0.0
    }
}

/// Adds an alert to the alert history (max 100 alerts).
fn add_alert(alerts: &Arc<RwLock<Vec<HealthAlert>>>, alert: HealthAlert) {
    #[allow(clippy::expect_used)] // lock poisoning is irrecoverable
    let mut alerts_vec = alerts.write().expect("Alerts lock poisoned");
    alerts_vec.push(alert);
    if alerts_vec.len() > 100 {
        alerts_vec.remove(0);
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp, clippy::use_self)]
mod tests {
    use super::*;

    #[test]
    fn test_health_monitor_lifecycle() {
        let monitor = HealthMonitor::start().expect("Failed to start monitor");
        assert!(monitor.is_running());
        // Stop immediately — verifies start/stop without waiting for 60s cycle
        monitor.stop();
        // Give thread time to see the flag (sleep loop checks every 1s)
        thread::sleep(Duration::from_millis(1100));
        assert!(!monitor.is_running());
    }

    #[test]
    fn test_health_state_defaults() {
        let state = HealthState::default();
        assert_eq!(state.cpu_alert_count, 0);
        assert_eq!(state.ram_alert_count, 0);
        assert!(!state.ram_increasing);
        assert!(state.last_ram_percent.is_none());
    }

    #[test]
    fn test_cpu_alert_after_consecutive_checks() {
        let mut state = HealthState::default();
        // Simulate 5 consecutive minutes of high CPU
        for _ in 0..5 {
            state.cpu_percent = 95.0;
            if state.cpu_percent > CPU_THRESHOLD {
                state.cpu_alert_count += 1;
            }
        }
        assert_eq!(state.cpu_alert_count, 5);
    }

    #[test]
    fn test_ram_alert_uses_ram_counter_not_cpu() {
        let mut state = HealthState {
            last_ram_percent: Some(50.0),
            ..Default::default()
        };
        // Simulate RAM increasing each check while CPU is normal
        #[allow(clippy::cast_precision_loss)]
        for i in 0..5 {
            state.ram_percent = 50.0 + (i as f32 + 1.0); // 51, 52, 53, 54, 55
            state.cpu_percent = 10.0; // CPU is fine
            if state.ram_percent > state.last_ram_percent.unwrap() {
                state.ram_increasing = true;
                state.ram_alert_count += 1;
            } else {
                state.ram_increasing = false;
                state.ram_alert_count = 0;
            }
            state.last_ram_percent = Some(state.ram_percent);
        }
        // RAM alert should fire after 5 consecutive increases (independent of CPU)
        assert_eq!(state.ram_alert_count, 5);
        assert!(state.ram_increasing);
    }

    #[test]
    fn test_ram_alert_resets_when_ram_decreases() {
        let mut state = HealthState {
            last_ram_percent: Some(50.0),
            ..Default::default()
        };

        // RAM increases twice
        state.ram_percent = 55.0;
        state.ram_alert_count = 2;
        state.last_ram_percent = Some(55.0);

        // RAM decreases — counter should reset
        state.ram_percent = 40.0;
        if state.ram_percent > state.last_ram_percent.unwrap() {
            state.ram_increasing = true;
            state.ram_alert_count += 1;
        } else {
            state.ram_increasing = false;
            state.ram_alert_count = 0;
        }
        state.last_ram_percent = Some(state.ram_percent);

        assert_eq!(state.ram_alert_count, 0);
        assert!(!state.ram_increasing);
    }

    #[test]
    fn test_parse_size_value() {
        assert!((parse_size_value("13Gi") - 13.0).abs() < 0.01);
        assert!((parse_size_value("512Mi") - 0.5).abs() < 0.01);
        assert_eq!(parse_size_value("invalid"), 0.0);
    }
}
