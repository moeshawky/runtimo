//! System Telemetry — Via Negativa: raw observation, no interpretation.
//!
//! Captures a snapshot of the host machine by reading `/proc` and `/sys`
//! directly. Every field is backed by a raw kernel filesystem read — no
//! shell-out for data available in `/proc`, no pgrep, no service name
//! guessing, no version detection.
//!
//! # Via Negativa Philosophy
//!
//! This module removes everything that is not direct observation:
//!
//! - **No pgrep** — tunnel detection reads `/proc/[0-9]*/comm` files
//!   (process names, not command lines). The observer no longer matches
//!   its own shell command as a running `cloudflared` process.
//! - **No service guessing** — port detection reads `/proc/net/tcp` and
//!   `/proc/net/tcp6` directly, returning raw `Vec<u16>`. Port 22 is
//!   just `22` — the consumer decides it is SSH.
//! - **No `ss -ltnp` parsing** — eliminated >50 lines of fragile
//!   positional output parsing.
//! - **No version detection** — no `sshd -V`, `nginx -v`, etc.
//! - **Raw /proc reads** — cpuinfo, meminfo, uptime, loadavg, net/tcp.
//! - **Shell-out only where no `/proc` equivalent exists** — `df` for
//!   disk, `curl` for public IP (opt-in), accelerator detection.
//!
//! # Example
//!
//! ```rust,ignore
//! use runtimo_core::Telemetry;
//!
//! let tel = Telemetry::capture();
//! tel.print_report();
//! ```
//!
//! # Performance
//!
//! Results are cached for 30 seconds via [`TELEMETRY_CACHE`] to avoid
//! repeated `/proc` reads on consecutive calls.

use crate::cmd::run_cmd;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

static TELEMETRY_CACHE: Mutex<Option<(Telemetry, std::time::Instant)>> = Mutex::new(None);
const CACHE_TTL_SECS: u64 = 30;

/// Full system telemetry snapshot.
///
/// Contains three sub-structures: [`SystemInfo`], [`HardwareInfo`],
/// and [`NetworkInfo`], plus a Unix timestamp. Service detection has been
/// removed in favor of raw listening ports in [`NetworkInfo`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)]
pub struct Telemetry {
    /// Unix timestamp (seconds) when the snapshot was taken.
    pub timestamp: u64,
    /// Basic system information (CPU, RAM, disk, uptime, load).
    pub system: SystemInfo,
    /// Special hardware devices (TPU, GPU, JAX availability).
    pub hardware: HardwareInfo,
    /// Network state (public IP, tunnel status, listening ports).
    pub network: NetworkInfo,
}

/// Basic system information — direct `/proc` reads only.
///
/// No shell commands are used for data available in `/proc`. Disk
/// information (`df`) is the only exception because Linux provides
/// no per-mount usage summary in `/proc`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)]
pub struct SystemInfo {
    /// CPU model string from `/proc/cpuinfo` `model name` field.
    pub cpu_model: String,
    /// Logical CPU core count from `/proc/cpuinfo` (counts `processor` entries).
    pub cpu_count: u32,
    /// Total RAM in human-readable form (e.g. `"32Gi"`) from `/proc/meminfo`
    /// `MemTotal` (kB → human).
    pub ram_total: String,
    /// Free RAM in human-readable form (e.g. `"750Mi"`) from `/proc/meminfo`
    /// `MemFree` (kB → human).
    pub ram_free: String,
    /// Available RAM in human-readable form (e.g. `"22Gi"`) from `/proc/meminfo`
    /// `MemAvailable` (kB → human). This is the memory usable for new
    /// allocations without swapping — more useful than `ram_free` for
    /// capacity planning.
    pub ram_available: String,
    /// Total disk space in human-readable form (e.g. `"100G"`) from `df -h /`.
    pub disk_total: String,
    /// Free disk space in human-readable form from `df -h /`.
    pub disk_free: String,
    /// Disk usage percentage as a string without `%` sign (e.g. `"45"`).
    pub disk_used_percent: String,
    /// Human-readable uptime (e.g. `"up 6 days, 3 hours"`) computed from
    /// `/proc/uptime`.
    pub uptime: String,
    /// Machine-parseable uptime in seconds from `/proc/uptime` first field.
    pub uptime_seconds: u64,
    /// Load average string (e.g. `"0.50, 0.30, 0.20"`) from `/proc/loadavg`
    /// first three fields.
    pub load_average: String,
}

/// Special hardware device information.
///
/// Detects accelerators generically — GPUs (nvidia-smi, rocm-smi, /dev/dri),
/// TPUs (/dev/accel*), and JAX availability. Reports what exists, not what
/// was expected. Shell commands are used here because accelerator detection
/// requires vendor-specific tools that have no `/proc` equivalent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)]
pub struct HardwareInfo {
    /// Detected accelerator devices (any kind). Empty vec = no accelerators found.
    #[serde(default)]
    pub accelerators: Vec<AcceleratorInfo>,
    /// Whether the `jax` Python package is importable.
    #[serde(default)]
    pub jax_available: bool,
    /// JAX version string (e.g. `"0.4.25"`), if available.
    #[serde(default)]
    pub jax_version: Option<String>,
    /// Number of JAX-visible devices, if available.
    #[serde(default)]
    pub jax_device_count: Option<usize>,
}

/// A detected hardware accelerator.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)]
pub struct AcceleratorInfo {
    /// Accelerator kind: "gpu", "tpu", "npu".
    pub kind: String,
    /// Number of devices of this kind detected.
    pub count: usize,
    /// Vendor name if identifiable (e.g. "nvidia", "amd", "google").
    #[serde(default)]
    pub vendor: Option<String>,
    /// Device model string if available.
    #[serde(default)]
    pub model: Option<String>,
}

/// Network state information.
///
/// Public IP capture is **opt-in** via `RUNTIMO_ENABLE_PUBLIC_IP=1`.
/// Without this env var, `public_ip` defaults to `"unknown"` to prevent
/// unintended external network metadata leakage.
///
/// Tunnel detection reads `/proc/[0-9]*/comm` files (process names only,
/// not command lines). This eliminates the self-match bug where `pgrep`
/// would match the shell that runs `pgrep` itself.
///
/// Listening ports are read directly from `/proc/net/tcp` and `/proc/net/tcp6`
/// — no `ss` shell-out, no service name guessing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)]
pub struct NetworkInfo {
    /// Public IP address (from `ifconfig.me` when `RUNTIMO_ENABLE_PUBLIC_IP=1`),
    /// or `"unknown"`.
    pub public_ip: String,
    /// Whether a `cloudflared` tunnel process is running (detected via
    /// `/proc/*/comm` content match, not pgrep).
    pub tunnel_running: bool,
    /// PID of the `cloudflared` process if found, extracted from the
    /// `/proc/<pid>` directory name.
    pub tunnel_pid: Option<u32>,
    /// Raw listening TCP ports from `/proc/net/tcp` and `/proc/net/tcp6`.
    /// Only ports in `LISTEN` (state `0A`) state are included.
    /// Sorted ascending, duplicates removed.
    #[serde(default)]
    pub listening_ports: Vec<u16>,
}

// ── /proc file reading helpers ───────────────────────────────────────────

/// Reads the entire contents of a `/proc` file into a `String`.
///
/// Returns an empty string if the file does not exist or cannot be read.
/// This is the only way to interact with `/proc` files in telemetry —
/// all data sources are read through this function.
fn read_proc_file(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Parses a `/proc/meminfo` key value in kB and returns the raw numeric value.
///
/// `/proc/meminfo` lines have the format `Key:    12345 kB`. This function
/// finds the line starting with `key`, extracts the numeric value (first
/// whitespace-delimited field after the colon), and parses it as `u64`.
///
/// Returns `0` if the key is not found or the value cannot be parsed.
fn parse_meminfo_kb(data: &str, key: &str) -> u64 {
    data.lines()
        .find(|l| l.starts_with(key))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
}

/// Converts a kilobyte count to a human-readable string.
///
/// Uses binary suffixes (KiB, MiB, GiB, TiB). Values >= 1000 KiB are
/// displayed with the next-higher unit. The output format matches the
/// `free -h` style: e.g. `"16Gi"`, `"750Mi"`, `"512Ki"`.
///
/// # Examples
///
/// - `format_mem_kb(512)` → `"512Ki"`
/// - `format_mem_kb(768000)` → `"750Mi"`
/// - `format_mem_kb(16777216)` → `"16Gi"`
fn format_mem_kb(kb: u64) -> String {
    if kb >= 1_048_576 {
        // GiB: >= 1024^2 KiB
        format!("{}Gi", kb / 1_048_576)
    } else if kb >= 1_024 {
        // MiB: >= 1024 KiB
        format!("{}Mi", kb / 1_024)
    } else {
        // KiB: raw value
        format!("{}Ki", kb)
    }
}

/// Formats a duration in seconds into a human-readable uptime string.
///
/// Breaks down the duration into days, hours, and minutes. Omits zero-value
/// units. The format matches `uptime -p` output: e.g. `"up 6 days, 3 hours,
/// 12 minutes"`.
///
/// # Examples
///
/// - `format_uptime(60)` → `"up 1 minute"`
/// - `format_uptime(3661)` → `"up 1 hour, 1 minute"`
/// - `format_uptime(526380)` → `"up 6 days, 2 hours, 13 minutes"`
fn format_uptime(total_seconds: u64) -> String {
    let days = total_seconds / 86_400;
    let hours = (total_seconds % 86_400) / 3_600;
    let minutes = (total_seconds % 3_600) / 60;

    let mut parts: Vec<String> = Vec::with_capacity(3);
    if days > 0 {
        parts.push(format!("{} day{}", days, if days == 1 { "" } else { "s" }));
    }
    if hours > 0 {
        parts.push(format!(
            "{} hour{}",
            hours,
            if hours == 1 { "" } else { "s" }
        ));
    }
    if minutes > 0 || parts.is_empty() {
        // Always show at least minutes
        parts.push(format!(
            "{} minute{}",
            minutes,
            if minutes == 1 { "" } else { "s" }
        ));
    }
    format!("up {}", parts.join(", "))
}

// ── Telemetry capture ────────────────────────────────────────────────────

impl Telemetry {
    /// Captures a full system telemetry snapshot.
    ///
    /// Results are cached for [`CACHE_TTL_SECS`] (30 seconds) to avoid
    /// repeated filesystem reads on consecutive calls. Network queries
    /// (public_ip, tunnel) are included in the cached value.
    pub fn capture() -> Self {
        let now = std::time::Instant::now();
        {
            let cache = TELEMETRY_CACHE.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((cached, instant)) = cache.as_ref() {
                if now.duration_since(*instant).as_secs() < CACHE_TTL_SECS {
                    return cached.clone();
                }
            }
        }

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());

        let telemetry = Self {
            timestamp,
            system: SystemInfo::capture(),
            hardware: HardwareInfo::capture(),
            network: NetworkInfo::capture(),
        };

        let mut cache = TELEMETRY_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        *cache = Some((telemetry.clone(), now));
        telemetry
    }

    /// Prints telemetry in a human-readable report to stdout.
    ///
    /// Output includes CPU cores, RAM available, machine-parseable uptime
    /// seconds, contextualized load average (with core count), raw listening
    /// ports, and tunnel PID.
    pub fn print_report(&self) {
        println!("\n{}", "=".repeat(60));
        println!(" RUNTIMO TELEMETRY [{}]", self.timestamp);
        println!("{}", "=".repeat(60));

        println!("\n--- SYSTEM ---");
        println!(
            " CPU   : {} ({} cores)",
            self.system.cpu_model, self.system.cpu_count
        );
        println!(
            " RAM   : {} total, {} free, {} available",
            self.system.ram_total, self.system.ram_free, self.system.ram_available
        );
        println!(
            " Disk  : {} total, {} free ({}% used)",
            self.system.disk_total, self.system.disk_free, self.system.disk_used_percent
        );
        // Machine-parseable uptime: "up 6 days (526380s)"
        println!(
            " Uptime: {} ({}s)",
            self.system.uptime, self.system.uptime_seconds
        );
        // Contextualized load: "3.19, 4.93, 7.68 (4 cores)"
        println!(
            " Load  : {} ({} cores)",
            self.system.load_average, self.system.cpu_count
        );

        println!("\n--- HARDWARE ---");
        if self.hardware.accelerators.is_empty() {
            println!(" Accelerators: none detected");
        } else {
            for acc in &self.hardware.accelerators {
                println!(
                    " {}: {}x {}{}",
                    acc.kind,
                    acc.count,
                    acc.model.as_deref().unwrap_or("unknown"),
                    acc.vendor
                        .as_ref()
                        .map(|v| format!(" ({})", v))
                        .unwrap_or_default()
                );
            }
        }
        if self.hardware.jax_available {
            println!(
                " JAX: v{} ({} devices)",
                self.hardware
                    .jax_version
                    .clone()
                    .unwrap_or_else(|| "unknown".into()),
                self.hardware.jax_device_count.unwrap_or(0)
            );
        }

        println!("\n--- NETWORK ---");
        println!(" Public IP: {}", self.network.public_ip);
        // Tunnel with PID: "cloudflared (PID 1234)" or "none"
        if self.network.tunnel_running {
            println!(
                " Tunnel: cloudflared (PID {})",
                self.network
                    .tunnel_pid
                    .map_or_else(|| "?".to_string(), |p| p.to_string())
            );
        } else {
            println!(" Tunnel: none");
        }
        if self.network.listening_ports.is_empty() {
            println!(" Listening ports: none");
        } else {
            let ports_str = self
                .network
                .listening_ports
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            println!(" Listening ports: {}", ports_str);
        }

        println!("\n{}", "=".repeat(60));
    }
}

// ── SystemInfo capture — direct /proc reads ──────────────────────────────

impl SystemInfo {
    fn capture() -> Self {
        // /proc/cpuinfo: extract model name and count logical processors
        let cpuinfo = read_proc_file("/proc/cpuinfo");
        let cpu_model = cpuinfo
            .lines()
            .find(|l| l.starts_with("model name"))
            .and_then(|l| l.split(':').nth(1))
            .map_or_else(|| "unknown".to_string(), |s| s.trim().to_string());
        // Count lines beginning with "processor" — each is a logical core
        let cpu_count: u32 = cpuinfo
            .lines()
            .filter(|l| l.starts_with("processor"))
            .count()
            .try_into()
            .unwrap_or(0);

        // /proc/meminfo: MemTotal, MemFree, MemAvailable (all in kB)
        let meminfo = read_proc_file("/proc/meminfo");
        let ram_total = format_mem_kb(parse_meminfo_kb(&meminfo, "MemTotal:"));
        let ram_free = format_mem_kb(parse_meminfo_kb(&meminfo, "MemFree:"));
        let ram_available = format_mem_kb(parse_meminfo_kb(&meminfo, "MemAvailable:"));

        // /proc/uptime: first field is uptime in seconds (fractional).
        // The value is always non-negative; cast truncation is safe.
        let uptime = read_proc_file("/proc/uptime");
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let uptime_seconds: u64 = uptime
            .split_whitespace()
            .next()
            .and_then(|s| s.parse::<f64>().ok())
            .map_or(0, |f: f64| f as u64);
        let uptime_str = format_uptime(uptime_seconds);

        // /proc/loadavg: first three fields are 1/5/15 min load averages
        let loadavg = read_proc_file("/proc/loadavg");
        let load_average = {
            // Extract first three whitespace-separated fields from /proc/loadavg
            let mut fields = loadavg.split_whitespace();
            match (fields.next(), fields.next(), fields.next()) {
                (Some(one), Some(five), Some(fifteen)) => {
                    format!("{one}, {five}, {fifteen}")
                }
                _ => String::from("unknown"),
            }
        };

        // Disk: no /proc equivalent; keep df shell-out
        let disk_total = run_cmd("df -h / | tail -1 | awk '{print $2}'");
        let disk_free = run_cmd("df -h / | tail -1 | awk '{print $4}'");
        let disk_pct_str = run_cmd("df / | tail -1 | awk '{print $5}'");
        let disk_used_percent = disk_pct_str.replace('%', "");

        Self {
            cpu_model,
            cpu_count,
            ram_total,
            ram_free,
            ram_available,
            disk_total,
            disk_free,
            disk_used_percent,
            uptime: uptime_str,
            uptime_seconds,
            load_average,
        }
    }
}

// ── HardwareInfo capture — vendor tools (no /proc equivalent) ────────────

impl HardwareInfo {
    fn capture() -> Self {
        let mut accelerators = Vec::new();

        // TPU devices via /dev/accel*
        let tpu_count: usize = run_cmd("ls /dev/accel* 2>/dev/null | wc -l")
            .parse()
            .unwrap_or(0);
        if tpu_count > 0 {
            accelerators.push(AcceleratorInfo {
                kind: "tpu".into(),
                count: tpu_count,
                vendor: Some("google".into()),
                model: None,
            });
        }

        // NVIDIA GPUs via nvidia-smi
        let nvidia_gpu_count: usize = run_cmd("nvidia-smi --list-gpus 2>/dev/null | wc -l")
            .parse()
            .unwrap_or(0);
        if nvidia_gpu_count > 0 {
            let model =
                run_cmd("nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | head -1");
            accelerators.push(AcceleratorInfo {
                kind: "gpu".into(),
                count: nvidia_gpu_count,
                vendor: Some("nvidia".into()),
                model: if model.is_empty() { None } else { Some(model) },
            });
        }

        // AMD GPUs via rocm-smi
        let amd_gpu_count: usize =
            run_cmd("rocm-smi --showproductname 2>/dev/null | grep -c 'GPU\\['")
                .parse()
                .unwrap_or(0);
        if amd_gpu_count > 0 {
            accelerators.push(AcceleratorInfo {
                kind: "gpu".into(),
                count: amd_gpu_count,
                vendor: Some("amd".into()),
                model: None,
            });
        }

        // Generic DRM devices (fallback for any GPU)
        if nvidia_gpu_count == 0 && amd_gpu_count == 0 {
            let dri_count: usize = run_cmd("ls /dev/dri/render* 2>/dev/null | wc -l")
                .parse()
                .unwrap_or(0);
            if dri_count > 0 {
                accelerators.push(AcceleratorInfo {
                    kind: "gpu".into(),
                    count: dri_count,
                    vendor: None,
                    model: Some("drm-render".into()),
                });
            }
        }

        let jax_available =
            run_cmd("timeout 10 python3 -c 'import jax' 2>/dev/null && echo yes || echo no")
                == "yes";
        let jax_version = if jax_available {
            Some(run_cmd(
                "timeout 10 python3 -c 'import jax; print(jax.__version__)'",
            ))
        } else {
            None
        };
        let jax_device_count = if jax_available {
            run_cmd("timeout 10 python3 -c 'import jax; print(len(jax.devices()))'")
                .parse()
                .ok()
        } else {
            None
        };

        Self {
            accelerators,
            jax_available,
            jax_version,
            jax_device_count,
        }
    }
}

// ── NetworkInfo capture — /proc for tunnels and ports ────────────────────

impl NetworkInfo {
    /// Captures network state with opt-in public IP, tunnel detection via
    /// `/proc/*/comm`, and listening ports from `/proc/net/tcp` + `tcp6`.
    ///
    /// Public IP is only queried when `RUNTIMO_ENABLE_PUBLIC_IP=1`. Without it,
    /// `public_ip` is set to `"unknown"`.
    ///
    /// Tunnel detection reads `/proc/[0-9]*/comm` files and checks if any
    /// contain `"cloudflared"`. The `comm` file holds only the process name
    /// (max 16 chars), never the command line — this eliminates the self-match
    /// bug where `pgrep -fa cloudflared` matches its own shell invocation.
    fn capture() -> Self {
        let public_ip = if std::env::var("RUNTIMO_ENABLE_PUBLIC_IP").as_deref() == Ok("1") {
            run_cmd(
                "curl -s --connect-timeout 5 --max-time 5 ifconfig.me 2>/dev/null || echo 'unknown'",
            )
        } else {
            "unknown".to_string()
        };

        let (tunnel_running, tunnel_pid) = detect_cloudflared();
        let listening_ports = read_listening_ports();

        Self {
            public_ip,
            tunnel_running,
            tunnel_pid,
            listening_ports,
        }
    }
}

/// Scans `/proc/[0-9]*/comm` for a `cloudflared` process.
///
/// # How it works
///
/// 1. Iterates all directory entries in `/proc` whose names consist solely
///    of ASCII digits (these are PID directories).
/// 2. Reads the `comm` file inside each PID directory — this file contains
///    only the process name (truncated to 15 chars by the kernel), never
///    the command line or arguments.
/// 3. If the trimmed content equals `"cloudflared"`, extracts the PID from
///    the directory name.
///
/// # Why `comm`, not `cmdline`
///
/// The `cmdline` file (`/proc/[pid]/cmdline`) contains the full command
/// line (null-delimited), including arguments like `--token <value>`.
/// Using `comm` avoids:
/// - Reading potentially sensitive command-line tokens.
/// - The self-match bug: `sh -c pgrep -fa cloudflared` contains `cloudflared`
///   in its command line but NOT in its `comm` file (which would be `sh`
///   or `pgrep`).
///
/// Returns `(true, Some(pid))` if found, `(false, None)` otherwise.
fn detect_cloudflared() -> (bool, Option<u32>) {
    // Read /proc directory — each numeric subdirectory is a PID
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return (false, None);
    };

    for entry in dir.flatten() {
        let path = entry.path();
        // Only consider entries whose filename is purely numeric (PIDs)
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }

        let comm_path = path.join("comm");
        let Ok(content) = std::fs::read_to_string(&comm_path) else {
            continue;
        };

        if content.trim() == "cloudflared" {
            if let Ok(pid) = name.parse::<u32>() {
                return (true, Some(pid));
            }
        }
    }

    (false, None)
}

/// Reads listening TCP ports from `/proc/net/tcp` and `/proc/net/tcp6`.
///
/// # Format
///
/// Each line (after the header) has the format:
/// ```text
///   0: 00000000:0016 00000000:0000 0A ...
/// ```
///
/// - Column 2 (`00000000:0016`) is the local address. The part after the
///   colon (`0016`) is the port number in hexadecimal.
/// - Column 4 (`0A`) is the socket state in hexadecimal. `0A` = `LISTEN`.
///
/// Only entries with state `0A` (LISTEN) are included. Ports are sorted
/// ascending and deduplicated.
///
/// # Why `/proc/net/tcp`, not `ss -ltnp`
///
/// - `/proc/net/tcp` is a kernel-provided procfs file — no subprocess,
///   no command parsing, no fragile positional output logic.
/// - `ss -ltnp` requires shell-out, parses variable-width columns, and
///   may produce output that varies across `iproute2` versions.
/// - The procfs format is stable kernel ABI.
fn read_listening_ports() -> Vec<u16> {
    let mut ports = Vec::new();

    for path in &["/proc/net/tcp", "/proc/net/tcp6"] {
        let data = read_proc_file(path);
        // Skip header line (starts with "  sl")
        for line in data.lines().skip(1) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            // Minimum columns: sl(0:) + local_address + rem_address + state
            if parts.len() < 4 {
                continue;
            }

            // Column 2 = local_address (e.g. "00000000:0016")
            // Column 4 = state (e.g. "0A" = LISTEN)
            // Use .get() for clippy::indexing_slicing compliance
            if parts.get(3) != Some(&"0A") {
                continue;
            }

            // Extract port hex from local_address (portion after ':')
            if let Some(port_hex) = parts.get(1).and_then(|addr| addr.split(':').nth(1)) {
                if let Ok(port) = u16::from_str_radix(port_hex, 16) {
                    ports.push(port);
                }
            }
        }
    }

    ports.sort_unstable();
    ports.dedup();
    ports
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── SystemInfo tests ────────────────────────────────────────────

    #[test]
    fn test_telemetry_capture() {
        let telemetry = Telemetry::capture();
        assert!(telemetry.timestamp > 0, "timestamp must be positive");

        let s = &telemetry.system;
        assert!(!s.cpu_model.is_empty(), "cpu_model must not be empty");
        assert!(s.cpu_count > 0, "cpu_count must be > 0");
        assert!(!s.ram_total.is_empty(), "ram_total must not be empty");
        assert!(!s.ram_free.is_empty(), "ram_free must not be empty");
        assert!(
            !s.ram_available.is_empty(),
            "ram_available must not be empty"
        );
        assert!(!s.disk_total.is_empty(), "disk_total must not be empty");
        assert!(s.uptime_seconds > 0, "uptime_seconds must be > 0");
        assert!(!s.load_average.is_empty(), "load_average must not be empty");

        let h = &telemetry.hardware;
        assert!(
            h.accelerators.iter().all(|a| !a.kind.is_empty()),
            "accelerator kind must not be empty"
        );
        assert!(
            h.accelerators.iter().all(|a| a.count > 0),
            "accelerator count must be > 0"
        );

        let net = &telemetry.network;
        assert!(!net.public_ip.is_empty(), "public_ip must not be empty");
        // Default: public_ip is "unknown" unless RUNTIMO_ENABLE_PUBLIC_IP=1
        assert_eq!(
            net.public_ip, "unknown",
            "public_ip should be 'unknown' by default (opt-in via RUNTIMO_ENABLE_PUBLIC_IP=1)"
        );
        // listening_ports is a Vec — can be empty in container/isolated env
        assert!(
            net.listening_ports.iter().all(|p| *p > 0),
            "all listening ports must be > 0"
        );
    }

    #[test]
    fn test_telemetry_cache_works() {
        let t1 = Telemetry::capture();
        let t2 = Telemetry::capture();
        assert_eq!(
            t1.timestamp, t2.timestamp,
            "cached telemetry should be identical"
        );
    }

    #[test]
    fn test_system_info_from_proc() {
        // Verify cpu_count, ram_available, uptime_seconds are populated
        // from /proc reads (not from shell commands that might fail in
        // minimal containers).
        let sys = SystemInfo::capture();
        assert!(sys.cpu_count > 0, "cpu_count from /proc/cpuinfo");
        assert!(
            !sys.ram_available.is_empty(),
            "ram_available from /proc/meminfo MemAvailable"
        );
        assert!(sys.uptime_seconds > 0, "uptime_seconds from /proc/uptime");
        // uptime string should be non-empty and start with "up"
        assert!(
            sys.uptime.starts_with("up "),
            "uptime string should start with 'up ': got '{}'",
            sys.uptime
        );
        // cpu_model should be non-empty
        assert!(
            !sys.cpu_model.is_empty(),
            "cpu_model from /proc/cpuinfo model name"
        );
    }

    #[test]
    fn test_cloudflared_detection() {
        // The cloudflared detection must NOT self-match.
        // This test verifies that detecting cloudflared doesn't find
        // the shell that is running the detection command (because it reads
        // /proc/*/comm, not pgrep).
        let (running, pid) = detect_cloudflared();

        // If cloudflared is actually running on this machine, it should be found.
        // But it should NEVER report pid of the detection process itself.
        if running {
            assert!(pid.is_some(), "tunnel_running implies tunnel_pid");
            let found_pid = pid.unwrap();
            // Verify the PID actually belongs to a cloudflared process
            let comm_path = format!("/proc/{}/comm", found_pid);
            if let Ok(content) = std::fs::read_to_string(&comm_path) {
                assert_eq!(
                    content.trim(),
                    "cloudflared",
                    "PID {} comm should be 'cloudflared', got '{}'",
                    found_pid,
                    content.trim()
                );
            }
        }
        // Even if not running, the function must return cleanly
        assert!(!running || pid.is_some());
    }

    #[test]
    fn test_listening_ports() {
        let ports = read_listening_ports();

        // Verify no duplicate ports
        let mut uniq = ports.clone();
        uniq.dedup();
        assert_eq!(
            ports.len(),
            uniq.len(),
            "listening ports must have no duplicates"
        );

        // Verify ports are sorted
        for w in ports.windows(2) {
            assert!(w[0] <= w[1], "listening ports must be sorted: {:?}", ports);
        }

        // All ports should be valid (1-65535)
        for &p in &ports {
            assert!(p > 0, "port 0 is not a valid listening port");
        }

        // If this runs on a live system, ports is a Vec — it can be empty
        // in isolated containers. That's valid — no asserting on length.
    }

    // ── Helper function tests ────────────────────────────────────────

    #[test]
    fn test_format_mem_kb() {
        assert_eq!(format_mem_kb(512), "512Ki");
        assert_eq!(format_mem_kb(1024), "1Mi");
        assert_eq!(format_mem_kb(1536), "1Mi"); // >1024 snaps to Mi
        assert_eq!(format_mem_kb(1048576), "1Gi");
        assert_eq!(format_mem_kb(2097152), "2Gi");
        assert_eq!(format_mem_kb(768000), "750Mi"); // ~750Mi
                                                    // Edge: 0 KB
        assert_eq!(format_mem_kb(0), "0Ki");
    }

    #[test]
    fn test_format_uptime() {
        assert!(
            format_uptime(0).contains("minute"),
            "zero uptime: {}",
            format_uptime(0)
        );
        assert!(
            format_uptime(60).contains("1 minute"),
            "60s: {}",
            format_uptime(60)
        );
        assert!(
            format_uptime(3600).contains("1 hour"),
            "3600s: {}",
            format_uptime(3600)
        );
        assert!(
            format_uptime(86400).contains("1 day"),
            "86400s: {}",
            format_uptime(86400)
        );
        // All start with "up "
        assert!(
            format_uptime(12345).starts_with("up "),
            "uptime should start with 'up '"
        );
    }

    #[test]
    fn test_parse_meminfo_kb() {
        let sample = "MemTotal:       32768000 kB\nMemFree:         8000000 kB\nMemAvailable:   22000000 kB\n";
        assert_eq!(parse_meminfo_kb(sample, "MemTotal:"), 32_768_000);
        assert_eq!(parse_meminfo_kb(sample, "MemFree:"), 8_000_000);
        assert_eq!(parse_meminfo_kb(sample, "MemAvailable:"), 22_000_000);
        // Missing key
        assert_eq!(parse_meminfo_kb(sample, "SwapTotal:"), 0);
        // Empty input
        assert_eq!(parse_meminfo_kb("", "MemTotal:"), 0);
    }

    // ── Backward compatibility tests ─────────────────────────────────

    #[test]
    fn test_accelerators_back_compat() {
        let hw = HardwareInfo {
            accelerators: vec![
                AcceleratorInfo {
                    kind: "gpu".into(),
                    count: 4,
                    vendor: Some("nvidia".into()),
                    model: Some("A100".into()),
                },
                AcceleratorInfo {
                    kind: "tpu".into(),
                    count: 8,
                    vendor: Some("google".into()),
                    model: None,
                },
            ],
            jax_available: false,
            jax_version: None,
            jax_device_count: None,
        };

        let total_tpu: usize = hw
            .accelerators
            .iter()
            .filter(|a| a.kind == "tpu")
            .map(|a| a.count)
            .sum();
        let total_gpu: usize = hw
            .accelerators
            .iter()
            .filter(|a| a.kind == "gpu")
            .map(|a| a.count)
            .sum();

        assert_eq!(total_tpu, 8, "total tpu should be 8");
        assert_eq!(total_gpu, 4, "total gpu should be 4");
    }

    #[test]
    fn test_accelerators_empty_is_valid() {
        let hw = HardwareInfo {
            accelerators: vec![],
            jax_available: false,
            jax_version: None,
            jax_device_count: None,
        };

        assert!(hw.accelerators.is_empty());
    }

    #[test]
    fn test_telemetry_serialization_roundtrip() {
        let hw = HardwareInfo {
            accelerators: vec![AcceleratorInfo {
                kind: "gpu".into(),
                count: 2,
                vendor: Some("nvidia".into()),
                model: Some("H100".into()),
            }],
            jax_available: true,
            jax_version: Some("0.4.30".into()),
            jax_device_count: Some(2),
        };

        let net = NetworkInfo {
            public_ip: "192.0.2.1".into(),
            tunnel_running: false,
            tunnel_pid: None,
            listening_ports: vec![22, 80, 443],
        };

        let json = serde_json::to_string(&hw).unwrap();
        let parsed: HardwareInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.accelerators.len(), 1);
        assert_eq!(parsed.accelerators[0].kind, "gpu");
        assert_eq!(parsed.accelerators[0].model.as_deref(), Some("H100"));

        let json = serde_json::to_string(&net).unwrap();
        let parsed: NetworkInfo = serde_json::from_str(&json).unwrap();
        assert!(parsed.listening_ports.contains(&22));
        assert!(parsed.listening_ports.contains(&443));
        assert!(!parsed.tunnel_running);
        assert!(parsed.tunnel_pid.is_none());
    }

    #[test]
    fn test_telemetry_deserialize_old_wal_event() {
        let old_json = r#"{
            "jax_available": true,
            "jax_version": "0.4.25",
            "jax_device_count": 8
        }"#;

        let parsed: HardwareInfo = serde_json::from_str(old_json).unwrap();
        assert!(
            parsed.accelerators.is_empty(),
            "old WAL events deserialize with empty accelerators"
        );
        assert!(parsed.jax_available);
    }

    #[test]
    fn test_network_info_listening_ports_roundtrip() {
        // Verify that listening_ports serializes/deserializes correctly
        let net = NetworkInfo {
            public_ip: "unknown".into(),
            tunnel_running: false,
            tunnel_pid: None,
            listening_ports: vec![22, 11434, 3389],
        };

        let json = serde_json::to_string(&net).unwrap();
        let parsed: NetworkInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.listening_ports, vec![22, 11434, 3389]);
        assert!(!parsed.tunnel_running);
    }
}
