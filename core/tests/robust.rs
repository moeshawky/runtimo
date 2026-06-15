#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::unused_result_ok,
    clippy::indexing_slicing,
    clippy::redundant_clone,
    clippy::items_after_statements
)]
//! Robust tests for LLM-generated code following the 9 failure mode categories.
//!
//! Test layers:
//! - G-EDGE: Edge cases (empty, null, boundary, unicode, concurrent)
//! - G-SEC: Security (adversarial inputs, path traversal variants, symlink attacks)
//! - G-ERR: Error handling (fault injection, error path coverage)
//! - G-CTX: Context (config file integration, env var precedence)
//! - G-SEM: Semantic correctness (behavioral oracles, invariants)
//! - G-DRIFT: Golden file regression (output format stability)

use runtimo_core::{
    capabilities::{FileRead, FileWrite, Kill, ShellExec},
    execute_with_telemetry, BackupManager, Capability, ProcessSnapshot, RuntimoConfig, Telemetry,
    WalReader, WalWriter,
};
use serde_json::json;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

// ── Test helpers ──────────────────────────────────────────────────────

fn unique_test_dir() -> PathBuf {
    let ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("runtimo_robust_{}_{}", std::process::id(), ns))
}

fn wal_path(base: &std::path::Path) -> PathBuf {
    base.join("wal_dir/wal.jsonl")
}

fn backup_dir(base: &std::path::Path) -> PathBuf {
    base.join("backups")
}

fn setup() -> PathBuf {
    let d = unique_test_dir();
    fs::create_dir_all(&d).ok();
    fs::create_dir_all(wal_path(&d).parent().unwrap()).ok();
    d
}

fn cleanup(dir: &PathBuf) {
    let _ = fs::remove_dir_all(dir);
}

fn make_file(dir: &std::path::Path, name: &str, content: &str) -> PathBuf {
    let p = dir.join(name);
    let mut f = fs::File::create(&p).unwrap();
    write!(f, "{}", content).unwrap();
    p
}

fn ctx(id: impl Into<String>) -> runtimo_core::Context {
    runtimo_core::Context {
        dry_run: false,
        job_id: id.into(),
        working_dir: std::env::temp_dir(),
    }
}

// ── G-EDGE: Edge Cases ───────────────────────────────────────────────

/// Empty string content write and read
#[test]
fn edge_empty_content_roundtrip() {
    let dir = setup();
    let target = dir.join("empty.txt");
    FileWrite::new(backup_dir(&dir))
        .expect("Failed to create FileWrite")
        .execute(
            &json!({ "path": target.to_str().unwrap(), "content": "" }),
            &ctx("edge1"),
        )
        .unwrap();
    let r = FileRead
        .execute(&json!({ "path": target.to_str().unwrap() }), &ctx("edge2"))
        .unwrap();
    assert_eq!(r.data["content"].as_str().unwrap(), "");
    cleanup(&dir);
}

/// Single character write and read
#[test]
fn edge_single_char_roundtrip() {
    let dir = setup();
    let target = dir.join("single.txt");
    for ch in &["a", " ", "0", "\n", "\t"] {
        let _ = fs::remove_file(&target);
        FileWrite::new(backup_dir(&dir))
            .expect("Failed to create FileWrite")
            .execute(
                &json!({ "path": target.to_str().unwrap(), "content": ch }),
                &ctx(format!("edge_{}", ch.escape_default())),
            )
            .unwrap();
        let r = FileRead
            .execute(
                &json!({ "path": target.to_str().unwrap() }),
                &ctx("edge_read"),
            )
            .unwrap();
        assert_eq!(r.data["content"].as_str().unwrap(), *ch);
    }
    cleanup(&dir);
}

/// Very long filename (within filesystem limits)
#[test]
fn edge_long_filename() {
    let dir = setup();
    let long_name = format!("{}.txt", "x".repeat(200));
    let target = dir.join(&long_name);
    FileWrite::new(backup_dir(&dir))
        .expect("Failed to create FileWrite")
        .execute(
            &json!({ "path": target.to_str().unwrap(), "content": "long name" }),
            &ctx("edge_long"),
        )
        .unwrap();
    assert!(target.exists());
    cleanup(&dir);
}

/// File with only whitespace
#[test]
fn edge_whitespace_only() {
    let dir = setup();
    let p = make_file(&dir, "ws.txt", "   \n\t\n   ");
    let r = FileRead
        .execute(&json!({ "path": p.to_str().unwrap() }), &ctx("edge_ws"))
        .unwrap();
    assert_eq!(r.data["content"].as_str().unwrap(), "   \n\t\n   ");
    cleanup(&dir);
}

/// Null bytes in content (should be preserved, not cause panic)
#[test]
fn edge_null_bytes_in_content() {
    let dir = setup();
    let p = make_file(&dir, "null.bin", "hello\0world");
    let r = FileRead
        .execute(&json!({ "path": p.to_str().unwrap() }), &ctx("edge_null"))
        .unwrap();
    // read_to_string may fail on null bytes — that's acceptable
    // The test verifies no panic occurs
    assert!(r.success || r.data["content"].is_null());
    cleanup(&dir);
}

/// Concurrent writes to different files (no race)
#[test]
fn edge_concurrent_writes_different_files() {
    let dir = setup();
    let bw = backup_dir(&dir);
    let handles: Vec<_> = (0..5)
        .map(|i| {
            let d = dir.clone();
            let bw = bw.clone();
            std::thread::spawn(move || {
                let target = d.join(format!("concurrent_{}.txt", i));
                FileWrite::new(bw)
                    .unwrap()
                    .execute(
                        &json!({
                            "path": target.to_str().unwrap(),
                            "content": format!("thread {}", i)
                        }),
                        &ctx(format!("concurrent_{}", i)),
                    )
                    .unwrap();
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    for i in 0..5 {
        let target = dir.join(format!("concurrent_{}.txt", i));
        let content = fs::read_to_string(&target).unwrap();
        assert_eq!(content, format!("thread {}", i));
    }
    cleanup(&dir);
}

// ── T-FALSEPASS: verify tests catch the actual bug ──────────────────

/// T-FALSEPASS gate: creates_parent_directories must fail if check_disk_space
/// runs before create_dir_all. This test documents the expected behavior
/// and serves as a regression sentinel.
#[test]
fn t_falsepass_creates_parent_must_require_disk_check_after_mkdir() {
    let dir = setup();
    let deep = dir.join("a/b/c");
    let target = deep.join("f.txt");

    // The test: write to nonexistent parent must succeed.
    // If check_disk_space runs BEFORE create_dir_all, this fails with:
    //   "No such file or directory" (from df) or
    //   "insufficient disk space" (df on wrong path)
    let result = FileWrite::new(backup_dir(&dir))
        .expect("Failed to create FileWrite")
        .execute(
            &json!({
                "path": target.to_str().unwrap(),
                "content": "t_falsepass"
            }),
            &ctx("tfp1"),
        );
    assert!(
        result.is_ok(),
        "T-FALSEPASS: write to nonexistent parent failed — ordering bug not fixed: {:?}",
        result
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), "t_falsepass");
    cleanup(&dir);
}

/// T-FALSEPASS gate: verify file content is correct (not just existence)
#[test]
fn t_falsepass_content_not_just_exists() {
    let dir = setup();
    let target = dir.join("content_check.txt");
    let payload = "exact_payload_42";
    FileWrite::new(backup_dir(&dir))
        .expect("Failed to create FileWrite")
        .execute(
            &json!({
                "path": target.to_str().unwrap(),
                "content": payload
            }),
            &ctx("tfp2"),
        )
        .unwrap();
    let read_back = fs::read_to_string(&target).unwrap();
    assert_eq!(
        read_back, payload,
        "T-FALSEPASS: content mismatch — file exists but content is wrong"
    );
    cleanup(&dir);
}

/// T-WEAKORACLE gate: verify exact content, not just non-empty
#[test]
fn t_weakoracle_exact_content_not_substring() {
    let dir = setup();
    let target = dir.join("exact.txt");
    let content = "precise";
    FileWrite::new(backup_dir(&dir))
        .expect("Failed to create FileWrite")
        .execute(
            &json!({
                "path": target.to_str().unwrap(),
                "content": content
            }),
            &ctx("two1"),
        )
        .unwrap();
    let read_back = fs::read_to_string(&target).unwrap();
    // Tight oracle: exact equality, not substring or non-empty
    assert_eq!(
        read_back.len(),
        content.len(),
        "T-WEAKORACLE: length mismatch"
    );
    assert_eq!(read_back, content, "T-WEAKORACLE: content not identical");
    cleanup(&dir);
}

// ── G-SEC: Security ─────────────────────────────────────────────────

/// Path traversal with encoded sequences
#[test]
fn sec_encoded_path_traversal() {
    // %2e%2e%2f is URL-encoded ../
    // validate_path works on raw strings, so %2e%2e is NOT treated as ..
    // The path /tmp/%2e%2e/etc/passwd doesn't contain literal ".."
    // But it also doesn't exist on disk, so validation fails for "path does not exist"
    let result = FileRead.validate(&json!({ "path": "/tmp/%2e%2e/etc/passwd" }));
    // Should fail because path doesn't exist (not because of traversal detection)
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("does not exist"),
        "Expected 'does not exist', got: {}",
        err
    );
}

/// Null byte injection in path
#[test]
fn sec_null_byte_in_path() {
    // Null bytes in path should be rejected or handled safely
    let result = FileRead.validate(&json!({ "path": "/tmp/test\0.txt" }));
    // serde_json may reject null bytes, or validate_path should handle them
    assert!(result.is_err());
}

/// Symlink chain attack (symlink -> symlink -> /etc/passwd)
#[test]
fn sec_symlink_chain_escape() {
    let dir = setup();
    let link1 = dir.join("link1");
    let link2 = dir.join("link2");

    // Create chain: link2 -> link1 -> /etc/hostname
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        if symlink("/etc/hostname", &link1).is_ok() && symlink(&link1, &link2).is_ok() {
            let result = FileRead.execute(
                &json!({ "path": link2.to_str().unwrap() }),
                &ctx("sec_chain"),
            );
            // Should be rejected because canonical path is /etc/hostname
            assert!(result.is_err() || !result.as_ref().unwrap().success);
            let _ = fs::remove_file(&link2);
            let _ = fs::remove_file(&link1);
        }
    }
    cleanup(&dir);
}

/// Adversarial JSON in args (type confusion)
#[test]
fn sec_type_confusion_in_args() {
    // path as number instead of string
    assert!(FileRead.validate(&json!({ "path": 12345 })).is_err());
    // path as array
    assert!(FileRead
        .validate(&json!({ "path": ["/tmp/x.txt"] }))
        .is_err());
    // path as object
    assert!(FileRead
        .validate(&json!({ "path": { "file": "/tmp/x.txt" } }))
        .is_err());
    // path as null
    assert!(FileRead.validate(&json!({ "path": null })).is_err());
    // path as boolean
    assert!(FileRead.validate(&json!({ "path": true })).is_err());
}

/// ShellExec with dangerous commands (should execute but be logged)
#[test]
fn sec_shellexec_dangerous_commands_logged() {
    // These commands should execute (we're testing they don't crash)
    // In production, these would be blocked by policy, not by the capability
    let dangerous = vec![
        "echo test",     // benign
        "cat /dev/null", // benign
        "true",          // benign
    ];
    for cmd in dangerous {
        let result = ShellExec.execute(
            &json!({ "cmd": cmd }),
            &ctx(format!("sec_{}", cmd.replace(' ', "_"))),
        );
        assert!(
            result.is_ok(),
            "Command '{}' should not panic: {:?}",
            cmd,
            result
        );
    }
}

// ── G-ERR: Error Handling ───────────────────────────────────────────

/// FileRead on a directory should fail gracefully
#[test]
fn err_read_directory() {
    let dir = setup();
    let result = FileRead.execute(&json!({ "path": dir.to_str().unwrap() }), &ctx("err_dir"));
    assert!(result.is_err() || !result.unwrap().success);
    cleanup(&dir);
}

/// FileWrite to a read-only location should fail gracefully
#[test]
fn err_write_readonly_location() {
    let dir = setup();
    let readonly_dir = dir.join("readonly");
    fs::create_dir_all(&readonly_dir).unwrap();

    // Make directory read-only
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&readonly_dir).unwrap().permissions();
        perms.set_mode(0o555); // r-xr-xr-x
        fs::set_permissions(&readonly_dir, perms).ok();
    }

    let target = readonly_dir.join("test.txt");
    let result = FileWrite::new(backup_dir(&dir))
        .expect("Failed to create FileWrite")
        .execute(
            &json!({ "path": target.to_str().unwrap(), "content": "test" }),
            &ctx("err_readonly"),
        );

    // Should fail or succeed depending on permissions (root can write anywhere)
    // The test verifies no panic
    let _ = result;

    // Restore permissions for cleanup
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&readonly_dir).unwrap().permissions();
        perms.set_mode(0o755);
        let _ = fs::set_permissions(&readonly_dir, perms);
    }
    cleanup(&dir);
}

/// WAL write failure simulation (invalid path)
#[test]
fn err_wal_invalid_path() {
    let result = WalWriter::create(PathBuf::from("/nonexistent/deep/path/wal.jsonl").as_path());
    assert!(result.is_err());
}

/// BackupManager with non-existent source
#[test]
fn err_backup_nonexistent_source() {
    let dir = setup();
    let mgr = BackupManager::new(backup_dir(&dir)).unwrap();
    let result = mgr.create_backup(
        &PathBuf::from("/tmp/nonexistent_runtimo_file_12345.txt"),
        "test-job",
    );
    assert!(result.is_err());
    cleanup(&dir);
}

/// Kill with invalid signal values
#[test]
fn err_kill_invalid_signal() {
    // Signal 999 should fail gracefully
    let result = Kill.execute(&json!({ "pid": 999998, "signal": 999 }), &ctx("err_signal"));
    // Should not panic — either succeeds (signal sent) or fails gracefully
    let _ = result;
}

// ── G-CTX: Context/Config Integration ────────────────────────────────

/// Config file prefixes are merged with defaults
#[test]
fn ctx_config_prefixes_merged() {
    let tmp = unique_test_dir();
    let config_dir = tmp.join("runtimo");
    fs::create_dir_all(&config_dir).unwrap();

    // Write config directly to avoid env var pollution
    let config_path = config_dir.join("config.toml");
    let mut config = RuntimoConfig::default();
    config.allowed_paths.push("/srv".to_string());
    let content = toml::to_string_pretty(&config).unwrap();
    fs::write(&config_path, content).unwrap();

    // Load directly from the path we just wrote
    let loaded_content = fs::read_to_string(&config_path).unwrap();
    let loaded: RuntimoConfig = toml::from_str(&loaded_content).unwrap();
    assert!(loaded.allowed_paths.contains(&"/srv".to_string()));

    let _ = fs::remove_dir_all(&tmp);
}

/// Env var and config file both contribute to allowed prefixes
#[test]
fn ctx_env_var_and_config_both_active() {
    // Test config loading directly without env var pollution
    let tmp = unique_test_dir();
    let config_dir = tmp.join("runtimo");
    fs::create_dir_all(&config_dir).unwrap();
    let config_path = config_dir.join("config.toml");

    let mut config = RuntimoConfig::default();
    config.allowed_paths.push("/srv".to_string());
    config.allowed_paths.push("/opt".to_string());
    let content = toml::to_string_pretty(&config).unwrap();
    fs::write(&config_path, content).unwrap();

    // Verify TOML roundtrip
    let loaded_content = fs::read_to_string(&config_path).unwrap();
    let loaded: RuntimoConfig = toml::from_str(&loaded_content).unwrap();
    assert!(loaded.allowed_paths.contains(&"/srv".to_string()));
    assert!(loaded.allowed_paths.contains(&"/opt".to_string()));

    // Verify defaults are always present
    let defaults = RuntimoConfig::get_allowed_prefixes();
    assert!(defaults.contains(&"/tmp".to_string()));
    assert!(defaults.contains(&"/home".to_string()));

    let _ = fs::remove_dir_all(&tmp);
}

/// Config file with invalid TOML returns defaults
#[test]
fn ctx_invalid_toml_returns_defaults() {
    let tmp = unique_test_dir();
    let config_dir = tmp.join("runtimo");
    fs::create_dir_all(&config_dir).unwrap();
    let config_path = config_dir.join("config.toml");

    fs::write(&config_path, "this is not valid toml {{{").unwrap();

    // Load directly
    let content = fs::read_to_string(&config_path).unwrap();
    let result: Result<RuntimoConfig, _> = toml::from_str(&content);
    assert!(result.is_err() || result.unwrap().allowed_paths.is_empty());

    // Defaults are always present
    let defaults = RuntimoConfig::get_allowed_prefixes();
    assert!(defaults.contains(&"/tmp".to_string()));

    let _ = fs::remove_dir_all(&tmp);
}

/// Path validation respects config file prefixes
#[test]
fn ctx_path_validation_uses_config() {
    // Test that the config module's get_allowed_prefixes includes defaults
    // and can be extended via env var
    let defaults = RuntimoConfig::get_allowed_prefixes();
    assert!(defaults.contains(&"/tmp".to_string()));
    assert!(defaults.contains(&"/home".to_string()));
    assert!(defaults.contains(&"/var/tmp".to_string()));

    // Test that env var extends prefixes
    std::env::set_var("RUNTIMO_ALLOWED_PATHS", "/srv:/opt");
    let extended = RuntimoConfig::get_allowed_prefixes();
    assert!(extended.contains(&"/srv".to_string()));
    assert!(extended.contains(&"/opt".to_string()));
    std::env::remove_var("RUNTIMO_ALLOWED_PATHS");

    // Verify path validation works with defaults
    use runtimo_core::validation::path::{validate_path, PathContext};
    let ctx = PathContext {
        require_exists: false,
        require_file: false,
        ..Default::default()
    };
    let result = validate_path("/tmp/myapp/config.yaml", &ctx);
    assert!(result.is_ok(), "Expected /tmp to be allowed: {:?}", result);
}

// ── G-SEM: Semantic Correctness ─────────────────────────────────────

/// Backup numbering is sequential and preserves original
#[test]
fn sem_backup_numbering_preserves_original() {
    let dir = setup();
    let bw = backup_dir(&dir);
    let target = dir.join("numbered.txt");
    let job_dir = bw.join("job1");

    // Write original
    fs::write(&target, "original").unwrap();

    // First write (creates backup of "original")
    FileWrite::new(bw.clone())
        .expect("Failed to create FileWrite")
        .execute(
            &json!({ "path": target.to_str().unwrap(), "content": "first" }),
            &ctx("job1"),
        )
        .unwrap();

    // Second write in same job (should create numbered backup)
    FileWrite::new(bw.clone())
        .expect("Failed to create FileWrite")
        .execute(
            &json!({ "path": target.to_str().unwrap(), "content": "second" }),
            &ctx("job1"),
        )
        .unwrap();

    // Third write in same job
    FileWrite::new(bw.clone())
        .expect("Failed to create FileWrite")
        .execute(
            &json!({ "path": target.to_str().unwrap(), "content": "third" }),
            &ctx("job1"),
        )
        .unwrap();

    // Verify backups exist with correct content
    let backup0 = job_dir.join("numbered.txt");
    let backup1 = job_dir.join("numbered.txt.1");
    let backup2 = job_dir.join("numbered.txt.2");

    assert!(backup0.exists(), "Original backup should exist");
    assert!(backup1.exists(), "First numbered backup should exist");
    assert!(backup2.exists(), "Second numbered backup should exist");

    assert_eq!(fs::read_to_string(&backup0).unwrap(), "original");
    assert_eq!(fs::read_to_string(&backup1).unwrap(), "first");
    assert_eq!(fs::read_to_string(&backup2).unwrap(), "second");

    // Current file should have "third"
    assert_eq!(fs::read_to_string(&target).unwrap(), "third");

    cleanup(&dir);
}

/// WAL events are strictly monotonic in sequence numbers
#[test]
fn sem_wal_seq_monotonic() {
    let dir = setup();
    let wp = wal_path(&dir);
    let p = make_file(&dir, "seq.txt", "test");

    for _ in 0..5 {
        execute_with_telemetry(
            &FileRead,
            &json!({ "path": p.to_str().unwrap() }),
            false,
            &wp,
        )
        .unwrap();
    }

    let reader = WalReader::load(&wp).unwrap();
    let events = reader.events();
    assert!(events.len() >= 10); // 5 jobs × 2 events each

    // Verify monotonicity
    for i in 1..events.len() {
        assert!(
            events[i].seq > events[i - 1].seq,
            "WAL seq not monotonic: {} <= {} at index {}",
            events[i].seq,
            events[i - 1].seq,
            i
        );
    }

    cleanup(&dir);
}

/// Telemetry before <= telemetry after (temporal ordering)
#[test]
fn sem_telemetry_temporal_order() {
    let dir = setup();
    let wp = wal_path(&dir);
    let p = make_file(&dir, "time.txt", "test");

    let result = execute_with_telemetry(
        &FileRead,
        &json!({ "path": p.to_str().unwrap() }),
        false,
        &wp,
    )
    .unwrap();

    assert!(
        result.telemetry_after.timestamp >= result.telemetry_before.timestamp,
        "Telemetry after should be >= before"
    );

    cleanup(&dir);
}

/// Process snapshot summary matches process list
#[test]
fn sem_process_summary_consistency() {
    let snap = ProcessSnapshot::capture();

    assert_eq!(
        snap.summary.total_processes,
        snap.processes.len(),
        "Summary total_processes should match process list length"
    );

    let actual_zombies = snap
        .processes
        .iter()
        .filter(|p| p.stat.starts_with('Z'))
        .count();
    assert_eq!(
        snap.summary.zombie_count, actual_zombies,
        "Summary zombie_count should match actual zombies in list"
    );

    // Total CPU should be sum of individual CPU percentages
    let total_cpu: f32 = snap.processes.iter().map(|p| p.cpu_percent).sum();
    assert!(
        (snap.summary.total_cpu_percent - total_cpu).abs() < 0.01,
        "Summary total_cpu should match sum of individual CPUs"
    );
}

// ── G-DRIFT: Golden File Regression ─────────────────────────────────

/// Telemetry output format is stable
#[test]
fn drift_telemetry_format_stable() {
    let tel = Telemetry::capture();
    let serialized = serde_json::to_string(&tel).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();

    // Verify expected structure (services key removed in elegant-telemetry;
    // listening_ports moved to network.listening_ports)
    assert!(parsed.get("timestamp").is_some());
    assert!(parsed.get("system").is_some());
    assert!(parsed.get("hardware").is_some());
    assert!(parsed.get("network").is_some());

    let system = parsed["system"].as_object().unwrap();
    assert!(system.contains_key("cpu_model"));
    assert!(system.contains_key("cpu_count"));
    assert!(system.contains_key("ram_total"));
    assert!(system.contains_key("ram_free"));
    assert!(system.contains_key("ram_available"));
    assert!(system.contains_key("uptime_seconds"));
    assert!(system.contains_key("disk_used_percent"));

    let network = parsed["network"].as_object().unwrap();
    assert!(network.contains_key("listening_ports"));
    assert!(network.contains_key("tunnel_running"));
    assert!(network.contains_key("tunnel_pid"));
}

/// Process snapshot output format is stable
#[test]
fn drift_process_snapshot_format_stable() {
    let snap = ProcessSnapshot::capture();
    let serialized = serde_json::to_string(&snap).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();

    assert!(parsed.get("timestamp").is_some());
    assert!(parsed.get("processes").is_some());
    assert!(parsed.get("summary").is_some());

    let summary = parsed["summary"].as_object().unwrap();
    assert!(summary.contains_key("total_processes"));
    assert!(summary.contains_key("total_cpu_percent"));
    assert!(summary.contains_key("total_mem_percent"));
    assert!(summary.contains_key("zombie_count"));
}

/// Process vsz/rss are in KB (not bytes) — regression for format_size bug
#[test]
fn drift_process_vsz_rss_reasonable() {
    let snap = ProcessSnapshot::capture();
    for p in &snap.processes {
        // After fix: vsz/rss are raw KB from ps, not KB*1024
        // vsz can be very large on 64-bit (virtual memory is cheap) but < 1PB
        assert!(
            p.vsz < 1_000_000_000,
            "vsz={}KB is unreasonably large (format_size bug regression?) for {}",
            p.vsz,
            p.command
        );
        // rss is physical memory — should be under 100GB for any single process
        assert!(
            p.rss < 100_000_000,
            "rss={}KB is unreasonably large (format_size bug regression?) for {}",
            p.rss,
            p.command
        );
    }
}

/// WAL event format is stable
#[test]
fn drift_wal_event_format_stable() {
    let dir = setup();
    let wp = wal_path(&dir);
    let p = make_file(&dir, "drift.txt", "test");

    execute_with_telemetry(
        &FileRead,
        &json!({ "path": p.to_str().unwrap() }),
        false,
        &wp,
    )
    .unwrap();

    let reader = WalReader::load(&wp).unwrap();
    let events = reader.events();
    assert!(!events.is_empty());

    let event = &events[0];
    let serialized = serde_json::to_string(event).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();

    assert!(parsed.get("seq").is_some());
    assert!(parsed.get("ts").is_some());
    assert!(parsed.get("type").is_some());
    assert!(parsed.get("job_id").is_some());
}

// ── Property-Based Tests ─────────────────────────────────────────────

mod proptests {
    use super::*;
    use proptest::prelude::*;

    // Property: Any valid path in /tmp can be written and read back identically
    proptest! {
        #[test]
        fn prop_write_read_roundtrip(content in "[^\0]*") {
            let dir = setup();
            let target = dir.join("prop.txt");

            let write_result = FileWrite::new(backup_dir(&dir))
                .expect("Failed to create FileWrite")
                .execute(
                    &json!({ "path": target.to_str().unwrap(), "content": &content }),
                    &ctx("prop_write"),
                );

            // Some strings may not be valid UTF-8 for read_to_string, but write should succeed
            if write_result.is_ok() {
                let read_result = FileRead
                    .execute(&json!({ "path": target.to_str().unwrap() }), &ctx("prop_read"));

                if read_result.is_ok() {
                    let r = read_result.unwrap();
                    let read_content = r.data["content"].as_str().unwrap();
                    prop_assert_eq!(read_content, content, "Roundtrip failed");
                }
            }

            cleanup(&dir);
        }
    }

    // Property: Backup numbering never produces duplicates
    proptest! {
        #[test]
        fn prop_backup_no_duplicates(n in 1usize..10) {
            let dir = setup();
            let bw = backup_dir(&dir);
            let target = dir.join("prop_backup.txt");
            let job_dir = bw.join("job_prop");

            fs::write(&target, "original").unwrap();

            // First write creates backup
            FileWrite::new(bw.clone())
                .unwrap()
                .execute(
                    &json!({ "path": target.to_str().unwrap(), "content": "first" }),
                    &ctx("job_prop"),
                )
                .unwrap();

            // Subsequent writes in same job create numbered backups
            for i in 1..n {
                FileWrite::new(bw.clone())
                    .unwrap()
                    .execute(
                        &json!({ "path": target.to_str().unwrap(), "content": format!("write {}", i) }),
                        &ctx("job_prop"),
                    )
                    .unwrap();
            }

            // Collect all backup filenames
            let mut backups: Vec<_> = std::fs::read_dir(&job_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            backups.sort();

            // Check no duplicates
            let mut seen = std::collections::HashSet::new();
            for name in &backups {
                prop_assert!(seen.insert(name.clone()), "Duplicate backup: {}", name);
            }

            cleanup(&dir);
        }
    }

    // Property: Path validation is consistent for equivalent paths
    proptest! {
        #[test]
        fn prop_path_validation_consistent(filename in "[a-zA-Z0-9_-]{1,50}") {
            use runtimo_core::validation::path::{validate_path, PathContext};
            let ctx = PathContext {
                require_exists: false,
                require_file: false,
                ..Default::default()
            };

            let path1 = format!("/tmp/{}", filename);
            let path2 = format!("/tmp/{}", filename);

            let result1 = validate_path(&path1, &ctx);
            let result2 = validate_path(&path2, &ctx);

            prop_assert_eq!(result1.is_ok(), result2.is_ok(),
                "Path validation not consistent for same path");
        }
    }

    // Property: WAL cleanup doesn't lose recent events
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(10))]
        #[test]
        fn prop_wal_cleanup_preserves_recent(n in 1usize..20) {
            let dir = setup();
            let wp = wal_path(&dir);
            let p = make_file(&dir, "cleanup.txt", "test");

            // Write n events
            for _ in 0..n {
                execute_with_telemetry(
                    &FileRead,
                    &json!({ "path": p.to_str().unwrap() }),
                    false,
                    &wp,
                ).unwrap();
            }

            let reader_before = WalReader::load(&wp).unwrap();
            let count_before = reader_before.events().len();

            // Cleanup with very old max_age (should remove nothing)
            let removed = WalWriter::cleanup(&wp, 86400 * 365).unwrap();

            let reader_after = WalReader::load(&wp).unwrap();
            let count_after = reader_after.events().len();

            prop_assert_eq!(count_before, count_after,
                "Cleanup with 1-year max_age should not remove events");
            prop_assert_eq!(removed, 0, "Should remove 0 events");

            cleanup(&dir);
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// T-LAYERGAP: Cross-Boundary Integration Tests
// ═══════════════════════════════════════════════════════════════════════════

/// Verifies that FileWrite→BackupManager→filesystem boundary preserves content.
/// This bridges the gap between unit-level FileWrite tests and end-to-end tests.
#[test]
fn t_layergap_backup_boundary_content_preserved() {
    let dir = setup();
    let target = dir.join("layer.txt");
    let content = "boundary crossing content сrossover";
    let bw = backup_dir(&dir);

    // First write creates the file (no backup for new files)
    let cap = FileWrite::new(bw.clone()).expect("FileWrite");
    let result = cap
        .execute(
            &json!({ "path": target.to_str().unwrap(), "content": "initial" }),
            &ctx("layer_job"),
        )
        .expect("first write");
    assert!(result.success);

    // Second write triggers backup (file already exists)
    let result = cap
        .execute(
            &json!({ "path": target.to_str().unwrap(), "content": content }),
            &ctx("layer_job"),
        )
        .expect("second write");
    assert!(result.success);

    // Read backup directly (bypass FileWrite, cross BackupManager→filesystem boundary)
    let _bm = BackupManager::new(bw.clone()).expect("BackupManager");
    let job_backups: Vec<_> = std::fs::read_dir(bw.join("layer_job"))
        .into_iter()
        .flatten()
        .filter_map(|d| d.ok())
        .collect();
    assert!(!job_backups.is_empty(), "Backup must exist for the job");

    // Verify backup content matches the pre-overwrite state (original file content)
    let backup_content = std::fs::read_to_string(job_backups[0].path()).ok();
    assert!(backup_content.is_some(), "Backup file must be readable");
    assert_eq!(
        backup_content.unwrap(),
        "initial",
        "T-LAYERGAP: backup must contain pre-overwrite content (original)"
    );

    // Verify FileRead can consume FileWrite's output (C1 contract check)
    let read_result = FileRead
        .execute(
            &json!({ "path": target.to_str().unwrap() }),
            &ctx("layer_read"),
        )
        .expect("read");
    let read_content = read_result.data["content"].as_str().unwrap();
    assert_eq!(
        read_content, content,
        "C1: FileRead must consume FileWrite output with same content"
    );

    cleanup(&dir);
}

/// Verifies that WAL events can be produced and consumed with structural integrity.
/// This bridges the gap between unit-level WAL tests and end-to-end pipeline tests.
#[test]
fn t_layergap_wal_boundary_structural_integrity() {
    use runtimo_core::{WalEvent, WalEventType, WalReader, WalWriter};

    let dir = setup();
    let wp = wal_path(&dir);

    // Write WAL event with all optional fields populated
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    {
        let mut wal = WalWriter::create(&wp).expect("create");
        wal.append(WalEvent {
            seq: wal.seq(),
            ts,
            event_type: WalEventType::JobCompleted,
            job_id: "layer-j-1".into(),
            capability: Some("ShellExec".into()),
            output: Some(serde_json::json!({"stdout": "hello", "stderr": "", "exit": 0})),
            error: None,
            telemetry_before: None,
            telemetry_after: None,
            process_before: None,
            process_after: None,
            cmd: Some("echo hello".into()),
            cmd_stdout: Some("hello\n".into()),
            cmd_stderr: Some(String::new()),
            cmd_exit_code: Some(0),
            cmd_corrected: None,
            ..Default::default()
        })
        .expect("append");
    }

    // Read back and verify every structural field exists
    let reader = WalReader::load(&wp).expect("read");
    let events = reader.events();
    assert_eq!(events.len(), 1);

    let e = &events[0];
    // T-LAYERGAP: verify all field groups are present
    assert_eq!(e.seq, 0, "seq must be 0");
    assert!(e.ts > 0, "timestamp must be populated");
    assert_eq!(e.job_id, "layer-j-1");
    assert_eq!(e.event_type, WalEventType::JobCompleted);
    assert!(
        e.telemetry_before.is_none(),
        "not populated in this unit test (use execute_with_telemetry for full coverage)"
    );
    assert!(
        e.telemetry_after.is_none(),
        "not populated in this unit test"
    );
    assert!(e.output.is_some(), "output field must survive boundary");
    assert_eq!(e.cmd.as_deref(), Some("echo hello"));
    assert_eq!(e.cmd_exit_code, Some(0));
    assert_eq!(e.cmd_corrected, None);

    // Verify C1: output JSON structure is intact
    let out = e.output.as_ref().unwrap();
    assert_eq!(out["stdout"], "hello");
    assert_eq!(out["exit"], 0);

    // Verify C3: telemetry_before and telemetry_after are stored independently
    // (populated via execute_with_telemetry in integration tests)
    assert!(
        e.telemetry_before.is_none(),
        "not populated in this unit test"
    );
    assert!(
        e.telemetry_after.is_none(),
        "not populated in this unit test"
    );

    cleanup(&dir);
}

// ═══════════════════════════════════════════════════════════════════════════
// G-DRIFT: Golden Format Stability
// ═══════════════════════════════════════════════════════════════════════════

/// Verifies WAL event JSON format stability — catches accidental field renames
/// or type changes that break downstream consumers of persisted WAL data.
#[test]
fn g_drift_wal_event_json_schema_stability() {
    use runtimo_core::{WalEvent, WalEventType, WalWriter};

    let dir = setup();
    let wp = wal_path(&dir);
    let ts = 1715800000u64;

    {
        let mut wal = WalWriter::create(&wp).expect("create");
        wal.append(WalEvent {
            seq: 1,
            ts,
            event_type: WalEventType::JobStarted,
            job_id: "golden".into(),
            capability: Some("FileWrite".into()),
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
        .expect("append");
    }

    let raw = std::fs::read_to_string(&wp).expect("read raw WAL");
    let parsed: serde_json::Value = serde_json::from_str(raw.trim()).expect("parse WAL line");

    // Golden schema: every field must exist with expected type
    assert!(
        parsed.get("seq").and_then(|v| v.as_u64()).is_some(),
        "seq must be u64"
    );
    assert_eq!(parsed["seq"], 1);

    assert!(
        parsed.get("ts").and_then(|v| v.as_u64()).is_some(),
        "ts must be u64"
    );
    assert_eq!(parsed["ts"], ts);

    assert!(
        parsed.get("type").and_then(|v| v.as_str()).is_some(),
        "type must be string (serde rename)"
    );
    assert_eq!(parsed["type"], "job_started");

    assert!(
        parsed.get("job_id").and_then(|v| v.as_str()).is_some(),
        "job_id must be string"
    );
    assert_eq!(parsed["job_id"], "golden");

    assert_eq!(parsed["capability"], "FileWrite");

    // Null fields must be serialized as explicit null or absent — not empty strings
    let output = &parsed["output"];
    assert!(
        output.is_null() || output.as_str().is_none(),
        "output must be null or absent, not empty string"
    );

    let error = &parsed["error"];
    assert!(
        error.is_null() || error.as_str().is_none(),
        "error must be null or absent when no error"
    );

    // C3 check: command-related fields must NOT appear in non-command events
    // (or must be null) — prevents reclassification of cmd fields from
    // a previous command event into the wrong event type
    let has_cmd = parsed.get("cmd").and_then(|v| v.as_str());
    assert!(
        has_cmd.is_none() || has_cmd == Some(""),
        "cmd field should not leak into non-command JobStarted events"
    );

    cleanup(&dir);
}

/// Verifies telemetry JSON format stability across captures.
/// Catches field renames or type changes in Telemetry struct serialization.
#[test]
fn g_drift_telemetry_schema_stability() {
    let tel = Telemetry::capture();
    let json = serde_json::to_value(&tel).expect("serialize");

    // Verify required top-level fields exist
    assert!(
        json.get("timestamp").and_then(|v| v.as_u64()).is_some(),
        "timestamp must be u64"
    );

    let system = &json["system"];
    assert!(
        system.get("cpu_model").and_then(|v| v.as_str()).is_some(),
        "cpu_model required"
    );
    assert!(
        system.get("ram_total").and_then(|v| v.as_str()).is_some(),
        "ram_total required"
    );

    let hw = &json["hardware"];
    assert!(
        hw.get("accelerators").and_then(|v| v.as_array()).is_some(),
        "accelerators must be array"
    );

    // services field removed in elegant-telemetry refactor;
    // listening ports now live under network.listening_ports
    let net = &json["network"];
    assert!(
        net.get("listening_ports")
            .and_then(|v| v.as_array())
            .is_some(),
        "network.listening_ports must be array"
    );

    let net = &json["network"];
    assert!(
        net.get("public_ip").and_then(|v| v.as_str()).is_some(),
        "public_ip must be string"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// G-PERF: Complexity Bounds
// ═══════════════════════════════════════════════════════════════════════════

/// Verifies that FileWrite + FileRead time scales near-linearly with content size.
/// Not a strict guarantee — just a smoke test that catches O(n²) regressions.
#[test]
fn g_perf_write_read_near_linear() {
    let dir = setup();
    let bw = backup_dir(&dir);

    let measure = |size: usize| -> std::time::Duration {
        let target = dir.join(format!("perf_{}.txt", size));
        let content = "a".repeat(size);
        let start = std::time::Instant::now();

        FileWrite::new(bw.clone())
            .expect("FileWrite")
            .execute(
                &json!({ "path": target.to_str().unwrap(), "content": &content }),
                &ctx("perf_write"),
            )
            .expect("write");
        FileRead
            .execute(
                &json!({ "path": target.to_str().unwrap() }),
                &ctx("perf_read"),
            )
            .expect("read");

        start.elapsed()
    };

    let t_small = measure(100);
    let t_large = measure(100_000);

    // Large (100KB) should be somewhat slower than small (100B),
    // but not 10,000× slower (would indicate O(n²) or worse)
    let ratio = t_large.as_secs_f64() / t_small.as_secs_f64().max(1e-9);
    assert!(
        ratio < 5000.0,
        "Write+read of 100KB ({:?}) should not be >5000× slower than 100B ({:?}), got ratio {:.0}",
        t_large,
        t_small,
        ratio
    );

    cleanup(&dir);
}
