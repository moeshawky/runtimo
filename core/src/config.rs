//! Persistent configuration for Runtimo.
//!
//! Reads/writes a TOML config file at `~/.config/runtimo/config.toml`.
//! Allowed path prefixes are merged from three sources (lowest to highest priority):
//! 1. Built-in defaults (`/tmp`, `/var/tmp`)
//! 2. `RUNTIMO_ALLOWED_PATHS` env var (colon-separated)
//! 3. Config file `allowed_paths` array
//! 4. Context-specific prefixes (programmatic override)

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Built-in default allowed prefixes.
const DEFAULT_PREFIXES: &[&str] = &["/tmp", "/var/tmp"];

/// Output rendering configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[allow(clippy::exhaustive_structs)]
pub struct OutputConfig {
    /// Output format: `human` or `json`.
    #[serde(default)]
    pub format: Option<String>,
    /// Renderer: `markdown` or `plain`.
    #[serde(default)]
    pub renderer: Option<String>,
}

/// WAL configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[allow(clippy::exhaustive_structs)]
pub struct WalConfig {
    /// WAL mode: `always` or `batch`.
    #[serde(default)]
    pub mode: Option<String>,
    /// Whether WAL is enabled.
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// Backup configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[allow(clippy::exhaustive_structs)]
pub struct BackupConfig {
    /// Whether backup is enabled.
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// Guards configuration (DAL and defense toggles).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[allow(clippy::exhaustive_structs)]
pub struct GuardsConfig {
    /// Design Assurance Level for this profile.
    #[serde(default)]
    pub dal: Option<String>,
    /// Whether blocklist is enabled.
    #[serde(default)]
    pub blocklist_enabled: Option<bool>,
    /// Whether critical-files denylist is enabled.
    #[serde(default)]
    pub critical_files_enabled: Option<bool>,
    /// Whether path whitelist is enabled.
    #[serde(default)]
    pub path_restriction_enabled: Option<bool>,
    /// Whether PATH sanitization is enabled.
    #[serde(default)]
    pub path_sanitization_enabled: Option<bool>,
}

/// Session configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[allow(clippy::exhaustive_structs)]
pub struct SessionConfig {
    /// Maximum concurrent sessions.
    #[serde(default)]
    pub max_sessions: Option<u32>,
    /// Session timeout in seconds.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Behavior on limit: `continue` or `stop`.
    #[serde(default)]
    pub on_limit: Option<String>,
}

/// Telemetry configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[allow(clippy::exhaustive_structs)]
pub struct TelemetryConfig {
    /// Whether telemetry is enabled.
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// Observe (sampling) configuration.
///
/// Controls the `runtimo observe` sampling pipeline. Values are resolved
/// with precedence CLI > env(RUNTIMO_OBSERVE_SAMPLE_HZ) > file > profile > default.
///
/// # Fields
/// * `sample_rate_hz` — samples per second for the sampler (default 50)
/// * `pressure_suspend_ms` — how long to suspend sampling under high pressure (default 1000 ms)
/// * `pressure_suspend_ms` — how long to suspend sampling under high pressure (default 1000 ms)
/// * Q3 default sample Hz: nexus `config.rs` and docs contain no sampling rate;
///   provisional 50 Hz adopted (`ASSUMPTION: 50 Hz default — nexus silent, provisional`).
/// * Q4 bundle retention: nexus uses 90 days for store (`nexus-reference.md:193 NEXUS_STORE_RETENTION_DAYS=90`);
///   for observe bundles reuse 7 days provisional per task (`ASSUMPTION: 7d bundle retention — nexus store uses 90d, observe provisional 7d`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[allow(clippy::exhaustive_structs)]
pub struct ObserveConfig {
    /// Samples per second.
    #[serde(default)]
    pub sample_rate_hz: Option<u64>,
    /// Suspension window under pressure in milliseconds.
    #[serde(default)]
    pub pressure_suspend_ms: Option<u64>,
}

/// Resolved effective configuration after merging precedence.
///
/// Precedence (highest to lowest): CLI > env > file > profile > builtin.
/// Provisionals for Gate 1: bare DAL = E, no transient --profile flag, ephemeral WAL = batch.
#[derive(Debug, Clone)]
#[allow(clippy::exhaustive_structs, clippy::struct_excessive_bools)]
pub struct ResolvedConfig {
    /// Effective profile name.
    pub profile: String,
    /// Effective DAL (A-E).
    pub dal: String,
    /// Effective WAL mode.
    pub wal_mode: String,
    /// Whether backup is enabled.
    pub backup_enabled: bool,
    /// Effective output format.
    pub output_format: String,
    /// Effective output renderer.
    pub output_renderer: String,
    /// Whether blocklist is enabled.
    pub blocklist_enabled: bool,
    /// Whether critical-files denylist is enabled.
    pub critical_files_enabled: bool,
    /// Whether allowed-prefix path whitelist is enabled.
    pub path_restriction_enabled: bool,
    /// Whether ShellExec PATH sanitization is enabled.
    pub path_sanitization_enabled: bool,
    /// Effective max sessions.
    pub session_max: u32,
    /// Effective session timeout.
    pub session_timeout: u64,
    /// Effective on-limit behavior.
    pub session_on_limit: String,
    /// Whether telemetry is enabled.
    pub telemetry_enabled: bool,
    /// Effective observe sample rate in Hz (resolved via CLI > env > file > profile > default(50)).
    pub observe_sample_hz: u64,
    /// Effective pressure suspend window in milliseconds.
    pub observe_pressure_suspend_ms: u64,
}

/// Runtimo persistent configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[allow(clippy::exhaustive_structs)]
pub struct RuntimoConfig {
    /// Additional allowed path prefixes (merged with defaults + env var).
    #[serde(default)]
    pub allowed_paths: Vec<String>,

    /// Design Assurance Level (A-E) for the llmosafe cognitive pipeline.
    ///
    /// When set, used unless the `RUNTIMO_DAL` env var is set (the env var
    /// takes precedence). Case-insensitive — `get_dal()` uppercases it and
    /// controls how strictly
    /// the cognitive safety pipeline gates execution:
    /// - A: No override (strictest)
    /// - B: Halt → Escalate
    /// - C: Halt/Escalate → Warn
    /// - D: Halt/Escalate/Exit → Warn (cap at Warn)
    /// - E: All decisions → Proceed (permissive)
    #[serde(default)]
    pub dal: Option<String>,

    /// Additional dangerous command patterns for ShellExec blocklist.
    ///
    /// Each entry is a substring that triggers rejection. Merged with the
    /// built-in blocklist. Example: `["curl", "wget"]` blocks those commands
    /// even when `RUNTIMO_ENABLE_NETWORK=1`.
    #[serde(default)]
    pub blocklist_overrides: Vec<String>,

    /// Per-capability timeout defaults (seconds).
    ///
    /// Maps capability name to default timeout. Overrides the built-in
    /// defaults (30s for most, 300s for ShellExec). Individual executions
    /// can still override via `timeout_secs` in args.
    #[serde(default)]
    pub capability_timeouts: HashMap<String, u64>,

    /// Environment variables applied to capability child processes and
    /// consulted by Runtimo's runtime gates.
    ///
    /// These mirror the `RUNTIMO_*` opt-in env vars (`RUNTIMO_ENABLE_NETWORK`,
    /// `RUNTIMO_ENABLE_INTERPRETERS`, `RUNTIMO_ENABLE_PUBLIC_IP`) so opt-in
    /// features can be persisted in the config file instead of requiring a
    /// process-level env var or a PATH wrapper. Values are merged into
    /// ShellExec child environments (still subject to the sensitive-var
    /// stripping; `PATH` stays sanitized) and take precedence over the process
    /// environment for gates. GitExec is not merged — it inherits the process
    /// environment and its network access is gated by URL validation/SSRF
    /// blocking, not by these flags.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Enable the ShellExec dangerous-command blocklist (default: enabled).
    ///
    /// When `false`, `is_dangerous_command()` always returns `None` — ShellExec
    /// behaves like a plain `sh -c` with no command filtering. The blocklist
    /// catches `rm`, `shred`, `mkfs`, fork bombs, env dumpers, etc.; disabling
    /// it surrenders that defense deliberately. Network and interpreter
    /// gating are separate and unaffected by this flag.
    #[serde(default)]
    pub blocklist_enabled: Option<bool>,

    /// Enable the critical-files denylist in FileWrite/Delete (default: enabled).
    ///
    /// When `false`, files in the `CRITICAL_FILES` denylist (`.bashrc`,
    /// `.env`, `.ssh/*`, etc.) can be written and deleted without the
    /// critical-file rejection.
    #[serde(default)]
    pub critical_files_enabled: Option<bool>,

    /// Enable the allowed-prefix path whitelist (default: enabled).
    ///
    /// When `false`, `validate_path()` skips the allowed-prefix check, so
    /// FileRead/FileWrite/Delete can operate on any path. This is the
    /// strongest opt-out — it removes the "only /tmp, /var/tmp and
    /// configured prefixes" constraint entirely.
    #[serde(default)]
    pub path_restriction_enabled: Option<bool>,

    /// Force `PATH=/usr/local/bin:/usr/bin:/bin` on ShellExec children
    /// (default: enabled).
    ///
    /// When `false`, child processes inherit the caller's `PATH`, allowing
    /// custom binaries outside the sanitized trio to resolve.
    #[serde(default)]
    pub path_sanitization_enabled: Option<bool>,

    /// Profile name: `minimal`, `ephemeral`, or `service`.
    #[serde(default)]
    pub profile: Option<String>,

    /// Output rendering table.
    #[serde(default)]
    pub output: OutputConfig,

    /// WAL table.
    #[serde(default)]
    pub wal: WalConfig,

    /// Backup table.
    #[serde(default)]
    pub backup: BackupConfig,

    /// Guards table.
    #[serde(default)]
    pub guards: GuardsConfig,

    /// Session table.
    #[serde(default)]
    pub session: SessionConfig,

    /// Telemetry table.
    #[serde(default)]
    pub telemetry: TelemetryConfig,

    /// Observe (sampling) table.
    #[serde(default)]
    pub observe: ObserveConfig,
}

impl RuntimoConfig {
    /// Known top-level config keys. Any other top-level key is a config
    /// error — it would otherwise be silently ignored by serde. Surfacing
    /// it catches typos like a bare `enable_network = true` instead of
    /// `[env] RUNTIMO_ENABLE_NETWORK = "1"`.
    const KNOWN_TOP_LEVEL_KEYS: &'static [&'static str] = &[
        "allowed_paths",
        "dal",
        "blocklist_overrides",
        "capability_timeouts",
        "env",
        "blocklist_enabled",
        "critical_files_enabled",
        "path_restriction_enabled",
        "path_sanitization_enabled",
        "profile",
        "output",
        "wal",
        "backup",
        "guards",
        "session",
        "telemetry",
        "observe",
    ];

    /// Returns the config file path following XDG spec.
    ///
    /// Uses `XDG_CONFIG_HOME` if set, otherwise `~/.config/runtimo/config.toml`.
    ///
    /// Falls back to `/tmp/runtimo/config.toml` with a stderr warning when
    /// neither `XDG_CONFIG_HOME` nor `HOME` is set. Configuration in `/tmp`
    /// is not persistent across reboots.
    pub fn config_path() -> PathBuf {
        // Per the XDG Base Directory spec, a non-absolute XDG_CONFIG_HOME is
        // invalid and must be ignored (fall through to HOME / /tmp) so the
        // config location never varies with CWD.
        let base = std::env::var("XDG_CONFIG_HOME")
            .ok()
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| PathBuf::from(h).join(".config"))
            });
        if let Some(dir) = base {
            dir.join("runtimo/config.toml")
        } else {
            eprintln!(
                "[runtimo] Warning: XDG_CONFIG_HOME and HOME unset — using /tmp/runtimo \
                 (config will not survive reboot)"
            );
            PathBuf::from("/tmp/runtimo/config.toml")
        }
    }

    /// Returns a commented TOML template for the given profile.
    ///
    /// Profiles:
    /// - `minimal`: passthrough - no overrides, builtin defaults.
    /// - `ephemeral`: json/plain, backup=false, wal batch, DAL=E, blocklist off, max 20/300 continue.
    /// - `service`: human/markdown, backup=true, wal always, DAL=A strict, max 100/3600 stop.
    #[must_use]
    pub fn init_template(profile: Option<&str>) -> String {
        let p = profile.unwrap_or("minimal").to_lowercase();
        match p.as_str() {
            "ephemeral" => r#"# Runtimo config - profile: ephemeral
# Optimized for short-lived, throwaway environments (notebooks, CI).
# Guards are relaxed - not for service machines.
profile = "ephemeral"

# ephemeral profile - json/plain, WAL batch, guards off

[output]
# json/plain - machine-readable, no decoration
format = "json"
renderer = "plain"

[wal]
# batch - fewer fsyncs for ephemeral speed
mode = "batch"
enabled = true

[backup]
# no backup for ephemeral - speed over safety
enabled = false

[guards]
# DAL=E permissive, blocklist off for ephemeral
dal = "E"
blocklist_enabled = false

[session]
max_sessions = 20
timeout_secs = 300
on_limit = "continue"

[telemetry]
enabled = false
"#
            .to_string(),
            "service" => r#"# Runtimo config - profile: service
# Optimized for long-running service machines.
# Strict guards - do not disable.
profile = "service"

[output]
# human/markdown - readable service logs
format = "human"
renderer = "markdown"

[wal]
# always - fsync every event for durability
mode = "always"
enabled = true

[backup]
enabled = true

[guards]
# DAL=A strict, blocklist on for service
dal = "A"
blocklist_enabled = true
critical_files_enabled = true
path_restriction_enabled = true

[session]
max_sessions = 100
timeout_secs = 3600
on_limit = "stop"

[telemetry]
enabled = true
"#
            .to_string(),
            _ => r#"# Runtimo config - profile: minimal
# Minimal passthrough - no overrides, use builtins.
# Uncomment to customize.
profile = "minimal"

# allowed_paths = ["/srv", "/opt"]
# dal = "A"

# [output]
# format = "human"
# renderer = "markdown"

# [wal]
# mode = "always"
# enabled = true

# [backup]
# enabled = true

# [guards]
# dal = "A"
# blocklist_enabled = true

# [session]
# max_sessions = 100
# timeout_secs = 3600
# on_limit = "stop"

# [telemetry]
# enabled = true
"#
            .to_string(),
        }
    }

    /// Writes a profile template to `target` path.
    ///
    /// Respects XDG config path when `target` is `None`. Errors if the file
    /// exists and `force` is false. Creates parent directories, writes the
    /// template, validates via parse round-trip, and removes the file on parse
    /// failure.
    ///
    /// Misapplication guard: if `profile` is `ephemeral` and target is outside
    /// `/tmp`, `/var/tmp`, `/kaggle/working`, or `$XDG_CONFIG_HOME`, emits a
    /// stderr warning but still allows the write.
    ///
    /// # Errors
    ///
    /// Returns an error if the target already exists and `force` is false,
    /// if parent directories cannot be created, if the file cannot be written,
    /// or if the generated config fails to parse.
    pub fn init_at(
        target: Option<&Path>,
        profile: Option<&str>,
        force: bool,
    ) -> Result<PathBuf, String> {
        let path = target.map_or_else(Self::config_path, PathBuf::from);

        // Misapplication guard - ephemeral outside expected dirs
        if profile.is_some_and(|p| p.to_lowercase() == "ephemeral") {
            let path_str = path.to_string_lossy();
            let xdg_home = std::env::var("XDG_CONFIG_HOME").unwrap_or_default();
            let in_tmp = path_str.starts_with("/tmp/") || path_str == "/tmp";
            let in_var_tmp = path_str.starts_with("/var/tmp/");
            let in_kaggle = path_str.starts_with("/kaggle/working/");
            let in_xdg = !xdg_home.is_empty() && path_str.starts_with(xdg_home.as_str());
            let in_home_config = path_str.contains(".config/runtimo");
            if !(in_tmp || in_var_tmp || in_kaggle || in_xdg || in_home_config) {
                eprintln!(
                    "[runtimo] WARNING: ephemeral profile target '{}' is outside /tmp, /var/tmp, /kaggle/working, or $XDG_CONFIG_HOME - ephemeral is for throwaway environments",
                    path.display()
                );
            }
        }

        if path.exists() && !force {
            return Err(format!(
                "config already exists at {} - use --force to overwrite",
                path.display()
            ));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let content = Self::init_template(profile);
        std::fs::write(&path, &content).map_err(|e| e.to_string())?;

        // Validate via round-trip parse
        let written = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let value: toml::Value = toml::from_str(&written).map_err(|e| {
            let _ = std::fs::remove_file(&path);
            format!("generated config failed to parse: {}", e)
        })?;
        Self::deserialize(value).map_err(|e| {
            let _ = std::fs::remove_file(&path);
            format!("generated config failed to validate: {}", e)
        })?;

        // Also verify load_result when target is the default config_path
        if target.is_none() || target == Some(Self::config_path().as_path()) {
            if let Err(e) = Self::load_result() {
                let _ = std::fs::remove_file(&path);
                return Err(format!("generated config failed load_result: {}", e));
            }
        }

        Ok(path)
    }

    /// Returns the resolved effective configuration.
    ///
    /// Precedence (highest to lowest): CLI > env > file > profile > builtin.
    /// For Gate 1 provisionals: CLI is not yet wired (no transient --profile flag),
    /// so env is the highest effective source. Bare DAL defaults to E, and
    /// ephemeral WAL defaults to batch.
    #[must_use]
    pub fn resolved(&self) -> ResolvedConfig {
        // Profile determination: file profile > builtin minimal
        let profile = self
            .profile
            .clone()
            .unwrap_or_else(|| "minimal".to_string())
            .to_lowercase();
        let profile = match profile.as_str() {
            "ephemeral" | "service" | "minimal" => profile,
            _ => "minimal".to_string(),
        };

        // DAL: env > file dal > file guards.dal > profile > builtin(E)
        let dal = if let Ok(v) = std::env::var("RUNTIMO_DAL") {
            v.to_uppercase()
        } else if let Some(d) = &self.dal {
            d.to_uppercase()
        } else if let Some(d) = &self.guards.dal {
            d.to_uppercase()
        } else if profile == "service" {
            "A".to_string()
        } else {
            "E".to_string()
        };

        // WAL mode: file wal.mode > profile > builtin
        let wal_mode = if let Some(m) = &self.wal.mode {
            m.clone()
        } else if profile == "ephemeral" {
            "batch".to_string()
        } else {
            "always".to_string()
        };

        // Backup: file backup.enabled > profile > builtin(true)
        let backup_enabled = if let Some(b) = self.backup.enabled {
            b
        } else {
            profile != "ephemeral"
        };
        // Output: file output > profile > builtin - ephemeral is json, others human
        let output_format = if let Some(f) = &self.output.format {
            f.clone()
        } else if profile == "ephemeral" {
            "json".to_string()
        } else {
            "human".to_string()
        };
        let output_renderer = if let Some(r) = &self.output.renderer {
            r.clone()
        } else if profile == "ephemeral" {
            "plain".to_string()
        } else {
            "markdown".to_string()
        };

        // Blocklist: top-level > guards > profile > builtin(true)
        let blocklist_enabled = if let Some(b) = self.blocklist_enabled {
            b
        } else if let Some(b) = self.guards.blocklist_enabled {
            b
        } else {
            profile != "ephemeral"
        };

        // Critical-files denylist: top-level > guards > profile > builtin(true)
        let critical_files_enabled = if let Some(b) = self.critical_files_enabled {
            b
        } else if let Some(b) = self.guards.critical_files_enabled {
            b
        } else {
            profile != "ephemeral"
        };

        // Path whitelist: top-level > guards > profile > builtin(true)
        let path_restriction_enabled = if let Some(b) = self.path_restriction_enabled {
            b
        } else if let Some(b) = self.guards.path_restriction_enabled {
            b
        } else {
            profile != "ephemeral"
        };

        // PATH sanitization: top-level > guards > profile > builtin(true)
        let path_sanitization_enabled = if let Some(b) = self.path_sanitization_enabled {
            b
        } else if let Some(b) = self.guards.path_sanitization_enabled {
            b
        } else {
            profile != "ephemeral"
        };

        // Session: file session > profile > builtin
        let session_max = if let Some(m) = self.session.max_sessions {
            m
        } else if profile == "ephemeral" {
            20
        } else {
            100
        };
        let session_timeout = if let Some(t) = self.session.timeout_secs {
            t
        } else if profile == "ephemeral" {
            300
        } else {
            3600
        };
        let session_on_limit = if let Some(o) = &self.session.on_limit {
            o.clone()
        } else if profile == "ephemeral" {
            "continue".to_string()
        } else {
            "stop".to_string()
        };

        let telemetry_enabled = if let Some(e) = self.telemetry.enabled {
            e
        } else {
            profile != "ephemeral"
        };

        // Observe: env(RUNTIMO_OBSERVE_SAMPLE_HZ) > file observe.sample_rate_hz > profile > builtin(50)
        // ASSUMPTION: 50 Hz default — nexus silent, provisional per task Q3.
        // Mirrors effective_observe_sample_hz: malformed env falls through to file, not unwrap_or(50).
        let observe_sample_hz = if let Ok(v) = std::env::var("RUNTIMO_OBSERVE_SAMPLE_HZ") {
            if let Ok(n) = v.parse::<u64>() {
                n
            } else {
                self.observe.sample_rate_hz.unwrap_or(50)
            }
        } else {
            self.observe.sample_rate_hz.unwrap_or(50)
        };
        let observe_pressure_suspend_ms = self.observe.pressure_suspend_ms.unwrap_or(1000);

        ResolvedConfig {
            profile,
            dal,
            wal_mode,
            backup_enabled,
            output_format,
            output_renderer,
            blocklist_enabled,
            critical_files_enabled,
            path_restriction_enabled,
            path_sanitization_enabled,
            session_max,
            session_timeout,
            session_on_limit,
            telemetry_enabled,
            observe_sample_hz,
            observe_pressure_suspend_ms,
        }
    }

    /// Convenience: load file and return resolved config.
    #[must_use]
    pub fn resolved_from_file() -> ResolvedConfig {
        Self::load().resolved()
    }

    /// Returns true if guards are considered off via the resolved profile.
    #[must_use]
    pub fn guards_off_via_profile(resolved: &ResolvedConfig) -> bool {
        !resolved.blocklist_enabled || resolved.dal == "E"
    }

    /// Returns the effective observe sample rate, honoring CLI precedence.
    ///
    /// Precedence: CLI override > env(RUNTIMO_OBSERVE_SAMPLE_HZ) > file > default(50).
    ///
    /// # Parameters
    /// * `cli_override` — value from CLI flag `--observe-sample-hz`, if provided.
    #[must_use]
    pub fn effective_observe_sample_hz(&self, cli_override: Option<u64>) -> u64 {
        if let Some(v) = cli_override {
            return v;
        }
        if let Ok(v) = std::env::var("RUNTIMO_OBSERVE_SAMPLE_HZ") {
            if let Ok(n) = v.parse::<u64>() {
                return n;
            }
        }
        if let Some(v) = self.observe.sample_rate_hz {
            return v;
        }
        50
    }

    /// Static helper: effective sample hz with CLI, env, file, profile, default.
    ///
    /// Convenience that loads config from disk then applies [`Self::effective_observe_sample_hz`].
    #[must_use]
    pub fn observe_sample_hz_with_cli(cli_override: Option<u64>) -> u64 {
        Self::load().effective_observe_sample_hz(cli_override)
    }

    /// Loads config from disk, returning defaults if the file doesn't exist or is invalid.
    ///
    /// Logs a warning to stderr when the file exists but cannot be read or parsed.
    /// Prefer [`Self::load_result`] for new code — it propagates errors so callers can
    /// distinguish "file doesn't exist" from "file is corrupt."
    #[must_use]
    pub fn load() -> Self {
        match Self::load_result() {
            Ok(config) => config,
            Err(e) => {
                eprintln!("[runtimo] Config load failed (using defaults): {}", e);
                Self::default()
            }
        }
    }

    /// Loads config from disk, propagating read and parse errors.
    ///
    /// # Input
    ///
    /// Reads from the path returned by [`Self::config_path`] if it exists.
    ///
    /// # Output
    ///
    /// `Ok(RuntimoConfig)` — Successfully deserialized config, or default if file doesn't exist.
    ///
    /// # Errors
    ///
    /// Returns `Err(String)` when the config file:
    /// - Exists but cannot be opened (permission denied, filesystem error)
    /// - Can be opened but contains invalid TOML syntax
    /// - Contains TOML that deserializes to a different type (schema mismatch)
    ///
    /// Returns `Ok(Self::default())` when:
    /// - The config file does not exist (first run / clean install)
    /// - The config file is empty (no config needed)
    pub fn load_result() -> Result<Self, String> {
        let path = Self::config_path();
        if path.exists() {
            let content = std::fs::read_to_string(&path)
                .map_err(|e| format!("Cannot read config file '{}': {}", path.display(), e))?;
            let value: toml::Value = toml::from_str(&content)
                .map_err(|e| format!("Cannot parse config file '{}': {}", path.display(), e))?;
            Self::warn_unknown_keys(&value, &path);
            Self::deserialize(value)
                .map_err(|e| format!("Cannot parse config file '{}': {}", path.display(), e))
        } else {
            Ok(Self::default())
        }
    }

    /// Prints a warning to stderr for any top-level config key that is not
    /// recognized. Unknown keys are silently ignored by serde — surfacing
    /// them catches typos before they cause silent behavior changes.
    fn warn_unknown_keys(value: &toml::Value, path: &std::path::Path) {
        if let toml::Value::Table(table) = value {
            for key in table.keys() {
                if !Self::KNOWN_TOP_LEVEL_KEYS.contains(&key.as_str()) {
                    eprintln!(
                        "[runtimo] Warning: unknown config key `{}` in {} (ignored)",
                        key,
                        path.display()
                    );
                }
            }
        }
    }

    /// Saves config to disk atomically, creating parent directories as needed.
    ///
    /// Uses a temp-file + `fsync` + `rename` pattern in the same directory
    /// so a crash or disk-full mid-write never leaves a truncated
    /// `config.toml`. On failure the original file is left byte-identical
    /// (either untouched or replaced only after the temp is fully synced).
    /// The parent directory is `fsync`ed after rename where possible to
    /// persist the directory entry. Mirrors the atomic pattern in
    /// `capabilities::file_write::atomic_write`.
    ///
    /// # Errors
    ///
    /// Returns an error if parent directories cannot be created, if the
    /// config cannot be serialized, or if temp creation / write / fsync /
    /// rename fails.
    pub fn save(&self) -> Result<(), String> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let content = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        // Temp file in the same directory so rename is atomic (same filesystem).
        // Pattern: .{filename}.tmp — matches file_write::atomic_write.
        let tmp_name = format!(
            ".{}.tmp",
            path.file_name().map_or_else(
                || "config.toml".to_string(),
                |n| n.to_string_lossy().into_owned()
            )
        );
        let tmp_path = path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join(&tmp_name);
        {
            use std::io::Write;
            let mut file = std::fs::File::create(&tmp_path).map_err(|e| e.to_string())?;
            file.write_all(content.as_bytes())
                .map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
        }
        std::fs::rename(&tmp_path, &path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp_path);
            e.to_string()
        })?;
        // Best-effort directory fsync to persist the rename.
        if let Some(parent) = path.parent() {
            if let Ok(dir) = std::fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        Ok(())
    }

    /// Returns the resolved Design Assurance Level.
    ///
    /// Priority (highest to lowest):
    /// 1. `RUNTIMO_DAL` env var (case-insensitive, uppercased)
    /// 2. Config file `dal` field (top-level `dal`)
    /// 3. Config file `[guards].dal` field
    /// 4. Profile default (`service` ⇒ `A`, otherwise `E`)
    /// 5. Built-in default `E` (permissive, bare install)
    ///
    /// Mirrors [`Self::resolved`] DAL precedence so the live gate
    /// ([`crate::llmosafe::LlmoSafeGuard`]) and the reported
    /// [`ResolvedConfig::dal`] agree for every state (empty, env,
    /// file, profile). The env var and config-file branches both
    /// uppercase the value, so `dal = "b"` and `RUNTIMO_DAL=b`
    /// resolve identically to `B`. Unknown values are uppercased
    /// and returned as-is; the DAL mapper in `llmosafe` falls back
    /// to `A` for unknown strings.
    ///
    /// # Side effects
    /// Reads `RUNTIMO_DAL` from the process environment and loads
    /// the config file from disk via [`Self::load`].
    #[must_use]
    pub fn get_dal() -> String {
        // Env var takes precedence (highest priority, mirrors resolved())
        if let Ok(env_dal) = std::env::var("RUNTIMO_DAL") {
            return env_dal.to_uppercase();
        }
        // Config file — mirror resolved() precedence: top-level dal > guards.dal > profile > builtin(E)
        let config = Self::load();
        if let Some(d) = config.dal {
            return d.to_uppercase();
        }
        if let Some(d) = config.guards.dal {
            return d.to_uppercase();
        }
        // Profile determination mirrors resolved(): file profile > builtin minimal, lowercased and validated
        let profile = config
            .profile
            .clone()
            .unwrap_or_else(|| "minimal".to_string())
            .to_lowercase();
        let profile = match profile.as_str() {
            "ephemeral" | "service" | "minimal" => profile,
            _ => "minimal".to_string(),
        };
        if profile == "service" {
            "A".to_string()
        } else {
            "E".to_string()
        }
    }

    /// Resolves an environment variable, preferring the config `[env]` table
    /// over the process environment.
    ///
    /// This lets `RUNTIMO_*` opt-in flags be persisted in `config.toml`
    /// (e.g. `[env] RUNTIMO_ENABLE_INTERPRETERS = "1"`) instead of requiring
    /// a process-level env var or a PATH wrapper.
    #[must_use]
    pub fn env_var(name: &str) -> Option<String> {
        let config = Self::load();
        if let Some(value) = config.env.get(name) {
            return Some(value.clone());
        }
        std::env::var(name).ok()
    }

    /// Returns the config `[env]` table (empty when unconfigured).
    ///
    /// Used to merge persisted env vars into capability child processes.
    #[must_use]
    pub fn env_map() -> HashMap<String, String> {
        Self::load().env
    }

    /// Returns the merged blocklist overrides from config.
    ///
    /// These are additional substrings that trigger rejection in ShellExec,
    /// merged on top of the built-in blocklist.
    #[must_use]
    pub fn get_blocklist_overrides() -> Vec<String> {
        let config = Self::load();
        config.blocklist_overrides
    }

    /// Whether the ShellExec dangerous-command blocklist is enabled.
    ///
    /// Single source of truth: delegates to `Self::load().resolved().blocklist_enabled`
    /// with precedence `blocklist_enabled` (top-level) > `[guards].blocklist_enabled` >
    /// `profile != "ephemeral"` > builtin `true`. The `ephemeral` profile disables
    /// the blocklist by default (resolves to `false`); `minimal` and `service`
    /// resolve to `true` when unconfigured.
    ///
    /// # Side effects
    /// Reads the config file from disk via [`Self::load`] (stderr warning on parse
    /// failure, falls back to defaults).
    ///
    /// # Returns
    /// `true` when the blocklist is active; `false` when disabled via config or
    /// ephemeral profile.
    #[must_use]
    pub fn blocklist_enabled() -> bool {
        Self::load().resolved().blocklist_enabled
    }

    /// Whether the FileWrite/Delete critical-files denylist is enabled.
    ///
    /// Single source of truth: delegates to `Self::load().resolved().critical_files_enabled`
    /// with precedence `critical_files_enabled` (top-level) > `[guards].critical_files_enabled` >
    /// `profile != "ephemeral"` > builtin `true`. The `ephemeral` profile disables
    /// the denylist by default (resolves to `false`); `minimal` and `service`
    /// resolve to `true` when unconfigured.
    ///
    /// # Side effects
    /// Reads the config file from disk via [`Self::load`].
    ///
    /// # Returns
    /// `true` when the critical-files denylist is active; `false` when disabled.
    #[must_use]
    pub fn critical_files_enabled() -> bool {
        Self::load().resolved().critical_files_enabled
    }

    /// Whether the allowed-prefix path whitelist is enabled.
    ///
    /// Single source of truth: delegates to `Self::load().resolved().path_restriction_enabled`
    /// with precedence `path_restriction_enabled` (top-level) > `[guards].path_restriction_enabled` >
    /// `profile != "ephemeral"` > builtin `true`. The `ephemeral` profile disables
    /// the whitelist by default (resolves to `false`); `minimal` and `service`
    /// resolve to `true` when unconfigured. When `false`, `validate_path()` skips
    /// the allowed-prefix check.
    ///
    /// # Side effects
    /// Reads the config file from disk via [`Self::load`].
    ///
    /// # Returns
    /// `true` when path restriction is active; `false` when disabled.
    #[must_use]
    pub fn path_restriction_enabled() -> bool {
        Self::load().resolved().path_restriction_enabled
    }

    /// Whether ShellExec children get a forced `PATH` (default: enabled).
    ///
    /// Single source of truth: delegates to `Self::load().resolved().path_sanitization_enabled`
    /// with precedence `path_sanitization_enabled` (top-level) > `[guards].path_sanitization_enabled` >
    /// `profile != "ephemeral"` > builtin `true`. The `ephemeral` profile disables
    /// sanitization by default (resolves to `false`); `minimal` and `service`
    /// resolve to `true` when unconfigured. When `false`, children inherit the
    /// caller's `PATH`, resolving custom binaries outside `/usr/local/bin:/usr/bin:/bin`.
    ///
    /// # Side effects
    /// Reads the config file from disk via [`Self::load`].
    ///
    /// # Returns
    /// `true` when PATH sanitization is active; `false` when disabled.
    #[must_use]
    pub fn path_sanitization_enabled() -> bool {
        Self::load().resolved().path_sanitization_enabled
    }

    /// Returns the default timeout for a capability, or the fallback.
    ///
    /// Checks config file `capability_timeouts` map, falls back to the
    /// provided default if no override is configured.
    #[must_use]
    pub fn get_capability_timeout(capability: &str, fallback: u64) -> u64 {
        let config = Self::load();
        config
            .capability_timeouts
            .get(capability)
            .copied()
            .unwrap_or(fallback)
    }

    /// Returns merged prefixes: defaults + env var + config file.
    ///
    /// Priority (lowest to highest):
    /// 1. Built-in defaults
    /// 2. `RUNTIMO_ALLOWED_PATHS` env var
    /// 3. Config file `allowed_paths`
    ///
    /// Empty strings are filtered out to prevent matching everything
    /// via `format!("{}/", "")` which produces `"/"` (N-014).
    /// Trailing slashes are stripped except for the root prefix `/`,
    /// so `/tmp/` and `/tmp` are equivalent and `/` is preserved
    /// (never stripped to `""`). This avoids the double-slash
    /// `"/tmp//"` compound that would fail `path_in_prefix`.
    #[must_use]
    pub fn get_allowed_prefixes() -> Vec<String> {
        let mut prefixes: Vec<String> = DEFAULT_PREFIXES.iter().map(|s| s.to_string()).collect();

        // Env var (colon-separated) — normalize trailing slash except root
        if let Ok(env_paths) = std::env::var("RUNTIMO_ALLOWED_PATHS") {
            for p in env_paths.split(':').filter(|s| !s.is_empty()) {
                let trimmed = p.trim().to_string();
                if trimmed.is_empty() {
                    continue;
                }
                let mut normalized = trimmed.trim_end_matches('/').to_string();
                if normalized.is_empty() {
                    normalized = "/".to_string();
                }
                if !prefixes.contains(&normalized) {
                    prefixes.push(normalized);
                }
            }
        }

        // Config file — same normalization
        let config = Self::load();
        for p in &config.allowed_paths {
            let trimmed = p.trim().to_string();
            if trimmed.is_empty() {
                continue;
            }
            let mut normalized = trimmed.trim_end_matches('/').to_string();
            if normalized.is_empty() {
                normalized = "/".to_string();
            }
            if !prefixes.contains(&normalized) {
                prefixes.push(normalized);
            }
        }

        prefixes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Mutex to serialize config tests that set XDG_CONFIG_HOME.
    /// Without this, concurrent tests fight over the process-global env var.
    static CONFIG_TEST_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn config_path_is_absolute() {
        let path = RuntimoConfig::config_path();
        assert!(path.is_absolute());
    }

    #[test]
    fn config_path_rejects_relative_xdg() {
        let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
        // A relative XDG_CONFIG_HOME is invalid per the XDG spec and must be
        // ignored so the config location never varies with CWD.
        std::env::set_var("XDG_CONFIG_HOME", "relative/path");
        let path = RuntimoConfig::config_path();
        assert!(
            path.is_absolute(),
            "relative XDG_CONFIG_HOME must fall through, got {:?}",
            path
        );
        assert!(
            !path.to_string_lossy().contains("relative/path"),
            "relative base must not leak into {:?}",
            path
        );
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn load_returns_defaults_when_no_file() {
        let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
        let tmp = std::env::temp_dir().join("runtimo_test_config_defaults");
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("XDG_CONFIG_HOME", &tmp);

        let config = RuntimoConfig::load();
        assert!(config.allowed_paths.is_empty());

        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn get_allowed_prefixes_includes_defaults_excludes_home() {
        let prefixes = RuntimoConfig::get_allowed_prefixes();
        assert!(prefixes.iter().any(|p| p == "/tmp"));
        assert!(prefixes.iter().any(|p| p == "/var/tmp"));
        assert!(!prefixes.iter().any(|p| p == "/home"));
    }

    #[test]
    fn save_and_load_roundtrip() {
        let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
        // Use a temp config path for this test
        let tmp = std::env::temp_dir().join("runtimo_test_config");
        std::env::set_var("XDG_CONFIG_HOME", &tmp);

        let mut config = RuntimoConfig::default();
        config.allowed_paths.push("/srv".to_string());
        config.allowed_paths.push("/opt".to_string());
        config.save().expect("save failed");

        let loaded = RuntimoConfig::load();
        assert_eq!(loaded.allowed_paths, vec!["/srv", "/opt"]);

        let prefixes = RuntimoConfig::get_allowed_prefixes();
        assert!(prefixes.contains(&"/srv".to_string()));
        assert!(prefixes.contains(&"/opt".to_string()));

        // Cleanup
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn test_toml_parse_failure_returns_defaults() {
        let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
        // GAP 12: Corrupt TOML file returns defaults, not panic
        let tmp = std::env::temp_dir().join("runtimo_test_config_corrupt");
        let config_dir = tmp.join("runtimo");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.toml");

        // Write corrupt TOML
        std::fs::write(&config_path, "this is {{{ not valid toml at all!!!").unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &tmp);

        let config = RuntimoConfig::load();
        // Must return defaults, not panic
        assert!(
            config.allowed_paths.is_empty(),
            "Corrupt TOML should return defaults"
        );

        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn test_empty_config_file_returns_defaults() {
        let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
        // GAP 12: Empty config file returns defaults
        let tmp = std::env::temp_dir().join("runtimo_test_config_empty");
        let config_dir = tmp.join("runtimo");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.toml");

        // Write empty file
        std::fs::write(&config_path, "").unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &tmp);

        let config = RuntimoConfig::load();
        assert!(
            config.allowed_paths.is_empty(),
            "Empty config should return defaults"
        );

        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn test_toml_missing_section_returns_defaults() {
        let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
        // GAP 12: Valid TOML but missing expected section
        let tmp = std::env::temp_dir().join("runtimo_test_config_missing");
        let config_dir = tmp.join("runtimo");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.toml");

        // Valid TOML but no allowed_paths array
        std::fs::write(&config_path, "[other_section]\nfoo = \"bar\"\n").unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &tmp);

        let config = RuntimoConfig::load();
        assert!(
            config.allowed_paths.is_empty(),
            "Missing section should return defaults"
        );

        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn env_var_prefers_config_env_over_process_env() {
        let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
        let tmp = std::env::temp_dir().join("runtimo_test_config_env");
        let config_dir = tmp.join("runtimo");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.toml");

        std::fs::write(&config_path, "[env]\nRUNTIMO_ENABLE_NETWORK = \"1\"\n").unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &tmp);
        std::env::set_var("RUNTIMO_ENABLE_NETWORK", "0");

        assert_eq!(
            RuntimoConfig::env_var("RUNTIMO_ENABLE_NETWORK").as_deref(),
            Some("1"),
            "config [env] must take precedence over process env"
        );

        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::remove_var("RUNTIMO_ENABLE_NETWORK");
    }

    #[test]
    fn env_map_returns_config_table() {
        let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
        let tmp = std::env::temp_dir().join("runtimo_test_config_env_map");
        let config_dir = tmp.join("runtimo");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.toml");

        std::fs::write(&config_path, "[env]\nRUNTIMO_ENABLE_INTERPRETERS = \"1\"\n").unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &tmp);

        let map = RuntimoConfig::env_map();
        assert_eq!(
            map.get("RUNTIMO_ENABLE_INTERPRETERS").map(String::as_str),
            Some("1")
        );

        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn unknown_top_level_key_ignored_but_loads() {
        let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
        let tmp = std::env::temp_dir().join("runtimo_test_config_unknown_key");
        let config_dir = tmp.join("runtimo");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.toml");

        // A typo'd top-level key must not break loading (but warns to stderr).
        std::fs::write(
            &config_path,
            "enable_network = true\nallowed_paths = [\"/srv\"]\n",
        )
        .unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &tmp);

        let config = RuntimoConfig::load_result().expect("should load despite unknown key");
        assert_eq!(config.allowed_paths, vec!["/srv".to_string()]);

        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn capability_timeout_reads_config() {
        let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
        let tmp = std::env::temp_dir().join("runtimo_test_config_timeout");
        let config_dir = tmp.join("runtimo");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.toml");

        std::fs::write(&config_path, "[capability_timeouts]\nShellExec = 500\n").unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &tmp);

        assert_eq!(RuntimoConfig::get_capability_timeout("ShellExec", 30), 500);
        assert_eq!(RuntimoConfig::get_capability_timeout("FileRead", 30), 30);

        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn defense_toggles_default_enabled() {
        let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
        let tmp = std::env::temp_dir().join("runtimo_test_config_toggles");
        let config_dir = tmp.join("runtimo");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&config_dir).unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &tmp);

        assert!(RuntimoConfig::blocklist_enabled());
        assert!(RuntimoConfig::critical_files_enabled());
        assert!(RuntimoConfig::path_restriction_enabled());
        assert!(RuntimoConfig::path_sanitization_enabled());

        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn defense_toggles_can_be_disabled_via_config() {
        let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
        let tmp = std::env::temp_dir().join("runtimo_test_config_toggles_off");
        let config_dir = tmp.join("runtimo");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.toml");

        std::fs::write(
            &config_path,
            "blocklist_enabled = false\ncritical_files_enabled = false\npath_restriction_enabled = false\npath_sanitization_enabled = false\n",
        )
        .unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &tmp);

        assert!(!RuntimoConfig::blocklist_enabled());
        assert!(!RuntimoConfig::critical_files_enabled());
        assert!(!RuntimoConfig::path_restriction_enabled());
        assert!(!RuntimoConfig::path_sanitization_enabled());

        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn dal_config_file_value_is_case_insensitive() {
        let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
        let tmp = std::env::temp_dir().join("runtimo_test_config_dal_case");
        let config_dir = tmp.join("runtimo");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.toml");

        std::fs::write(&config_path, "dal = \"b\"\n").unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &tmp);

        assert_eq!(
            RuntimoConfig::get_dal(),
            "B",
            "config dal = \"b\" must resolve to uppercase B"
        );

        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn dal_env_var_value_is_case_insensitive() {
        std::env::set_var("RUNTIMO_DAL", "e");
        assert_eq!(
            RuntimoConfig::get_dal(),
            "E",
            "RUNTIMO_DAL=e must resolve to uppercase E"
        );
        std::env::remove_var("RUNTIMO_DAL");
    }

    // ── Gate 1: profile seam tests ─────────────────────────────────

    #[test]
    fn init_template_roundtrip_minimal() {
        let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
        let tmp = std::env::temp_dir().join("runtimo_test_init_minimal");
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("XDG_CONFIG_HOME", &tmp);
        let content = RuntimoConfig::init_template(Some("minimal"));
        let val: toml::Value = toml::from_str(&content).expect("minimal template must parse");
        let cfg = RuntimoConfig::deserialize(val).expect("minimal template must deserialize");
        assert_eq!(cfg.profile.as_deref(), Some("minimal"));
        let resolved = cfg.resolved();
        assert_eq!(resolved.profile, "minimal");
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn init_template_roundtrip_ephemeral() {
        let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
        let tmp = std::env::temp_dir().join("runtimo_test_init_ephemeral");
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("XDG_CONFIG_HOME", &tmp);
        let content = RuntimoConfig::init_template(Some("ephemeral"));
        let val: toml::Value = toml::from_str(&content).expect("ephemeral template must parse");
        let cfg = RuntimoConfig::deserialize(val).expect("ephemeral template must deserialize");
        assert_eq!(cfg.profile.as_deref(), Some("ephemeral"));
        assert_eq!(cfg.output.format.as_deref(), Some("json"));
        assert_eq!(cfg.output.renderer.as_deref(), Some("plain"));
        assert_eq!(cfg.backup.enabled, Some(false));
        assert_eq!(cfg.wal.mode.as_deref(), Some("batch"));
        assert_eq!(cfg.guards.dal.as_deref(), Some("E"));
        assert_eq!(cfg.guards.blocklist_enabled, Some(false));
        assert_eq!(cfg.session.max_sessions, Some(20));
        assert_eq!(cfg.session.timeout_secs, Some(300));
        assert_eq!(cfg.session.on_limit.as_deref(), Some("continue"));
        let resolved = cfg.resolved();
        assert_eq!(resolved.dal, "E");
        assert_eq!(resolved.wal_mode, "batch");
        assert!(!resolved.backup_enabled);
        assert_eq!(resolved.session_max, 20);
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn init_template_roundtrip_service() {
        let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
        let tmp = std::env::temp_dir().join("runtimo_test_init_service");
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("XDG_CONFIG_HOME", &tmp);
        let content = RuntimoConfig::init_template(Some("service"));
        let val: toml::Value = toml::from_str(&content).expect("service template must parse");
        let cfg = RuntimoConfig::deserialize(val).expect("service template must deserialize");
        assert_eq!(cfg.profile.as_deref(), Some("service"));
        assert_eq!(cfg.output.format.as_deref(), Some("human"));
        assert_eq!(cfg.output.renderer.as_deref(), Some("markdown"));
        assert_eq!(cfg.backup.enabled, Some(true));
        assert_eq!(cfg.wal.mode.as_deref(), Some("always"));
        assert_eq!(cfg.guards.dal.as_deref(), Some("A"));
        assert_eq!(cfg.guards.blocklist_enabled, Some(true));
        assert_eq!(cfg.session.max_sessions, Some(100));
        assert_eq!(cfg.session.timeout_secs, Some(3600));
        assert_eq!(cfg.session.on_limit.as_deref(), Some("stop"));
        let resolved = cfg.resolved();
        assert_eq!(resolved.dal, "A");
        assert_eq!(resolved.wal_mode, "always");
        assert!(resolved.backup_enabled);
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn init_at_exists_without_force_fails() {
        let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
        let tmp = std::env::temp_dir().join("runtimo_test_init_exists");
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("XDG_CONFIG_HOME", &tmp);
        let path = tmp.join("runtimo/config.toml");
        RuntimoConfig::init_at(None, Some("minimal"), false).expect("first init should succeed");
        assert!(path.exists());
        let err = RuntimoConfig::init_at(None, Some("minimal"), false).unwrap_err();
        assert!(
            err.contains("already exists") && err.contains("--force"),
            "error must mention already exists and --force, got: {}",
            err
        );
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn init_at_force_overwrites() {
        let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
        let tmp = std::env::temp_dir().join("runtimo_test_init_force");
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("XDG_CONFIG_HOME", &tmp);
        RuntimoConfig::init_at(None, Some("minimal"), false).expect("init minimal");
        RuntimoConfig::init_at(None, Some("ephemeral"), true).expect("force overwrite");
        let cfg = RuntimoConfig::load_result().expect("should load after force");
        assert_eq!(cfg.profile.as_deref(), Some("ephemeral"));
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn init_at_roundtrip_via_load_result() {
        let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
        let tmp = std::env::temp_dir().join("runtimo_test_init_roundtrip");
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("XDG_CONFIG_HOME", &tmp);
        for profile in ["minimal", "ephemeral", "service"] {
            let _ = std::fs::remove_dir_all(&tmp);
            std::fs::create_dir_all(tmp.join("runtimo")).ok();
            std::env::set_var("XDG_CONFIG_HOME", &tmp);
            RuntimoConfig::init_at(None, Some(profile), true).expect("init_at");
            let cfg = RuntimoConfig::load_result().expect("load_result after init_at");
            assert_eq!(cfg.profile.as_deref(), Some(profile));
        }
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn resolved_precedence_env_over_file_over_profile() {
        let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
        let tmp = std::env::temp_dir().join("runtimo_test_resolved_prec");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("runtimo")).unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &tmp);
        let content = "profile = \"ephemeral\"\ndal = \"C\"\n[guards]\ndal = \"C\"\n";
        std::fs::write(tmp.join("runtimo/config.toml"), content).unwrap();
        std::env::remove_var("RUNTIMO_DAL");
        let cfg = RuntimoConfig::load().resolved();
        assert_eq!(cfg.dal, "C", "file DAL should beat profile E");
        std::env::set_var("RUNTIMO_DAL", "B");
        let cfg2 = RuntimoConfig::load().resolved();
        assert_eq!(cfg2.dal, "B", "env DAL should beat file C");
        std::env::remove_var("RUNTIMO_DAL");
        std::fs::write(tmp.join("runtimo/config.toml"), "profile = \"ephemeral\"\n").unwrap();
        let cfg3 = RuntimoConfig::load().resolved();
        assert_eq!(cfg3.dal, "E", "profile ephemeral should give E");
        std::fs::write(tmp.join("runtimo/config.toml"), "").unwrap();
        let cfg4 = RuntimoConfig::load().resolved();
        assert_eq!(cfg4.dal, "E", "bare should be E per provisional");
        std::fs::write(
            tmp.join("runtimo/config.toml"),
            "profile = \"ephemeral\"\n[wal]\nmode = \"always\"\n",
        )
        .unwrap();
        let cfg5 = RuntimoConfig::load().resolved();
        assert_eq!(
            cfg5.wal_mode, "always",
            "file wal should beat profile batch"
        );
        std::fs::write(tmp.join("runtimo/config.toml"), "profile = \"ephemeral\"\n").unwrap();
        let cfg6 = RuntimoConfig::load().resolved();
        assert_eq!(cfg6.wal_mode, "batch", "ephemeral WAL must be batch");
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::remove_var("RUNTIMO_DAL");
    }

    #[test]
    fn resolved_cli_over_env_placeholder() {
        let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
        let tmp = std::env::temp_dir().join("runtimo_test_cli_prec");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("runtimo")).unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &tmp);
        std::fs::write(
            tmp.join("runtimo/config.toml"),
            "profile = \"service\"\n[guards]\ndal = \"A\"\n",
        )
        .unwrap();
        std::env::set_var("RUNTIMO_DAL", "E");
        let cfg = RuntimoConfig::load().resolved();
        assert_eq!(cfg.dal, "E");
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::remove_var("RUNTIMO_DAL");
    }

    #[test]
    fn resolved_malformed_env_falls_through_to_file() {
        let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
        let tmp = std::env::temp_dir().join("runtimo_test_malformed_env");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("runtimo")).unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &tmp);
        std::fs::write(
            tmp.join("runtimo/config.toml"),
            "[observe]\nsample_rate_hz = 77\n",
        )
        .unwrap();
        std::env::set_var("RUNTIMO_OBSERVE_SAMPLE_HZ", "not_a_number");
        let cfg = RuntimoConfig::load();
        // resolved() must fall through malformed env to file value 77, not 50
        let resolved = cfg.resolved();
        assert_eq!(
            resolved.observe_sample_hz, 77,
            "malformed env must fall through to file value 77, got {}",
            resolved.observe_sample_hz
        );
        // effective_observe_sample_hz must also fall through
        assert_eq!(
            cfg.effective_observe_sample_hz(None),
            77,
            "effective must also fall through malformed env to file"
        );
        // Verify with malformed env and no file → default 50
        std::fs::write(tmp.join("runtimo/config.toml"), "").unwrap();
        let cfg2 = RuntimoConfig::load();
        assert_eq!(cfg2.resolved().observe_sample_hz, 50);
        assert_eq!(cfg2.effective_observe_sample_hz(None), 50);
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::remove_var("RUNTIMO_OBSERVE_SAMPLE_HZ");
    }
    #[test]
    fn get_dal_agrees_with_resolved_for_all_states() {
        let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
        let tmp = std::env::temp_dir().join("runtimo_test_dal_agreement");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("runtimo")).unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &tmp);
        let cfg_path = tmp.join("runtimo/config.toml");
        std::env::remove_var("RUNTIMO_DAL");

        // State 1: bare install (no config file, no env) — both accessors E.
        std::fs::remove_file(&cfg_path).ok();
        assert_eq!(
            RuntimoConfig::get_dal(),
            RuntimoConfig::load().resolved().dal,
            "bare install must agree"
        );
        assert_eq!(
            RuntimoConfig::get_dal(),
            "E",
            "bare install resolves to provisioned E"
        );

        // State 2: top-level `dal` in the config file.
        std::fs::write(&cfg_path, "dal = \"C\"\n").unwrap();
        assert_eq!(
            RuntimoConfig::get_dal(),
            RuntimoConfig::load().resolved().dal,
            "file dal must agree"
        );
        assert_eq!(RuntimoConfig::get_dal(), "C");

        // State 3: `[guards].dal` only (no top-level dal).
        std::fs::write(&cfg_path, "[guards]\ndal = \"D\"\n").unwrap();
        assert_eq!(
            RuntimoConfig::get_dal(),
            RuntimoConfig::load().resolved().dal,
            "guards dal must agree"
        );
        assert_eq!(RuntimoConfig::get_dal(), "D");

        // State 4: top-level dal beats [guards].dal.
        std::fs::write(&cfg_path, "dal = \"B\"\n[guards]\ndal = \"D\"\n").unwrap();
        assert_eq!(
            RuntimoConfig::get_dal(),
            RuntimoConfig::load().resolved().dal,
            "top-level dal must beat guards"
        );
        assert_eq!(RuntimoConfig::get_dal(), "B");

        // State 5: profile service without dal — both A.
        std::fs::write(&cfg_path, "profile = \"service\"\n").unwrap();
        assert_eq!(
            RuntimoConfig::get_dal(),
            RuntimoConfig::load().resolved().dal,
            "service profile must agree"
        );
        assert_eq!(
            RuntimoConfig::get_dal(),
            "A",
            "service profile defaults to A"
        );

        // State 6: profile ephemeral without dal — both E (provisioned).
        std::fs::write(&cfg_path, "profile = \"ephemeral\"\n").unwrap();
        assert_eq!(
            RuntimoConfig::get_dal(),
            RuntimoConfig::load().resolved().dal,
            "ephemeral profile must agree"
        );
        assert_eq!(
            RuntimoConfig::get_dal(),
            "E",
            "ephemeral profile defaults to E"
        );

        // State 7: env var beats file and profile; both accessors agree.
        std::fs::write(&cfg_path, "dal = \"C\"\n[guards]\ndal = \"D\"\n").unwrap();
        std::env::set_var("RUNTIMO_DAL", "b");
        assert_eq!(
            RuntimoConfig::get_dal(),
            RuntimoConfig::load().resolved().dal,
            "env var must agree"
        );
        assert_eq!(RuntimoConfig::get_dal(), "B", "env var beats file");
        std::env::remove_var("RUNTIMO_DAL");

        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn get_allowed_prefixes_normalizes_config_file_entries() {
        let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
        let tmp = std::env::temp_dir().join("runtimo_test_prefix_norm");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("runtimo")).unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &tmp);
        let cfg_path = tmp.join("runtimo/config.toml");

        // Trailing slashes are stripped; root "/" survives normalization.
        std::fs::write(
            &cfg_path,
            "allowed_paths = [\"/runtimo_prefix_x/\", \"/runtimo_prefix_y\", \"/\"]\n",
        )
        .unwrap();

        let prefixes = RuntimoConfig::get_allowed_prefixes();
        assert!(
            prefixes.contains(&"/runtimo_prefix_x".to_string()),
            "trailing slash must be stripped: {prefixes:?}"
        );
        assert!(
            !prefixes.contains(&"/runtimo_prefix_x/".to_string()),
            "no slash-suffixed duplicate: {prefixes:?}"
        );
        assert!(
            prefixes.contains(&"/runtimo_prefix_y".to_string()),
            "plain entry preserved: {prefixes:?}"
        );
        assert!(
            prefixes.contains(&"/".to_string()),
            "root prefix must survive normalization: {prefixes:?}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn save_failure_leaves_original_byte_identical() {
        let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
        let tmp = std::env::temp_dir().join("runtimo_test_save_atomic");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("runtimo")).unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &tmp);
        let cfg_path = tmp.join("runtimo/config.toml");

        // Establish a known original via a successful save.
        let config = RuntimoConfig {
            profile: Some("minimal".to_string()),
            ..Default::default()
        };
        config.save().expect("initial save must succeed");
        let original = std::fs::read(&cfg_path).unwrap();

        // Force save() to fail at temp-file creation: .config.toml.tmp
        // exists as a directory, so File::create hits EISDIR. The rename —
        // the only operation that touches the original — can never run.
        std::fs::create_dir_all(cfg_path.with_file_name(".config.toml.tmp")).unwrap();

        let err = config
            .save()
            .expect_err("save must fail while .config.toml.tmp is a directory");
        assert!(!err.is_empty());

        // Original file is byte-identical after the failed save.
        assert_eq!(
            std::fs::read(&cfg_path).unwrap(),
            original,
            "original must be byte-identical after failed save"
        );

        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn guard_accessors_agree_with_resolved_when_guards_table_false() {
        let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
        let tmp = std::env::temp_dir().join("runtimo_test_guards_table_false");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("runtimo")).unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &tmp);
        let cfg_path = tmp.join("runtimo/config.toml");

        // Witness: [guards] false must disable via resolved and accessor.
        std::fs::write(
            &cfg_path,
            "[guards]\nblocklist_enabled = false\ncritical_files_enabled = false\npath_restriction_enabled = false\npath_sanitization_enabled = false\n",
        )
        .unwrap();

        let resolved = RuntimoConfig::load().resolved();
        assert!(
            !resolved.blocklist_enabled,
            "guards blocklist false → resolved false"
        );
        assert!(
            !resolved.critical_files_enabled,
            "guards critical_files false → resolved false"
        );
        assert!(
            !resolved.path_restriction_enabled,
            "guards path_restriction false → resolved false"
        );
        assert!(
            !resolved.path_sanitization_enabled,
            "guards path_sanitization false → resolved false"
        );

        // Single source: accessors must agree with resolved.
        assert_eq!(
            RuntimoConfig::blocklist_enabled(),
            resolved.blocklist_enabled,
            "blocklist accessor must agree with resolved (guards false)"
        );
        assert_eq!(
            RuntimoConfig::critical_files_enabled(),
            resolved.critical_files_enabled,
            "critical_files accessor must agree with resolved (guards false)"
        );
        assert_eq!(
            RuntimoConfig::path_restriction_enabled(),
            resolved.path_restriction_enabled,
            "path_restriction accessor must agree with resolved (guards false)"
        );
        assert_eq!(
            RuntimoConfig::path_sanitization_enabled(),
            resolved.path_sanitization_enabled,
            "path_sanitization accessor must agree with resolved (guards false)"
        );

        assert!(!RuntimoConfig::blocklist_enabled());
        assert!(!RuntimoConfig::critical_files_enabled());
        assert!(!RuntimoConfig::path_restriction_enabled());
        assert!(!RuntimoConfig::path_sanitization_enabled());

        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn guard_accessors_agree_with_resolved_when_ephemeral_profile() {
        let _guard = CONFIG_TEST_MUTEX.lock().unwrap();
        let tmp = std::env::temp_dir().join("runtimo_test_ephemeral_agreement");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("runtimo")).unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &tmp);
        let cfg_path = tmp.join("runtimo/config.toml");

        // Witness: profile=ephemeral must disable all guards via resolved and accessor.
        std::fs::write(&cfg_path, "profile = \"ephemeral\"\n").unwrap();

        let resolved = RuntimoConfig::load().resolved();
        assert_eq!(resolved.profile, "ephemeral");
        assert!(!resolved.blocklist_enabled, "ephemeral → blocklist false");
        assert!(
            !resolved.critical_files_enabled,
            "ephemeral → critical_files false"
        );
        assert!(
            !resolved.path_restriction_enabled,
            "ephemeral → path_restriction false"
        );
        assert!(
            !resolved.path_sanitization_enabled,
            "ephemeral → path_sanitization false"
        );

        assert_eq!(
            RuntimoConfig::blocklist_enabled(),
            resolved.blocklist_enabled,
            "blocklist accessor must agree with resolved (ephemeral)"
        );
        assert_eq!(
            RuntimoConfig::critical_files_enabled(),
            resolved.critical_files_enabled,
            "critical_files accessor must agree with resolved (ephemeral)"
        );
        assert_eq!(
            RuntimoConfig::path_restriction_enabled(),
            resolved.path_restriction_enabled,
            "path_restriction accessor must agree with resolved (ephemeral)"
        );
        assert_eq!(
            RuntimoConfig::path_sanitization_enabled(),
            resolved.path_sanitization_enabled,
            "path_sanitization accessor must agree with resolved (ephemeral)"
        );

        // Ephemeral must be OFF.
        assert!(!RuntimoConfig::blocklist_enabled());
        assert!(!RuntimoConfig::critical_files_enabled());
        assert!(!RuntimoConfig::path_restriction_enabled());
        assert!(!RuntimoConfig::path_sanitization_enabled());

        // Top-level explicit true must beat ephemeral (precedence check).
        std::fs::write(
            &cfg_path,
            "profile = \"ephemeral\"\nblocklist_enabled = true\ncritical_files_enabled = true\npath_restriction_enabled = true\npath_sanitization_enabled = true\n",
        )
        .unwrap();
        let resolved2 = RuntimoConfig::load().resolved();
        assert!(
            resolved2.blocklist_enabled,
            "top-level true must beat ephemeral"
        );
        assert!(
            resolved2.critical_files_enabled,
            "top-level true must beat ephemeral"
        );
        assert!(
            resolved2.path_restriction_enabled,
            "top-level true must beat ephemeral"
        );
        assert!(
            resolved2.path_sanitization_enabled,
            "top-level true must beat ephemeral"
        );
        assert_eq!(
            RuntimoConfig::blocklist_enabled(),
            resolved2.blocklist_enabled
        );
        assert_eq!(
            RuntimoConfig::critical_files_enabled(),
            resolved2.critical_files_enabled
        );
        assert_eq!(
            RuntimoConfig::path_restriction_enabled(),
            resolved2.path_restriction_enabled
        );
        assert_eq!(
            RuntimoConfig::path_sanitization_enabled(),
            resolved2.path_sanitization_enabled
        );

        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var("XDG_CONFIG_HOME");
    }
}
