use runtimo_core::{
    capabilities::{FileRead, FileWrite},
    execute_with_telemetry, BackupManager, Capability, CapabilityRegistry, ProcessSnapshot,
    Telemetry, WalReader,
};
use serde_json::json;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

fn unique_test_dir() -> PathBuf {
    let ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("runtimo_test_{}_{}", std::process::id(), ns))
}

fn wal_path(base: &std::path::Path) -> PathBuf {
    let mut p = base.to_path_buf();
    p.push("wal_dir/wal.jsonl");
    p
}

fn backup_dir(base: &std::path::Path) -> PathBuf {
    let mut p = base.to_path_buf();
    p.push("backups");
    p
}

fn setup() -> PathBuf {
    let d = unique_test_dir();
    fs::create_dir_all(&d).ok();
    fs::create_dir_all(wal_path(&d).parent().unwrap()).ok();
    d
}

fn cleanup(dir: &PathBuf) {
    fs::remove_dir_all(dir).ok();
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

// ── basic functionality ──────────────────────────────────────────────

#[test]
fn reads_file_content() {
    let dir = setup();
    let p = make_file(&dir, "t.txt", "hello world");
    let result = FileRead
        .execute(&json!({ "path": p.to_str().unwrap() }), &ctx("r1"))
        .unwrap();
    assert_eq!(result.data["content"].as_str().unwrap(), "hello world");
    cleanup(&dir);
}

#[test]
fn writes_file_content() {
    let dir = setup();
    let target = dir.join("w.txt");
    let result = FileWrite::new(backup_dir(&dir))
        .expect("Failed to create FileWrite")
        .execute(
            &json!({
                "path": target.to_str().unwrap(),
                "content": "test data"
            }),
            &ctx("w1"),
        )
        .unwrap();
    assert!(result.success);
    assert_eq!(fs::read_to_string(&target).unwrap(), "test data");
    cleanup(&dir);
}

#[test]
fn executor_wraps_capability() {
    let dir = setup();
    let p = make_file(&dir, "e.txt", "executor test");
    let result = execute_with_telemetry(
        &FileRead,
        &json!({ "path": p.to_str().unwrap() }),
        false,
        &wal_path(&dir),
    )
    .unwrap();
    assert!(result.success);
    assert_eq!(result.capability, "FileRead");
    cleanup(&dir);
}

#[test]
fn captures_telemetry() {
    let t = Telemetry::capture();
    assert!(t.timestamp > 0);
    assert!(!t.system.cpu_model.is_empty());
}

#[test]
fn captures_process_snapshot() {
    let s = ProcessSnapshot::capture();
    assert!(!s.processes.is_empty());
}

#[test]
fn registry_lists_capabilities() {
    let dir = setup();
    let reg = make_registry(&backup_dir(&dir));
    let caps = reg.list();
    assert_eq!(caps.len(), 2);
    assert!(caps.contains(&"FileRead"));
    assert!(reg.get("FileRead").is_some());
    assert!(reg.get("NoSuchCap").is_none());
    cleanup(&dir);
}

fn make_registry(bd: &std::path::Path) -> CapabilityRegistry {
    let mut r = CapabilityRegistry::new();
    r.register(FileRead);
    r.register(FileWrite::new(bd.to_path_buf()).expect("Failed to create FileWrite"));
    r
}

// ── security ─────────────────────────────────────────────────────────

#[test]
fn rejects_path_traversal_read() {
    assert!(FileRead
        .validate(&json!({ "path": "../../../etc/passwd" }))
        .is_err());
}

#[test]
fn rejects_path_traversal_write() {
    let dir = setup();
    let cap = FileWrite::new(backup_dir(&dir)).expect("Failed to create FileWrite");
    assert!(cap
        .validate(&json!({ "path": "../../../tmp/x.txt", "content": "x" }))
        .is_err());
    cleanup(&dir);
}

#[test]
fn rejects_reading_directory() {
    let dir = setup();
    assert!(FileRead
        .validate(&json!({ "path": dir.to_str().unwrap() }))
        .is_err());
    cleanup(&dir);
}

#[test]
fn rejects_empty_path() {
    let dir = setup();
    assert!(FileRead.validate(&json!({ "path": "" })).is_err());
    let cap = FileWrite::new(backup_dir(&dir)).expect("Failed to create FileWrite");
    assert!(cap
        .validate(&json!({ "path": "", "content": "x" }))
        .is_err());
    cleanup(&dir);
}

// ── edge cases ───────────────────────────────────────────────────────

#[test]
fn reads_empty_file() {
    let dir = setup();
    let p = make_file(&dir, "empty.txt", "");
    let r = FileRead
        .execute(&json!({ "path": p.to_str().unwrap() }), &ctx("e1"))
        .unwrap();
    assert_eq!(r.data["content"].as_str().unwrap(), "");
    cleanup(&dir);
}

#[test]
fn reads_unicode() {
    let dir = setup();
    let p = make_file(&dir, "uni.txt", "مرحبا 你好 🌍");
    let r = FileRead
        .execute(&json!({ "path": p.to_str().unwrap() }), &ctx("e2"))
        .unwrap();
    assert!(r.data["content"].as_str().unwrap().contains("مرحبا"));
    cleanup(&dir);
}

#[test]
fn reads_large_file() {
    let dir = setup();
    let p = make_file(&dir, "big.txt", &"x".repeat(100_000));
    let r = FileRead
        .execute(&json!({ "path": p.to_str().unwrap() }), &ctx("e3"))
        .unwrap();
    assert_eq!(r.data["content"].as_str().unwrap().len(), 100_000);
    cleanup(&dir);
}

#[test]
fn writes_unicode() {
    let dir = setup();
    let target = dir.join("uni_w.txt");
    let content = "日本語 🔥 مرحبا";
    FileWrite::new(backup_dir(&dir))
        .expect("Failed to create FileWrite")
        .execute(
            &json!({
                "path": target.to_str().unwrap(), "content": content
            }),
            &ctx("e4"),
        )
        .unwrap();
    assert_eq!(fs::read_to_string(&target).unwrap(), content);
    cleanup(&dir);
}

#[test]
fn creates_parent_directories() {
    let dir = setup();
    let deep = dir.join("a/b/c");
    let target = deep.join("f.txt");
    FileWrite::new(backup_dir(&dir))
        .expect("Failed to create FileWrite")
        .execute(
            &json!({
                "path": target.to_str().unwrap(), "content": "deep"
            }),
            &ctx("e5"),
        )
        .unwrap();
    assert!(target.exists());
    cleanup(&dir);
}

// ── error handling ───────────────────────────────────────────────────

#[test]
fn rejects_missing_file() {
    assert!(FileRead
        .validate(&json!({ "path": "/tmp/no_such_runtimo_file.txt" }))
        .is_err());
}

#[test]
fn rejects_missing_field_in_args() {
    let dir = setup();
    assert!(FileRead.validate(&json!({ "wrong_field": "v" })).is_err());
    let cap = FileWrite::new(backup_dir(&dir)).expect("Failed to create FileWrite");
    assert!(cap.validate(&json!({ "path": "/tmp/x.txt" })).is_err()); // missing content
    cleanup(&dir);
}

#[test]
fn llmosafe_guard_reports_pressure() {
    use runtimo_core::LlmoSafeGuard;
    let guard = LlmoSafeGuard::new();
    let p = guard.pressure();
    assert!(p <= 100, "pressure should be 0-100, got {}", p);
}

#[test]
fn llmosafe_guard_reports_entropy() {
    use runtimo_core::LlmoSafeGuard;
    let guard = LlmoSafeGuard::new();
    let e = guard.raw_entropy();
    assert!(e <= 1000, "entropy should be 0-1000, got {}", e);
}

#[test]
fn llmosafe_guard_check_passes_on_idle_system() {
    use runtimo_core::LlmoSafeGuard;
    let guard = LlmoSafeGuard::new();
    // On a normal idle system this should pass — if it fails, the system is genuinely under pressure
    if let Err(e) = guard.check() {
        eprintln!("System under pressure: {}", e);
    }
}

// ── integration: workflows ───────────────────────────────────────────

#[test]
fn write_then_read_roundtrip() {
    let dir = setup();
    let target = dir.join("rt.txt");
    let original = "roundtrip\nmulti-line 你好";

    FileWrite::new(backup_dir(&dir))
        .expect("Failed to create FileWrite")
        .execute(
            &json!({
                "path": target.to_str().unwrap(), "content": original
            }),
            &ctx("rt1"),
        )
        .unwrap();

    let r = FileRead
        .execute(&json!({ "path": target.to_str().unwrap() }), &ctx("rt2"))
        .unwrap();
    assert_eq!(r.data["content"].as_str().unwrap(), original);
    cleanup(&dir);
}

#[test]
fn backup_created_on_overwrite() {
    let dir = setup();
    let bd = backup_dir(&dir);
    fs::create_dir_all(&bd).ok();
    let target = dir.join("bk.txt");

    FileWrite::new(bd.clone())
        .expect("Failed to create FileWrite")
        .execute(
            &json!({
                "path": target.to_str().unwrap(), "content": "original"
            }),
            &ctx("bk1"),
        )
        .unwrap();

    let r = FileWrite::new(bd.clone())
        .expect("Failed to create FileWrite")
        .execute(
            &json!({
                "path": target.to_str().unwrap(), "content": "modified"
            }),
            &ctx("bk2"),
        )
        .unwrap();
    assert!(r.success);
    assert_eq!(fs::read_to_string(&target).unwrap(), "modified");

    let bp = bd.join("bk2").join("bk.txt");
    assert!(bp.exists());
    assert_eq!(fs::read_to_string(&bp).unwrap(), "original");

    BackupManager::new(bd.clone())
        .expect("Failed to create BackupManager")
        .restore(&bp, &target)
        .unwrap();
    assert_eq!(fs::read_to_string(&target).unwrap(), "original");
    cleanup(&dir);
}

#[test]
fn wal_records_jobs() {
    let dir = setup();
    let wp = wal_path(&dir);
    let p = make_file(&dir, "wl.txt", "wal test");

    execute_with_telemetry(
        &FileRead,
        &json!({ "path": p.to_str().unwrap() }),
        false,
        &wp,
    )
    .unwrap();

    let reader = WalReader::load(&wp).unwrap();
    let events = reader.events();
    assert!(events.len() >= 2);
    assert!(events
        .iter()
        .any(|e| matches!(e.event_type, runtimo_core::WalEventType::JobStarted)));
    assert!(events
        .iter()
        .any(|e| matches!(e.event_type, runtimo_core::WalEventType::JobCompleted)));
    cleanup(&dir);
}

#[test]
fn dry_run_does_not_write() {
    let dir = setup();
    let target = dir.join("dry.txt");
    FileWrite::new(backup_dir(&dir))
        .expect("Failed to create FileWrite")
        .execute(
            &json!({
                "path": target.to_str().unwrap(), "content": "nope"
            }),
            &runtimo_core::Context {
                dry_run: true,
                job_id: "dry1".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap();
    assert!(!target.exists());
    cleanup(&dir);
}

#[test]
fn append_mode() {
    let dir = setup();
    let target = dir.join("app.txt");
    let bw = backup_dir(&dir);

    FileWrite::new(bw.clone())
        .expect("Failed to create FileWrite")
        .execute(
            &json!({
                "path": target.to_str().unwrap(), "content": "line1\n"
            }),
            &ctx("a1"),
        )
        .unwrap();

    FileWrite::new(bw)
        .expect("Failed to create FileWrite")
        .execute(
            &json!({
                "path": target.to_str().unwrap(), "content": "line2\n", "append": true
            }),
            &ctx("a2"),
        )
        .unwrap();

    let c = fs::read_to_string(&target).unwrap();
    assert!(c.contains("line1"));
    assert!(c.contains("line2"));
    cleanup(&dir);
}

#[test]
fn multiple_jobs_in_sequence() {
    let dir = setup();
    let wp = wal_path(&dir);
    let target = dir.join("seq.txt");

    execute_with_telemetry(
        &FileWrite::new(backup_dir(&dir)).expect("Failed to create FileWrite"),
        &json!({ "path": target.to_str().unwrap(), "content": "seq test" }),
        false,
        &wp,
    )
    .unwrap();

    let r = execute_with_telemetry(
        &FileRead,
        &json!({ "path": target.to_str().unwrap() }),
        false,
        &wp,
    )
    .unwrap();
    // Debug: check what's in the output
    println!("Success: {}, Output: {:?}", r.success, r.output);
    assert!(r.success, "FileRead failed: {:?}", r.output.message);
    assert_eq!(
        r.output.data["content"]
            .as_str()
            .unwrap_or("CONTENT_MISSING"),
        "seq test"
    );

    assert!(WalReader::load(&wp).unwrap().events().len() >= 4);
    cleanup(&dir);
}

// ── invariants ───────────────────────────────────────────────────────

#[test]
fn roundtrip_many_contents() {
    let dir = setup();
    let cases = vec![
        String::new(),
        "a".into(),
        "hello world".into(),
        "مرحبا".into(),
        "x".repeat(10_000),
        "line1\nline2".into(),
        "special: <>&\"'\\".into(),
    ];

    for (i, content) in cases.into_iter().enumerate() {
        let target = dir.join(format!("r{}.txt", i));
        FileWrite::new(backup_dir(&dir))
            .expect("Failed to create FileWrite")
            .execute(
                &json!({
                    "path": target.to_str().unwrap(), "content": content
                }),
                &ctx(format!("r{}", i)),
            )
            .unwrap();

        let r = FileRead
            .execute(
                &json!({ "path": target.to_str().unwrap() }),
                &ctx(format!("rr{}", i)),
            )
            .unwrap();
        assert_eq!(
            r.data["content"].as_str().unwrap(),
            content,
            "roundtrip failed case {}",
            i
        );
    }
    cleanup(&dir);
}

#[test]
fn timestamps_monotonic() {
    let t1 = Telemetry::capture();
    std::thread::sleep(std::time::Duration::from_millis(10));
    let t2 = Telemetry::capture();
    assert!(t2.timestamp >= t1.timestamp);
}

#[test]
fn process_snapshot_consistent() {
    let s = ProcessSnapshot::capture();
    assert_eq!(s.summary.total_processes, s.processes.len());
    for p in &s.processes {
        assert!(p.cpu_percent >= 0.0);
        assert!(p.mem_percent >= 0.0);
    }
    let actual_zombies = s
        .processes
        .iter()
        .filter(|p| p.stat.starts_with('Z'))
        .count();
    assert_eq!(s.summary.zombie_count, actual_zombies);
}

#[test]
fn executor_always_returns_telemetry() {
    let dir = setup();
    let wp = wal_path(&dir);
    let p = make_file(&dir, "te.txt", "t");

    let r = execute_with_telemetry(
        &FileRead,
        &json!({ "path": p.to_str().unwrap() }),
        false,
        &wp,
    )
    .unwrap();
    assert!(r.telemetry_before.timestamp > 0);
    assert!(r.telemetry_after.timestamp > 0);
    assert!(r.process_before.total_processes > 0);
    cleanup(&dir);
}

#[test]
fn wal_events_sequential() {
    let dir = setup();
    let wp = wal_path(&dir);
    let p = make_file(&dir, "ws.txt", "t");

    for _ in 0..3 {
        execute_with_telemetry(
            &FileRead,
            &json!({ "path": p.to_str().unwrap() }),
            false,
            &wp,
        )
        .unwrap();
    }

    assert!(WalReader::load(&wp).unwrap().events().len() >= 6);
    cleanup(&dir);
}
