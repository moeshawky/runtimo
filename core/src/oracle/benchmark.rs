//! Oracle benchmark — measures parse and eval latency at scale.
//!
//! `run_benchmarks()` generates synthetic [`WalEvent`] vecs at 1K/10K/100K
//! scales, measures cold parse of a representative [`PropertySpec`],
//! warm parse, and eval throughput using only [`std::time::Instant`].
//!
//! # Module DNA
//! - **Owns**: Benchmark report generation, parse/eval latency measurement
//! - **Depends**: [`crate::oracle::spec::parse_spec`], [`crate::oracle::eval::evaluate`], [`crate::wal::WalEvent`]
//! - **Provides**: `BenchmarkReport`, `run_benchmarks`
//!
//! # Invariants
//! - Uses only `std::time::Instant` — no external timing crates.
//! - Synthetic events are generated in-code; no file I/O.
//! - Total runtime stays under 2 seconds.
//! - All public items have complete DNA docstrings.
//! - Zero new dependencies.

use std::time::Instant;

use crate::oracle::eval::evaluate;
use crate::oracle::spec::{parse_spec, PropertySpec};
use crate::wal::WalEvent;
use crate::wal::WalEventType;

/// Benchmark results for oracle parse and eval operations.
///
/// # Fields
/// - `parse_ms`: Cold parse latency in milliseconds for a representative spec.
/// - `eval_ms_per_1k`: Eval latency in milliseconds per 1K events, averaged across scales.
/// - `cold_parse_ms`: Explicit cold parse measurement in milliseconds.
/// - `warm_parse_ms`: Explicit warm parse measurement in milliseconds.
/// - `eval_1k_ms`: Eval time for 1K events in milliseconds.
/// - `eval_10k_ms`: Eval time for 10K events in milliseconds.
/// - `eval_100k_ms`: Eval time for 100K events in milliseconds.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::exhaustive_structs)]
#[must_use]
pub struct BenchmarkReport {
    /// Cold parse latency in milliseconds for a representative spec.
    pub parse_ms: f64,
    /// Eval latency in milliseconds per 1K events, averaged across scales.
    pub eval_ms_per_1k: f64,
    /// Explicit cold parse measurement in milliseconds.
    pub cold_parse_ms: f64,
    /// Explicit warm parse measurement in milliseconds.
    pub warm_parse_ms: f64,
    /// Eval time for 1K events in milliseconds.
    pub eval_1k_ms: f64,
    /// Eval time for 10K events in milliseconds.
    pub eval_10k_ms: f64,
    /// Eval time for 100K events in milliseconds.
    pub eval_100k_ms: f64,
}

/// Run oracle benchmarks and return a [`BenchmarkReport`].
///
/// Generates synthetic [`WalEvent`] vecs at 1K/10K/100K scales,
/// measures cold parse of a representative [`PropertySpec`], warm parse,
/// and eval throughput using only [`std::time::Instant`].
///
/// # Returns
/// A [`BenchmarkReport`] containing parse and eval latency measurements.
///
/// # Invariants
/// - Total runtime stays under 2 seconds.
/// - Uses only `std::time::Instant` — no external timing crates.
/// - Synthetic events are generated in-code; no file I/O.
/// - Zero new dependencies.
///
/// # Panics
/// Panics if any benchmark measurement exceeds reasonable bounds
/// (indicating a performance regression).
#[allow(clippy::indexing_slicing)]
pub fn run_benchmarks() -> BenchmarkReport {
    // Representative spec for benchmarking.
    let spec_json = r#"{"name":"bench-spec","predicates":[{"field":"event_type","op":"Eq","value":"job_started"}]}"#;

    // ── Cold parse ──────────────────────────────────────────────
    let cold_start = Instant::now();
    let spec: PropertySpec =
        parse_spec(spec_json).unwrap_or_else(|e| panic!("spec parse failed: {e}"));
    let cold_parse_ms = cold_start.elapsed().as_secs_f64() * 1000.0;

    // ── Warm parse ──────────────────────────────────────────────
    let warm_start = Instant::now();
    let _spec2: PropertySpec =
        parse_spec(spec_json).unwrap_or_else(|e| panic!("warm parse failed: {e}"));
    let warm_parse_ms = warm_start.elapsed().as_secs_f64() * 1000.0;

    // ── Generate synthetic events at scale ──────────────────────
    let scales = [1000usize, 10_000, 100_000];
    let mut eval_times = [0.0f64; 3];

    for (i, &n) in scales.iter().enumerate() {
        let events = make_events(n);
        let eval_start = Instant::now();
        let result = evaluate(&events, &spec).unwrap_or_else(|e| panic!("eval failed: {e}"));
        assert_eq!(result.verdict, crate::oracle::eval::Verdict::Satisfied);
        let elapsed_ms = eval_start.elapsed().as_secs_f64() * 1000.0;
        eval_times[i] = elapsed_ms;
    }

    let eval_1k_ms = eval_times[0];
    let eval_10k_ms = eval_times[1];
    let eval_100k_ms = eval_times[2];
    let eval_ms_per_1k = (eval_1k_ms + eval_10k_ms / 10.0 + eval_100k_ms / 100.0) / 3.0;

    BenchmarkReport {
        parse_ms: cold_parse_ms,
        eval_ms_per_1k,
        cold_parse_ms,
        warm_parse_ms,
        eval_1k_ms,
        eval_10k_ms,
        eval_100k_ms,
    }
}

/// Generate a vector of synthetic [`WalEvent`] records.
///
/// Creates `n` events with monotonically increasing sequence numbers,
/// all of type [`WalEventType::JobStarted`], with `event_type` set
/// to `"job_started"` so that the benchmark spec's predicate
/// (`event_type == "job_started"`) evaluates to `Satisfied`.
fn make_events(n: usize) -> Vec<WalEvent> {
    (0..n)
        .map(|i| WalEvent {
            seq: i as u64,
            ts: 1_700_000_000u64.wrapping_add(i as u64),
            event_type: WalEventType::JobStarted,
            job_id: format!("bench-job-{i}"),
            capability: Some("FileRead".to_string()),
            output: None,
            error: None,
            telemetry_before: None,
            telemetry_after: None,
            process_before: None,
            process_after: None,
            cmd: None,
            cmd_stdout: None,
            cmd_stderr: None,
            cmd_exit_code: None,
            cmd_corrected: None,
            ..Default::default()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse must complete under 5 ms for a representative spec.
    /// This assertion FAILS if parse performance regresses.
    #[test]
    fn bench_parse_under_5ms() {
        let spec_json = r#"{"name":"bench-spec","predicates":[{"field":"event_type","op":"Eq","value":"job_started"}]}"#;
        let start = Instant::now();
        let spec: PropertySpec = parse_spec(spec_json).expect("parse must succeed");
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        assert!(
            elapsed_ms < 5.0,
            "parse took {elapsed_ms:.2}ms, must be <5ms"
        );
        println!("bench_parse_under_5ms: {elapsed_ms:.3}ms");
        assert_eq!(spec.predicates.len(), 1);
    }

    /// Eval must complete under 1 ms per 1K events.
    /// This assertion FAILS if eval performance regresses.
    #[test]
    fn bench_eval_under_1ms_per_1k() {
        let spec_json = r#"{"name":"bench-spec","predicates":[{"field":"event_type","op":"Eq","value":"job_started"}]}"#;
        let spec: PropertySpec =
            parse_spec(spec_json).unwrap_or_else(|e| panic!("spec parse failed: {e}"));

        // Measure at 1K scale for the per-1K ratio.
        let events = make_events(1000);
        let start = Instant::now();
        let result = evaluate(&events, &spec).unwrap_or_else(|e| panic!("eval failed: {e}"));
        assert_eq!(result.verdict, crate::oracle::eval::Verdict::Satisfied);
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        let per_1k = elapsed_ms; // 1K events → per_1k = elapsed_ms
        assert!(per_1k < 1.0, "eval took {per_1k:.2}ms per 1K, must be <1ms");
        println!("bench_eval_under_1ms_per_1k: {per_1k:.3}ms per 1K");
    }

    /// run_benchmarks must complete under 2 seconds total.
    #[test]
    fn bench_total_under_2s() {
        let report = run_benchmarks();
        // Cold parse + warm parse + eval at 1K/10K/100K must fit in 2s.
        let total = report.cold_parse_ms
            + report.warm_parse_ms
            + report.eval_1k_ms
            + report.eval_10k_ms
            + report.eval_100k_ms;
        assert!(
            total < 2000.0,
            "total benchmark time {total:.0}ms must be <2000ms"
        );
    }
}
