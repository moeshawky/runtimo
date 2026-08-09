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
use std::path::PathBuf;

/// Built-in default allowed prefixes.
const DEFAULT_PREFIXES: &[&str] = &["/tmp", "/var/tmp"];

/// Runtimo persistent configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[allow(clippy::exhaustive_structs)]
pub struct RuntimoConfig {
    /// Additional allowed path prefixes (merged with defaults + env var).
    #[serde(default)]
    pub allowed_paths: Vec<String>,

    /// Design Assurance Level (A-E) for the llmosafe cognitive pipeline.
    ///
    /// When set, overrides the `RUNTIMO_DAL` env var. Case-insensitive —
    /// `get_dal()` uppercases the resolved value. Controls how strictly
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
    ];

    /// Returns the config file path following XDG spec.
    ///
    /// Uses `XDG_CONFIG_HOME` if set, otherwise `~/.config/runtimo/config.toml`.
    ///
    /// Falls back to `/tmp/runtimo/config.toml` with a stderr warning when
    /// neither `XDG_CONFIG_HOME` nor `HOME` is set. Configuration in `/tmp`
    /// is not persistent across reboots.
    pub fn config_path() -> PathBuf {
        let base = std::env::var("XDG_CONFIG_HOME")
            .ok()
            .map(PathBuf::from)
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

    /// Saves config to disk, creating parent directories as needed.
    ///
    /// # Errors
    ///
    /// Returns an error if parent directories cannot be created or if the config
    /// file cannot be serialized/written to disk.
    pub fn save(&self) -> Result<(), String> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let content = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&path, content).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Returns the resolved Design Assurance Level.
    ///
    /// Priority (highest to lowest):
    /// 1. `RUNTIMO_DAL` env var
    /// 2. Config file `dal` field
    /// 3. Default: `A`
    ///
    /// The env var and config-file branches both uppercase the value, so
    /// `dal = "b"` and `RUNTIMO_DAL=b` resolve identically to `B`.
    #[must_use]
    pub fn get_dal() -> String {
        // Env var takes precedence
        if let Ok(env_dal) = std::env::var("RUNTIMO_DAL") {
            return env_dal.to_uppercase();
        }
        // Config file
        let config = Self::load();
        config.dal.unwrap_or_else(|| "A".to_string()).to_uppercase()
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
    /// Defaults to enabled when unconfigured. `blocklist_enabled = false` in
    /// config.toml makes ShellExec behave like plain `sh -c`.
    #[must_use]
    pub fn blocklist_enabled() -> bool {
        Self::load().blocklist_enabled.unwrap_or(true)
    }

    /// Whether the FileWrite/Delete critical-files denylist is enabled.
    ///
    /// Defaults to enabled when unconfigured.
    #[must_use]
    pub fn critical_files_enabled() -> bool {
        Self::load().critical_files_enabled.unwrap_or(true)
    }

    /// Whether the allowed-prefix path whitelist is enabled.
    ///
    /// Defaults to enabled when unconfigured. `path_restriction_enabled =
    /// false` removes the "only /tmp, /var/tmp and configured
    /// prefixes" constraint for FileRead/FileWrite/Delete.
    #[must_use]
    pub fn path_restriction_enabled() -> bool {
        Self::load().path_restriction_enabled.unwrap_or(true)
    }

    /// Whether ShellExec children get a forced `PATH` (default: enabled).
    ///
    /// When `false`, children inherit the caller's `PATH`, resolving custom
    /// binaries outside `/usr/local/bin:/usr/bin:/bin`.
    #[must_use]
    pub fn path_sanitization_enabled() -> bool {
        Self::load().path_sanitization_enabled.unwrap_or(true)
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
    #[must_use]
    pub fn get_allowed_prefixes() -> Vec<String> {
        let mut prefixes: Vec<String> = DEFAULT_PREFIXES.iter().map(|s| s.to_string()).collect();

        // Env var (colon-separated)
        if let Ok(env_paths) = std::env::var("RUNTIMO_ALLOWED_PATHS") {
            for p in env_paths.split(':').filter(|s| !s.is_empty()) {
                let trimmed = p.trim().to_string();
                if trimmed.is_empty() {
                    continue;
                }
                if !prefixes.contains(&trimmed) {
                    prefixes.push(trimmed);
                }
            }
        }

        // Config file
        let config = Self::load();
        for p in &config.allowed_paths {
            let trimmed = p.trim().to_string();
            if trimmed.is_empty() {
                continue;
            }
            if !prefixes.contains(&trimmed) {
                prefixes.push(trimmed);
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
}
