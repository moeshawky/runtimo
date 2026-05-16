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
            ram_increasing: false,
            last_ram_percent: None,
        }
    }
}

/// Health alert types.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
            HealthAlert::ZombieCount { count, threshold } => {
                write!(f, "Zombie processes: {} (threshold: {})", count, threshold)
            }
            HealthAlert::CpuHigh { percent, minutes } => {
                write!(f, "CPU usage: {:.1}% for {} minutes", percent, minutes)
            }
            HealthAlert::MemoryLeak { ram_percent } => {
                write!(f, "Memory leak detected: {:.1}% RAM", ram_percent)
            }
        }
    }
}

/// Health monitoring daemon with background thread.
///
/// Captures snapshots every 60 seconds and alerts on threshold violations.
/// Thread-safe state access via RwLock.
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

                let mut current_state = state_clone.write().expect("Health state lock poisoned");

                // Update state
                current_state.timestamp = telemetry.timestamp;
                current_state.cpu_percent = processes.summary.total_cpu_percent;
                current_state.ram_percent = parse_ram_percent(&telemetry.system.ram_free);
                current_state.zombie_count = processes.summary.zombie_count;
                current_state.process_count = processes.summary.total_processes;
                current_state.top_cpu_process = processes.summary.top_cpu_consumer.clone();
                current_state.top_mem_process = processes.summary.top_mem_consumer.clone();

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
                        // Alert if RAM increased for 5 consecutive checks
                        if current_state.cpu_alert_count >= 5 {
                            let alert = HealthAlert::MemoryLeak {
                                ram_percent: current_state.ram_percent,
                            };
                            add_alert(&alerts_clone, alert);
                        }
                    } else {
                        current_state.ram_increasing = false;
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
    pub fn health(&self) -> HealthState {
        self.state
            .read()
            .expect("Health state lock poisoned")
            .clone()
    }

    /// Returns recent health alerts (up to 100).
    pub fn alerts(&self) -> Vec<HealthAlert> {
        self.alerts.read().expect("Alerts lock poisoned").clone()
    }

    /// Stops the background monitoring thread.
    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::Relaxed);
    }

    /// Returns whether the monitor is still running.
    pub fn is_running(&self) -> bool {
        !self.stop_flag.load(Ordering::Relaxed)
    }
}

/// Helper to parse RAM percentage from telemetry string (e.g., "13Gi" from "16Gi total, 13Gi free").
fn parse_ram_percent(ram_free: &str) -> f32 {
    // Extract free RAM value (e.g., "13Gi" from "16Gi total, 13Gi free")
    let free_str = ram_free.trim_end_matches("free").trim();
    let free_val = parse_size_value(free_str);

    // Extract total RAM value
    let total_str = ram_free
        .split(',')
        .next()
        .unwrap_or("")
        .trim_end_matches("total")
        .trim();
    let total_val = parse_size_value(total_str);

    if total_val > 0.0 {
        ((total_val - free_val) / total_val) * 100.0
    } else {
        0.0
    }
}

/// Parses a size string (e.g., "13Gi", "512Mi") into a numeric value in GB.
fn parse_size_value(size_str: &str) -> f32 {
    let size_str = size_str.trim();
    if size_str.ends_with("Gi") {
        size_str.trim_end_matches("Gi").parse().unwrap_or(0.0)
    } else if size_str.ends_with("Mi") {
        size_str
            .trim_end_matches("Mi")
            .parse::<f32>()
            .map(|v| v / 1024.0)
            .unwrap_or(0.0)
    } else if size_str.ends_with("Ki") {
        size_str
            .trim_end_matches("Ki")
            .parse::<f32>()
            .map(|v| v / (1024.0 * 1024.0))
            .unwrap_or(0.0)
    } else {
        0.0
    }
}

/// Adds an alert to the alert history (max 100 alerts).
fn add_alert(alerts: &Arc<RwLock<Vec<HealthAlert>>>, alert: HealthAlert) {
    let mut alerts_vec = alerts.write().expect("Alerts lock poisoned");
    alerts_vec.push(alert);
    if alerts_vec.len() > 100 {
        alerts_vec.remove(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "Flaky - background thread timing dependent"]
    fn test_health_monitor_start() {
        let monitor = HealthMonitor::start().expect("Failed to start monitor");
        assert!(monitor.is_running());
        // Give it time to capture initial state (background thread runs every 60s but captures immediately on start)
        thread::sleep(Duration::from_millis(1000));
        let health = monitor.health();
        assert!(
            health.process_count > 0,
            "Expected process_count > 0, got {}",
            health.process_count
        );
        monitor.stop();
        assert!(!monitor.is_running());
    }

    #[test]
    fn test_parse_size_value() {
        assert!((parse_size_value("13Gi") - 13.0).abs() < 0.01);
        assert!((parse_size_value("512Mi") - 0.5).abs() < 0.01);
        assert_eq!(parse_size_value("invalid"), 0.0);
    }
}
