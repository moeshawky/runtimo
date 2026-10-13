//! Binary-level e2e for the oracle CLI surface (`runtimo oracle evaluate`).
//!
//! Spawns the compiled `runtimo` binary via `env!("CARGO_BIN_EXE_runtimo")`
//! and asserts the process contract — exit code, stdout JSON fields, stderr
//! disclosure — that unit tests inside `src/main.rs` cannot reach. Covers:
//! the facts skip/source-mismatch/shorthand-reject regression pins (F2/F1/F4
//! of the 0.10.2 oracle wave) and the WAL dropped-line disclosure across
//! both oracle surfaces — `oracle evaluate --wal` and
//! `observe --verify --properties` (DEBT-1).
//!
//! Isolation: every test writes its evidence into a unique temp directory
//! (`temp_dir` + pid + nanos) and points `RUNTIMO_STATE_DIR` /
//! `RUNTIMO_ALLOWED_PATHS` at it, so no test reads another test's files or
//! the developer's real state. The directory is removed when the test ends.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::unused_result_ok,
    clippy::indexing_slicing,
    clippy::redundant_clone
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

/// Property spec asserting the observed event type is `job_started`.
const JOB_STARTED_SPEC: &str =
    r#"{"name":"prop","predicates":[{"field":"event_type","op":"Eq","value":"job_started"}]}"#;

/// A minimal well-formed WAL event line (`WalEvent` requires seq/ts/type/job_id).
fn valid_wal_line() -> &'static str {
    r#"{"seq":1,"ts":1715800000,"type":"job_started","job_id":"j1"}"#
}

/// Creates a unique scratch directory for one test's evidence files.
///
/// # Outputs
/// Freshly created directory under the system temp dir; uniqueness comes
/// from the tag, process id, and nanosecond clock so parallel test
/// binaries never share state.
fn scratch_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = std::env::temp_dir().join(format!(
        "runtimo_oracle_e2e_{}_{}_{}",
        tag,
        std::process::id(),
        nanos
    ));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Runs the compiled CLI with the given args, isolated into `state_dir`.
///
/// # Inputs
/// `args`: CLI arguments after the binary name. `state_dir`: unique
/// directory exported as `RUNTIMO_STATE_DIR` and `RUNTIMO_ALLOWED_PATHS`.
///
/// # Outputs
/// Captured process output (exit status plus stdout/stderr, collected
/// separately by `std::process::Command` so stream interleaving cannot
/// corrupt assertions).
fn run_cli(args: &[&str], state_dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_runtimo"))
        .args(args)
        .env("RUNTIMO_STATE_DIR", state_dir)
        .env("RUNTIMO_ALLOWED_PATHS", state_dir)
        .output()
        .expect("spawn runtimo binary")
}

/// Parses stdout as JSON; panics with the raw text when it is not JSON.
fn stdout_json(output: &Output) -> serde_json::Value {
    let text = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {text}"))
}

/// stderr as a lossy string (disclosure assertions).
fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Regression pin (F2): a facts-vocabulary predicate on a never-observed
/// field skips with disclosure — exit 0, `skipped_count` non-zero,
/// `unobserved_fields` naming the field.
#[test]
fn facts_typo_field_skips_with_disclosure() {
    let dir = scratch_dir("facts_skip");
    let facts = dir.join("facts.jsonl");
    std::fs::write(&facts, "{\"run_id\":\"r1\",\"extra\":{\"drop_count\":5}}\n")
        .expect("write facts");

    let output = run_cli(
        &[
            "oracle",
            "evaluate",
            "--facts",
            facts.to_str().unwrap(),
            "--properties",
            r#"{"name":"skip","predicates":[{"field":"extra.typo_xyz","op":"Gt","value":0}]}"#,
            "--json",
        ],
        &dir,
    );

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        stderr_text(&output)
    );
    let json = stdout_json(&output);
    assert_eq!(json["verdict"], "Satisfied");
    assert!(
        json["skipped_count"].as_u64().unwrap() >= 1,
        "skipped_count must be >= 1: {json}"
    );
    assert!(
        json["unobserved_fields"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v == "extra.typo_xyz")),
        "unobserved_fields must contain the typo field: {json}"
    );
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("extra.typo_xyz"),
        "stderr must name the unobserved field: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Regression pin (F1): a WAL-vocabulary predicate on the facts source is
/// a source mismatch — exit 2, `Error` verdict, field named on stderr.
#[test]
fn facts_source_mismatch_is_error() {
    let dir = scratch_dir("facts_mismatch");
    let facts = dir.join("facts.jsonl");
    std::fs::write(&facts, "{\"run_id\":\"r1\"}\n").expect("write facts");

    let output = run_cli(
        &[
            "oracle",
            "evaluate",
            "--facts",
            facts.to_str().unwrap(),
            "--properties",
            r#"{"name":"mismatch","predicates":[{"field":"event_type","op":"Eq","value":"job_started"}]}"#,
            "--json",
        ],
        &dir,
    );

    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr: {}",
        stderr_text(&output)
    );
    let json = stdout_json(&output);
    assert_eq!(json["verdict"], "Error");
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("event_type"),
        "stderr must name the mismatched field: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Regression pin (F4): the two-token shorthand `"extra.drop_count =="`
/// fails parse (missing value) — exit 2, stderr says so.
#[test]
fn two_token_shorthand_rejected() {
    let dir = scratch_dir("shorthand");
    let facts = dir.join("facts.jsonl");
    std::fs::write(&facts, "{\"run_id\":\"r1\",\"extra\":{\"drop_count\":5}}\n")
        .expect("write facts");

    let output = run_cli(
        &[
            "oracle",
            "evaluate",
            "--facts",
            facts.to_str().unwrap(),
            "--properties",
            r#"{"name":"short","predicates":["extra.drop_count =="]}"#,
        ],
        &dir,
    );

    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr: {}",
        stderr_text(&output)
    );
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("missing value"),
        "stderr must mention the missing value: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Regression pin (DEBT-1): a WAL with one valid event and one corrupt
/// line keeps the verdict but discloses the drop — exit 0, JSON
/// `dropped_lines == 1`, stderr names the drop.
#[test]
fn corrupt_wal_discloses_dropped_lines() {
    let dir = scratch_dir("wal_corrupt");
    let wal = dir.join("wal.jsonl");
    std::fs::write(&wal, format!("{}\nGARBAGE NOT JSON\n", valid_wal_line()))
        .expect("write corrupt wal");

    let output = run_cli(
        &[
            "oracle",
            "evaluate",
            "--wal",
            wal.to_str().unwrap(),
            "--properties",
            JOB_STARTED_SPEC,
            "--json",
        ],
        &dir,
    );

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        stderr_text(&output)
    );
    let json = stdout_json(&output);
    assert_eq!(json["verdict"], "Satisfied");
    assert_eq!(
        json["dropped_lines"].as_u64(),
        Some(1),
        "dropped_lines must be 1: {json}"
    );
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("dropped"),
        "stderr must disclose the drop: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Disclosure pin (DEBT-1 sibling): `observe --verify --properties` over
/// a corrupt WAL names the drop on stderr and reports `dropped_lines` in
/// JSON; the clean control reports zero drops with no warning. Exit code
/// is deliberately not asserted — `--verify` legitimately exits 1 on a raw
/// WAL (bundle admissibility fails); only the disclosure is pinned.
#[test]
fn observe_verify_discloses_dropped_lines() {
    let dir = scratch_dir("verify_dropped");
    let wal = dir.join("wal.jsonl");

    // Corrupt: one valid event plus one unparseable line.
    std::fs::write(&wal, format!("{}\nGARBAGE NOT JSON\n", valid_wal_line()))
        .expect("write corrupt wal");
    let output = run_cli(
        &[
            "observe",
            "--verify",
            wal.to_str().unwrap(),
            "--properties",
            JOB_STARTED_SPEC,
            "--json",
        ],
        &dir,
    );
    let json = stdout_json(&output);
    assert!(
        json["dropped_lines"].as_u64().unwrap() >= 1,
        "dropped_lines must be >= 1: {json}"
    );
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("current + archived files"),
        "stderr must scope the drop to the current + archived files: {stderr}"
    );

    // Negative control: same property over a clean WAL — zero drops, no
    // drop warning on stderr.
    std::fs::write(&wal, format!("{}\n", valid_wal_line())).expect("write clean wal");
    let clean = run_cli(
        &[
            "observe",
            "--verify",
            wal.to_str().unwrap(),
            "--properties",
            JOB_STARTED_SPEC,
            "--json",
        ],
        &dir,
    );
    let clean_json = stdout_json(&clean);
    assert_eq!(
        clean_json["dropped_lines"].as_u64(),
        Some(0),
        "dropped_lines must be 0 on clean input: {clean_json}"
    );
    let clean_stderr = stderr_text(&clean);
    assert!(
        !clean_stderr.contains("current + archived files"),
        "clean WAL must not emit a drop warning: {clean_stderr}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Negative control: the same property over a clean WAL reports zero drops
/// and emits no warning — absence proof for the DEBT-1 disclosure.
#[test]
fn clean_wal_reports_zero_drops_silently() {
    let dir = scratch_dir("wal_clean");
    let wal = dir.join("wal.jsonl");
    std::fs::write(&wal, format!("{}\n", valid_wal_line())).expect("write clean wal");

    let output = run_cli(
        &[
            "oracle",
            "evaluate",
            "--wal",
            wal.to_str().unwrap(),
            "--properties",
            JOB_STARTED_SPEC,
            "--json",
        ],
        &dir,
    );

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        stderr_text(&output)
    );
    let json = stdout_json(&output);
    assert_eq!(json["verdict"], "Satisfied");
    assert_eq!(
        json["dropped_lines"].as_u64(),
        Some(0),
        "dropped_lines must be 0 on clean input: {json}"
    );
    let stderr = stderr_text(&output);
    assert!(
        !stderr.contains("dropped"),
        "clean WAL must not emit a drop warning: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
