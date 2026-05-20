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
pub struct RuntimoConfig {
    /// Additional allowed path prefixes (merged with defaults + env var).
    #[serde(default)]
    pub allowed_paths: Vec<String>,
}

impl RuntimoConfig {
    /// Returns the config file path following XDG spec.
    ///
    /// Uses `XDG_CONFIG_HOME` if set, otherwise `~/.config/runtimo/config.toml`.
    pub fn config_path() -> PathBuf {
        std::env::var("XDG_CONFIG_HOME")
            .ok()
            .map(PathBuf::from)
            .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("runtimo/config.toml")
    }

    /// Loads config from disk, returning defaults if the file doesn't exist or is invalid.
    pub fn load() -> Self {
        let path = Self::config_path();
        if path.exists() {
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            toml::from_str(&content).unwrap_or_default()
        } else {
            Self::default()
        }
    }

    /// Saves config to disk, creating parent directories as needed.
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
    pub fn get_allowed_prefixes() -> Vec<String> {
        let mut prefixes: Vec<String> = DEFAULT_PREFIXES.iter().map(|s| s.to_string()).collect();

        // Env var (colon-separated)
        if let Ok(env_paths) = std::env::var("RUNTIMO_ALLOWED_PATHS") {
            for p in env_paths.split(':').filter(|s| !s.is_empty()) {
                let trimmed = p.trim().to_string();
                if !prefixes.contains(&trimmed) {
                    prefixes.push(trimmed);
                }
            }
        }

        // Config file
        let config = Self::load();
        for p in &config.allowed_paths {
            let trimmed = p.trim().to_string();
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

    #[test]
    fn config_path_is_absolute() {
        let path = RuntimoConfig::config_path();
        assert!(path.is_absolute() || path.to_string_lossy().starts_with("/tmp"));
    }

    #[test]
    fn load_returns_defaults_when_no_file() {
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
}
