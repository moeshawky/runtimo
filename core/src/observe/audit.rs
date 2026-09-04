//! Exhaustive low-volume audit hook for observe.
//!
//! Captures imports, spawns, raises, and dynamic loads — low-volume by
//! definition (not the hot loop). Uses a bounded channel (512) with
//! drop-newest + `TRUNCATED` marker semantics, never silent loss.
//!
//! No per-line tracer — grep-guard the CUT (no per-line analogue).
//!
//! # Nexus reuse
//! * `message.rs` envelope fields (`message_id`, `correlation_id`, etc.) —
//!   audit events carry `id` + `ts` + `kind` envelope, REDACTED tokens never logged.
//! * `bus.rs` bounded queue pattern — but with drop counting + `TRUNCATED`.

use crate::wal::{WalEvent, WalEventType};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Maximum audit channel capacity (bounded).
pub const AUDIT_CHANNEL_CAP: usize = 512;

/// Kind of audited operation (low-volume exhaustive).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::exhaustive_enums)]
pub enum AuditKind {
    /// Python `import` (or Rust `use` equivalent): `import os`.
    Import,
    /// Process spawn: `subprocess.Popen`, `fork`, `spawn`.
    Spawn,
    /// Exception raise.
    Raise,
    /// Dynamic load: `importlib.import_module`, `ctypes.CDLL`, `dlopen`.
    DynamicLoad,
    /// File open/write (low-volume audit).
    FileOp,
}

/// A single audit event (exhaustive, low-volume).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[allow(clippy::exhaustive_structs)]
pub struct AuditEvent {
    /// Monotonic id within this hook.
    pub id: u64,
    /// Unix nanoseconds.
    pub ts_ns: u64,
    /// Kind of operation.
    pub kind: AuditKind,
    /// Target (module name, command, exception type, library path) — REDACTED never logged.
    pub target: String,
    /// Caller location `file:line` if known.
    pub location: Option<String>,
    /// Whether this event is a TRUNCATED marker (overflow).
    #[serde(default)]
    pub truncated: bool,
}

/// Returns `REDACTED` if `input` contains a secret pattern case-insensitively.
///
/// Matches `auth_token`, `bearer`, or `api_key` (case-insensitive). This is
/// always-on in release (not `debug_assert!`), consistent with G-SEC.
fn redact_secret(input: &str) -> bool {
    let lower = input.to_ascii_lowercase();
    lower.contains("auth_token") || lower.contains("bearer") || lower.contains("api_key")
}

impl AuditEvent {
    /// Creates an audit event.
    ///
    /// `target` is truncated to 1 KiB and secrets are redacted — any target
    /// containing `auth_token`, `bearer`, or `api_key` (case-insensitive) is
    /// replaced with `REDACTED` before storage. This is always-on (not
    /// `debug_assert!`) so release builds also redact.
    #[must_use]
    pub fn new(id: u64, kind: AuditKind, target: impl Into<String>) -> Self {
        let ts_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        let mut t: String = target.into();
        if t.len() > 1024 {
            t.truncate(1024);
            t.push_str("...[truncated]");
        }
        // G-SEC: always-on redaction — replace secret-bearing targets with REDACTED.
        if redact_secret(&t) {
            t = "REDACTED".to_string();
        }
        Self {
            id,
            ts_ns,
            kind,
            target: t,
            location: None,
            truncated: false,
        }
    }

    /// Converts to a WAL event for bundle persistence.
    ///
    /// Redacts `target` again at this boundary: if the stored `target` (or
    /// `location`) contains a secret pattern, it is replaced with `REDACTED`
    /// before serialization, so WAL never contains secrets even if `AuditEvent`
    /// was constructed via direct struct literal.
    #[must_use]
    pub fn to_wal_event(&self) -> WalEvent {
        let et = if self.truncated {
            WalEventType::ObserveTruncated
        } else {
            WalEventType::ObserveBatch
        };
        let safe_target = if redact_secret(&self.target) {
            "REDACTED".to_string()
        } else {
            self.target.clone()
        };
        let safe_location = self.location.as_ref().map(|loc| {
            if redact_secret(loc) {
                "REDACTED".to_string()
            } else {
                loc.clone()
            }
        });
        WalEvent {
            seq: self.id,
            ts: self.ts_ns / 1_000_000_000,
            event_type: et,
            job_id: format!("audit-{}", self.id),
            output: Some(serde_json::json!({
                "kind": format!("{:?}", self.kind),
                "target": safe_target,
                "location": safe_location,
                "truncated": self.truncated,
            })),
            mono_ns: Some(self.ts_ns),
            wall_ns: Some(self.ts_ns),
            ..Default::default()
        }
    }
}

/// Inner state of the audit hook (bounded channel).
#[derive(Debug)]
struct AuditInner {
    queue: VecDeque<AuditEvent>,
    next_id: u64,
    dropped: u64,
}

/// Exhaustive low-volume audit hook.
///
/// Bounded channel 512, full → drop newest + `TRUNCATED` marker (never silent).
/// No per-line hook inside hot loop — this is the CUT grep-guard.
#[derive(Clone)]
pub struct AuditHook {
    inner: Arc<Mutex<AuditInner>>,
    cap: usize,
}

impl Default for AuditHook {
    fn default() -> Self {
        Self::new()
    }
}

impl AuditHook {
    /// Creates a hook with capacity 512.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(AUDIT_CHANNEL_CAP)
    }

    /// Creates with explicit capacity (for tests).
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(AuditInner {
                queue: VecDeque::with_capacity(cap),
                next_id: 0,
                dropped: 0,
            })),
            cap,
        }
    }

    /// Records an audit event.
    ///
    /// If the bounded channel is full, the new event is dropped and `dropped`
    /// increments. A `TRUNCATED` marker is inserted lazily on `drain`/`flush`
    /// — never silent.
    pub fn record(&self, kind: AuditKind, target: impl Into<String>) -> Option<AuditEvent> {
        let mut inner = self.inner.lock().unwrap();
        if inner.queue.len() >= self.cap {
            inner.dropped += 1;
            return None;
        }
        let id = inner.next_id;
        inner.next_id += 1;
        let ev = AuditEvent::new(id, kind, target);
        inner.queue.push_back(ev.clone());
        Some(ev)
    }

    /// Records with location.
    pub fn record_with_location(
        &self,
        kind: AuditKind,
        target: impl Into<String>,
        location: impl Into<String>,
    ) -> Option<AuditEvent> {
        let mut inner = self.inner.lock().unwrap();
        if inner.queue.len() >= self.cap {
            inner.dropped += 1;
            return None;
        }
        let id = inner.next_id;
        inner.next_id += 1;
        let mut ev = AuditEvent::new(id, kind, target);
        ev.location = Some(location.into());
        // Redact location as well at record time.
        if let Some(loc) = &ev.location {
            if redact_secret(loc) {
                ev.location = Some("REDACTED".to_string());
            }
        }
        inner.queue.push_back(ev.clone());
        Some(ev)
    }

    /// Drains all queued events, appending a `TRUNCATED` marker if drops occurred.
    ///
    /// The marker has `truncated: true` and `target` encodes the count.
    pub fn drain(&self) -> Vec<AuditEvent> {
        let mut inner = self.inner.lock().unwrap();
        let mut out: Vec<AuditEvent> = inner.queue.drain(..).collect();
        if inner.dropped > 0 {
            let mut marker = AuditEvent::new(inner.next_id, AuditKind::Raise, format!("TRUNCATED dropped={}", inner.dropped));
            marker.truncated = true;
            inner.next_id += 1;
            out.push(marker);
            inner.dropped = 0;
        }
        out
    }

    /// Returns current queue length.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().queue.len()
    }

    /// Whether queue is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns dropped count (not yet materialized as marker).
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.inner.lock().unwrap().dropped
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn audit_fixture_exact_count() {
        // Fixture: exactly 10 imports + 2 spawns + 1 raise + 1 dynamic load = 14 events.
        let hook = AuditHook::with_capacity(512);
        for i in 0..10 {
            hook.record(AuditKind::Import, format!("mod_{i}")).unwrap();
        }
        hook.record(AuditKind::Spawn, "subprocess.Popen(['ls'])").unwrap();
        hook.record(AuditKind::Spawn, "fork()").unwrap();
        hook.record(AuditKind::Raise, "ValueError").unwrap();
        hook.record(AuditKind::DynamicLoad, "ctypes.CDLL('libfoo.so')")
            .unwrap();
        let events = hook.drain();
        assert_eq!(events.len(), 14, "fixture must be exactly 14 events");
        assert_eq!(
            events.iter().filter(|e| e.kind == AuditKind::Import).count(),
            10
        );
        assert_eq!(
            events.iter().filter(|e| e.kind == AuditKind::Spawn).count(),
            2
        );
        assert_eq!(
            events.iter().filter(|e| e.kind == AuditKind::Raise).count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind == AuditKind::DynamicLoad)
                .count(),
            1
        );
        // Convert to WAL and verify no secrets.
        for ev in &events {
            let wal = ev.to_wal_event();
            let json = serde_json::to_string(&wal).unwrap();
            assert!(!json.to_lowercase().contains("auth_token"));
        }
    }

    #[test]
    fn audit_bounded_drop_newest_truncated() {
        let hook = AuditHook::with_capacity(4);
        for i in 0..4 {
            assert!(hook.record(AuditKind::Import, format!("m{i}")).is_some());
        }
        // Next should be dropped.
        assert!(hook.record(AuditKind::Import, "overflow").is_none());
        assert_eq!(hook.dropped(), 1);
        let drained = hook.drain();
        // 4 + 1 TRUNCATED marker
        assert_eq!(drained.len(), 5);
        assert!(drained.last().unwrap().truncated);
        assert!(drained.last().unwrap().target.contains("TRUNCATED"));
        assert_eq!(hook.dropped(), 0);
    }

    #[test]
    fn audit_no_per_line_grep_guard() {
        // Ensure this file contains no runtime tracer analogue.
        // Build needles via concatenation so the guard strings do not appear
        // literally in this file (grep-guard the CUT).
        let src = include_str!("audit.rs");
        let needle_a = format!("{}{}{}", "TraceEvent", "::", "Line");
        let needle_b = format!("{}{}{}", "line", "_", "tracer");
        assert!(
            !src.contains(&needle_a) || src.matches(&needle_a).count() <= 1,
            "audit.rs must not contain tracer"
        );
        // Count occurrences: allow zero; the concatenated value is not present
        // verbatim, so any occurrence would be from runtime code.
        let count_b = src.matches(&needle_b).count();
        assert_eq!(count_b, 0, "audit.rs must not contain tracer");
    }

    #[test]
    fn audit_secret_redaction_replaces_with_redacted() {
        // Always-on redaction: targets containing auth_token/bearer/api_key
        // (case-insensitive) must be replaced with REDACTED in both new() and to_wal_event().
        let patterns = ["auth_token=abc", "AUTH_TOKEN=xyz", "Bearer secret", "BEARER token", "api_key=123", "API_KEY=xyz"];
        for pat in patterns {
            let ev = AuditEvent::new(0, AuditKind::Import, pat);
            assert_eq!(
                ev.target, "REDACTED",
                "AuditEvent::new must redact pattern {:?}, got {:?}",
                pat, ev.target
            );
            let wal = ev.to_wal_event();
            let output = wal.output.as_ref().unwrap();
            assert_eq!(
                output["target"], "REDACTED",
                "to_wal_event must redact target for {:?}",
                pat
            );
            // Ensure raw secret never appears in serialized WAL
            let json = serde_json::to_string(&wal).unwrap_or_default();
            // Check via case-insensitive containment of the secret pattern prefix — wal json should not contain original
            // We assert output target == REDACTED already, so just guard.
            assert!(
                !json.to_lowercase().contains(&pat.to_lowercase()[..4]),
                "WAL json must not leak secret pattern"
            );
        }
        // Also verify via hook record path
        let hook = AuditHook::with_capacity(10);
        let ev = hook.record(AuditKind::Import, "my api_key leaked").unwrap();
        assert_eq!(ev.target, "REDACTED");
        // Verify to_wal_event path for direct struct literal (bypass new)
        let direct = AuditEvent {
            id: 99,
            ts_ns: 123,
            kind: AuditKind::Spawn,
            target: "auth_token_direct".to_string(),
            location: Some("bearer location".to_string()),
            truncated: false,
        };
        let wal2 = direct.to_wal_event();
        let out2 = wal2.output.as_ref().unwrap();
        assert_eq!(out2["target"], "REDACTED");
        assert_eq!(out2["location"], "REDACTED");
        // Non-secret targets must not be redacted
        let clean = AuditEvent::new(1, AuditKind::Import, "os");
        assert_eq!(clean.target, "os");
        assert_eq!(clean.to_wal_event().output.as_ref().unwrap()["target"], "os");
    }
}
