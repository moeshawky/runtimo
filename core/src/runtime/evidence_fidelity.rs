//! Evidence fidelity — evidence quality classification.
//!
//! # Ownership
//! - **Writer**: Providers
//! - **Reader**: RuntimeFactV1
//! - **Serialization**: serde JSON
//! - **Versioning**: enum (stable — no new variants without semver)
//!
//! # Global Invariant
//! - `NOT OBSERVED != FALSE` — absence of evidence is not evidence
//!   of absence. A missing observation does not mean the fact is false.

use serde::{Deserialize, Serialize};

/// Evidence fidelity classification.
///
/// Indicates the quality and source of evidence for a runtime fact.
///
/// # Variants
/// - `ExactRuntime` — Evidence from exact runtime instrumentation
///   (e.g., JFR class-load, EventPipe exception).
/// - `KernelObserved` — Evidence from kernel-level observation
///   (e.g., Tetragon exec).
/// - `Sampled` — Evidence from sampling (e.g., OTel sampled traces).
/// - `Derived` — Evidence derived from inference (not directly observed).
///
/// # Global Invariant
/// - `NOT OBSERVED != FALSE` — absence of evidence is not evidence
///   of absence. A missing observation does not mean the fact is false.
/// - `Sampled` from `sema` is deferred (OTel blocked); shape defined
///   but not yet produced by a provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::exhaustive_enums)] // new variants are semver-breaking
pub enum EvidenceFidelity {
    /// Exact runtime instrumentation (JFR class-load, EventPipe exception).
    ExactRuntime,
    /// Kernel-level observation (Tetragon exec).
    KernelObserved,
    /// Sampled evidence (OTel sampled traces; deferred for sema).
    Sampled,
    /// Derived evidence (inference, not directly observed).
    Derived,
}

impl EvidenceFidelity {
    /// Returns `true` if this fidelity represents directly observed evidence.
    #[must_use]
    pub fn is_observed(&self) -> bool {
        matches!(
            self,
            Self::ExactRuntime | Self::KernelObserved | Self::Sampled
        )
    }

    /// Returns `true` if this fidelity represents derived (inferred) evidence.
    #[must_use]
    pub fn is_derived(&self) -> bool {
        matches!(self, Self::Derived)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_fidelity_round_trip() {
        for fidelity in [
            EvidenceFidelity::ExactRuntime,
            EvidenceFidelity::KernelObserved,
            EvidenceFidelity::Sampled,
            EvidenceFidelity::Derived,
        ] {
            let json = serde_json::to_string(&fidelity).unwrap();
            let deserialized: EvidenceFidelity = serde_json::from_str(&json).unwrap();
            assert_eq!(fidelity, deserialized);
        }
    }

    #[test]
    fn evidence_fidelity_is_observed() {
        assert!(EvidenceFidelity::ExactRuntime.is_observed());
        assert!(EvidenceFidelity::KernelObserved.is_observed());
        assert!(EvidenceFidelity::Sampled.is_observed());
        assert!(!EvidenceFidelity::Derived.is_observed());
    }

    #[test]
    fn evidence_fidelity_not_observed_not_false() {
        // Invariant: NOT OBSERVED != FALSE.
        // Absence of a fact with a given fidelity does not mean the fact is false.
        // This is documented as a global invariant, not enforced by the type.
        let derived = EvidenceFidelity::Derived;
        // A Derived fact being absent does not mean the underlying
        // event did not occur — it means we have no derived evidence.
        assert!(derived.is_derived());
    }
}
