//! Provider status — provider health surface.
//!
//! # Ownership
//! - **Writer**: Provider Supervisor
//! - **Reader**: WAL, CLI
//! - **Serialization**: serde JSON
//! - **Versioning**: semver
//!
//! Provider failure must surface as `Degraded`/`Unavailable`, never silent.

use serde::{Deserialize, Serialize};

/// Provider health status.
///
/// # Variants
/// - `Unavailable` — provider is not available (no fallback).
/// - `Degraded(String)` — provider is available but degraded; the string
///   carries the degradation reason.
/// - `Active` — provider is fully operational.
/// - `Failed(String)` — provider has failed; the string carries the failure reason.
///
/// # Invariants
/// - Provider failure always surfaces as `Degraded` or `Unavailable`, never silent.
/// - `Active` indicates no known issues.
/// - `Failed` indicates a terminal condition requiring intervention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[allow(clippy::exhaustive_enums)] // new variants are semver-breaking
pub enum ProviderStatus {
    /// Provider is not available.
    Unavailable {
        /// Provider name.
        provider: String,
        /// Provider version.
        version: String,
        /// Reason for unavailability.
        reason: String,
    },
    /// Provider is available but degraded.
    Degraded {
        /// Provider name.
        provider: String,
        /// Provider version.
        version: String,
        /// Operating mode.
        mode: String,
        /// Degradation reason.
        reason: String,
    },
    /// Provider is fully operational.
    Active {
        /// Provider name.
        provider: String,
        /// Provider version.
        version: String,
        /// Operating mode.
        mode: String,
    },
    /// Provider has failed.
    Failed {
        /// Provider name.
        provider: String,
        /// Provider version.
        version: String,
        /// Operating mode.
        mode: String,
        /// Failure reason.
        reason: String,
    },
}

impl ProviderStatus {
    /// Returns the provider name.
    #[must_use]
    pub fn provider_name(&self) -> &str {
        match self {
            Self::Unavailable { provider, .. }
            | Self::Degraded { provider, .. }
            | Self::Active { provider, .. }
            | Self::Failed { provider, .. } => provider,
        }
    }

    /// Returns the provider version.
    #[must_use]
    pub fn version(&self) -> &str {
        match self {
            Self::Unavailable { version, .. }
            | Self::Degraded { version, .. }
            | Self::Active { version, .. }
            | Self::Failed { version, .. } => version,
        }
    }

    /// Returns `true` if the provider is in an active (non-failed) state.
    #[must_use]
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active { .. })
    }

    /// Returns `true` if the provider is unavailable or failed.
    #[must_use]
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable { .. } | Self::Failed { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_status_round_trip() {
        let status = ProviderStatus::Active {
            provider: "tetragon".to_string(),
            version: "1.7.1".to_string(),
            mode: "monitor".to_string(),
        };
        let json = serde_json::to_string(&status).unwrap();
        let deserialized: ProviderStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(status, deserialized);
        assert!(deserialized.is_active());
    }

    #[test]
    fn provider_status_degraded_round_trip() {
        let status = ProviderStatus::Degraded {
            provider: "jfr".to_string(),
            version: "0.1.0".to_string(),
            mode: "exact".to_string(),
            reason: "high overhead".to_string(),
        };
        let json = serde_json::to_string(&status).unwrap();
        let deserialized: ProviderStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(status, deserialized);
        assert!(!deserialized.is_active());
        assert!(!deserialized.is_unavailable());
    }

    #[test]
    fn provider_status_unknown_field_ignored() {
        // Forward-compat: unknown fields are ignored.
        let json = r#"{"status":"active","provider":"tetragon","version":"1.7.1","mode":"monitor","unknown_field":"ignored"}"#;
        let status: ProviderStatus = serde_json::from_str(json).unwrap();
        assert!(matches!(status, ProviderStatus::Active { .. }));
    }

    #[test]
    fn provider_status_name_and_version() {
        let status = ProviderStatus::Failed {
            provider: "otel".to_string(),
            version: "0.0.1".to_string(),
            mode: "sampled".to_string(),
            reason: "blocked".to_string(),
        };
        assert_eq!(status.provider_name(), "otel");
        assert_eq!(status.version(), "0.0.1");
        assert!(status.is_unavailable());
    }
}
