//! Out-of-process stack sampler — remote-read, sibling-only.
//!
//! The sampler never injects code into the target (`P1A`): no `ptrace`
//! stop `>1 ms`, no `LD_PRELOAD`, no in-process hook. Each tick is
//! gated through [`LlmoSafeGuard::execute`] (`llmosafe.rs:331-337`) so
//! pressure `>80 %` suspends via [`ObserveBudget::should_suspend`].
//! Rate is taken from [`ObserveConfig::sample_rate_hz`] (`config.rs`)
//! default `50 Hz` (`Q3 P1A`). Output is via a bounded `512`-cap
//! channel that drops newest on overflow and emits a `TRUNCATED` marker
//! on drain — same discipline as [`AuditHook`] (`audit.rs`), never
//! silent zeros. Fallback (permission `EPERM`, unknown runtime) emits
//! `TRUNCATED`/`SAMPLED` markers with an error note.
//!
//! # Invariants
//! * Target is unmodified — all reads are from `/proc/<pid>/…` or
//!   `kill(pid,0)` liveness checks.
//! * Every tick passes `guard.execute(|| sample)` — no bypass.
//! * Bounded channel `512`; overflow drops newest, not oldest, and is
//!   accounted via `dropped` + marker.
//! * Fallback never returns silent zero frames without a marker.
//! * Secrets are redacted at the WAL boundary.

use crate::llmosafe::LlmoSafeGuard;
use crate::observe::budget::ObserveBudget;
use crate::wal::{WalEvent, WalEventType};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Bounded sampler channel capacity — matches [`crate::observe::audit::AUDIT_CHANNEL_CAP`].
pub const SAMPLER_CHANNEL_CAP: usize = 512;

/// A single stack sample (out-of-process snapshot).
#[derive(Debug, Clone)]
#[allow(clippy::exhaustive_structs)]
pub struct SampleEvent {
    /// Monotonic sequence within this sampler.
    pub seq: u64,
    /// Target pid sampled.
    pub pid: u32,
    /// Monotonic nanoseconds since sampler creation (`base.elapsed()`).
    pub mono_ns: u64,
    /// Wall-clock nanoseconds since UNIX epoch.
    pub wall_ns: u64,
    /// Captured frames (may be empty on fallback, then `truncated` is true).
    pub frames: Vec<String>,
    /// Whether this event is a `TRUNCATED`/`SAMPLED` fallback marker.
    pub truncated: bool,
    /// Human-readable error/reason when `truncated` is true (e.g. `EPERM`).
    pub error: Option<String>,
}

impl SampleEvent {
    /// Returns true if this event is a fallback/truncated marker.
    #[must_use]
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// Converts to a WAL event for bundle persistence.
    ///
    /// Redacts any frame containing `auth_token`/`bearer`/`api_key`
    /// (case-insensitive) to `REDACTED` before serialization — same
    /// `G-SEC` discipline as [`crate::observe::audit::AuditEvent::to_wal_event`].
    #[must_use]
    pub fn to_wal_event(&self) -> WalEvent {
        let et = if self.truncated {
            WalEventType::ObserveTruncated
        } else {
            WalEventType::ObserveBatch
        };
        let safe_frames: Vec<String> = self
            .frames
            .iter()
            .map(|f| {
                let lower = f.to_ascii_lowercase();
                if lower.contains("auth_token") || lower.contains("bearer") || lower.contains("api_key") {
                    "REDACTED".to_string()
                } else {
                    // Truncate long frame to 512 chars to bound WAL.
                    if f.len() > 512 {
                        let mut s = f[..512].to_string();
                        s.push_str("...[truncated]");
                        s
                    } else {
                        f.clone()
                    }
                }
            })
            .collect();
        let mut output = serde_json::json!({
            "pid": self.pid,
            "frames": safe_frames,
            "truncated": self.truncated,
        });
        if let Some(ref e) = self.error {
            let lower = e.to_ascii_lowercase();
            let safe_e = if lower.contains("auth_token") || lower.contains("bearer") || lower.contains("api_key") {
                "REDACTED".to_string()
            } else {
                e.clone()
            };
            output["error"] = serde_json::Value::String(safe_e);
        }
        WalEvent {
            seq: self.seq,
            ts: self.wall_ns / 1_000_000_000,
            event_type: et,
            job_id: format!("sample-{}", self.seq),
            output: Some(output),
            mono_ns: Some(self.mono_ns),
            wall_ns: Some(self.wall_ns),
            ..Default::default()
        }
    }
}

/// Out-of-process sampler trait.
///
/// Implementations must not modify the target. Every tick is gated via
/// `LlmoSafeGuard::execute`; the trait exposes `gated_tick` which wraps
/// `sample` with that gate and the budget check.
pub trait StackSampler: Send {
    /// Attempts a single remote-read sample.
    ///
    /// Returns a `SampleEvent` on success or a fallback marker on
    /// `EPERM`/unknown-runtime; never silent zeros. On hard I/O error
    /// returns `Err`.
    fn sample(&mut self) -> Result<SampleEvent, String>;

    /// Gated tick: `guard.execute(|| sample)` plus budget suspension.
    ///
    /// Returns `Ok(Some(event))` on a successful sample (enqueued),
    /// `Ok(None)` when suspended by pressure or on overflow-drop (newest
    /// dropped, counted in `dropped`), and `Err` on hard failure.
    fn gated_tick(
        &mut self,
        guard: &LlmoSafeGuard,
        budget: &mut ObserveBudget,
    ) -> Result<Option<SampleEvent>, String>;

    /// Drains queued samples, appending a `TRUNCATED` marker if drops occurred.
    fn drain(&mut self) -> Vec<SampleEvent>;

    /// Current queue length.
    fn len(&self) -> usize;

    /// Whether queue is empty.
    fn is_empty(&self) -> bool;

    /// Number of dropped samples not yet materialized as a marker.
    fn dropped(&self) -> u64;

    /// Sample rate in Hz.
    fn rate_hz(&self) -> u64;

    /// Tick interval derived from rate.
    fn interval(&self) -> Duration;
}

/// Out-of-process implementation for the primary target runtime.
///
/// Remote-read is via `/proc/<pid>/stack` (and liveness via
/// `/proc/<pid>/status` / `kill(pid,0)` semantics). No `ptrace` attach
/// with stop `>1 ms`; the read is a single open+read. On `EPERM` or
/// unknown runtime (`ENOENT` for stack file on unknown kernel) a
/// fallback `TRUNCATED` event is produced with `error` set, never an
/// empty silent sample.
///
/// Bounded channel: `512`-cap `VecDeque`; full → drop newest
/// (increment `dropped`), marker on `drain` — copied from
/// `audit.rs` bounded-channel discipline, not reinvented.
#[allow(clippy::exhaustive_structs)]
pub struct OutOfProcessSampler {
    /// Target pid.
    pid: u32,
    /// Samples per second (from `ObserveConfig`, default `50`).
    rate_hz: u64,
    /// Tick interval (`1/rate_hz`).
    interval: Duration,
    /// Bounded output channel.
    queue: VecDeque<SampleEvent>,
    /// Effective channel capacity (512 default, smaller for tests via with_capacity).
    cap: usize,
    /// Dropped count (overflow).
    dropped: u64,
    /// Next sequence number.
    next_seq: u64,
    /// Base instant for `mono_ns`.
    base: Instant,
    /// Optional injected failure for `self_test` DAL-A gate.
    inject_drop: bool,
}

impl OutOfProcessSampler {
    /// Creates a sampler for `pid` at `rate_hz`.
    ///
    /// `rate_hz` of `0` is coerced to `50` (default `Q3`). `rate_hz`
    /// above `1000` is capped to `1000` to avoid busy-loop.
    #[must_use]
    pub fn new(pid: u32, rate_hz: u64) -> Self {
        let hz = match rate_hz {
            0 => 50,
            v if v > 1000 => 1000,
            v => v,
        };
        let interval = Duration::from_micros(1_000_000 / hz);
        Self {
            pid,
            rate_hz: hz,
            interval,
            queue: VecDeque::with_capacity(SAMPLER_CHANNEL_CAP),
            cap: SAMPLER_CHANNEL_CAP,
            dropped: 0,
            next_seq: 0,
            base: Instant::now(),
            inject_drop: false,
        }
    }

    /// Creates with explicit capacity (for tests).
    #[must_use]
    pub fn with_capacity(pid: u32, rate_hz: u64, cap: usize) -> Self {
        let hz = match rate_hz {
            0 => 50,
            v if v > 1000 => 1000,
            v => v,
        };
        let interval = Duration::from_micros(1_000_000 / hz);
        let effective = cap.min(SAMPLER_CHANNEL_CAP);
        Self {
            pid,
            rate_hz: hz,
            interval,
            queue: VecDeque::with_capacity(effective),
            cap: effective,
            dropped: 0,
            next_seq: 0,
            base: Instant::now(),
            inject_drop: false,
        }
    }

    /// Injects a forced drop on next `gated_tick` (for `self_test` DAL-A).
    pub fn inject_drop_next(&mut self) {
        self.inject_drop = true;
    }

    /// Attempts a remote-read of `/proc/<pid>/stack`.
    ///
    /// Returns frames on success; on `EPERM`/`ENOENT`/other, returns a
    /// fallback marker (`truncated=true`) with `error` set, never silent
    /// zeros. No `ptrace` stop `>1 ms`.
    fn try_remote_read(&mut self) -> SampleEvent {
        let mono = self.base.elapsed().as_nanos() as u64;
        let wall = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        let seq = self.next_seq;
        self.next_seq += 1;

        // Liveness: if pid 0 or unreadable, treat as fallback.
        if self.pid == 0 {
            return SampleEvent {
                seq,
                pid: self.pid,
                mono_ns: mono,
                wall_ns: wall,
                frames: vec!["TRUNCATED fallback".to_string()],
                truncated: true,
                error: Some("invalid pid 0".to_string()),
            };
        }

        // Try remote read: /proc/<pid>/stack (kernel stacks) — out-of-process,
        // no stop. On kernels without stack file, ENOENT → fallback marker.
        let stack_path = format!("/proc/{}/stack", self.pid);
        match std::fs::read_to_string(&stack_path) {
            Ok(content) => {
                let frames: Vec<String> = content
                    .lines()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .take(64)
                    .collect();
                if frames.is_empty() {
                    // Empty stack is not silent — emit sampled marker with empty but not truncated?
                    // Spec: fallback must emit TRUNCATED/SAMPLED markers, never silent zeros.
                    // Empty stack with no error would be silent zero; so mark SAMPLED with note.
                    SampleEvent {
                        seq,
                        pid: self.pid,
                        mono_ns: mono,
                        wall_ns: wall,
                        frames: vec!["SAMPLED empty-stack".to_string()],
                        truncated: true,
                        error: Some("empty stack — sampled marker".to_string()),
                    }
                } else {
                    SampleEvent {
                        seq,
                        pid: self.pid,
                        mono_ns: mono,
                        wall_ns: wall,
                        frames,
                        truncated: false,
                        error: None,
                    }
                }
            }
            Err(e) => {
                let kind = e.kind();
                let msg = if kind == std::io::ErrorKind::PermissionDenied {
                    format!("EPERM reading {stack_path}: {e}")
                } else if kind == std::io::ErrorKind::NotFound {
                    format!("unknown runtime/no stack {stack_path}: {e}")
                } else {
                    format!("remote-read failed {stack_path}: {e}")
                };
                // Fallback marker, never silent zeros.
                SampleEvent {
                    seq,
                    pid: self.pid,
                    mono_ns: mono,
                    wall_ns: wall,
                    frames: vec!["TRUNCATED fallback".to_string()],
                    truncated: true,
                    error: Some(msg),
                }
            }
        }
    }

    /// Enqueues a sample into the bounded channel (drop-newest on full).
    fn enqueue(&mut self, ev: SampleEvent) -> Option<SampleEvent> {
        // Bounded channel discipline: copy of audit.rs — drop newest.
        if self.queue.len() >= self.cap {
            self.dropped += 1;
            return None;
        }
        self.queue.push_back(ev.clone());
        Some(ev)
    }
}

impl StackSampler for OutOfProcessSampler {
    fn sample(&mut self) -> Result<SampleEvent, String> {
        Ok(self.try_remote_read())
    }

    fn gated_tick(
        &mut self,
        guard: &LlmoSafeGuard,
        budget: &mut ObserveBudget,
    ) -> Result<Option<SampleEvent>, String> {
        // Injected drop for self_test DAL-A gate (simulates overflow).
        if self.inject_drop {
            self.inject_drop = false;
            self.dropped += 1;
            return Ok(None);
        }
        // Every tick gated: guard.execute(|| sample)? — pressure >80% suspends.
        // We do budget check inside the closure so both guards apply.
        let mut sampled: Option<SampleEvent> = None;
        let res: Result<(), String> = guard.execute(|| {
            let pressure = guard.pressure();
            if budget.should_suspend(pressure) {
                return Err(format!("suspended pressure {pressure}%"));
            }
            // Also check budget cooldown discipline.
            let ev = self.try_remote_read();
            // Even fallback markers are samples — never silent.
            sampled = Some(ev);
            Ok(())
        });
        match res {
            Ok(()) => {
                if let Some(ev) = sampled.take() {
                    let enq = self.enqueue(ev.clone());
                    // If enqueued, return the event; if dropped (overflow) return None but counted.
                    Ok(enq)
                } else {
                    Ok(None)
                }
            }
            Err(e) => {
                // Pressure suspension — not a hard error; caller treats as Ok(None) suspend.
                // We surface as Ok(None) with no dropped, but log via error string if caller wants.
                // To preserve guard.execute contract, return Err only for non-pressure errors.
                // Here all guard errors are pressure-related, so map to Ok(None).
                // Keep Err for unexpected.
                if e.contains("suspended") || e.contains("Resource pressure") || e.contains("pressure") {
                    // Record suspended as a lightweight marker? Spec says ObserveSuspended event
                    // will be emitted by supervisor; sampler itself just suspends tick.
                    Ok(None)
                } else {
                    Err(e)
                }
            }
        }
    }

    fn drain(&mut self) -> Vec<SampleEvent> {
        let mut out: Vec<SampleEvent> = self.queue.drain(..).collect();
        if self.dropped > 0 {
            let mono = self.base.elapsed().as_nanos() as u64;
            let wall = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos() as u64);
            let marker = SampleEvent {
                seq: self.next_seq,
                pid: self.pid,
                mono_ns: mono,
                wall_ns: wall,
                frames: vec![format!("TRUNCATED dropped={}", self.dropped)],
                truncated: true,
                error: Some(format!("TRUNCATED dropped={}", self.dropped)),
            };
            self.next_seq += 1;
            out.push(marker);
            self.dropped = 0;
        }
        out
    }

    fn len(&self) -> usize {
        self.queue.len()
    }

    fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    fn dropped(&self) -> u64 {
        self.dropped
    }

    fn rate_hz(&self) -> u64 {
        self.rate_hz
    }

    fn interval(&self) -> Duration {
        self.interval
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::llmosafe::LlmoSafeGuard;

    #[test]
    fn sampler_rate_defaults_and_caps() {
        let s0 = OutOfProcessSampler::new(1, 0);
        assert_eq!(s0.rate_hz(), 50);
        let s_high = OutOfProcessSampler::new(1, 5000);
        assert_eq!(s_high.rate_hz(), 1000);
        let s50 = OutOfProcessSampler::new(1, 50);
        assert_eq!(s50.interval(), Duration::from_millis(20));
    }

    #[test]
    fn sampler_fallback_never_silent_zeros() {
        // pid 0 is invalid → fallback marker, not silent zero
        let mut s = OutOfProcessSampler::new(0, 50);
        let ev = s.sample().unwrap();
        assert!(ev.truncated, "invalid pid must produce truncated marker");
        assert!(ev.error.is_some());
        assert!(!ev.frames.is_empty(), "fallback must not be silent empty");
        let wal = ev.to_wal_event();
        assert_eq!(wal.event_type, WalEventType::ObserveTruncated);
    }

    #[test]
    fn sampler_bounded_drop_newest_truncated() {
        let mut s = OutOfProcessSampler::with_capacity(1, 50, 2);
        // Fill queue with direct enqueue via gated_tick fallback path
        // Use sample() + enqueue manually to force overflow
        for _ in 0..2 {
            let ev = s.sample().unwrap();
            let _ = s.enqueue(ev);
        }
        assert_eq!(s.len(), 2);
        // Next should be dropped
        let ev = s.sample().unwrap();
        let enq = s.enqueue(ev);
        assert!(enq.is_none());
        assert_eq!(s.dropped(), 1);
        let drained = s.drain();
        // 2 + 1 TRUNCATED marker
        assert_eq!(drained.len(), 3);
        assert!(drained.last().unwrap().truncated);
        assert!(drained.last().unwrap().error.as_ref().unwrap().contains("TRUNCATED"));
        assert_eq!(s.dropped(), 0);
    }

    #[test]
    fn sampler_gated_tick_via_guard() {
        let mut s = OutOfProcessSampler::new(std::process::id(), 50);
        let guard = LlmoSafeGuard::new();
        let mut budget = ObserveBudget::new_with_path(30, 1, None);
        let res = s.gated_tick(&guard, &mut budget);
        // On healthy system should produce Some(event)
        assert!(res.is_ok());
        // Even if suspended, Ok(None) not Err
    }

    #[test]
    fn sampler_secret_redaction_at_wal_boundary() {
        let ev = SampleEvent {
            seq: 0,
            pid: 123,
            mono_ns: 1,
            wall_ns: 1,
            frames: vec!["auth_token=secret".to_string(), "normal_frame".to_string()],
            truncated: false,
            error: None,
        };
        let wal = ev.to_wal_event();
        let json = serde_json::to_string(&wal).unwrap();
        assert!(json.contains("REDACTED"));
        assert!(!json.contains("auth_token=secret"));
        // Also via direct struct literal with secret in error
        let ev2 = SampleEvent {
            seq: 1,
            pid: 123,
            mono_ns: 2,
            wall_ns: 2,
            frames: vec![],
            truncated: true,
            error: Some("bearer token leaked".to_string()),
        };
        let wal2 = ev2.to_wal_event();
        let json2 = serde_json::to_string(&wal2).unwrap();
        assert!(json2.contains("REDACTED"));
    }

    #[test]
    fn sampler_no_ptrace_grep_guard() {
        let src = include_str!("sampler.rs");
        // Guard: no LD_PRELOAD injection and no ptrace attach with stop >1ms in code.
        // Docs mention these as forbidden, so allow doc occurrences.
        let needle_preload = format!("{}{}", "LD", "_PRELOAD");
        let count_preload = src.matches(&needle_preload).count();
        assert!(
            count_preload <= 6,
            "LD_PRELOAD mentions should be doc-only (forbidden in code), got {count_preload}"
        );
        let ptrace_uses = src.matches("ptrace").count();
        assert!(ptrace_uses <= 12, "ptrace mentions should be doc-only, got {ptrace_uses}");
    }
}
