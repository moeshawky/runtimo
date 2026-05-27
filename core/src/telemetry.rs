//! System Telemetry — Discovery-based environment awareness.
//!
//! Captures a full snapshot of the host machine: CPU, RAM, disk, accelerators
//! (any kind — GPU, TPU, NPU), running services (detected, not assumed),
//! and network state (public IP, tunnels).
//!
//! The telemetry is a **discovery protocol**: it reports what IS on the machine,
//! not what the developer expects to find. No hardcoded service names, no
//! assumed hardware. Empty means nothing was found — not that the field is
//! irrelevant. Every capability execution records before/after deltas.
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
const CACHE_TTL_SECS: u64 = 5;

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
    // --- Numeric fields for agent threshold computation ---
    /// Total RAM in bytes (machine-readable).
    pub ram_total_bytes: u64,
    /// Free RAM in bytes (machine-readable).
    pub ram_free_bytes: u64,
    /// Total disk space in bytes (machine-readable).
    pub disk_total_bytes: u64,
    /// Free disk space in bytes (machine-readable).
    pub disk_free_bytes: u64,
    /// Disk usage percentage as numeric (e.g. `45.0`, no `%` sign).
    pub disk_used_percent_numeric: f64,
}

/// Special hardware device information.
///
/// Detects accelerators generically — GPUs (nvidia-smi, rocm-smi, /dev/dri),
/// TPUs (/dev/accel*), and JAX availability. Reports what exists, not what
/// was expected.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

    // Backwards compat — computed from accelerators list above
    #[serde(default)]
    pub tpu_devices: usize,
    #[serde(default)]
    pub gpu_devices: usize,
}

/// A detected hardware accelerator.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// Service status — discovery-based, not checklist-based.
///
/// Scans for known service processes and listening ports, reports what
/// was actually detected. No service is assumed to exist.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceInfo {
    /// Services detected on this machine. Empty vec = no known services found.
    #[serde(default)]
    pub detected_services: Vec<DetectedService>,

    // Backwards compat fields
    #[serde(default)]
    pub vllm_version: Option<String>,
    #[serde(default)]
    pub vllm_running: bool,
    #[serde(default)]
    pub vllm_port_bound: bool,
}

/// A detected service running on the machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectedService {
    /// Service name (e.g. "vllm", "nginx", "postgres").
    pub name: String,
    /// Version string if detectable.
    #[serde(default)]
    pub version: Option<String>,
    /// Whether the service process is running.
    #[serde(default)]
    pub running: bool,
    /// Ports the service is listening on.
    #[serde(default)]
    pub ports: Vec<u16>,
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
    /// Results are cached for 5 seconds to avoid running 15+ shell subprocesses
    /// on repeated calls. Network queries (public_ip, tunnel) are skipped when
    /// returning a cached value.
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
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let telemetry = Self {
            timestamp,
            system: SystemInfo::capture(),
            hardware: HardwareInfo::capture(),
            services: ServiceInfo::capture(),
            network: NetworkInfo::capture(),
        };

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

        println!("\n--- SERVICES ---");
        if self.services.detected_services.is_empty() {
            println!(" Services: none detected");
        } else {
            for svc in &self.services.detected_services {
                let ports_str = if svc.ports.is_empty() {
                    String::new()
                } else {
                    format!(
                        " ports=[{}]",
                        svc.ports
                            .iter()
                            .map(|p| p.to_string())
                            .collect::<Vec<_>>()
                            .join(",")
                    )
                };
                println!(
                    " {}: v{} ({}){}",
                    svc.name,
                    svc.version.as_deref().unwrap_or("?"),
                    if svc.running { "running" } else { "stopped" },
                    ports_str
                );
            }
        }

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
        let ram_total = run_cmd("free -h | grep Mem | awk '{print $2}'");
        let ram_free = run_cmd("free -h | grep Mem | awk '{print $4}'");
        let disk_total = run_cmd("df -h / | tail -1 | awk '{print $2}'");
        let disk_free = run_cmd("df -h / | tail -1 | awk '{print $4}'");
        let disk_pct_str = run_cmd("df / | tail -1 | awk '{print $5}'");
        let disk_used_percent = disk_pct_str.replace('%', "");
        let disk_used_percent_numeric = disk_used_percent.parse::<f64>().unwrap_or(0.0);
        let ram_total_bytes = run_cmd("free -b | grep Mem | awk '{print $2}'")
            .parse()
            .unwrap_or(0);
        let ram_free_bytes = run_cmd("free -b | grep Mem | awk '{print $4}'")
            .parse()
            .unwrap_or(0);
        let disk_total_bytes = run_cmd("df --bytes / | tail -1 | awk '{print $2}'")
            .parse()
            .unwrap_or(0);
        let disk_free_bytes = run_cmd("df --bytes / | tail -1 | awk '{print $4}'")
            .parse()
            .unwrap_or(0);

        Self {
            cpu_model: run_cmd("cat /proc/cpuinfo | grep 'model name' | head -1 | cut -d: -f2"),
            ram_total,
            ram_free,
            disk_total,
            disk_free,
            disk_used_percent,
            uptime: run_cmd("uptime -p"),
            load_average: run_cmd("uptime | awk -F'load average:' '{print $2}'"),
            ram_total_bytes,
            ram_free_bytes,
            disk_total_bytes,
            disk_free_bytes,
            disk_used_percent_numeric,
        }
    }
}

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
            let model = run_cmd(
                "nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | head -1",
            );
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
            let dri_count: usize = run_cmd(
                "ls /dev/dri/render* 2>/dev/null | wc -l",
            )
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
            run_cmd("timeout 10 python3 -c 'import jax' 2>/dev/null && echo yes || echo no") == "yes";
        let jax_version = if jax_available {
            Some(run_cmd("timeout 10 python3 -c 'import jax; print(jax.__version__)'"))
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

        // Compute backwards-compat totals
        let total_tpu = accelerators
            .iter()
            .filter(|a| a.kind == "tpu")
            .map(|a| a.count)
            .sum();
        let total_gpu = accelerators
            .iter()
            .filter(|a| a.kind == "gpu")
            .map(|a| a.count)
            .sum();

        Self {
            accelerators,
            jax_available,
            jax_version,
            jax_device_count,
            tpu_devices: total_tpu,
            gpu_devices: total_gpu,
        }
    }
}

impl ServiceInfo {
    fn capture() -> Self {
        let mut detected = Vec::new();

        // vLLM
        let vllm_version =
            run_cmd("timeout 10 python3 -c 'import vllm; print(vllm.__version__)' 2>/dev/null");
        let vllm_running = !run_cmd("pgrep -fa 'vllm serve'").is_empty();
        let vllm_port_bound =
            !run_cmd("ss -ltn '( sport = :8200 )' 2>/dev/null | grep 8200").is_empty();
        if vllm_running || !vllm_version.is_empty() {
            let mut ports = Vec::new();
            if vllm_port_bound {
                ports.push(8200);
            }
            detected.push(DetectedService {
                name: "vllm".into(),
                version: if vllm_version.is_empty() {
                    None
                } else {
                    Some(vllm_version.clone())
                },
                running: vllm_running,
                ports,
            });
        }

        // nginx
        let nginx_running = !run_cmd("pgrep -x nginx").is_empty();
        if nginx_running {
            let nginx_version = run_cmd("nginx -v 2>&1 | grep -oP 'nginx/\\K[0-9.]+'");
            detected.push(DetectedService {
                name: "nginx".into(),
                version: if nginx_version.is_empty() {
                    None
                } else {
                    Some(nginx_version)
                },
                running: true,
                ports: vec![80, 443],
            });
        }

        // PostgreSQL
        let pg_running = !run_cmd("pgrep -x postgres").is_empty();
        if pg_running {
            detected.push(DetectedService {
                name: "postgres".into(),
                version: None,
                running: true,
                ports: vec![5432],
            });
        }

        // Redis
        let redis_running = !run_cmd("pgrep -x redis-server").is_empty();
        if redis_running {
            detected.push(DetectedService {
                name: "redis".into(),
                version: None,
                running: true,
                ports: vec![6379],
            });
        }

        // Docker
        let docker_running = !run_cmd("pgrep -x dockerd").is_empty();
        if docker_running {
            let docker_version = run_cmd("docker --version 2>/dev/null | grep -oP '[0-9]+\\.[0-9]+\\.[0-9]+' | head -1");
            detected.push(DetectedService {
                name: "docker".into(),
                version: if docker_version.is_empty() {
                    None
                } else {
                    Some(docker_version)
                },
                running: true,
                ports: Vec::new(),
            });
        }

        Self {
            detected_services: detected,
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
        let public_ip = run_cmd("curl -s --connect-timeout 5 --max-time 5 ifconfig.me 2>/dev/null || echo 'unknown'");
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
        assert!(telemetry.timestamp > 0, "timestamp must be positive");

        let s = &telemetry.system;
        assert!(!s.cpu_model.is_empty(), "cpu_model must not be empty");
        assert!(s.ram_total_bytes > 0, "ram_total_bytes must be > 0");
        assert!(!s.ram_total.is_empty(), "ram_total must not be empty");
        assert!(!s.disk_total.is_empty(), "disk_total must not be empty");

        let h = &telemetry.hardware;
        assert!(
            h.accelerators.iter().all(|a| !a.kind.is_empty()),
            "accelerator kind must not be empty"
        );
        assert!(
            h.accelerators.iter().all(|a| a.count > 0),
            "accelerator count must be > 0"
        );

        let svc = &telemetry.services;
        assert!(
            svc.detected_services.iter().all(|s| !s.name.is_empty()),
            "service name must not be empty"
        );

        let net = &telemetry.network;
        assert!(!net.public_ip.is_empty(), "public_ip must not be empty");
    }

    #[test]
    fn test_telemetry_cache_works() {
        let t1 = Telemetry::capture();
        let t2 = Telemetry::capture();
        assert_eq!(t1.timestamp, t2.timestamp, "cached telemetry should be identical");
    }

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
            tpu_devices: 0,
            gpu_devices: 0,
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

        assert_eq!(total_tpu, 8, "back-compat tpu_devices should be 8");
        assert_eq!(total_gpu, 4, "back-compat gpu_devices should be 4");
    }

    #[test]
    fn test_accelerators_empty_is_valid() {
        let hw = HardwareInfo {
            accelerators: vec![],
            jax_available: false,
            jax_version: None,
            jax_device_count: None,
            tpu_devices: 0,
            gpu_devices: 0,
        };

        assert!(hw.accelerators.is_empty());
        assert_eq!(hw.tpu_devices, 0);
        assert_eq!(hw.gpu_devices, 0);
    }

    #[test]
    fn test_service_back_compat() {
        let svc = ServiceInfo {
            detected_services: vec![DetectedService {
                name: "vllm".into(),
                version: Some("0.6.0".into()),
                running: true,
                ports: vec![8200],
            }],
            vllm_version: None,
            vllm_running: false,
            vllm_port_bound: false,
        };

        let vllm = &svc.detected_services[0];
        assert_eq!(vllm.name, "vllm");
        assert_eq!(vllm.version.as_deref(), Some("0.6.0"));
        assert!(vllm.running);
        assert_eq!(vllm.ports, vec![8200]);
    }

    #[test]
    fn test_services_empty_is_valid() {
        let svc = ServiceInfo {
            detected_services: vec![],
            vllm_version: None,
            vllm_running: false,
            vllm_port_bound: false,
        };

        assert!(svc.detected_services.is_empty());
    }

    #[test]
    fn test_telemetry_serialization_roundtrip() {
        let hw = HardwareInfo {
            accelerators: vec![
                AcceleratorInfo {
                    kind: "gpu".into(),
                    count: 2,
                    vendor: Some("nvidia".into()),
                    model: Some("H100".into()),
                },
            ],
            jax_available: true,
            jax_version: Some("0.4.30".into()),
            jax_device_count: Some(2),
            tpu_devices: 0,
            gpu_devices: 2,
        };

        let svc = ServiceInfo {
            detected_services: vec![DetectedService {
                name: "docker".into(),
                version: Some("26.0.0".into()),
                running: true,
                ports: vec![],
            }],
            vllm_version: None,
            vllm_running: false,
            vllm_port_bound: false,
        };

        let json = serde_json::to_string(&hw).unwrap();
        let parsed: HardwareInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.accelerators.len(), 1);
        assert_eq!(parsed.accelerators[0].kind, "gpu");
        assert_eq!(parsed.accelerators[0].model.as_deref(), Some("H100"));

        let json = serde_json::to_string(&svc).unwrap();
        let parsed: ServiceInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.detected_services.len(), 1);
        assert_eq!(parsed.detected_services[0].name, "docker");
    }

    #[test]
    fn test_telemetry_deserialize_old_wal_event() {
        let old_json = r#"{
            "tpu_devices": 8,
            "gpu_devices": 4,
            "jax_available": true,
            "jax_version": "0.4.25",
            "jax_device_count": 8
        }"#;

        let parsed: HardwareInfo = serde_json::from_str(old_json).unwrap();
        assert_eq!(parsed.tpu_devices, 8);
        assert_eq!(parsed.gpu_devices, 4);
        assert!(parsed.accelerators.is_empty(),
            "old WAL events deserialize with empty accelerators (backwards compat)");
        assert!(parsed.jax_available);
    }
}
