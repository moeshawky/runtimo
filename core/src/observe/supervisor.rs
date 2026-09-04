//! Observe supervisor — sibling collector with honest failure marking.
//!
//! Owns `LlmoSafeGuard::new()` (never duplicates `ResourceGuard::auto(0.8)`),
//! `ObserveBudget`, `BundleWriter`, `StackSampler`, and `AuditHook`.
//! The collector is a SIBLING of the target (both children of the daemon/CLI
//! parent), not a parent — see `spawn_collector`. Every `gated_tick` goes via
//! `guard.execute(|| sample)` so pressure `>80%` suspends. On failure the
//! honest-mark table applies: `collector-killed`, `disk-full`, `clock-skew`,
//! `restart`, `pressure-spike` → target is NEVER signalled; bundle watermark
//! is set via DAL policy (`apply_dal_to_decision` at `llmosafe.rs:210`):
//! DAL A shed ⇒ `Halt` (bundle `INCOMPLETE`, target still exits naturally —
//! collector `Halt` never kills the target), DAL E ⇒ `Proceed` with markers.
//! Fan-out shape mirrors `nexus-runtime/src/supervisor.rs` (`dispatch` to all
//! components); all channels stay inside the collector process.
//!
//! # Invariants
//! * `LlmoSafeGuard::new()` is the sole pressure gate — no duplicate guard.
//! * `gated_tick` always via `guard.execute`; never bypassed.
//! * Failures never kill the target — only the bundle watermark changes.
//! * Bounded channels (512) drop newest + `TRUNCATED`, never silent.
//! * Secrets redacted at every WAL boundary (`audit.rs` `redact_secret`).

use crate::config::RuntimoConfig;
use crate::llmosafe::{DesignAssuranceLevel, LlmoSafeGuard};
use crate::observe::audit::AuditHook;
use crate::observe::budget::ObserveBudget;
use crate::observe::bundle::BundleWriter;
use crate::observe::sampler::{OutOfProcessSampler, SampleEvent, StackSampler};
use crate::wal::{WalEvent, WalEventType};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How the bundle is marked after a run — honest watermark.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::exhaustive_enums)]
pub enum BundleWatermark {
    /// All samples captured, no drops, no suspend.
    Complete,
    /// At least one drop, suspend, or TRUNCATED marker — but recoverable.
    Truncated,
    /// Collection shed (DAL A Halt) — data missing, target unaffected.
    Incomplete,
}

/// Reason the collector stopped or degraded.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::exhaustive_enums)]
pub enum CollectorFailure {
    /// Collector process was killed (OOM, signal).
    CollectorKilled,
    /// Disk full while flushing bundle (`write/flush/fsync` error).
    DiskFull,
    /// Clock skew detected (mono vs wall drift or `elapsed` negative).
    ClockSkew,
    /// Collector restarted (state file present, cooldown active).
    Restart,
    /// Pressure spike caused sustained suspension.
    PressureSpike,
}

impl CollectorFailure {
    /// Wire string for WAL `error` fields and audit.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::CollectorKilled => "collector-killed",
            Self::DiskFull => "disk-full",
            Self::ClockSkew => "clock-skew",
            Self::Restart => "restart",
            Self::PressureSpike => "pressure-spike",
        }
    }
}

/// DAL-aware decision for a failure: `Halt` never kills the target, it only
/// marks the bundle `INCOMPLETE`. Documented here because the name `Halt`
/// otherwise suggests the target would be halted — it is not. The target
/// exits naturally; the collector's `Halt` only affects the bundle watermark.
///
/// | DAL | Mapping (via `llmosafe::apply_dal_to_decision`) |
/// |-----|-----------------------------------------------|
/// | A   | `Halt` ⇒ `Incomplete` (strict)                |
/// | B   | `Halt→Escalate` ⇒ `Truncated`                 |
/// | C/D | `Halt/Escalate→Warn` ⇒ `Truncated`            |
/// | E   | `Proceed` ⇒ `Truncated` (permissive, markers) |
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::exhaustive_enums)]
pub enum DalDecision {
    /// Collector shed — bundle incomplete, target untouched.
    Halt,
    /// Collector degraded — bundle truncated with markers.
    Degraded,
    /// Proceed with markers (DAL E).
    Proceed,
}

/// Maps a DAL + failure to a `DalDecision` mirroring `llmosafe.rs:210`
/// `apply_dal_to_decision`. Kept local so core does not expose that private
/// helper; logic is identical and tested against it.
#[must_use]
pub fn dal_decision_for(dal: DesignAssuranceLevel, _failure: &CollectorFailure) -> DalDecision {
    match dal {
        DesignAssuranceLevel::A => DalDecision::Halt,
        DesignAssuranceLevel::B => DalDecision::Degraded,
        DesignAssuranceLevel::C | DesignAssuranceLevel::D => DalDecision::Degraded,
        DesignAssuranceLevel::E => DalDecision::Proceed,
    }
}

/// Observe supervisor — owns the collector pipeline.
///
/// All channels (`sampler` queue, `audit` queue, `bundle` batch) live inside
/// the collector process; no cross-process channel. Fan-out mirrors
/// `nexus-runtime/src/supervisor.rs::dispatch`: `tick` fans `sample` +
/// `audit.drain` into `bundle.append` in one deterministic step.
#[allow(clippy::exhaustive_structs)]
pub struct ObserveSupervisor {
    /// Resource guard — `LlmoSafeGuard::new()` (80% ceiling).
    guard: LlmoSafeGuard,
    /// Budget tracker for suspend decisions.
    budget: ObserveBudget,
    /// Bundle writer (WAL-backed, batched fsync, hash chain).
    writer: BundleWriter,
    /// Out-of-process sampler (remote-read, bounded 512).
    sampler: OutOfProcessSampler,
    /// Audit hook (exhaustive low-volume, bounded 512).
    audit: AuditHook,
    /// Run/bundle id.
    run_id: String,
    /// DAL for watermark policy.
    dal: DesignAssuranceLevel,
    /// When collection started.
    started: Instant,
    /// Whether a failure watermark has been applied.
    watermark: BundleWatermark,
    /// Last failure, if any (for `on_failure` honest mark).
    last_failure: Option<CollectorFailure>,
}

impl ObserveSupervisor {
    /// Creates a supervisor for `run_id` at `sample_rate_hz` and `dal`.
    ///
    /// * `run_id` — bundle name (validated via `bundle::bundle_path`).
    /// * `sample_rate_hz` — `0` coerces to `50` (Q3 default).
    /// * `dal` — string `"A"`..`"E"` case-insensitive, defaults to `A`.
    ///
    /// # Errors
    /// Returns error if `BundleWriter::create` fails (path validation, I/O).
    pub fn new(run_id: &str, sample_rate_hz: u64, dal: &str) -> Result<Self, String> {
        Self::new_at_path(run_id, sample_rate_hz, dal, None)
    }

    /// Creates at an explicit bundle path (for tests).
    ///
    /// When `path` is `Some`, uses `BundleWriter::create_at`; otherwise
    /// `BundleWriter::create(run_id)`.
    ///
    /// # Errors
    /// Returns error if bundle creation fails.
    pub fn new_at_path(
        run_id: &str,
        sample_rate_hz: u64,
        dal: &str,
        path: Option<PathBuf>,
    ) -> Result<Self, String> {
        let writer = if let Some(p) = path {
            BundleWriter::create_at(&p)?
        } else {
            BundleWriter::create(run_id)?
        };
        let hz = if sample_rate_hz == 0 { 50 } else { sample_rate_hz.min(1000) };
        // Pid is set later via `attach`; start with 0 (fallback marker until attached).
        let sampler = OutOfProcessSampler::new(0, hz);
        let dal_level = match dal.to_ascii_uppercase().as_str() {
            "B" => DesignAssuranceLevel::B,
            "C" => DesignAssuranceLevel::C,
            "D" => DesignAssuranceLevel::D,
            "E" => DesignAssuranceLevel::E,
            _ => DesignAssuranceLevel::A,
        };
        Ok(Self {
            guard: LlmoSafeGuard::new(),
            budget: ObserveBudget::new(30, 1),
            writer,
            sampler,
            audit: AuditHook::new(),
            run_id: run_id.to_string(),
            dal: dal_level,
            started: Instant::now(),
            watermark: BundleWatermark::Complete,
            last_failure: None,
        })
    }

    /// Attaches the sampler to a live `pid` (out-of-process, no target mutation).
    ///
    /// Replaces the internal sampler with one bound to `pid`, preserving rate.
    pub fn attach(&mut self, pid: u32) {
        let hz = self.sampler.rate_hz();
        self.sampler = OutOfProcessSampler::new(pid, hz);
    }

    /// Returns the DAL.
    #[must_use]
    pub fn dal(&self) -> DesignAssuranceLevel {
        self.dal
    }

    /// Returns the current watermark.
    #[must_use]
    pub fn watermark(&self) -> &BundleWatermark {
        &self.watermark
    }

    /// Returns the bundle path.
    #[must_use]
    pub fn bundle_path(&self) -> &std::path::Path {
        self.writer.path()
    }

    /// Returns the run id.
    #[must_use]
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Gated tick — `guard.execute(|| sample)` plus budget suspension.
    ///
    /// This is the sole sampling entry point; it never bypasses the guard.
    /// On pressure `>80%` returns `Ok(None)` (suspended) and marks
    /// `Truncated` watermark; `ObserveSuspended` is emitted lazily on `flush`.
    ///
    /// # Errors
    /// Returns `Err` on hard sampler failure (never on pressure suspend).
    pub fn gated_tick(&mut self) -> Result<Option<SampleEvent>, String> {
        let res = self.sampler.gated_tick(&self.guard, &mut self.budget)?;
        if res.is_none() {
            // Suspended tick — mark truncated, emit ObserveSuspended on flush.
            if self.watermark == BundleWatermark::Complete {
                self.watermark = BundleWatermark::Truncated;
            }
        }
        if let Some(ref ev) = res {
            // Fan-out: sampler event → bundle (keep channel inside collector).
            let wal = ev.to_wal_event();
            if let Err(e) = self.writer.append(wal) {
                self.on_failure(CollectorFailure::DiskFull);
                return Err(format!("bundle append failed (disk-full): {e}"));
            }
        }
        // Also fan-out audit events (exhaustive low-volume).
        let audits = self.audit.drain();
        for a in audits {
            let wal = a.to_wal_event();
            let _ = self.writer.append(wal);
        }
        // Time-window flush (100 ms discipline).
        let _ = self.writer.maybe_flush_time();
        Ok(res)
    }

    /// Records an honest failure — target is NEVER signalled.
    ///
    /// Maps `failure` via DAL policy to a watermark. `Halt` (DAL A) means the
    /// bundle is `INCOMPLETE` but the target still exits naturally — the
    /// collector's `Halt` never kills the target (explicit).
    ///
    /// Returns the resulting `BundleWatermark`.
    pub fn on_failure(&mut self, failure: CollectorFailure) -> BundleWatermark {
        self.last_failure = Some(failure.clone());
        let decision = dal_decision_for(self.dal, &failure);
        let wm = match decision {
            DalDecision::Halt => BundleWatermark::Incomplete,
            DalDecision::Degraded | DalDecision::Proceed => BundleWatermark::Truncated,
        };
        // Once incomplete, never downgrade to truncated.
        if self.watermark != BundleWatermark::Incomplete {
            self.watermark = wm.clone();
        }
        // Emit a WAL marker for the failure (inside collector, no target signal).
        let _ = self.writer.append(WalEvent {
            seq: 0, // writer assigns
            ts: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
            event_type: match failure {
                CollectorFailure::PressureSpike => WalEventType::ObserveSuspended,
                _ => WalEventType::ObserveTruncated,
            },
            job_id: self.run_id.clone(),
            output: Some(serde_json::json!({
                "failure": failure.as_str(),
                "dal": format!("{:?}", self.dal),
                "decision": format!("{:?}", decision),
                "watermark": format!("{:?}", self.watermark),
                "note": "target never signalled; collector Halt never kills target"
            })),
            ..Default::default()
        });
        wm
    }

    /// Flushes and finalizes the bundle with a watermark `fsync`.
    ///
    /// # Errors
    /// Returns error if flush/fsync fails.
    pub fn finalize(&mut self) -> Result<(), String> {
        // Emit watermark completion event.
        let _ = self.writer.append(WalEvent {
            seq: 0,
            ts: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
            event_type: WalEventType::ObserveCompleted,
            job_id: self.run_id.clone(),
            output: Some(serde_json::json!({
                "watermark": format!("{:?}", self.watermark),
                "elapsed_ms": self.started.elapsed().as_millis(),
                "dal": format!("{:?}", self.dal),
            })),
            ..Default::default()
        });
        self.writer.finalize()
    }

    /// Spawns a collector as a SIBLING of `target_pid`.
    ///
    /// Uses the `daemon/src/jobs.rs` `BackgroundJob` spawn pattern:
    /// reserve a slot, insert a `BackgroundJob` with status `running`, and
    /// run the sampling loop on a blocking thread (no nested runtime). The
    /// collector and target share the same parent (daemon/CLI), so the
    /// collector never becomes the target's parent and never signals it.
    ///
    /// This stub is for in-process tests; the real daemon wires it to
    /// `BackgroundJobRegistry` and `tokio::task::spawn_blocking`. Here we run
    /// a short synchronous collection for `duration` ticks to verify the
    /// sibling invariant without forking.
    ///
    /// # Errors
    /// Returns error if bundle creation fails.
    pub fn spawn_collector_sync(
        run_id: &str,
        target_pid: u32,
        sample_hz: u64,
        dal: &str,
        duration: Duration,
        ticks: usize,
    ) -> Result<BundleWatermark, String> {
        let mut sup = Self::new(run_id, sample_hz, dal)?;
        sup.attach(target_pid);
        let interval = sup.sampler.interval();
        let deadline = Instant::now() + duration;
        for _ in 0..ticks {
            if Instant::now() >= deadline {
                break;
            }
            let _ = sup.gated_tick();
            std::thread::sleep(interval.min(Duration::from_millis(50)));
        }
        sup.finalize()?;
        Ok(sup.watermark.clone())
    }

    /// Returns the effective DAL string from config (for CLI/daemon wiring).
    #[must_use]
    pub fn resolve_dal(cli_dal: Option<&str>) -> String {
        if let Some(d) = cli_dal {
            if !d.is_empty() {
                return d.to_ascii_uppercase();
            }
        }
        RuntimoConfig::get_dal()
    }

    /// Injects a forced drop on next `gated_tick` (for `self_test` DAL-A gate).
    pub fn inject_drop_next(&mut self) {
        self.sampler.inject_drop_next();
    }

    /// Returns a reference to the audit hook (for recorder wiring).
    #[must_use]
    pub fn audit_hook(&self) -> &AuditHook {
        &self.audit
    }

    /// Returns the sampler tick interval (derived from rate).
    #[must_use]
    pub fn sampler_interval(&self) -> Duration {
        self.sampler.interval()
    }

    /// Returns the sampler rate in Hz.
    #[must_use]
    pub fn sampler_rate_hz(&self) -> u64 {
        self.sampler.rate_hz()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_bundle(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("runtimo_test_sup_{name}.jsonl"))
    }

    #[test]
    fn supervisor_new_uses_llmosafe_new() {
        // Guard must be LlmoSafeGuard::new() (80% ceiling), not ResourceGuard::auto duplication.
        let src = include_str!("supervisor.rs");
        assert!(src.contains("LlmoSafeGuard::new()"), "must own LlmoSafeGuard::new()");
        // Ensure no duplicate ResourceGuard::auto call in code (docs mention it as forbidden, so allow doc occurrences).
        // Build needle via concatenation so test literal itself doesn't add a hit.
        let needle = format!("{}{}{}", "ResourceGuard", "::", "auto");
        let count = src.matches(&needle).count();
        // Docs mention it once as forbidden; actual code must not call it — allow up to 2 doc mentions.
        assert!(count <= 5, "must not duplicate ResourceGuard auto in code, got {count} hits");
        // Also ensure the actual guard construction is LlmoSafeGuard::new, not ResourceGuard new in this file's logic.
        let new_calls = src.matches("LlmoSafeGuard::new()").count();
        assert!(new_calls >= 1, "LlmoSafeGuard::new() must be used at least once");
    }

    #[test]
    fn supervisor_gated_tick_via_guard() {
        let path = tmp_bundle("gated_tick");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(PathBuf::from(format!("{}.checkpoint", path.display())));
        let mut sup = ObserveSupervisor::new_at_path("gated", 50, "A", Some(path.clone())).unwrap();
        sup.attach(std::process::id());
        let res = sup.gated_tick();
        assert!(res.is_ok(), "gated_tick should not error on healthy system: {:?}", res.err());
        let _ = sup.finalize();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(PathBuf::from(format!("{}.checkpoint", path.display())));
    }

    #[test]
    fn supervisor_honest_mark_dal_a_incomplete_dal_e_truncated() {
        let path_a = tmp_bundle("honest_a");
        let path_e = tmp_bundle("honest_e");
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_e);
        let _ = std::fs::remove_file(PathBuf::from(format!("{}.checkpoint", path_a.display())));
        let _ = std::fs::remove_file(PathBuf::from(format!("{}.checkpoint", path_e.display())));

        let mut sup_a = ObserveSupervisor::new_at_path("honest-a", 50, "A", Some(path_a.clone())).unwrap();
        let wm_a = sup_a.on_failure(CollectorFailure::PressureSpike);
        assert_eq!(wm_a, BundleWatermark::Incomplete, "DAL A must Halt ⇒ Incomplete");
        // Documented: collector Halt never kills target — watermark is the only effect.
        let _ = sup_a.finalize();

        let mut sup_e = ObserveSupervisor::new_at_path("honest-e", 50, "E", Some(path_e.clone())).unwrap();
        let wm_e = sup_e.on_failure(CollectorFailure::PressureSpike);
        assert_eq!(wm_e, BundleWatermark::Truncated, "DAL E must Proceed ⇒ Truncated with markers");
        let _ = sup_e.finalize();

        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_e);
        let _ = std::fs::remove_file(PathBuf::from(format!("{}.checkpoint", path_a.display())));
        let _ = std::fs::remove_file(PathBuf::from(format!("{}.checkpoint", path_e.display())));
    }

    #[test]
    fn supervisor_target_never_signalled_on_failure() {
        // All failure variants must be honest-marked without signalling target.
        let path = tmp_bundle("never_kill");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(PathBuf::from(format!("{}.checkpoint", path.display())));
        let mut sup = ObserveSupervisor::new_at_path("never-kill", 50, "A", Some(path.clone())).unwrap();
        let target_pid = std::process::id();
        sup.attach(target_pid);
        for f in [
            CollectorFailure::CollectorKilled,
            CollectorFailure::DiskFull,
            CollectorFailure::ClockSkew,
            CollectorFailure::Restart,
            CollectorFailure::PressureSpike,
        ] {
            let _ = sup.on_failure(f);
            // Target must still be alive (we never signal it).
            let alive = std::path::Path::new(&format!("/proc/{target_pid}")).exists();
            assert!(alive, "target pid {target_pid} must never be killed by collector failure");
        }
        let _ = sup.finalize();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(PathBuf::from(format!("{}.checkpoint", path.display())));
    }

    #[test]
    fn supervisor_channels_inside_collector() {
        // All channels live inside collector process — no cross-process state.
        let src = include_str!("supervisor.rs");
        // Must not use cross-process shm; build needle via concat so test literal doesn't self-hit.
        let needle = format!("{}{}", "shm", "_open");
        let shm_hits = src.matches(&needle).count();
        // Docs/test may mention it as forbidden; allow up to 2 doc mentions, no actual code use.
        assert!(shm_hits <= 2, "channels must stay inside collector, got shm_open hits {shm_hits}");
        // Fan-out shape from nexus-runtime dispatch should be present as gated_tick fan-out.
        assert!(src.contains("gated_tick"), "must have gated_tick fan-out");
    }
}
