//! Versioned runtime contracts — taxonomy + provenance, not acquisition.
//!
//! This module defines the minimal versioned contracts for the Runtimo
//! runtime. Each contract is designed for demonstrated consumers only —
//! fields without consumers are rejected (see `docs/runtime-contracts.md`
//! for the rejected-fields list).
//!
//! # Ownership
//!
//! | Contract | Writer | Reader | Serialization | Versioning |
//! |----------|--------|--------|---------------|------------|
//! | `ProviderStatus` | Provider Supervisor | WAL, CLI | serde JSON | semver |
//! | `RunProcessKey` | Executor | WAL, RuntimeFactV1 | serde JSON | semver |
//! | `RunManifestV1` | Executor | WAL, CLI | serde JSON | schema_version |
//! | `RuntimeLocator` | Providers | WAL, CLI | serde JSON | semver |
//! | `EvidenceFidelity` | Providers | RuntimeFactV1 | serde JSON | enum (stable) |
//! | `RuntimeFactV1` | Providers | WAL, CLI, Oracle | serde JSON | semver |
//!
//! # Invariants
//!
//! - All contracts are serializable with `serde` and comparable with `PartialEq + Eq`.
//! - Unknown fields are ignored during deserialization (forward-compat).
//! - `RunProcessKey` equality includes `process_start_time` — same PID with
//!   different start_time are distinct keys.
//! - `NOT OBSERVED != FALSE` — absence of evidence is not evidence of absence.
//! - PID is never used alone; always paired with `process_start_time`.
//! - No raw provider output types leak into `RuntimeLocator`.
//! - No `SymbolUID` inside raw adapters.
//! - No custom unwinder/parser/symbolizer is introduced.

pub mod evidence_fidelity;
pub mod provider_status;
pub mod run_manifest_v1;
pub mod run_process_key;
pub mod runtime_fact_export;
pub mod runtime_fact_v1;
pub mod runtime_locator;

// Re-export for convenience
pub use evidence_fidelity::EvidenceFidelity;
pub use provider_status::ProviderStatus;
pub use run_manifest_v1::RunManifestV1;
pub use run_manifest_v1::{RepositoryIdentity, Target};
pub use run_process_key::RunProcessKey;
pub use runtime_fact_export::{resolve_locator, RuntimeFactExport, SymbolUID, SymbolUidResolution};
pub use runtime_fact_v1::{ArtifactRef, RuntimeFactFamily, RuntimeFactV1};
pub use runtime_locator::RuntimeLocator;
