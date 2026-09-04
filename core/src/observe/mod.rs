//! Observe subsystem — sampling, bundling, budgeting, and auditing.
//!
//! The observe pipeline collects low-overhead samples from a target process
//! without modifying it (sibling supervision only). All data is WAL-backed
//! via [`BundleWriter`] with hash-chained batches and checkpointing.
//!
//! # Modules
//! * `bundle` — WAL-backed, hash-chained bundle writer with batched fsync
//! * `budget` — resource-budget tracker reusing the `ResourceHistory` pattern
//! * `audit` — exhaustive low-volume hook for imports/spawns/raises/dynamic loads
//!
//! # Invariants
//! * Bundle writes are always WAL-backed; never memory-only.
//! * Bounded channels drop newest on overflow with `TRUNCATED` marking — never silent.
//! * In-process pub/sub stays inside the collector process only.
//! * No hook runs inside the target hot loop.
//! * Secrets/tokens are never logged.

pub mod audit;
pub mod budget;
pub mod bundle;
pub mod sampler;
pub mod self_test;
pub mod supervisor;

pub use audit::{AuditEvent, AuditHook};
pub use budget::ObserveBudget;
pub use bundle::{bundle_path, verify_bundle, BundleWriter, VerifyResult};
pub use sampler::{OutOfProcessSampler, SampleEvent, StackSampler, SAMPLER_CHANNEL_CAP};
pub use supervisor::ObserveSupervisor;
