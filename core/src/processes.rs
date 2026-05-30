//! Process Execution Awareness — What's running and consuming resources.
//!
//! Tracks processes, resource consumption, and execution context.
//! Captures a snapshot via `ps` with explicit format, computes summaries
//! (total CPU%, memory%, zombie count), and identifies top consumers.
//!
//! # Example
//!
//! ```rust,ignore
//! use runtimo_core::ProcessSnapshot;
//!
//! let snap = ProcessSnapshot::capture();
//! println!("Processes: {}", snap.summary.total_processes);
//! println!("Zombies: {}", snap.summary.zombie_count);
//!
//! for proc in snap.top_by_cpu(5) {
//!     println!("{}: {:.1}% CPU", proc.command, proc.cpu_percent);
//! }
//! ```

use crate::cmd::run_cmd;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

static PROCESS_CACHE: Mutex<Option<(ProcessSnapshot, std::time::Instant)>> = Mutex::new(None);
const CACHE_TTL_SECS: u64 = 30;

/// Process list snapshot at a point in time.
///
/// Contains the raw process list, a computed summary, and a timestamp.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessSnapshot {
    /// Unix timestamp (seconds) when the snapshot was taken.
    pub timestamp: u64,
    /// Individual process records parsed from `ps -eo`.
    pub processes: Vec<ProcessInfo>,
    /// Aggregated summary statistics.
    pub summary: ProcessSummary,
}

/// Information about a single running process.
///
/// Parsed from one line of `ps -eo` output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessInfo {
    /// Process ID.
    pub pid: u32,
    /// Parent Process ID (PPID) for lineage tracking.
    pub ppid: u32,
    /// Owning user name.
    pub user: String,
    /// CPU usage percentage.
    pub cpu_percent: f32,
    /// Memory usage percentage.
    pub mem_percent: f32,
    /// Virtual memory size in kilobytes (KB).
    pub vsz: u64,
    /// Resident set size in kilobytes (KB).
    pub rss: u64,
    /// Process state string (e.g. `"S"`, `"R"`, `"Z"`).
    pub stat: String,
    /// Start time of the process.
    pub start_time: String,
    /// Elapsed running time.
    pub elapsed: String,
    /// Full command line.
    pub command: String,
}

/// Aggregated summary of a process snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessSummary {
    /// Total number of processes.
    pub total_processes: usize,
    /// Sum of all process CPU percentages.
    pub total_cpu_percent: f32,
    /// Sum of all process memory percentages.
    pub total_mem_percent: f32,
    /// Command name of the top CPU consumer.
    pub top_cpu_consumer: Option<String>,
    /// Command name of the top memory consumer.
    pub top_mem_consumer: Option<String>,
    /// Number of zombie (`Z` state) processes.
    pub zombie_count: usize,
}

impl ProcessSnapshot {
    /// Captures a full process snapshot via `ps` with explicit format.
    ///
    /// Results are cached for 30 seconds to avoid re-parsing on
    /// repeated calls within the same execution window.
    pub fn capture() -> Self {
        let now = std::time::Instant::now();
        {
            // Handle poison error by recovering from the poisoned state
            let cache = PROCESS_CACHE.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((cached, instant)) = cache.as_ref() {
                if now.duration_since(*instant).as_secs() < CACHE_TTL_SECS {
                    return cached.clone();
                }
            }
        }

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let mut processes = Vec::new();
        // Use ps with explicit format to get PPID: PID,PPID,USER,CPU,MEM,VSZ,RSS,STAT,START,TIME,COMMAND
        // This gives us parent process ID for lineage tracking
        let ps_output =
            run_cmd("ps -eo pid,ppid,user,%cpu,%mem,vsz,rss,stat,start,time,comm --no-headers");

        for line in ps_output.lines() {
            if let Some(proc) = parse_ps_line(line) {
                processes.push(proc);
            }
        }

        let summary = ProcessSummary::compute(&processes);

        let snapshot = Self {
            timestamp,
            processes,
            summary,
        };

        // Handle poison error by recovering from the poisoned state
        let mut cache = PROCESS_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        *cache = Some((snapshot.clone(), now));
        snapshot
    }

    /// Returns all zombie processes with their PID, command, and PPID.
    ///
    /// Zombies are defunct child processes whose parent has not yet called
    /// `waitpid(2)`. They consume no resources but each occupies a PID slot.
    pub fn zombies(&self) -> Vec<&ProcessInfo> {
        self.processes
            .iter()
            .filter(|p| p.stat.starts_with('Z'))
            .collect()
    }

    /// Clears the process snapshot cache.
    ///
    /// Use before capturing an after-kill snapshot to ensure fresh data.
    pub fn clear_cache() {
        let mut cache = PROCESS_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        *cache = None;
    }

    /// Returns the top `n` processes by CPU usage.
    pub fn top_by_cpu(&self, n: usize) -> Vec<&ProcessInfo> {
        let mut procs: Vec<_> = self.processes.iter().collect();
        procs.sort_by(|a, b| {
            b.cpu_percent
                .partial_cmp(&a.cpu_percent)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        procs.into_iter().take(n).collect()
    }

    /// Returns the top `n` processes by memory usage.
    pub fn top_by_mem(&self, n: usize) -> Vec<&ProcessInfo> {
        let mut procs: Vec<_> = self.processes.iter().collect();
        procs.sort_by(|a, b| {
            b.mem_percent
                .partial_cmp(&a.mem_percent)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        procs.into_iter().take(n).collect()
    }

    /// Prints a human-readable process report to stdout.
    pub fn print_report(&self) {
        println!("\n{}", "=".repeat(80));
        println!(" PROCESS SNAPSHOT [{}]", self.timestamp);
        println!("{}", "=".repeat(80));

        println!("\n--- SUMMARY ---");
        println!(" Total Processes: {}", self.summary.total_processes);
        println!(" Total CPU: {:.1}%", self.summary.total_cpu_percent);
        println!(" Total Memory: {:.1}%", self.summary.total_mem_percent);
        println!(" Zombies: {}", self.summary.zombie_count);

        if let Some(ref top_cpu) = self.summary.top_cpu_consumer {
            println!(
                " Top CPU: {} ({:.1}%)",
                top_cpu,
                self.processes
                    .iter()
                    .find(|p| p.command == *top_cpu)
                    .map(|p| p.cpu_percent)
                    .unwrap_or(0.0)
            );
        }

        if let Some(ref top_mem) = self.summary.top_mem_consumer {
            println!(
                " Top Memory: {} ({:.1}%)",
                top_mem,
                self.processes
                    .iter()
                    .find(|p| p.command == *top_mem)
                    .map(|p| p.mem_percent)
                    .unwrap_or(0.0)
            );
        }

        println!("\n--- TOP 10 BY CPU ---");
        for (i, proc) in self.top_by_cpu(10).iter().enumerate() {
            println!(
                "{:2}. {:6} {:6} {:5.1} {:5.1} {:8} {:8} {:?} {}",
                i + 1,
                proc.pid,
                proc.user,
                proc.cpu_percent,
                proc.mem_percent,
                format_size(proc.vsz),
                format_size(proc.rss),
                proc.stat,
                truncate(&proc.command, 50)
            );
        }

        println!("\n--- TOP 10 BY MEMORY ---");
        for (i, proc) in self.top_by_mem(10).iter().enumerate() {
            println!(
                "{:2}. {:6} {:6} {:5.1} {:5.1} {:8} {:8} {:?} {}",
                i + 1,
                proc.pid,
                proc.user,
                proc.cpu_percent,
                proc.mem_percent,
                format_size(proc.vsz),
                format_size(proc.rss),
                proc.stat,
                truncate(&proc.command, 50)
            );
        }

        println!("\n{}", "=".repeat(80));
    }
}

/// Parses a single line of process output into a [`ProcessInfo`].
///
/// Expected format: PID PPID USER %CPU %MEM VSZ RSS STAT START TIME COMMAND
/// Returns `None` if the line has fewer than 10 whitespace-separated fields.
fn parse_ps_line(line: &str) -> Option<ProcessInfo> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 10 {
        return None;
    }

    let pid = parts[0].parse().ok()?;
    let ppid = parts[1].parse().ok()?;
    let user = parts[2].to_string();
    let cpu_percent = parts[3].parse().unwrap_or(0.0);
    let mem_percent = parts[4].parse().unwrap_or(0.0);
    let vsz: u64 = parts[5].parse().unwrap_or(0);
    let rss: u64 = parts[6].parse().unwrap_or(0);
    let stat = parts[7].to_string();
    let start_time = parts[8].to_string();
    let elapsed = parts[9].to_string();
    let command = parts.get(10..).map(|s| s.join(" ")).unwrap_or_default();

    Some(ProcessInfo {
        pid,
        ppid,
        user,
        cpu_percent,
        mem_percent,
        vsz,
        rss,
        stat,
        start_time,
        elapsed,
        command,
    })
}

impl ProcessSummary {
    fn compute(processes: &[ProcessInfo]) -> Self {
        let total_processes = processes.len();
        let total_cpu_percent: f32 = processes.iter().map(|p| p.cpu_percent).sum();
        let total_mem_percent: f32 = processes.iter().map(|p| p.mem_percent).sum();

        let top_cpu_consumer = processes
            .iter()
            .max_by(|a, b| {
                a.cpu_percent
                    .partial_cmp(&b.cpu_percent)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|p| p.command.clone());

        let top_mem_consumer = processes
            .iter()
            .max_by(|a, b| {
                a.mem_percent
                    .partial_cmp(&b.mem_percent)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|p| p.command.clone());

        let zombie_count = processes.iter().filter(|p| p.stat.starts_with('Z')).count();

        Self {
            total_processes,
            total_cpu_percent,
            total_mem_percent,
            top_cpu_consumer,
            top_mem_consumer,
            zombie_count,
        }
    }
}

/// Formats a size in kilobytes as a human-readable string (K/M/G).
#[allow(clippy::cast_precision_loss)]
fn format_size(kb: u64) -> String {
    if kb >= 1024 * 1024 {
        format!("{:.1}G", kb as f64 / (1024.0 * 1024.0))
    } else if kb >= 1024 {
        format!("{:.1}M", kb as f64 / 1024.0)
    } else {
        format!("{}K", kb)
    }
}

/// Truncates a string to `max_len` characters, appending `"..."` if truncated.
///
/// Uses `char_indices()` for safe UTF-8 boundary slicing — never panics on
/// multi-byte characters.
fn truncate(s: &str, max_len: usize) -> String {
    if s.chars().count() > max_len {
        let end = max_len.saturating_sub(3);
        let byte_end = s.char_indices().nth(end).map(|(i, _)| i).unwrap_or(s.len());
        format!("{}...", &s[..byte_end])
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_process_snapshot() {
        let snapshot = ProcessSnapshot::capture();
        assert!(!snapshot.processes.is_empty());
        assert!(snapshot.summary.total_processes > 0);
    }

    #[test]
    fn test_truncate_ascii() {
        assert_eq!(truncate("hello world", 8), "hello...");
        assert_eq!(truncate("short", 10), "short");
    }

    #[test]
    fn test_truncate_multibyte_utf8() {
        // This must NOT panic — the old code panicked on multi-byte boundaries
        let cjk = "你好世界这是一个很长的命令行参数"; // 15 CJK chars
        let result = truncate(cjk, 8);
        assert!(result.ends_with("..."));
        // Should not panic and should be valid UTF-8
        assert!(result.is_char_boundary(result.len()));
    }

    #[test]
    fn test_format_size() {
        // format_size expects KB input
        assert_eq!(format_size(0), "0K");
        assert_eq!(format_size(512), "512K");
        assert_eq!(format_size(1024), "1.0M");
        assert_eq!(format_size(1024 * 1024), "1.0G");
        assert_eq!(format_size(1024 * 512), "512.0M");
        assert_eq!(format_size(1024 * 1024 * 2), "2.0G");
    }

    #[test]
    fn test_process_vsz_rss_in_kb() {
        let snap = ProcessSnapshot::capture();
        // Every process should have vsz/rss as reasonable KB values
        // (not multiplied by 1024 — that was the old bug)
        for p in &snap.processes {
            // vsz can be very large on 64-bit systems (virtual memory is cheap)
            // but should not exceed 1PB (1024*1024*1024 KB)
            assert!(
                p.vsz < 1_000_000_000,
                "vsz={}KB is unreasonably large for {}",
                p.vsz,
                p.command
            );
            // rss is physical memory — should be under 100GB for any single process
            assert!(
                p.rss < 100_000_000,
                "rss={}KB is unreasonably large for {}",
                p.rss,
                p.command
            );
        }
    }
}
