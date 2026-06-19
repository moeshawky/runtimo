//! Persistent configuration for Runtimo.
//!
//! Reads/writes a TOML config file at `~/.config/runtimo/config.toml`.
//! Allowed path prefixes are merged from three sources (lowest to highest priority):
//! 1. Built-in defaults (`/tmp`, `/var/tmp`, `/home`)
//! 2. `RUNTIMO_ALLOWED_PATHS` env var (colon-separated)
//! 3. Config file `allowed_paths` array
//! 4. Context-specific prefixes (programmatic override)

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Built-in default allowed prefixes.
const DEFAULT_PREFIXES: &[&str] = &["/tmp", "/var/tmp", "/home"];

/// Runtimo persistent configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[allow(clippy::exhaustive_structs)]
pub struct RuntimoConfig {
    /// Additional allowed path prefixes (merged with defaults + env var).
    #[serde(default)]
    pub allowed_paths: Vec<String>,
}

impl RuntimoConfig {
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
            toml::from_str(&content)
                .map_err(|e| format!("Cannot parse config file '{}': {}", path.display(), e))
        } else {
            Ok(Self::default())
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
    fn get_allowed_prefixes_includes_defaults() {
        let prefixes = RuntimoConfig::get_allowed_prefixes();
        assert!(prefixes.iter().any(|p| p == "/tmp"));
        assert!(prefixes.iter().any(|p| p == "/var/tmp"));
        assert!(prefixes.iter().any(|p| p == "/home"));
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
}
