//! System Telemetry — Environment awareness for the capability runtime.
//!
//! Captures a full snapshot of the host machine: CPU, RAM, disk, TPU/GPU
//! devices, running services (vLLM), and network state (public IP, tunnels).
//!
//! Inspired by the Kaggle session telemetry pattern. Every capability execution
//! records telemetry before and after to detect resource deltas.
//!
//! # Example
//!
//! ```rust,ignore
//! use runtimo_core::Telemetry;
//!
//! let tel = Telemetry::capture();
//! tel.print_report();
//! // RUNTIMO TELEMETRY [1715800000]
//! // CPU   : AMD EPYC 7T83
//! // RAM   : 16Gi total, 13Gi free
//! // ...
//! ```

use crate::cmd::run_cmd;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

static TELEMETRY_CACHE: Mutex<Option<(Telemetry, std::time::Instant)>> = Mutex::new(None);
const CACHE_TTL_SECS: u64 = 30;

/// Full system telemetry snapshot.
///
/// Contains four sub-structures: [`SystemInfo`], [`HardwareInfo`],
/// [`ServiceInfo`], and [`NetworkInfo`], plus a Unix timestamp.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Telemetry {
    /// Unix timestamp (seconds) when the snapshot was taken.
    pub timestamp: u64,
    /// Basic system information (CPU model, RAM, disk, uptime, load).
    pub system: SystemInfo,
    /// Special hardware devices (TPU, GPU, JAX availability).
    pub hardware: HardwareInfo,
    /// Service status (vLLM version, running state, port binding).
    pub services: ServiceInfo,
    /// Network state (public IP, tunnel status).
    pub network: NetworkInfo,
}

/// Basic system information from `/proc` and shell commands.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemInfo {
    /// CPU model string (from `/proc/cpuinfo`).
    pub cpu_model: String,
    /// Total RAM (human-readable, e.g. `"16Gi"`).
    pub ram_total: String,
    /// Free RAM (human-readable, e.g. `"13Gi"`).
    pub ram_free: String,
    /// Total disk space (human-readable, e.g. `"100G"`).
    pub disk_total: String,
    /// Free disk space (human-readable).
    pub disk_free: String,
    /// Disk usage percentage (e.g. `"45%"`).
    pub disk_used_percent: String,
    /// System uptime (e.g. `"up 3 days, 2 hours"`).
    pub uptime: String,
    /// Load average (e.g. `" 0.50,  0.30,  0.20"`).
    pub load_average: String,
}

/// Special hardware device information.
///
/// Detects TPU accelerators (`/dev/accel*`), NVIDIA GPUs (`nvidia-smi`),
/// and JAX availability (Python import check).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardwareInfo {
    /// Number of TPU accelerator devices detected.
    pub tpu_devices: usize,
    /// Number of NVIDIA GPU devices detected.
    pub gpu_devices: usize,
    /// Whether the `jax` Python package is importable.
    pub jax_available: bool,
    /// JAX version string (e.g. `"0.4.25"`), if available.
    pub jax_version: Option<String>,
    /// Number of JAX-visible devices, if available.
    pub jax_device_count: Option<usize>,
}

/// Service status information.
///
/// Currently tracks vLLM: version, whether the process is running,
/// and whether port 8200 is bound.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceInfo {
    /// vLLM version string (e.g. `"0.4.0"`), if installed.
    pub vllm_version: Option<String>,
    /// Whether a `vllm serve` process is running.
    pub vllm_running: bool,
    /// Whether port 8200 is currently bound.
    pub vllm_port_bound: bool,
}

/// Network state information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkInfo {
    /// Public IP address (from `ifconfig.me`), or `"unknown"`.
    pub public_ip: String,
    /// Whether a `cloudflared` tunnel process is running.
    pub tunnel_running: bool,
    /// The full `cloudflared` process command line, if running.
    pub tunnel_name: Option<String>,
}

impl Telemetry {
    /// Captures a full system telemetry snapshot.
    ///
    /// Results are cached for 30 seconds to avoid running 15+ shell subprocesses
    /// on repeated calls. Network queries (public_ip, tunnel) are skipped when
    /// returning a cached value.
    pub fn capture() -> Self {
        let now = std::time::Instant::now();
        {
            // Handle poison error by recovering from the poisoned state
            let cache = TELEMETRY_CACHE.lock().unwrap_or_else(|e| e.into_inner());
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

        let telemetry = Self {
            timestamp,
            system: SystemInfo::capture(),
            hardware: HardwareInfo::capture(),
            services: ServiceInfo::capture(),
            network: NetworkInfo::capture(),
        };

        // Handle poison error by recovering from the poisoned state
        let mut cache = TELEMETRY_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        *cache = Some((telemetry.clone(), now));
        telemetry
    }

    /// Prints telemetry in a human-readable report to stdout.
    pub fn print_report(&self) {
        println!("\n{}", "=".repeat(60));
        println!(" RUNTIMO TELEMETRY [{}]", self.timestamp);
        println!("{}", "=".repeat(60));

        println!("\n--- SYSTEM ---");
        println!(" CPU   : {}", self.system.cpu_model);
        println!(
            " RAM   : {} total, {} free",
            self.system.ram_total, self.system.ram_free
        );
        println!(
            " Disk  : {} total, {} free ({}% used)",
            self.system.disk_total, self.system.disk_free, self.system.disk_used_percent
        );
        println!(" Uptime: {}", self.system.uptime);
        println!(" Load  : {}", self.system.load_average);

        println!("\n--- HARDWARE ---");
        println!(" TPU Devices: {}", self.hardware.tpu_devices);
        println!(" GPU Devices: {}", self.hardware.gpu_devices);
        if self.hardware.jax_available {
            println!(
                " JAX: v{} ({} devices)",
                self.hardware
                    .jax_version
                    .clone()
                    .unwrap_or_else(|| "unknown".into()),
                self.hardware.jax_device_count.unwrap_or(0)
            );
        } else {
            println!(" JAX: Not available");
        }

        println!("\n--- SERVICES ---");
        match &self.services.vllm_version {
            Some(v) => println!(
                " vLLM: v{} ({})",
                v,
                if self.services.vllm_running {
                    "running"
                } else {
                    "not running"
                }
            ),
            None => println!(" vLLM: not installed"),
        }
        println!(
            " Port 8200: {}",
            if self.services.vllm_port_bound {
                "BOUND"
            } else {
                "NOT BOUND"
            }
        );

        println!("\n--- NETWORK ---");
        println!(" Public IP: {}", self.network.public_ip);
        println!(
            " Tunnel: {} ({})",
            if self.network.tunnel_running {
                "running"
            } else {
                "not running"
            },
            self.network
                .tunnel_name
                .clone()
                .unwrap_or_else(|| "unknown".into())
        );

        println!("\n{}", "=".repeat(60));
    }
}

impl SystemInfo {
    fn capture() -> Self {
        Self {
            cpu_model: run_cmd("cat /proc/cpuinfo | grep 'model name' | head -1 | cut -d: -f2"),
            ram_total: run_cmd("free -h | grep Mem | awk '{print $2}'"),
            ram_free: run_cmd("free -h | grep Mem | awk '{print $4}'"),
            disk_total: run_cmd("df -h / | tail -1 | awk '{print $2}'"),
            disk_free: run_cmd("df -h / | tail -1 | awk '{print $4}'"),
            disk_used_percent: run_cmd("df -h / | tail -1 | awk '{print $5}'"),
            uptime: run_cmd("uptime -p"),
            load_average: run_cmd("uptime | awk -F'load average:' '{print $2}'"),
        }
    }
}

impl HardwareInfo {
    fn capture() -> Self {
        let tpu_devices = run_cmd("ls /dev/accel* 2>/dev/null | wc -l")
            .parse()
            .unwrap_or(0);

        let gpu_devices = run_cmd("nvidia-smi --list-gpus 2>/dev/null | wc -l")
            .parse()
            .unwrap_or(0);

        let jax_available =
            run_cmd("python3 -c 'import jax' 2>/dev/null && echo yes || echo no") == "yes";
        let jax_version = if jax_available {
            Some(run_cmd("python3 -c 'import jax; print(jax.__version__)'"))
        } else {
            None
        };
        let jax_device_count = if jax_available {
            run_cmd("python3 -c 'import jax; print(len(jax.devices()))'")
                .parse()
                .ok()
        } else {
            None
        };

        Self {
            tpu_devices,
            gpu_devices,
            jax_available,
            jax_version,
            jax_device_count,
        }
    }
}

impl ServiceInfo {
    fn capture() -> Self {
        let vllm_version = run_cmd("python3 -c 'import vllm; print(vllm.__version__)' 2>/dev/null");
        let vllm_running = !run_cmd("pgrep -fa 'vllm serve'").is_empty();
        let vllm_port_bound =
            !run_cmd("ss -ltn '( sport = :8200 )' 2>/dev/null | grep 8200").is_empty();

        Self {
            vllm_version: if vllm_version.is_empty() {
                None
            } else {
                Some(vllm_version)
            },
            vllm_running,
            vllm_port_bound,
        }
    }
}

impl NetworkInfo {
    fn capture() -> Self {
        let public_ip = run_cmd("curl -s ifconfig.me 2>/dev/null || echo 'unknown'");
        let tunnel_output = run_cmd("pgrep -fa cloudflared");
        let tunnel_running = !tunnel_output.is_empty();
        let tunnel_name = if tunnel_running {
            Some(tunnel_output)
        } else {
            None
        };

        Self {
            public_ip,
            tunnel_running,
            tunnel_name,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_telemetry_capture() {
        let telemetry = Telemetry::capture();
        assert!(telemetry.timestamp > 0);
    }
}
