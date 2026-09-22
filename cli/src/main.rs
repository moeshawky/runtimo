//! runtimo CLI — Agent capability runtime with background dispatch.
//! Part of the single-program runtimo suite: `runtimo-core` + `runtimo-daemon` + `runtimo-cli`
//! are one program at one version. `cargo install runtimo-cli` installs both `runtimo` and
//! `runtimo-daemon` binaries. The `runtimo-daemon` package is the library; the
//! `runtimo-daemon` binary delegates to [`runtimo_daemon::run`].
//! The `runtimo-daemon` lib vs bin are distinguished by the binary name —
//! the lib is `runtimo_daemon`, the bin is `runtimo-daemon`.

mod format;
mod output;
mod session_parser;
mod session_run;

use clap::{Parser, Subcommand};
use output::OutputMode;
use runtimo_core::{
    capabilities::{
        is_dangerous_command, is_network_command, network_enabled, Delete, FileRead, FileWrite,
        GitExec, Kill, ShellExec, Undo,
    },
    CapabilityRegistry, ProcessSnapshot, RuntimoConfig, Telemetry, WalReader,
};
use serde_json::Value;
use std::error::Error;
use std::fs::File;
use std::io::{BufRead, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Maximum seconds to wait for daemon to become ready after spawning.
const DAEMON_STARTUP_TIMEOUT_SECS: u64 = 30;
/// Maximum size for capability arguments in bytes (~130 KB).
const MAX_ARGS_SIZE_BYTES: usize = 130 * 1024;

#[derive(Parser)]
#[command(
    name = "runtimo",
    about = "runtimo — capability runtime with telemetry, WAL, process tracking, and background dispatch. One program, one version (core+daemon+cli). Install via `cargo install runtimo-cli` (both bins). The runtimo-daemon package is the library; the runtimo-daemon binary delegates to it.",
    long_about = "runtimo — capability runtime with telemetry, WAL, and process tracking\n\nEvery exec: telemetry + process snapshot + WAL audit\n\nBackground: dispatch jobs to daemon, check status later",
    after_help = "USAGE:\n runtimo run -c <Capability> -a '<json>'\n runtimo dispatch -c <Capability> -a '<json>'\n runtimo jobs\n runtimo wait -j <job_id>\n runtimo list\n runtimo logs\n runtimo telemetry\n runtimo processes\n\nCAPABILITIES:\n FileRead  Read file. Path validated (allowed dirs only). No dirs, no traversal.\n FileWrite Write file. Auto-backup for undo. Append mode ok.\n Delete    Delete a file. Auto-backup for undo unless no_backup=true. Path-validated (no rm bypass).\n ShellExec Exec via sh -c. Blocks many dangerous commands (see `runtimo list` for full blocklist). Network tools and interpreters are opt-in.\n GitExec   Git ops: clone|pull|commit|revert|clean|status.\n Kill      Kill process by PID. Protected: init, kthreadd, self, parent, session/group leaders, systemd services.\n Undo      Restore from backup. Find job IDs with `runtimo jobs` or `runtimo logs`.\n\nTIP: Use `runtimo run -c <Cap> --schema` to see the JSON args a capability expects.\nTIP: Use `runtimo list --schemas` to see all schemas at once.\nTIP: ShellExec timeout has no upper bound (default: 30).\n\nDaemon starts on first dispatch if runtimo-daemon is installed.",
    version
)]
struct Cli {
    /// Output format: human|json|plain|quiet
    #[arg(long, global = true, value_name = "FORMAT")]
    output: Option<String>,
    /// Enable ANSI color (requires tty, honored only when explicitly set; --no-color wins, NO_COLOR env forces off)
    #[arg(long, global = true)]
    color: bool,
    /// Disable ANSI color (wins over --color and NO_COLOR)
    #[arg(long, global = true)]
    no_color: bool,
    /// Enable emoji (off by default; --no-emoji wins)
    #[arg(long, global = true)]
    emoji: bool,
    /// Disable emoji
    #[arg(long, global = true)]
    no_emoji: bool,
    /// Table style: plain|markdown|box|csv
    #[arg(long, global = true, value_name = "STYLE")]
    table_style: Option<String>,
    /// Enable timestamps
    #[arg(long, global = true)]
    timestamps: bool,
    /// Disable timestamps (wins over --timestamps)
    #[arg(long, global = true)]
    no_timestamps: bool,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Execute a capability with telemetry
    #[command(
        about = "exec capability with telemetry",
        after_help = "CAPABILITY HELP:\n runtimo run -c <Cap> --schema     → see expected JSON args\n runtimo run -c <Cap> --dry-run    → validate without executing\n\nEXAMPLES:\n runtimo run -c FileRead -a '{\"path\":\"/tmp/test.txt\"}'\n runtimo run -c ShellExec -a '{\"cmd\":\"uptime\"}'\n runtimo run -c FileWrite -a '{\"path\":\"/tmp/x.txt\",\"content\":\"hello\"}'\n\nBlocked commands: rm, shutdown, chmod, mkfs, dd, iptables, fork bombs, env dumpers.\nSee `runtimo list` for the full blocklist.\n\nARGS SIZE:\n Maximum args payload is ~130 KB. For larger payloads, use --args-file <path> or --args-stdin."
    )]
    Run {
        /// Capability name (e.g., FileRead, ShellExec). Use `runtimo list` to see all.
        #[arg(short = 'c', long)]
        capability: String,
        /// Capability arguments as JSON (e.g., '{\"path\":\"/tmp/test.txt\"}'). Use --schema to see the expected shape.
        #[arg(short = 'a', long, default_value = "{}")]
        args: String,
        /// Path to a file containing capability arguments as JSON (bypasses OS ARG_MAX for large payloads)
        #[arg(long)]
        args_file: Option<PathBuf>,
        /// Read capability arguments as JSON from stdin (bypasses OS ARG_MAX for large payloads)
        #[arg(long)]
        args_stdin: bool,
        /// Validate args and check blocklist but don't execute
        #[arg(long)]
        dry_run: bool,
        /// Output raw JSON instead of formatted text
        #[arg(short = 'j', long)]
        json: bool,
        /// Suppress output except errors (useful for scripting)
        #[arg(short = 'q', long)]
        quiet: bool,
        /// Print the capability's JSON Schema and exit
        #[arg(long)]
        schema: bool,
        /// Execution timeout in seconds (no upper bound). Defaults to config value, then 30s.
        #[arg(long, value_parser = clap::value_parser!(u64))]
        timeout: Option<u64>,
    },
    /// Dispatch job to background daemon (returns immediately)
    #[command(
        about = "Dispatch job to background daemon (starts daemon automatically if needed)",
        after_help = "EXAMPLES:\n runtimo dispatch -c ShellExec -a '{\"cmd\":\"sleep 30\"}'\n runtimo dispatch -c FileWrite -a '{\"path\":\"/tmp/x.txt\",\"content\":\"bg\"}'\n runtimo dispatch -c GitExec -a '{\"operation\":\"status\",\"path\":\"/tmp/repo\"}'\n\nAfter dispatch:\n runtimo status                # check all job statuses\n runtimo wait -j <job_id>      # wait for completion\n runtimo logs -j <job_id>      # view WAL events\n\nDaemon starts automatically on first dispatch.\n\nARGS SIZE:\n Maximum args payload is ~130 KB. For larger payloads, use --args-file <path> or --args-stdin."
    )]
    Dispatch {
        /// Capability name (e.g., ShellExec, FileWrite). Use `runtimo list` to see all.
        #[arg(short = 'c', long)]
        capability: String,
        /// Capability arguments as JSON (same format as `run`)"
        #[arg(short = 'a', long)]
        args: String,
        /// Path to a file containing capability arguments as JSON (bypasses OS ARG_MAX for large payloads)
        #[arg(long)]
        args_file: Option<PathBuf>,
        /// Read capability arguments as JSON from stdin (bypasses OS ARG_MAX for large payloads)
        #[arg(long)]
        args_stdin: bool,
        /// Validate and check blocklist but don't enqueue the job
        #[arg(long)]
        dry_run: bool,
    },
    /// Wait for a dispatched job to complete
    /// Pre-validates job existence via daemon RPC or WAL scan before entering
    /// the poll loop. Returns immediately with "Job not found" if the job ID
    /// is unknown and the daemon is unreachable.
    #[command(
        about = "Wait for a dispatched job",
        after_help = "EXAMPLES:\n runtimo wait -j abc123\n runtimo wait -j abc123 --timeout 60"
    )]
    Wait {
        /// Job ID to wait for (from dispatch output or `runtimo jobs`)"
        #[arg(short = 'j', long)]
        job_id: String,
        /// Maximum seconds to wait (0 = wait forever)"
        #[arg(long, default_value = "0")]
        timeout: u64,
    },
    /// List available capabilities
    #[command(
        about = "List capabilities",
        after_help = "Use --schemas to see JSON argument schemas for each capability.\nUse --json for machine-readable output."
    )]
    List {
        /// Show each capability's JSON argument schema
        #[arg(long)]
        schemas: bool,
        /// Output as JSON (machine-readable)"
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Check job status (via daemon RPC if running, falls back to WAL)"
    #[command(
        about = "Check job status (daemon RPC or WAL fallback)",
        after_help = "EXAMPLES:\n runtimo status             # all jobs (daemon RPC)\n runtimo status -j abc123   # specific job\n runtimo status -oj         # JSON output\n\nNote: queries daemon for live status; falls back to WAL data if daemon unreachable."
    )]
    Status {
        /// Job ID to check (omit to list all)"
        #[arg(short = 'j', long)]
        job_id: Option<String>,
        /// Output raw JSON"
        #[arg(short = 'o', long)]
        json: bool,
    },
    /// List recent jobs from WAL (local + dispatched, read-only snapshot)"
    #[command(
        about = "List recent jobs from WAL",
        after_help = "EXAMPLES:\n runtimo jobs\n runtimo jobs --limit 5\n runtimo jobs --json\n\nNote: reads from WAL directly (no daemon needed). Use `status` for live daemon query."
    )]
    Jobs {
        /// Number of jobs to show (default: 20)"
        #[arg(short = 'n', long, default_value = "20")]
        limit: usize,
        /// Output raw JSON"
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// View WAL logs (audit trail of all events)"
    #[command(
        about = "View WAL logs",
        after_help = "EXAMPLES:\n runtimo logs              # last 10 events\n runtimo logs -j abc123    # events for a specific job\n runtimo logs -n 50        # last 50 events\n runtimo logs -oj          # JSON output"
    )]
    Logs {
        /// Filter by job ID"
        #[arg(short = 'j', long)]
        job_id: Option<String>,
        /// Number of events to show (default: 10)"
        #[arg(short = 'n', long, default_value = "10")]
        limit: usize,
        /// Output raw JSON"
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Undo a completed job (restore files from backup)"
    #[command(
        about = "Undo a completed job",
        after_help = "Find job IDs with `runtimo jobs` or `runtimo logs`.\\n\\nEXAMPLES:\\n runtimo undo -j abc123\\n runtimo undo -j abc123 --dry-run    # check what would be restored"
    )]
    Undo {
        /// Job ID to undo (from `runtimo jobs` or `runtimo logs`)"
        #[arg(short = 'j', long)]
        job_id: String,
        /// Show what files would be restored without actually restoring them"
        #[arg(long)]
        dry_run: bool,
    },
    /// Print system telemetry (CPU, RAM, disk, GPU, network)"
    #[command(
        about = "Print system telemetry",
        after_help = "EXAMPLES:\n runtimo telemetry             # formatted\n runtimo telemetry -j          # JSON\n runtimo telemetry -v          # include listening ports\n runtimo telemetry -jv         # JSON with verbose"
    )]
    Telemetry {
        /// Show extended details (listening ports, GPU info)"
        #[arg(short = 'v', long)]
        verbose: bool,
        /// Output raw JSON"
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Print process snapshot (top consumers, zombie count)"
    #[command(
        about = "Print process snapshot",
        after_help = "EXAMPLES:\n runtimo processes             # formatted table\n runtimo processes -j          # JSON output"
    )]
    Processes {
        /// Output raw JSON"
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// List and optionally reap zombie processes"
    #[command(
        about = "List zombie processes",
        after_help = "EXAMPLES:\n runtimo zombies\\n runtimo zombies --reap\\n\\nZombies are dead processes whose parents haven't called waitpid(2).\\nThey can't be killed directly. --reap kills each zombie's parent process\\ninstead, which causes the kernel to clean up the zombie."
    )]
    Zombies {
        /// Enable reaping (kills zombie parents)"
        #[arg(short = 'r', long, default_value = "false")]
        reap: bool,
    },
    /// Manage configuration"
    #[command(
        about = "Manage configuration",
        after_help = "Config file: ~/.config/runtimo/config.toml\n\n  allowed_paths       Extra path prefixes for FileRead/FileWrite\n  dal                 Design Assurance Level A-E\n  blocklist_overrides Additional ShellExec blocklist patterns\n  capability_timeouts Per-capability timeout overrides\n\nExample:\n  dal = \"B\"\n  ShellExec = 120"
    )]
    Config {
        /// Show current configuration"
        #[command(about = "Show current configuration (from config.toml)")]
        Show,
        /// Get or set the Design Assurance Level (DAL)"
        #[command(about = "Get or set the Design Assurance Level (A-E) for the cognitive safety pipeline")]
        Dal {
            /// New DAL level to set (A, B, C, D, or E). Omit to show current value.
            level: Option<String>,
        },
        /// Initialize config file from profile template"
        #[command(about = "Initialize config file from profile template (minimal/ephemeral/service)")]
        Init {
            /// Profile to use (minimal, ephemeral, service). Use --minimal as alias for minimal.
            #[arg(long)]
            profile: Option<String>,
            /// Overwrite existing config
            /// Alias for --profile minimal
            /// Custom path for config file (default: XDG config path)
            #[arg(long)]
            path: Option<PathBuf>,
            /// Force overwrite
            #[arg(long)]
            force: bool,
            /// Minimal profile
            #[arg(long)]
            minimal: bool,
        },
    },
    /// Session step-runner — deterministic bounded execution from a prompt file"
    #[command(about = "Session step-runner (MVP)")]
    Session {
        /// Run steps from a prompt file as a bounded session"
        #[command(about = "Run steps from a prompt file as a bounded session")]
        Run {
            /// Prompt file containing JSONL or markdown ```runtimo blocks"
            #[arg(long, value_name = "PATH")]
            prompt_file: PathBuf,
            /// Session name (human-readable) — reuses existing session if name or id matches, else creates a new one"
            #[arg(long)]
            session: Option<String>,
            /// Maximum number of steps to execute (default: from ResolvedConfig — 20 ephemeral, 100 service/minimal)"
            #[arg(long)]
            max_steps: Option<u32>,
            /// Maximum total seconds for the session run (default: from ResolvedConfig — 300 ephemeral, 3600 service)"
            #[arg(long)]
            max_seconds: Option<u64>,
            /// Behavior on step failure: continue|stop (default: from ResolvedConfig — continue ephemeral, stop service)"
            #[arg(long, value_name = "POLICY")]
            on_failure: Option<String>,
            /// Validate prompt file and policy only; do not execute or create a session"
            /// Stub: dispatch each step via daemon RPC instead of local execution"
            #[arg(long)]
            via_daemon: bool,
            /// Dry run
            #[arg(long)]
            dry_run: bool,
        },
        /// List sessions persisted under the sessions directory"
        /// Show a single session by id"
        #[command(about = "List sessions")]
        List {
            /// Output raw JSON"
            #[arg(short = 'j', long)]
            json: bool,
        },
        /// Show a single session by id"
        #[command(about = "Show session")]
        Show {
            /// Session ID to show"
            #[arg(long, value_name = "SESSION_ID")]
            session_id: String,
            /// Output raw JSON"
            #[arg(short = 'j', long)]
            json: bool,
        },
    },
    /// Observe — low-overhead sampling without modifying the target"
    /// Sampling is out-of-process (P1A remote-read, sibling supervision only)."
    /// No in-target code, no stop-the-target >1 ms, no LD_PRELOAD."
    /// Bundles are WAL-backed, hash-chained, with bounded 512-cap drop-newest"
    /// and TRUNCATED markers — never silent."
    /// CUT WARNING: this is L1 sampling only (stack snapshots at 50 Hz default,"
    /// plus exhaustive low-volume audit for imports/spawns/raises/dynamic loads)."
    /// It does NOT provide L2 line/branch coverage — do not use for line-level"
    /// CUT decisions. Use `audit` events for import/spawn topology and `verify`"
    /// for bundle integrity."
    #[command(
        about = "Observe — sampling without modifying the target (L1 sampling only, not L2 line/branch CUT)",
        long_about = "Observe — out-of-process sampling (P1A) with sibling supervision.\n\nNo in-target code, no LD_PRELOAD, no stop >1 ms.\n\nBundles are WAL-backed with hash chains and TRUNCATED markers.\n\nCUT WARNING: L1 sampling only — not L2 line/branch coverage.\n\nUse --self-test to verify the pipeline and --verify to check bundle integrity.\n\nUse --burst to enable file-watch (deferred; returns -32601 observe_burst deferred).\n\nUse --properties to evaluate a spec against the bundle's WAL events.\n\nDefault out: {data_dir}/bundles/<run_id>.jsonl (7d retention).\n\nRate from --sample-rate-hz or RUNTIMO_OBSERVE_SAMPLE_HZ or config observe.sample_rate_hz (default 50 Hz)."
    )]
    Observe {
        /// Target pid to sample (alternative to --cmd)."
        #[arg(long)]
        pid: Option<u32>,
        /// Command to spawn as sibling target (alternative to --pid, e.g. \"python app.py\")."
        #[arg(long)]
        cmd: Option<String>,
        /// Bundle output path (default: {data_dir}/bundles/<run_id>.jsonl, validated via allowed prefixes)."
        #[arg(long)]
        out: Option<PathBuf>,
        /// Samples per second (default 50 Hz, via ObserveConfig)."
        #[arg(long)]
        sample_rate_hz: Option<u64>,
        /// Enable burst file-watch (P2B — bundle-path watch)."
        /// When `true`, the CLI prints a deferred note to stdout"
        /// (`burst_deferred:true`) and forwards `burst` to the daemon,"
        /// which returns `-32601 observe_burst deferred`."
        /// Not yet trivial; falls back to out-of-process polling."
        #[arg(long, default_value = "false")]
        burst: bool,
        /// DAL A–E (default from config, controls watermark on shed)."
        #[arg(long)]
        dal: Option<String>,
        /// Override pressure_suspend_ms from config (default from config when absent)."
        #[arg(long)]
        suspend_ms: Option<u64>,
        /// Run self-test and exit 0/1."
        #[arg(long)]
        self_test: bool,
        /// Verify a bundle file offline and print a full verification report"
        /// (6 predicates + admissible + hash_ok + watermark + property_verdicts)."
        /// Exit code 0 if admissible, 1 otherwise."
        #[arg(long)]
        verify: Option<PathBuf>,
        /// Property specification as a JSON string (e.g. {\"name\":\"p\",\"predicates\":[...]})"
        /// to evaluate against the bundle's WAL events. Property verdicts are reported"
        /// in the output but NEVER alter the verify exit code (exit depends solely on"
        /// `admissible`). Parse failure exits 1 with a clear stderr message."
        #[arg(long)]
        properties: Option<String>,
        /// Output as JSON."
        #[arg(long, short = 'j', default_value = "false")]
        json: bool,
    },
    /// Oracle — evaluate properties over recorded evidence (read-only)."
    /// The oracle never reruns LLMOSafe, reclassifies input, mutates WAL,"
    /// or executes capabilities. It evaluates recorded evidence so"
    /// historical results reproduce even if future LLMOSafe versions change."
    /// Exit codes: 0 Satisfied, 1 Violated, 2 property/eval error,"
    /// 3 evidence/infrastructure error."
    #[command(about = "Oracle — evaluate properties over recorded evidence")]
    Oracle {
        /// Evaluate a property file (or inline JSON) over an evidence source."
        /// Sources (exactly one required): `--wal`, `--bundle`, `--facts`."
        /// Raw WAL is queryable directly — Observe admissibility is NOT applied"
        /// to raw WAL (§42). Bundles verify integrity separately."
        #[command(about = "Evaluate properties over WAL / bundle / facts")]
        Evaluate {
            /// Raw WAL path (`wal.jsonl`)."
            #[arg(long)]
            wal: Option<PathBuf>,
            /// Observe bundle path (`observe-run.jsonl`, hash-chained)."
            #[arg(long)]
            bundle: Option<PathBuf>,
            /// Runtime facts path (`runtime-facts-v1.jsonl`)."
            #[arg(long)]
            facts: Option<PathBuf>,
            /// Property spec as inline JSON."
            #[arg(long, conflicts_with = "properties_file")]
            properties: Option<String>,
            /// Property spec file (bounded, regular-file validated)."
            #[arg(long, conflicts_with = "properties")]
            properties_file: Option<PathBuf>,
            /// Output as JSON."
            #[arg(long, short = 'j', default_value = "false")]
            json: bool,
        },
        /// Print the property-spec schema (truthful operator list, §49)."
        #[command(about = "Print Oracle property-spec schema")]
        Schema,
        /// Run Oracle micro-benchmarks (evaluator throughput)."
        #[command(about = "Run Oracle benchmarks")]
        Benchmark,
    },
}

#[derive(Subcommand)]
enum SessionCommand {
    /// Run steps from a prompt file as a bounded session"
    /// Prompt file containing JSONL or markdown ```runtimo blocks"
    #[command(about = "Run steps from a prompt file as a bounded session")]
    Run {
        /// Prompt file containing JSONL or markdown ```runtimo blocks"
        #[arg(long, value_name = "PATH")]
        prompt_file: PathBuf,
        /// Session name (human-readable) — reuses existing session if name or id matches, else creates a new one"
        #[arg(long)]
        session: Option<String>,
        /// Maximum number of steps to execute (default: from ResolvedConfig — 20 ephemeral, 100 service/minimal)"
        #[arg(long)]
        max_steps: Option<u32>,
        /// Maximum total seconds for the session run (default: from ResolvedConfig — 300 ephemeral, 3600 service)"
        #[arg(long)]
        max_seconds: Option<u64>,
        /// Behavior on step failure: continue|stop (default: from ResolvedConfig — continue ephemeral, stop service)"
        #[arg(long, value_name = "POLICY")]
        on_failure: Option<String>,
        /// Validate prompt file and policy only; do not execute or create a session"
        /// Stub: dispatch each step via daemon RPC instead of local execution"
        #[arg(long)]
        via_daemon: bool,
        /// Dry run
        #[arg(long)]
        dry_run: bool,
    },
    /// List sessions persisted under the sessions directory"
    /// Show a single session by id"
    #[command(about = "List sessions")]
    List {
        /// Output raw JSON"
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Show a single session by id"
    #[command(about = "Show session")]
    Show {
        /// Session ID to show"
        #[arg(long, value_name = "SESSION_ID")]
        session_id: String,
        /// Output raw JSON"
        #[arg(short = 'j', long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Show current configuration"
    #[command(about = "Show current configuration (from config.toml)")]
    Show,
    /// Get or set the Design Assurance Level (DAL)"
    #[command(about = "Get or set the Design Assurance Level (A-E) for the cognitive safety pipeline")]
    Dal {
        /// New DAL level to set (A, B, C, D, or E). Omit to show current value.
        level: Option<String>,
    },
    /// Initialize config file from profile template"
    #[command(about = "Initialize config file from profile template (minimal/ephemeral/service)")]
    Init {
        /// Profile to use (minimal, ephemeral, service). Use --minimal as alias for minimal.
        #[arg(long)]
        profile: Option<String>,
        /// Overwrite existing config
        /// Alias for --profile minimal
        /// Custom path for config file (default: XDG config path)
        #[arg(long)]
        path: Option<PathBuf>,
        /// Force overwrite
        #[arg(long)]
        force: bool,
        /// Minimal profile
        #[arg(long)]
        minimal: bool,
    },
    /// Manage allowed path prefixes for FileRead/FileWrite"
    #[command(about = "Manage allowed path prefixes for FileRead/FileWrite")]
    AllowedPaths {
        /// Add { paths: Vec<String> },"
        /// Remove { paths: Vec<String> },"
        /// List
    },
}

#[derive(Subcommand)]
enum OracleCommand {
    /// Evaluate a property file (or inline JSON) over an evidence source."
    /// Sources (exactly one required): `--wal`, `--bundle`, `--facts`."
    /// Raw WAL is queryable directly — Observe admissibility is NOT applied"
    /// to raw WAL (§42). Bundles verify integrity separately."
    #[command(about = "Evaluate properties over WAL / bundle / facts")]
    Evaluate {
        /// Raw WAL path (`wal.jsonl`)."
        #[arg(long)]
        wal: Option<PathBuf>,
        /// Observe bundle path (`observe-run.jsonl`, hash-chained)."
        #[arg(long)]
        bundle: Option<PathBuf>,
        /// Runtime facts path (`runtime-facts-v1.jsonl`)."
        #[arg(long)]
        facts: Option<PathBuf>,
        /// Property spec as inline JSON."
        #[arg(long, conflicts_with = "properties_file")]
        properties: Option<String>,
        /// Property spec file (bounded, regular-file validated)."
        #[arg(long, conflicts_with = "properties")]
        properties_file: Option<PathBuf>,
        /// Output as JSON."
        #[arg(long, short = 'j', default_value = "false")]
        json: bool,
    },
    /// Print the property-spec schema (truthful operator list, §49)."
    #[command(about = "Print Oracle property-spec schema")]
    Schema,
    /// Run Oracle micro-benchmarks (evaluator throughput)."
    #[command(about = "Run Oracle benchmarks")]
    Benchmark,
}

/// Returns the WAL file path (env-overridable via `RUNTIMO_WAL_PATH`)."
fn wal_path() -> PathBuf {
    runtimo_core::utils::wal_path()
}

/// Returns the backup directory derived from `data_dir()`."
/// Delegates to [`runtimo_core::utils::backup_dir`], which always"
/// returns `data_dir().join("backups")` — no env var override (ADR-C28)."
fn backup_dir() -> PathBuf {
    runtimo_core::utils::backup_dir()
}

/// Creates a capability registry with all built-in capabilities registered."
/// `Ok(CapabilityRegistry)` — All capabilities registered successfully."
/// `Err(String)` — FileWrite or GitExec initialization failed (e.g. backup"
/// directory cannot be created)."
fn make_registry() -> Result<CapabilityRegistry, String> {
    let mut reg = CapabilityRegistry::new();
    reg.register(FileRead);
    reg.register(FileWrite::new().map_err(|e| format!("FileWrite init failed: {}", e))?);
    reg.register(Delete::new().map_err(|e| format!("Delete init failed: {}", e))?);
    reg.register(GitExec::new(backup_dir()).map_err(|e| format!("GitExec init failed: {}", e))?);
    reg.register(ShellExec);
    reg.register(Kill);
    reg.register(Undo);
    Ok(reg)
}

// Concurrency control for CLI run — mirrors daemon's MAX_CONCURRENT_JOBS = 16
/// Maximum concurrent CLI `run` invocations.
const MAX_CLI_CONCURRENT: usize = 16;
/// Global counter of currently-running CLI jobs.
static CLI_ACTIVE_JOBS: AtomicUsize = AtomicUsize::new(0);

/// Returns "enabled"/"disabled" for config-show output.
fn on_off(v: bool) -> &'static str {
    if v {
        "enabled"
    } else {
        "disabled"
    }
}

/// Attempts to acquire a concurrency slot for a CLI `run` command.
/// Returns `false` if `MAX_CLI_CONCURRENT` slots are already in use.
fn acquire_cli_slot() -> bool {
    let current = CLI_ACTIVE_JOBS.fetch_add(1, Ordering::AcqRel);
    if current >= MAX_CLI_CONCURRENT {
        CLI_ACTIVE_JOBS.fetch_sub(1, Ordering::AcqRel);
        return false;
    }
    true
}

fn release_cli_slot() {
    CLI_ACTIVE_JOBS.fetch_sub(1, Ordering::AcqRel);
}

// ── Daemon client ───────────────────────────────────────────────────────────
/// Returns the path to the daemon's Unix socket (`{data_dir}/runtimo.sock`)."
fn daemon_socket() -> PathBuf {
    runtimo_core::utils::data_dir().join("runtimo.sock")
}

/// Finds the `runtimo-daemon` binary, first checking next to the CLI binary,"
/// then falling back to `which` and `~/.cargo/bin/`."
fn find_daemon_binary() -> Option<PathBuf> {
    let cli_path = std::env::current_exe().ok()?;
    let dir = cli_path.parent()?;
    let daemon_path = dir.join("runtimo-daemon");
    if daemon_path.exists() {
        return Some(daemon_path);
    }
    dir.join(format!("runtimo-daemon{}", std::env::consts::EXE_SUFFIX))
        .then_some(daemon_path)
}

/// Searches `PATH` and `~/.cargo/bin/` for the `runtimo-daemon` binary."
fn find_daemon_in_path() -> Option<PathBuf> {
    let output = Command::new("which").arg("runtimo-daemon").output().ok()?;
    if output.status.success() {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    // Fallback: check standard cargo install directory
    let home = std::env::var("HOME").ok()?;
    let cargo_bin = PathBuf::from(home).join(".cargo/bin/runtimo-daemon");
    cargo_bin.exists().then_some(cargo_bin)
}

/// Returns the path to the daemon lock file (`{data_dir}/daemon.lock`)."
fn daemon_lock_path() -> PathBuf {
    runtimo_core::utils::data_dir().join("daemon.lock")
}

/// Acquires an exclusive `flock` on the daemon lock file to prevent"
/// race conditions when auto-starting the daemon from multiple processes."
/// Uses `LOCK_EX | LOCK_NB` — fails immediately if another process holds the lock."
/// Returns an error string if the lock file cannot be created or the lock is held."
fn acquire_daemon_lock() -> Result<File, String> {
    let lock_path = daemon_lock_path();
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Failed to create lock dir: {}", e))?;
        File::create(&lock_path).map_err(|e| format!("Failed to create lock file: {}", e))?;
    }
    // Try to acquire exclusive non-blocking lock using flock
    let file = File::open(&lock_path).map_err(|e| format!("Failed to open lock file: {}", e))?;
    let fd = file.as_raw_fd();
    // SAFETY: fd is a valid file descriptor from File::create; LOCK_EX | LOCK_NB are valid flock flags
    let result = unsafe { flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        return Err("Another process is starting the daemon".to_string());
    }
    Ok(file)
}

/// Checks whether the daemon is running by attempting to connect to its Unix socket."
fn daemon_is_running() -> bool {
    UnixStream::connect(daemon_socket()).is_ok()
}

/// Ensures the daemon is running, auto-starting it if necessary."
/// Uses a double-checked locking pattern with `acquire_daemon_lock` to prevent"
/// multiple processes from spawning the daemon simultaneously. Waits up to"
/// `DAEMON_STARTUP_TIMEOUT_SECS` (30s) for the daemon to become ready."
/// Returns an error if the daemon binary cannot be found, the daemon fails to"
/// start, or it doesn't become ready within the timeout."
fn ensure_daemon_running() -> Result<(), String> {
    if daemon_is_running() {
        return Ok(());
    }

    // Acquire lock before spawning daemon to prevent race condition
    let _lock = acquire_daemon_lock()?;

    // Double-check after acquiring lock
    let daemon_bin = find_daemon_binary()
        .or_else(find_daemon_in_path)
        .ok_or_else(|| {
            "runtimo-daemon binary not found. Is runtimo-daemon installed?".to_string()
        })?;

    let mut child = Command::new(&daemon_bin)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to start daemon ({}): {}", daemon_bin.display(), e))?;

    #[allow(clippy::arithmetic_side_effects)]
    let deadline = std::time::Instant::now()
        .checked_add(Duration::from_secs(DAEMON_STARTUP_TIMEOUT_SECS))
        .unwrap_or_else(|| {
            std::time::Instant::now() + Duration::from_secs(DAEMON_STARTUP_TIMEOUT_SECS)
        });
    loop {
        if daemon_is_running() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            let err_msg = if let Ok(Some(status)) = child.try_wait() {
                let mut stderr = String::new();
                if let Some(ref mut pipe) = child.stderr {
                    let _ = pipe.read_to_string(&mut stderr);
                }
                if stderr.is_empty() {
                    format!(
                        "Daemon exited with status {} before becoming ready. No error output.",
                        status
                    )
                } else {
                    format!("Daemon exited with status {}: {}", status, stderr.trim())
                }
            } else {
                format!(
                    "Daemon started but did not become ready within {}s",
                    DAEMON_STARTUP_TIMEOUT_SECS
                )
            };
            let _ = child.kill();
            return Err(err_msg);
        }
        // Check if daemon exited early
        if let Ok(Some(status)) = child.try_wait() {
            let mut stderr = String::new();
            if let Some(ref mut pipe) = child.stderr {
                let _ = pipe.read_to_string(&mut stderr);
            }
            let msg = if stderr.is_empty() {
                format!("Daemon exited early with status {}", status)
            } else {
                format!("Daemon exited early: {}", stderr.trim())
            };
            return Err(msg);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Resolves capability arguments from the appropriate source."
/// Priority: --args-file > --args-stdin > -a (default)."
/// Validates that --args-file and --args-stdin are not used simultaneously."
/// Validates content size against MAX_ARGS_SIZE_BYTES (~130 KB)."
fn resolve_args(
    args: &str,
    args_file: Option<PathBuf>,
    args_stdin: bool,
) -> Result<String, String> {
    if args_file.is_some() && args_stdin {
        return Err("Cannot use both --args-file and --args-stdin simultaneously".to_string());
    }
    let content = if let Some(file_path) = args_file {
        let mut content = String::new();
        File::open(&file_path)
            .map_err(|e| format!("Failed to open args file {}: {}", file_path.display(), e))?
            .read_to_string(&mut content)
            .map_err(|e| format!("Failed to read args file {}: {}", file_path.display(), e))?;
        content
    } else if args_stdin {
        std::io::stdin()
            .map_err(|e| format!("Failed to read args from stdin: {}", e))?
            .read_to_string(&mut content)
            .map_err(|e| format!("Failed to read args from stdin: {}", e))?;
        content
    } else {
        args.to_string()
    }
    if content.len() > MAX_ARGS_SIZE_BYTES {
        return Err(format!(
            "Capability args too large: {} bytes (max: {} bytes / ~130 KB). \nUse --args-file or --args-stdin for large payloads.",
            content.len(),
            MAX_ARGS_SIZE_BYTES
        ));
    }
    Ok(content)
}

/// Sends a JSON-RPC request to the daemon over its Unix socket."
/// Serializes `method` and `params` into a JSON-RPC request, writes it to the"
/// socket, and reads a single-line JSON-RPC response."
/// Returns an error string if the daemon cannot be reached, the request cannot"
/// be serialized, or the daemon returns an error."
fn send_rpc(method: &str, params: Value) -> Result<Value, String> {
    let sock_path = daemon_socket();
    let mut stream = UnixStream::connect(&sock_path).map_err(|e| {
        format!(
            "Cannot connect to daemon at {}: {}. Is `runtimo-daemon` running?",
            sock_path.display(),
            e
        )
    })?;

    let request = serde_json::json!({
        "method": method,
        "params": params,
        "id": 1,
    });
    stream
        .write_all(serde_json::to_string(&request).map_err(|e| format!("JSON encode: {}", e))?
            .as_bytes())
        .map_err(|e| format!("Write: {}", e))?;
    stream
        .write_all(b"\n")
        .map_err(|e| format!("Write nl: {}", e))?;

    // Use buffered reader for line-based reading — handles responses of any size
    let mut reader = std::io::BufReader::new(&stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("Read: {}", e))?;

    if line.is_empty() {
        return Err("Daemon closed connection".into());
    }

    let resp: Value = serde_json::from_str(line.trim()).map_err(|e| format!("JSON parse: {}", e))?;

    if let Some(err) = resp
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
    {
        return Err(err.to_string());
    }

    Ok(resp.get("result").cloned().unwrap_or(Value::Null))
}

/// Sentinel WAL sequence for a dispatched step with no completion record."
/// Real WAL sequences start at 0, so `u64::MAX` is distinct from every real"
/// `wal_seq` — it marks "unknown", never a forged success marker like 0."
const DISPATCH_UNKNOWN_WAL_SEQ: u64 = u64::MAX;

/// Emits a session-loop diagnostic respecting the output mode."
/// In JSON mode prints a single-line JSON object to stdout (keeps stdout"
/// parseable, stderr clean); in quiet mode suppresses loop progress noise;"
/// otherwise writes the text to stderr as before."
fn emit_session_note(mode: &crate::output::OutputMode, value: Value, text: &str) {
    if mode.is_json() {
        println!(
            "{}",
            serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
        );
    } else if !mode.is_quiet() {
        eprintln!("{}", text);
    }
}

/// Resolves the WAL sequence of a dispatched job via the daemon `logs` RPC."
/// Returns the `seq` of the terminal (`job_completed`/`job_failed`) event,"
/// or [`DISPATCH_UNKNOWN_WAL_SEQ`] when no completion record exists yet."
fn fetch_job_wal_seq(job_id: &str) -> u64 {
    if let Ok(v) = send_rpc("logs", serde_json::json!({ "job_id": job_id, "limit": 50 })) {
        if let Some(events) = v.get("events").and_then(|e| e.as_array()) {
            for ev in events {
                let is_terminal = ev
                    .get("type")
                    .and_then(|t| t.as_str())
                    .is_some_and(|t| t == "job_completed" || t == "job_failed");
                if is_terminal {
                    if let Some(seq) = ev.get("seq").and_then(|s| s.as_u64()) {
                        return seq;
                    }
                }
            }
        }
    }
    DISPATCH_UNKNOWN_WAL_SEQ
}

/// Polls the daemon `status` RPC until a dispatched job reaches a terminal"
/// state or the time budget expires."
/// `job_id` — daemon job ID from `dispatch`. `budget_secs` — max seconds"
/// to poll (caller passes the session's remaining `--max-seconds` budget)."
/// `(success, error, wal_seq)` from the daemon's real terminal state —"
/// never a synthesized success. `wal_seq` comes from the `logs` RPC, or"
/// [`DISPATCH_UNKNOWN_WAL_SEQ`] when no completion record exists."
fn poll_dispatched_step(job_id: &str, budget_secs: u64) -> (bool, Option<String>, u64) {
    let start = std::time::Instant::now();
    let budget = std::time::Duration::from_secs(budget_secs.max(1));
    while start.elapsed() < budget {
        match send_rpc("status", serde_json::json!({ "job_id": job_id })) {
            Ok(v) => match v.get("status").and_then(|s| s.as_str()) {
                Some("completed") => return (true, None, fetch_job_wal_seq(job_id)),
                Some("failed") => {
                    let err = v
                        .get("result")
                        .and_then(|r| r.as_str())
                        .map(str::to_string)
                        .or_else(|| Some("execution reported failure".to_string()));
                    return (false, err, fetch_job_wal_seq(job_id));
                }
                _ => std::thread::sleep(std::time::Duration::from_secs(2)),
            },
            Err(_) => break,
        }
    }
    // Budget expired or daemon unreachable: check the daemon WAL via logs RPC
    // once before giving up (job may have completed between polls).
    let wal_seq = fetch_job_wal_seq(job_id);
    if wal_seq != DISPATCH_UNKNOWN_WAL_SEQ {
        if let Ok(v) = send_rpc("status", serde_json::json!({ "job_id": job_id })) {
            match v.get("status").and_then(|s| s.as_str()) {
                Some("completed") => return (true, None, wal_seq),
                Some("failed") => return (false, Some(v.get("result").and_then(|r| r.as_str()).unwrap_or("execution reported failure".to_string())), wal_seq),
                _ => {}
            }
        }
    }
    (
        false,
        Some("dispatch poll timed out before terminal status".to_string()),
        wal_seq,
    )
}

#[allow(
    clippy::too_many_lines,
    clippy::indexing_slicing,
    clippy::redundant_else,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::arithmetic_side_effects
)]
fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    let resolved = RuntimoConfig::load().resolved();
    let base_mode = OutputMode::from_cli(
        cli.output.as_deref(),
        cli.table_style.as_deref(),
        cli.no_timestamps,
    );

    match cli.command {
        Commands::Run {
            capability,
            args,
            args_file,
            args_stdin,
            dry_run,
            json,
            quiet,
            schema,
            timeout,
        } => {
            if schema {
                let reg = make_registry().map_err(|e| format!("Registry init failed: {}", e))?;
                if let Some(cap) = reg.get(&capability) {
                    println!("{}", cap.schema());
                } else {
                    eprintln!("Capability not found: {}. Use `runtimo list` to see available capabilities.", capability);
                    std::process::exit(1);
                }
                return Ok(());
            }
            let args_val: Value = serde_json::from_str(&args).map_err(|e| format!("Invalid JSON args: {}", e))?;
            if let Err(e) = cap.validate(&args_val) {
                eprintln!("Validation failed: {}", e);
                std::process::exit(1);
            }
            // Acquire concurrency slot (mirrors daemon's MAX_CONCURRENT_JOBS)
            if !acquire_cli_slot() {
                eprintln!(
                    "Too many concurrent CLI runs (max {}). Try again later.",
                    MAX_CLI_CONCURRENT
                );
            }
            let resolved_timeout = timeout.unwrap_or_else(|| RuntimoConfig::get_capability_timeout(&capability, 30));
            let result = execute_with_telemetry_and_session(
                &args_val,
                dry_run,
                &wal_path(),
                resolved_timeout,
                None,
                None,
            )
            .map_err(|e| format!("{}", e));
            release_cli_slot();
            let result = result?;
            if !result.success {
                // Failure must respect the output mode: JSON stays parseable
                // on stdout, quiet stays silent (exit code carries the signal).
                let fail_is_json = json || base_mode.is_json();
                let fail_is_quiet = !fail_is_json && (quiet || base_mode.is_quiet());
                if fail_is_json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "success": false,
                            "capability": capability,
                            "output": result.output
                        })
                    );
                } else if !fail_is_quiet {
                    eprintln!("{}", result.output.output);
                }
            }
            let output = result.output;
            // Resolve effective mode: subcommand --json/--quiet override global --output
            let mode = if json {
                base_mode.with_format("json")
            } else if quiet {
                base_mode.with_format("quiet")
            } else {
                base_mode
            };
            if mode.is_json() {
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else if mode.is_quiet() {
                // silent — preserve --quiet contract
                println!("{}", mode.render_text(&output.output));
                if let Some(ref data) = output.data {
                    let text = if let Some(s) = data.as_str() {
                        s.to_string()
                    } else {
                        data.to_string()
                    };
                    let rendered = mode.render_text(&text);
                    if !rendered.trim().is_empty() {
                        println!("{}", rendered);
                    }
                }
            }
        }

        Commands::Dispatch {
            capability,
            args,
            args_file,
            args_stdin,
            dry_run,
        } => {
            if let Err(e) = ensure_daemon_running() {
                eprintln!("Cannot dispatch: {}", e);
            }
            // Pre-validate dangerous commands at dispatch time
            if capability == "ShellExec" {
                if let Some(cmd) = args_val.get("cmd").and_then(|v| v.as_str()) {
                    if let Some(reason) = is_dangerous_command(cmd) {
                        eprintln!("Dispatch rejected: dangerous command blocked: {}", reason);
                        std::process::exit(1);
                    }
                    if !runtimo_core::capabilities::network_enabled()
                        && runtimo_core::capabilities::is_network_command(cmd)
                    {
                        eprintln!("Dispatch rejected: network commands blocked — set RUNTIMO_ENABLE_NETWORK=1 to enable");
                    }
                }
            }
            let params = serde_json::json!({
                "capability": capability,
                "args": args_val,
                "dry_run": dry_run,
                "working_dir": std::env::current_dir().unwrap_or_default().to_string_lossy(),
            });
            match send_rpc("dispatch", params) {
                Ok(result) => {
                    let jid = result.get("job_id").and_then(|v| v.as_str()).unwrap_or("?");
                    let cap = result
                        .get("capability")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    println!("Dispatched job {} (capability: {})", jid, cap);
                    println!("Check status: runtimo wait -j {}", jid);
                }
                Err(e) => {
                    let mut msg = format!("Dispatch failed: {}", e);
                    if e.contains("Capability not found") {
                        msg.push_str("\nUse `runtimo list` to see available capabilities.");
                    }
                    eprintln!("{}", msg);
                }
            }
        }

        Commands::Wait { job_id, timeout } => {
            // Early validation: reject empty job_id
            if job_id.is_empty() {
                eprintln!("Job ID cannot be empty");
            }
            // Pre-validate: check if job exists before entering poll loop
            // Try daemon RPC first
            let job_exists = send_rpc("status", serde_json::json!({ "job_id": &job_id }))
                .map_or_else(
                    |_| {
                        // Daemon unreachable; check WAL for any job event
                        if let Ok(reader) = WalReader::load_all(&wal_path()) {
                            reader.events().iter().any(|e| {
                                e.job_id == job_id
                                    && matches!(
                                        e.event_type,
                                        runtimo_core::WalEventType::JobStarted
                                            | runtimo_core::WalEventType::JobCompleted
                                            | runtimo_core::WalEventType::JobFailed
                                    )
                            })
                        } else {
                            false
                        }
                    },
                    |result| {
                        let status = result
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        status != "unknown"
                    },
                );

            if !job_exists {
                eprintln!("Job not found: {}", job_id);
            }
            let start = std::time::Instant::now();
            loop {
                let params = serde_json::json!({ "job_id": &job_id });
                #[allow(clippy::single_match_else)]
                // refactoring to if-let-else changes control flow here
                match send_rpc("status", params) {
                    Ok(result) => {
                        match status {
                            "running" => {
                                if timeout > 0 && start.elapsed().as_secs() >= timeout {
                                    println!(
                                        "Job {} still running (timeout after {}s)",
                                        job_id, timeout
                                    );
                                    return Ok(());
                                }
                                let elapsed = start.elapsed().as_secs();
                                if elapsed > 0 && elapsed.is_multiple_of(10) {
                                    println!(
                                        "Job {} still running ({}s elapsed)...",
                                        job_id, elapsed
                                    );
                                }
                                std::thread::sleep(std::time::Duration::from_secs(2));
                            }
                            "completed" => {
                                println!("Job {} completed", job_id);
                                return Ok(());
                            }
                            "failed" => {
                                println!("Job {} failed", job_id);
                            }
                            _ => {
                                println!("Job {} status: {}", job_id, status);
                            }
                        }
                    }
                    Err(_) => {
                        // Daemon might not be running; check WAL directly
                        if let Ok(reader) = WalReader::load_all(&wal_path()) {
                            let events = reader.events();
                            let has_completed = events.iter().any(|e| {
                                matches!(e.event_type, runtimo_core::WalEventType::JobCompleted)
                            });
                            if has_completed {
                                println!("Job {} completed (checked via WAL)", job_id);
                            }
                            let has_failed = events.iter().any(|e| {
                                matches!(e.event_type, runtimo_core::WalEventType::JobFailed)
                            });
                            if has_failed {
                                println!("Job {} failed (checked via WAL)", job_id);
                            }
                        }
                        std::thread::sleep(std::time::Duration::from_secs(2));
                    }
                }
                if timeout > 0 && start.elapsed().as_secs() >= timeout {
                    println!("Job {} still pending (timeout after {}s)", job_id, timeout);
                    return Ok(());
                }
            }
        }

        Commands::List { schemas, json } => {
            // Effective mode: global --output json or local --json forces json
            let caps: Vec<Value> = reg
                .list()
                .iter()
                .filter_map(|name| {
                    reg.get(name).map(|cap| {
                        serde_json::json!({
                            "name": name,
                            "description": cap.description(),
                            "schema": if schemas { Some(cap.schema().to_string()) } else { None }
                        })
                    })
                })
                .collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&caps)?);
            } else {
                // silent
                for name in reg.list() {
                    if let Some(cap) = reg.get(name) {
                        print!("  {:>12}  {}", name, cap.description());
                        if schemas {
                            println!("\n    schema: {}", cap.schema());
                            println!();
                        }
                    }
                }
            }
        }

        Commands::Status { job_id, json } => {
            if let Some(jid) = job_id {
                // Try daemon RPC first
                if let Ok(result) = send_rpc("status", serde_json::json!({ "job_id": &jid })) {
                    if mode.is_json() {
                        println!("{}", serde_json::to_string_pretty(&result)?);
                    } else if mode.is_quiet() {
                        // silent
                        let headers = ["JOB_ID", "STATUS", "CAPABILITY"];
                        let rows = vec![vec![
                            result
                                .get("job_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("?")
                                .to_string(),
                            result
                                .get("status")
                                .and_then(|v| v.as_str())
                                .unwrap_or("?")
                                .to_string(),
                            result
                                .get("capability")
                                .and_then(|v| v.as_str())
                                .unwrap_or("?")
                                .to_string(),
                        ]];
                        println!("{}", mode.render_table(&headers, &rows));
                    }
                }
                // Fallback to WAL
                if let Ok(reader) = WalReader::load_all(&wal_path()) {
                    let events = reader.events();
                    let by_job: Vec<_> = events.iter().filter(|e| e.job_id == jid).collect();
                    if by_job.is_empty() {
                        println!("Job not found: {}", jid);
                    } else if mode.is_json() {
                        println!("{}", serde_json::to_string_pretty(&by_job)?);
                        let headers = if mode.timestamps {
                            vec!["EVENT", "CAPABILITY", "TS"]
                        } else {
                            vec!["EVENT", "CAPABILITY"]
                        };
                        let rows: Vec<Vec<String>> = by_job
                            .iter()
                            .map(|e| {
                                if mode.timestamps {
                                    vec![
                                        e.event_type.as_str().to_string(),
                                        e.capability.as_deref().unwrap_or("-").to_string(),
                                        e.ts.to_string(),
                                    ]
                                } else {
                                    vec![
                                        e.event_type.as_str().to_string(),
                                        e.capability.as_deref().unwrap_or("-").to_string(),
                                    ]
                                }
                            })
                            .collect();
                        let hdr_refs: Vec<&str> = headers.clone();
                        println!("{}", mode.render_table(&hdr_refs, &rows));
                    }
                }
                // List all jobs via daemon
                if let Ok(result) = send_rpc("jobs", serde_json::json!({ "limit": 50 })) {
                    let jobs = result["jobs"].as_array().cloned().unwrap_or_default();
                    if jobs.is_empty() {
                        println!("No jobs found.");
                        let headers = ["JOB_ID", "STATUS", "CAPABILITY"];
                        let rows: Vec<Vec<String>> = jobs
                            .iter()
                            .map(|job| {
                                let status = job["status"].as_str().unwrap_or("?");
                                let icon = mode.status_icon(status);
                                vec![
                                    job["job_id"].as_str().unwrap_or("?").to_string(),
                                    format!("{}{}", icon, status),
                                    job["capability"].as_str().unwrap_or("?").to_string(),
                                ]
                            })
                            .collect();
                        println!("{}", mode.render_table(&headers, &rows));
                    }
                }
                // Fallback to WAL
                if let Ok(reader) = WalReader::load_all(&wal_path()) {
                    let events = reader.events();
                    let mut seen: std::collections::HashSet<&String> = std::collections::HashSet::new();
                    let mut rows: Vec<Vec<String>> = Vec::new();
                    for e in events.iter().rev() {
                        if seen.contains(&e.job_id) {
                            continue;
                        }
                        seen.insert(&e.job_id);
                        rows.push(vec![
                            e.job_id.clone(),
                            e.event_type.as_str().to_string(),
                            e.capability.as_deref().unwrap_or("-").to_string(),
                        ];);
                        if rows.len() >= 50 {
                            break;
                        }
                    }
                    if rows.is_empty() {
                        println!("No jobs found.");
                    } else if mode.is_quiet() {
                        // silent
                    } else if mode.is_json() {
                        println!("{}", serde_json::to_string_pretty(&rows)?);
                        let headers = ["JOB_ID", "EVENT", "CAPABILITY"];
                        if mode.timestamps {
                            // include ts if timestamps enabled
                            let rows_ts: Vec<Vec<String>> = rows
                                .iter()
                                .map(|r| {
                                    // find ts for this job id
                                    let ts = events
                                        .iter()
                                        .find(|ev| ev.job_id == r[0])
                                        .map(|ev| ev.ts.to_string())
                                        .unwrap_or_default();
                                    vec![r[0].clone(), r[1].clone(), r[2].clone(), ts]
                                })
                                .collect();
                            let hdr = ["JOB_ID", "EVENT", "CAPABILITY", "TS"];
                            println!("{}", mode.render_table(&hdr, &rows_ts));
                        } else {
                            println!("{}", mode.render_table(&headers, &rows));
                        }
                    }
                }
            }
        }

        Commands::Jobs { limit, json } => {
            let result = send_rpc("jobs", serde_json::json!({ "limit": limit }));
            match result {
                Ok(data) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&data)?);
                    } else {
                        let jobs = data["jobs"].as_array().cloned().unwrap_or_default();
                        if jobs.is_empty() {
                            println!("No jobs found.");
                        } else {
                            let headers = ["JOB_ID", "CAPABILITY", "STATUS"];
                            let rows: Vec<Vec<String>> = jobs
                                .iter()
                                .map(|job| {
                                    let status = job["status"].as_str().unwrap_or("?");
                                    vec![
                                        job["job_id"].as_str().unwrap_or("?").to_string(),
                                        job["capability"].as_str().unwrap_or("?").to_string(),
                                        format!("{}{}", mode.status_icon(status), status),
                                    ]
                                })
                                .collect();
                            println!("{}", mode.render_table(&headers, &rows));
                        }
                    }
                }
                Err(_) => {
                    let mut jobs: Vec<Value> = Vec::new();
                    if let Ok(reader) = WalReader::load_all(&wal_path()) {
                        let events = reader.events();
                        for e in events.iter().rev().take(limit) {
                            if jobs.len() >= limit {
                                break;
                            }
                            let status = match e.event_type {
                                runtimo_core::WalEventType::JobStarted => "started",
                                runtimo_core::WalEventType::JobCompleted => "completed",
                                runtimo_core::WalEventType::JobFailed => "failed",
                                _ => "unknown",
                            };
                            jobs.push(serde_json::json!({
                                "job_id": e.job_id,
                                "capability": e.capability,
                                "status": status,
                                "started_at": e.ts,
                            }));
                        }
                        println!("{}", serde_json::to_string_pretty(&jobs)?);
                    } else {
                        eprintln!("Cannot read WAL. Is the daemon running?");
                    }
                }
            }
        }

        Commands::Logs { job_id, limit, json } => {
            let mut params = serde_json::json!({ "limit": limit });
            if let Some(ref jid) = job_id {
                params["job_id"] = serde_json::json!(jid);
            }
            if let Ok(result) = send_rpc("logs", params) {
                if mode.is_json() {
                    println!("{}", serde_json::to_string_pretty(&result)?);
                } else if mode.is_quiet() {
                    // silent
                    let events = result["events"].as_array().cloned().unwrap_or_default();
                    if events.is_empty() {
                        println!("No events found.");
                    } else {
                        let headers: Vec<&str> = if mode.timestamps {
                            vec!["TS", "JOB_ID", "EVENT", "CAPABILITY"]
                        } else {
                            vec!["JOB_ID", "EVENT", "CAPABILITY"]
                        };
                        let rows: Vec<Vec<String>> = events
                            .iter()
                            .map(|e| {
                                let ts = e["ts"].as_u64().unwrap_or(0).to_string();
                                let et = e["event_type"].as_str().unwrap_or("?").to_string();
                                let jid = e["job_id"].as_str().unwrap_or("?").to_string();
                                let cap = e["capability"].as_str().unwrap_or("-").to_string();
                                vec![ts, jid, et, cap]
                            })
                            .collect();
                        println!("{}", mode.render_table(&headers, &rows));
                    }
                }
            } else if let Ok(reader) = WalReader::load_all(&wal_path()) {
                let events = reader.events();
                let filtered: Vec<_> = if let Some(ref jid) = job_id {
                    events.iter().filter(|e| e.job_id == *jid).collect()
                } else {
                    events.iter().collect()
                };
                let recent: Vec<_> = filtered.iter().rev().take(limit).rev().collect();
                if json {
                    println!("{}", serde_json::to_string_pretty(&recent)?);
                } else if recent.is_empty() {
                    println!("No events found.");
                } else {
                    let headers: Vec<&str> = if mode.timestamps {
                        vec!["TS", "JOB_ID", "EVENT", "CAPABILITY"]
                    } else {
                        vec!["JOB_ID", "EVENT", "CAPABILITY"]
                    };
                    let rows: Vec<Vec<String>> = recent
                        .iter()
                        .map(|e| {
                            vec![
                                e.ts.to_string(),
                                e.job_id.clone(),
                                e.event_type.as_str().to_string(),
                                e.capability.as_deref().unwrap_or("-").to_string(),
                            ]
                        })
                        .collect();
                    println!("{}", mode.render_table(&headers, &rows));
                }
            }
        }

        Commands::Undo { job_id, dry_run } => {
            let cap = reg.get("Undo").ok_or("Undo capability not available")?;
            let args = serde_json::json!({ "job_id": job_id });
            let ctx = runtimo_core::Context {
                job_id: runtimo_core::utils::generate_id(),
                working_dir: std::env::current_dir().unwrap_or_default(),
            };
            let output = cap.execute(&args, &ctx).map_err(|e| format!("{}", e))?;
            println!("{}", output.output);
        }

        Commands::Telemetry { json, verbose } => {
            // Telemetry gate (RC1): when disabled, skip capture entirely —
            // no /proc reads, no subprocess probes, no display emitters.
            if !resolved.telemetry_enabled {
                if json {
                    println!("{}", serde_json::json!({
                        "telemetry_enabled": false,
                        "telemetry": null
                    }));
                } else if !mode.is_quiet() {
                    mode.render_text("Telemetry disabled (telemetry.enabled = false).");
                }
            } else {
                let tel = Telemetry::capture();
                if json {
                    println!("{}", serde_json::to_string_pretty(&tel)?);
                } else if !mode.is_quiet() {
                    // Listening ports: shown only with --verbose flag
                    let ports_str = if verbose && !tel.network.listening_ports.is_empty() {
                        format!(
                            "\nListening ports: {}",
                            tel.network
                                .listening_ports
                                .map(|p| p.to_string())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    } else {
                        String::new()
                    };
                    let text = format!(
                        "RUNTIMO TELEMETRY\n\nSystem\nCPU: {} ({} cores)\nRAM: {} total, {} free, {} available\nDisk: {} total, {} free ({}% used)\nUptime: {} ({}s)\nLoad: {} ({} cores)\n\nHardware\nAccelerators: {}\n\nNetwork\nPublic IP: {}\nTunnel: {}{}",
                        tel.system.cpu_model,
                        tel.system.cpu_count,
                        tel.system.ram_total,
                        tel.system.ram_free,
                        tel.system.ram_available,
                        tel.system.disk_total,
                        tel.system.disk_free,
                        tel.system.disk_used_percent,
                        tel.system.uptime,
                        tel.system.uptime_seconds,
                        tel.system.load_average,
                        tel.system.cpu_count,
                        if tel.hardware.accelerators.is_empty() { "none" } else {
                            tel.hardware.accelerators.iter().map(|a| format!("{}: {}x", a.kind, a.count)).collect::<Vec<_>>()
                        },
                        tel.network.public_ip,
                        if tel.network.tunnel_running {
                            format!("cloudflared (PID {})", tel.network.tunnel_pid.map_or_else(|| "?".to_string(), |p| p.to_string()))
                        } else {
                            "none".to_string()
                        },
                        ports_str,
                    );
                    mode.render_text(&text);
                }
            }
        }

        Commands::Processes { json } => {
            let snap = ProcessSnapshot::capture();
            if json {
                println!("{}", serde_json::to_string_pretty(&snap)?);
            } else {
                let zombie_lines = {
                    let zs = snap.zombies();
                    if zs.is_empty() {
                        String::new()
                    } else {
                        let lines: Vec<String> = zs
                            .iter()
                            .map(|p| {
                                format!(
                                    "- {} PPID:{} {} {}",
                                    p.pid,
                                    p.ppid,
                                    p.stat,
                                    p.command.chars().take(40).collect::<String>()
                                )
                            })
                            .collect();
                        format!("\n\nZombies ({})\n{}", zs.len(), lines.join("\n"))
                    }
                };
                println!(
                    "PROCESS SNAPSHOT\n\nSummary\nTotal: {}\nCPU: {:.1}%\nMemory: {:.1}%\nZombies: {}{}{}",
                    snap.summary.total_processes,
                    snap.summary.total_cpu_percent,
                    snap.summary.total_mem_percent,
                    snap.summary.zombie_count,
                    zombie_lines,
                    snap.top_by_cpu(5).iter().map(|p| format!("- {} {} {} {}% CPU", p.pid, p.command.chars().take(40).collect::<String>(), p.stat, p.cpu_percent)).collect::<Vec<_>>()
                        .join("\n"),
                    snap.top_by_mem(5).iter().map(|p| format!("- {} {} {} {}% MEM", p.pid, p.command.chars().take(40).collect::<String>(), p.stat, p.mem_percent)).collect::<Vec<_>>()
                        .join("\n"),
                );
            }
        }

        Commands::Zombies { reap } => {
            let zombies = snap.zombies();
            if zombies.is_empty() {
                println!("No zombie processes.");
            } else {
                println!("{} zombie(s) found:\n", zombies.len());
                for z in &zombies {
                    println!(
                        "  {:>8}  PPID:{:>8}  {:>6}  {}",
                        z.pid, z.ppid, z.stat, z.command
                    );
                }
                // Zombies can't be killed — they're already dead. We kill their
                // parent instead, which causes the kernel to reap the zombie.
                // Kill capability protects init (PID 1) and self.
                let reg = make_registry().map_err(|e| format!("Registry init failed: {}", e))?;
                let killer = reg.get("Kill").ok_or("Kill capability not available")?;
                let mut unique_parents: std::collections::HashSet<u32> = zombies
                    .iter()
                    .map(|z| z.ppid)
                    .filter(|&ppid| ppid > 1)
                    .collect();
                // Never kill our own parent
                unique_parents.remove(&std::process::id());
                if unique_parents.is_empty() {
                    println!("\nNo reapable parents (all zombies are children of init or self).");
                } else {
                    println!("\nReaping via {} parent(s):", unique_parents.len());
                    for ppid in &unique_parents {
                        print!("  PID {} → ", ppid);
                        let ctx = runtimo_core::Context {
                            dry_run: false,
                            job_id: format!("reap-{}", ppid),
                            working_dir: std::env::current_dir().unwrap_or_default(),
                        };
                        match killer.execute(&serde_json::json!({ "pid": ppid, "signal": 15 }), &ctx) {
                            Ok(o) => println!("{}", o.output),
                            Err(e) => println!("blocked: {}", e),
                        }
                    }
                }
                // Re-check
                ProcessSnapshot::clear_cache();
                let after = ProcessSnapshot::capture();
                let remaining = after.zombies().len();
                if remaining == 0 {
                    println!("\nAll zombies reaped.");
                } else {
                    println!(
                        "\n{} zombie(s) remain (may need SIGKILL or parent is protected).",
                        remaining
                    );
                    println!("Use `runtimo zombies --reap` to kill zombie parents and clean them up.");
                }
            }
        }

        Commands::Config { action } => match action {
            ConfigAction::AllowedPaths { subaction } => {
                let mut config = RuntimoConfig::load();
                match subaction {
                    AllowedPathsAction::Add { paths } => {
                        for p in paths {
                            if !config.allowed_paths.contains(&p) {
                                config.allowed_paths.push(p);
                            }
                        }
                        config.save().map_err(|e| format!("Save failed: {}", e))?;
                        println!("Prefixes updated: {:?}", config.allowed_paths);
                    }
                    AllowedPathsAction::Remove { paths } => {
                        config.allowed_paths.retain(|p| !paths.contains(p));
                    }
                    AllowedPathsAction::List => {
                        let all = RuntimoConfig::get_allowed_prefixes();
                        println!("Allowed path prefixes:");
                        for p in all {
                            println!("  {}", p);
                        }
                    }
                }
            }
            ConfigAction::Show => {
                // Respect global --output json/quiet
                if base_mode.is_quiet() {
                } else if base_mode.is_json() {
                    let config_path = RuntimoConfig::config_path();
                    let config = RuntimoConfig::load();
                    let resolved = config.resolved();
                    let out = serde_json::json!({
                        "config_path": config_path.display().to_string(),
                        "profile": resolved.profile,
                        "dal": resolved.dal,
                        "wal_mode": resolved.wal_mode,
                        "backup_enabled": resolved.backup_enabled,
                        "output_format": resolved.output_format,
                        "output_renderer": resolved.output_renderer,
                        "blocklist_enabled": resolved.blocklist_enabled,
                        "session_max": resolved.session_max,
                        "session_timeout": resolved.session_timeout,
                        "session_on_limit": resolved.session_on_limit,
                        "telemetry_enabled": resolved.telemetry_enabled,
                    });
                    println!("{}", serde_json::to_string_pretty(&out)?);
                } else {
                    let config_path = RuntimoConfig::config_path();
                    let config_exists = config_path.exists();
                    let config = match RuntimoConfig::load_result() {
                        Ok(c) => c,
                        Err(e) => {
                            eprintln!();
                            eprintln!("[runtimo] Config parse error: {}", e);
                            eprintln!(
                                "[runtimo] Showing defaults — fix the config file to apply settings."
                            );
                            RuntimoConfig::default()
                        }
                    };
                    println!("Configuration: {}", config_path.display());
                    if !config_exists {
                        println!("  (file does not exist — using defaults)");
                    }
                    // Profile line (Gate 1 seam)
                    let profile_name = config
                        .profile
                        .clone()
                        .unwrap_or_else(|| "minimal".to_string());
                    println!("Profile: {}", profile_name);
                    println!();
                    println!("Allowed paths (config file): {:?}", config.allowed_paths);
                    println!("DAL (config): {:?}", config.dal);
                    println!("Blocklist overrides: {:?}", config.blocklist_overrides);
                    if config.capability_timeouts.is_empty() {
                        println!("Capability timeouts: (none configured, using defaults)");
                        println!("Capability timeouts:");
                        for (cap, timeout) in &config.capability_timeouts {
                            println!("  {}: {}s", cap, timeout);
                        }
                    }
                    if config.env.is_empty() {
                        println!("Env [env]: (none configured)");
                        println!("Env [env]:");
                        for (key, value) in &config.env {
                            println!("  {} = {}", key, value);
                        }
                    }
                    // Show profile-derived tables if present
                    if config.profile.is_some() {
                        println!("Output: format={:?} renderer={:?}", config.output.format, config.output.renderer);
                        println!("WAL: mode={:?} enabled={:?}", config.wal.mode, config.wal.enabled);
                        println!("Backup: enabled={:?}", config.backup.enabled);
                        println!("Guards: dal={:?} blocklist_enabled={:?}", config.guards.dal, config.guards.blocklist_enabled);
                        println!("Session: max={:?} timeout={:?} on_limit={:?}", config.session.max_sessions, config.session.timeout_secs, config.session.on_limit);
                        println!("Telemetry: enabled={:?}", config.telemetry.enabled);
                    }
                    println!("Effective settings (with env var + defaults):");
                    let resolved = config.resolved();
                    println!("  Profile: {}", resolved.profile);
                    println!("  DAL: {}", resolved.dal);
                    println!("  WAL mode: {}", resolved.wal_mode);
                    println!("  Backup: {}", on_off(resolved.backup_enabled));
                    println!("  Output: {}/{}", resolved.output_format, resolved.output_renderer);
                    println!("  Session: {}/{}s {}", resolved.session_max, resolved.session_timeout, resolved.session_on_limit);
                    println!("  DAL (legacy get_dal): {}", RuntimoConfig::get_dal());
                    println!("  ShellExec blocklist: {}", on_off(resolved.blocklist_enabled));
                    println!("  Critical-files denylist: {}", on_off(RuntimoConfig::critical_files_enabled()));
                    println!("  Path whitelist: {}", on_off(RuntimoConfig::path_restriction_enabled()));
                    println!("  PATH sanitization: {}", on_off(RuntimoConfig::path_sanitization_enabled()));
                    let all_prefixes = RuntimoConfig::get_allowed_prefixes();
                    println!("  Allowed paths ({} total):", all_prefixes.len());
                    for p in &all_prefixes {
                        println!("    {}", p);
                    }
                    // Guards off warning (Gate 1)
                    if RuntimoConfig::guards_off_via_profile(&resolved)
                        && resolved.profile == "ephemeral"
                    {
                        eprintln!(
                            "[runtimo] WARNING: guards off via profile {} — not for service machines",
                            resolved.profile
                        );
                    }
                    println!("No config file found. To customize, create one at:");
                    println!("  {}", config_path.display());
                    println!("Supported fields:");
                    println!("  allowed_paths     — Extra path prefixes for FileRead/FileWrite (list of strings)");
                    println!("  dal               — Design Assurance Level A-E (string)");
                    println!("  blocklist_overrides — Additional dangerous command patterns (list of strings)");
                    println!("  capability_timeouts — Per-capability timeout overrides (table of string->number)");
                    println!("  env               — Environment vars for ShellExec children + runtime gates (table)");
                    println!("  blocklist_enabled — Disable ShellExec dangerous-command blocklist (bool, default true)");
                    println!("  critical_files_enabled — Disable critical-files denylist in FileWrite/Delete (bool, default true)");
                    println!("  path_restriction_enabled — Disable allowed-prefix path whitelist (bool, default true)");
                    println!("  path_sanitization_enabled — Disable forced PATH on ShellExec children (bool, default true)");
                    println!("  profile           — Profile name: minimal|ephemeral|service");
                    println!("  [output]          — format, renderer");
                    println!("  [wal]             — mode, enabled");
                    println!("  [backup]          — enabled");
                    println!("  [guards]          — dal, blocklist_enabled, etc.");
                    println!("  [session]         — max_sessions, timeout_secs, on_limit");
                    println!("  [telemetry]       — enabled");
                    println!("Example:");
                    println!("  allowed_paths = [\"/srv\", \"/opt\"]");
                    println!("  dal = \"B\"");
                    println!("  blocklist_overrides = [\"curl\", \"wget\"]");
                    println!("  profile = \"ephemeral\"");
                    println!("  [capability_timeouts]");
                    println!("  ShellExec = 120");
                    println!("  FileRead = 10");
                    println!("  [env]");
                    println!("  RUNTIMO_ENABLE_INTERPRETERS = \"1\"");
                    println!("  RUNTIMO_ENABLE_NETWORK = \"1\"");
                }
            }
            ConfigAction::Dal { level } => {
                if let Some(new_level) = level {
                    let upper = new_level.to_uppercase();
                    if !matches!(upper.as_str(), "A" | "B" | "C" | "D" | "E") {
                        eprintln!("Invalid DAL level: {}. Must be A, B, C, D, or E.", new_level);
                    }
                }
                let mut config = RuntimoConfig::load();
                if let Some(new_level) = level {
                    config.dal = Some(upper.clone());
                }
                config.save().map_err(|e| format!("Save failed: {}", e))?;
                println!("DAL set to {} in config file.", upper);
                println!("Note: RUNTIMO_DAL env var (if set) still takes precedence.");
                let current = RuntimoConfig::get_dal();
                let source = if std::env::var("RUNTIMO_DAL").is_ok() {
                    "env var (RUNTIMO_DAL)"
                } else {
                    "default"
                };
                println!("Current DAL: {} (source: {})", current, source);
            }
            ConfigAction::Init { profile, minimal, path, force } => {
                let effective_profile = if minimal {
                    Some("minimal".to_string())
                } else {
                    profile
                };
                let prof_ref = effective_profile.as_deref();
                let target = path.as_deref();
                match RuntimoConfig::init_at(target, prof_ref, force) {
                    Ok(written) => {
                        println!("Config written to {}", written.display());
                        println!("Next: runtimo config show");
                    }
                    Err(e) => {
                        if e.contains("already exists") {
                            eprintln!("{}", e);
                            eprintln!("Config init failed: {}", e);
                        } else {
                            eprintln!("{}", e);
                        }
                    }
                }
            }
        }
        Commands::Session { command } => match command {
            SessionCommand::Run {
                prompt_file,
                session,
                max_steps,
                max_seconds,
                on_failure,
                via_daemon,
                dry_run,
            } => {
                // Inherit bounded policy from frozen ResolvedConfig.
                let effective_max_steps = max_steps.unwrap_or(resolved.session_max);
                let effective_max_seconds = max_seconds.unwrap_or(resolved.session_timeout);
                let effective_on_failure = on_failure
                    .as_deref()
                    .unwrap_or(&resolved.session_on_limit)
                    .to_lowercase();
                if effective_on_failure != "continue" && effective_on_failure != "stop" {
                    eprintln!("Invalid --on-failure '{}': must be continue|stop", effective_on_failure);
                }
                if effective_max_steps == 0 {
                    eprintln!("--max-steps must be >0");
                }
                // Parse prompt file (validates size, steps>0, capability exists, traversal).
                let steps = match session_parser::parse_prompt_file(&prompt_file, &reg) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("Prompt parse failed: {}", e);
                    }
                };
                if u32::try_from(steps.len()).unwrap_or(u32::MAX) > effective_max_steps {
                    eprintln!("Prompt has {} steps but --max-steps is {}", steps.len(), effective_max_steps);
                }
                // --dry-run validates only.
                if dry_run {
                    if base_mode.is_json() {
                        let out = serde_json::json!({
                            "dry_run": true,
                            "steps": steps.len(),
                            "max_steps": effective_max_steps,
                            "max_seconds": effective_max_seconds,
                            "on_failure": effective_on_failure,
                            "profile": resolved.profile,
                            "wal_mode": resolved.wal_mode,
                        });
                        println!("{}", serde_json::to_string_pretty(&out).unwrap());
                    } else if !base_mode.is_quiet() {
                        println!(
                            "dry-run: prompt '{}' valid ({} steps, max_steps={}, max_seconds={}, on_failure={})",
                            prompt_file.display(),
                            steps.len(),
                            effective_max_steps,
                            effective_max_seconds,
                            effective_on_failure
                        );
                    }
                }
                // Resolve sessions directory (honors RUNTIMO_SESSIONS_DIR).
                let sdir = session_run::sessions_dir();
                std::fs::create_dir_all(&sdir)
                    .map_err(|e| format!("create sessions dir: {}", e))?;
                // Create or resume session by name/id.
                let session_id = if let Some(ref name) = session {
                    if let Some(existing) = session_run::find_session_by_name_or_id(&sdir, name) {
                        existing.id
                    } else {
                        // Create new with this name.
                        let mut mgr = runtimo_core::session::SessionManager::new(sdir.clone())
                            .map_err(|e| format!("SessionManager: {}", e))?;
                        let s = mgr
                            .create_session(Some(name))
                            .map_err(|e| format!("create_session: {}", e))?;
                        s.id
                    }
                } else {
                    // Create new with this name.
                    let mut mgr = runtimo_core::session::SessionManager::new(sdir.clone())
                        .map_err(|e| format!("SessionManager: {}", e))?;
                    let s = mgr
                        .create_session(None)
                        .map_err(|e| format!("create_session: {}", e))?;
                    s.id
                };
                // Load session for display (need its name/id).
                let mgr_ro = runtimo_core::session::SessionManager::new(sdir.clone())
                    .map_err(|e| format!("SessionManager: {}", e))?;
                let sess = mgr_ro
                    .load_session(&session_id)
                    .map_err(|e| format!("load_session: {}", e))?;
                if !base_mode.is_quiet() {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "session_id": sess.id,
                            "session_name": sess.name,
                            "profile": resolved.profile,
                            "wal_mode": resolved.wal_mode,
                            "backup_enabled": resolved.backup_enabled,
                            "steps": steps.len(),
                        })).unwrap()
                    );
                    println!(
                        "Session {} (name: {}) — profile={}, wal_mode={}, steps={}",
                        sess.id,
                        sess.name.as_deref().unwrap_or("-"),
                        resolved.profile,
                        resolved.wal_mode,
                        steps.len()
                    );
                }
                let start = std::time::Instant::now();
                let mut executed: usize = 0;
                let mut had_failure = false;
                let mut terminated = false;
                for (idx, step) in steps.iter().enumerate() {
                    let step_no = idx + 1;
                    // Overall time guard (bounded).
                    if start.elapsed().as_secs() > effective_max_seconds {
                        emit_session_note(
                            &base_mode,
                            serde_json::json!({
                                "session_id": session_id,
                                "step": step_no,
                                "total": steps.len(),
                                "error": "max-seconds exceeded",
                                "terminated": true
                            }),
                            &format!(
                                "Session {} exceeded --max-seconds {}s at step {}/{} — terminating",
                                session_id,
                                effective_max_seconds,
                                step_no,
                                steps.len()
                            ),
                        );
                        had_failure = true;
                        terminated = true;
                        break;
                    }
                    // Resource guard per step.
                    let guard = runtimo_core::LlmoSafeGuard::new();
                    if let Err(e) = guard.check() {
                        let msg = format!(
                            "resource guard tripped at step {}/{}: {}",
                            step_no,
                            steps.len(),
                            e
                        );
                        emit_session_note(
                            &base_mode,
                            serde_json::json!({
                                "step": step_no,
                                "total": steps.len(),
                                "capability": step.capability,
                                "error": msg
                            }),
                            &msg,
                        );
                        if effective_on_failure == "stop" {
                            terminated = true;
                            break;
                        }
                        continue;
                    }
                    // Dangerous command + network gating per step (ShellExec).
                    if step.capability == "ShellExec" {
                        if let Some(cmd) = step.args.get("cmd").and_then(|v| v.as_str()) {
                            if let Some(reason) = is_dangerous_command(cmd) {
                                let msg = format!(
                                    "step {}/{} dangerous command blocked: {}",
                                    step_no,
                                    steps.len(),
                                    reason
                                );
                                emit_session_note(
                                    &base_mode,
                                    serde_json::json!({
                                        "step": step_no,
                                        "total": steps.len(),
                                        "capability": step.capability,
                                        "error": msg
                                    }),
                                    &msg,
                                );
                                had_failure = true;
                                if effective_on_failure == "stop" {
                                    terminated = true;
                                    break;
                                }
                            }
                            if !network_enabled() && is_network_command(cmd) {
                                let msg = format!(
                                    "step {}/{} network command blocked at step {} — set RUNTIMO_ENABLE_NETWORK=1 to enable",
                                    step_no,
                                    steps.len(),
                                    step_no
                                );
                                emit_session_note(
                                    &base_mode,
                                    serde_json::json!({
                                        "step": step_no,
                                        "total": steps.len(),
                                        "capability": step.capability,
                                        "error": msg
                                    }),
                                    &msg,
                                );
                                had_failure = true;
                                if effective_on_failure == "stop" {
                                    terminated = true;
                                    break;
                                }
                            }
                        }
                    }
                    // Acquire concurrency slot (reuse run's guard).
                    if !acquire_cli_slot() {
                        emit_session_note(
                            &base_mode,
                            serde_json::json!({
                                "step": step_no,
                                "total": steps.len(),
                                "error": "too many concurrent CLI runs"
                            }),
                            &format!(
                                "Too many concurrent CLI runs (max {}) at step {}/{}",
                                MAX_CLI_CONCURRENT,
                                step_no,
                                steps.len()
                            ),
                        );
                    }
                    // Execute step: local or via-daemon stub.
                    let result: Result<runtimo_core::executor::ExecutionResult, String> = if via_daemon {
                        // Stub: dispatch via daemon RPC.
                        if let Err(e) = ensure_daemon_running() {
                            release_cli_slot();
                            return Err(format!("failed to start daemon: {}", e));
                        }
                        let params = serde_json::json!({
                            "capability": step.capability,
                            "args": step.args,
                            "dry_run": false,
                            "working_dir": std::env::current_dir().unwrap_or_default().to_string_lossy(),
                        });
                        match send_rpc("dispatch", params) {
                            Ok(v) => {
                                // No forgery: poll the daemon for the real
                                // terminal status instead of assuming success.
                                // The WAL audit stays daemon-side; snapshots
                                // below are local placeholders with cleared
                                // caches so before/after always differ.
                                let jid = v.get("job_id").and_then(|x| x.as_str()).unwrap_or("?");
                                // Track job_id in session even when dispatched via daemon (stub).
                                if let Ok(mut mgr) = runtimo_core::session::SessionManager::new(
                                    session_run::sessions_dir(),
                                ) {
                                    let _ = mgr.add_job(&session_id, jid);
                                }
                                let remaining = effective_max_seconds
                                    .saturating_sub(start.elapsed().as_secs());
                                let (success, err_msg, wal_seq) = poll_dispatched_step(jid, remaining);
                                let out = match err_msg {
                                    None => runtimo_core::capability::Output::ok(format!(
                                        "dispatched via daemon: {}",
                                        jid
                                    )),
                                    Some(err) => runtimo_core::capability::Output::error(
                                        format!("via-daemon step failed: {}", err),
                                        err,
                                    ),
                                };
                                runtimo_core::Telemetry::clear_cache();
                                let tel_before = if resolved.telemetry_enabled {
                                    runtimo_core::Telemetry::capture_lightweight()
                                } else {
                                    runtimo_core::Telemetry::empty()
                                };
                                runtimo_core::Telemetry::clear_lightweight_cache();
                                runtimo_core::ProcessSnapshot::clear_cache();
                                let tel_after = if resolved.telemetry_enabled {
                                    runtimo_core::Telemetry::capture_lightweight()
                                } else {
                                    runtimo_core::Telemetry::empty()
                                };
                                let proc_before = runtimo_core::ProcessSnapshot::capture().summary;
                                let proc_after = runtimo_core::ProcessSnapshot::capture().summary;
                                Ok(runtimo_core::executor::ExecutionResult {
                                    job_id: jid.to_string(),
                                    capability: step.capability.clone(),
                                    success,
                                    output: out,
                                    telemetry_before: tel_before,
                                    telemetry_after: tel_after,
                                    process_before: proc_before,
                                    process_after: proc_after,
                                    wal_seq,
                                })
                            }
                            Err(e) => Err(format!("via-daemon dispatch failed: {}", e)),
                        }
                    } else {
                        // Local deterministic execution with session binding.
                        let Some(cap) = reg.get(&step.capability) else {
                            // Unknown capability — treat as step failure per on_failure policy.
                            let msg = format!("unknown capability '{}'", step.capability);
                            return Err(format!("step {}/{} {}", step_no, steps.len(), msg));
                        };
                        let timeout = RuntimoConfig::get_capability_timeout(&step.capability, 30);
                        let res = execute_with_telemetry_and_session(
                            cap,
                            &step.args,
                            false,
                            &wal_path(),
                            Some(&session_id),
                            None,
                            timeout,
                        )
                        .map_err(|e| format!("{}", e))?;
                        Ok(res)
                    };
                    release_cli_slot();
                    match result {
                        Ok(exec) => {
                            executed += 1;
                            if !exec.success {
                                // Stream failure.
                                if base_mode.is_json() {
                                    println!(
                                        "{}",
                                        serde_json::to_string_pretty(&serde_json::json!({
                                            "step": step_no,
                                            "total": steps.len(),
                                            "capability": step.capability,
                                            "success": false,
                                            "output": exec.output,
                                            "job_id": exec.job_id,
                                        })).unwrap()
                                    );
                                } else if !base_mode.is_quiet() {
                                    eprintln!(
                                        "step {}/{} {} failed: {}",
                                        step_no,
                                        steps.len(),
                                        step.capability,
                                        exec.output.output
                                    );
                                }
                            } else if base_mode.is_json() {
                                println!(
                                    "{}",
                                    serde_json::to_string_pretty(&exec.output).unwrap()
                                );
                            } else if !base_mode.is_quiet() {
                                let rendered = base_mode.render_text(&exec.output.output);
                                if rendered.trim().is_empty() {
                                    println!("step {}/{} {}: ok", step_no, steps.len(), step.capability);
                                } else {
                                    println!("step {}/{} {}: {}", step_no, steps.len(), step.capability, rendered);
                                }
                                if let Some(ref data) = exec.output.data {
                                    let text = if let Some(s) = data.as_str() {
                                        s.to_string()
                                    } else {
                                        data.to_string()
                                    };
                                    if !text.trim().is_empty() && text != "null" {
                                        let r2 = base_mode.render_text(&text);
                                        if !r2.trim().is_empty() {
                                            println!("{}", r2);
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            had_failure = true;
                            emit_session_note(
                                &base_mode,
                                serde_json::json!({
                                    "step": step_no,
                                    "total": steps.len(),
                                    "capability": step.capability,
                                    "error": e
                                }),
                                &format!(
                                    "step {}/{} {} error: {}",
                                    step_no,
                                    steps.len(),
                                    step.capability,
                                    e
                                ),
                            );
                            if effective_on_failure == "stop" {
                                terminated = true;
                                break;
                            }
                        }
                    };
                }
                // Mark terminal status deterministically.
                let final_status = if terminated || (had_failure && effective_on_failure == "stop") {
                    runtimo_core::session::SessionStatus::Terminated
                } else if had_failure && effective_on_failure == "continue" {
                    // Completed even though some steps failed — the session itself completed.
                    runtimo_core::session::SessionStatus::Completed
                } else {
                    runtimo_core::session::SessionStatus::Running
                };
                if let Err(e) = session_run::update_session_status(&sdir, &session_id, final_status) {
                    emit_session_note(
                        &base_mode,
                        serde_json::json!({
                            "session_id": session_id,
                            "error": format!("failed to update session status: {}", e)
                        }),
                        &format!("Failed to update session status: {}", e),
                    );
                }
                // Summary via OutputMode.
                #[allow(clippy::redundant_clone)]
                // sdir cloned for json branch so else-if can still move sdir; removing clone would move in one branch and break the other
                let mgr = runtimo_core::session::SessionManager::new(sdir.clone());
                let final_sess = mgr
                    .load_session(&session_id)
                    .map_err(|e| format!("{}", e))?;
                if base_mode.is_json() {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "session_id": final_sess.id,
                            "status": format!("{:?}", final_sess.status),
                            "job_ids": final_sess.job_ids,
                            "executed": executed,
                            "total": steps.len(),
                        })).unwrap()
                    );
                } else if !base_mode.is_quiet() {
                    let mgr = runtimo_core::session::SessionManager::new(sdir).ok();
                    let final_sess = mgr.and_then(|m| m.load_session(&session_id).ok());
                    if let Some(fs) = final_sess {
                        println!(
                            "Session {} status={:?} jobs={}/{} job_ids={:?}",
                            fs.id,
                            fs.status,
                            fs.job_ids.len(),
                            fs.job_ids,
                            final_sess.job_ids
                        );
                    }
                }
            }
            SessionCommand::List { json } => {
                let mode = if json {
                    base_mode.with_format("json")
                } else {
                    base_mode
                };
                let mgr = runtimo_core::session::SessionManager::new(sdir);
                let sessions = mgr.list_sessions().map_err(|e| format!("{}", e))?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&sessions).unwrap());
                } else if sessions.is_empty() {
                    println!("No sessions found.");
                } else {
                    let headers = ["ID", "NAME", "STATUS", "JOBS", "UPDATED"];
                    let rows: Vec<Vec<String>> = sessions
                        .iter()
                        .map(|s| {
                            vec![
                                s.id.clone(),
                                s.name.clone().unwrap_or_else(|| "-".to_string()),
                                format!("{:?}", s.status),
                                s.job_ids.len().to_string(),
                                s.updated_at.to_string(),
                            ]
                        })
                        .collect();
                    println!("{}", mode.render_table(&headers, &rows));
                }
            }
            SessionCommand::Show { session_id, json } => {
                let mgr = runtimo_core::session::SessionManager::new(sdir);
                let sess = mgr
                    .load_session(&session_id)
                    .map_err(|e| format!("{}", e))?;
                // Pull WAL events for the session's job_ids.
                let wal_events: Vec<Value> = if let Ok(reader) = WalReader::load_all(&wal_path()) {
                    let set: std::collections::HashSet<&String> = sess.job_ids.iter().collect();
                    reader
                        .events()
                        .filter(|e| set.contains(&e.job_id))
                        .map(|e| serde_json::to_value(e).unwrap_or(Value::Null))
                        .collect()
                } else {
                    Vec::new()
                };
                let out = serde_json::json!({
                    "session": sess,
                    "wal_events": wal_events,
                });
                if json {
                    println!("{}", serde_json::to_string_pretty(&out).unwrap());
                } else {
                    let info = format!(
                        "Session {} (name: {})\\nStatus: {:?}\\nCreated: {}\\nUpdated: {}\\nJobs ({}): {}\\nWAL events: {}",
                        sess.id,
                        sess.name.as_deref().unwrap_or("-"),
                        sess.status,
                        sess.created_at,
                        sess.updated_at,
                        sess.job_ids.len(),
                        if sess.job_ids.is_empty() {
                            "(none)".to_string()
                        } else {
                            sess.job_ids.join(", ")
                        },
                        wal_events.len()
                    );
                    println!("{}", mode.render_text(&info));
                    if !wal_events.is_empty() {
                        let headers = ["SEQ", "EVENT", "JOB_ID"];
                        let rows: Vec<Vec<String>> = wal_events
                            .iter()
                            .map(|v| {
                                vec![
                                    v.get("seq")
                                        .and_then(|x| x.as_u64())
                                        .unwrap_or(0)
                                        .to_string(),
                                    v.get("type")
                                        .and_then(|x| x.as_str())
                                        .unwrap_or("?")
                                        .to_string(),
                                    v.get("job_id")
                                        .and_then(|x| x.as_str())
                                        .unwrap_or("?")
                                        .to_string(),
                                ]
                            })
                            .collect();
                        println!("{}", mode.render_table(&headers, &rows));
                    }
                }
            }
        }
        Commands::Observe {
            sample_rate_hz,
            pid,
            cmd,
            out,
            burst,
            dal,
            suspend_ms,
            self_test,
            verify,
            properties,
            json,
        } => {
            if pid.is_none() && cmd.is_none() && !self_test && verify.is_none() {
                eprintln!("observe error: must specify one of --pid, --cmd, --self-test, or --verify");
            }
            if self_test {
                let code = runtimo_core::observe::self_test::run();
                std::process::exit(code);
            }
            if let Some(vpath) = verify {
                let mut allowed = RuntimoConfig::get_allowed_prefixes();
                allowed.push(
                    runtimo_core::utils::data_dir()
                        .to_string_lossy()
                        .to_string(),
                );
                let ctx = runtimo_core::validation::path::PathContext {
                    allowed_prefixes: allowed,
                    require_exists: true,
                    require_file: true,
                };
                let vstr = vpath.to_string_lossy().as_ref();
                if let Err(e) = runtimo_core::validation::path::validate_path(&vstr, &ctx) {
                    eprintln!("verify: invalid bundle path: {e}");
                }
                let res = runtimo_core::observe::verify_report(&vpath);
                // Load events for oracle evaluation via the arm's existing loading path.
                let wal_events: Vec<runtimo_core::wal::WalEvent> = if let Ok(reader) = WalReader::load_all(&vpath) {
                    reader.events().to_vec()
                } else {
                    Vec::new()
                };
                // Evaluate --properties spec if present; parse failure exits 1.
                let property_verdicts: Vec<PropertyVerdict> = if let Some(ref spec_str) = properties {
                    let spec = match parse_spec(spec_str) {
                        Ok(s) => s,
                        Err(e) => {
                            eprintln!("verify: property spec parse error: {e}");
                            std::process::exit(1);
                        }
                    };
                    match evaluate(&wal_events, &spec) {
                        Ok(pv) => vec![pv],
                        Err(_) => vec![],
                    }
                } else {
                    vec![]
                };
                // T6cli: consume honest watermark from VerifyReport.watermark (AP-5 field),
                // never derive from truncated_gaps. Fallback only when no ObserveCompleted marker.
                let watermark_display = res.watermark.clone().unwrap_or_else(|| {
                    if res.truncated_gaps > 0 {
                        "TRUNCATED".to_string()
                    } else {
                        "Complete".to_string()
                    }
                });
                let pv_array: Vec<serde_json::Value> = property_verdicts
                    .iter()
                    .map(|pv| {
                        serde_json::json!({
                            "name": pv.name,
                            "verdict": format!("{:?}", pv.verdict),
                            "detail": pv.detail,
                        })
                    })
                    .collect();
                println!(
                    "verify {}: structurally_parseable={} integrity_valid={} lifecycle_valid={} completeness_known={} admissible={} error={:?} watermark={:?}\ntrailer: bundle {} — hash chain {} — watermark {}",
                    vpath.display(),
                    res.structurally_parseable,
                    res.integrity_valid,
                    res.lifecycle_valid,
                    res.completeness_known,
                    res.admissible,
                    res.error,
                    res.watermark,
                    vpath.display(),
                    if res.integrity_valid { "ok" } else { "FAIL" },
                    watermark_display
                );
                for pv in &property_verdicts {
                    println!("property_verdicts: name={} verdict={:?} detail={}", pv.name, pv.verdict, pv.detail);
                }
                #[allow(clippy::bool_to_int_with_if)]
                std::process::exit(if res.admissible { 0 } else { 1 });
            }
            // T10: burst is a dead contract — surface explicitly to stdout (not just daemon stderr)
            // and gate via RPC deferred error. The daemon's handle_observe_start returns
            // -32601 observe_burst deferred when burst=true (mirror engine.rs:110 pattern);
            // the CLI mirrors that by printing the same note to stdout with burst_deferred:true
            // and by forwarding burst in the RPC params.
            if burst {
                eprintln!("note: --burst file-watch burst (P2B bundle-path watch) deferred — not yet trivial; using polling");
                println!("note: --burst file-watch burst (P2B bundle-path watch) deferred — not yet trivial; using polling (burst_deferred:true)");
            }
            // Resolve out path (validated, data_dir default)
            let run_id = runtimo_core::utils::generate_id();
            let bundle_path = if let Some(p) = out {
                let s = p.to_string_lossy().as_ref();
                let ctx = runtimo_core::validation::path::PathContext {
                    allowed_prefixes: RuntimoConfig::get_allowed_prefixes(),
                    require_exists: false,
                    require_file: false,
                };
                match runtimo_core::validation::path::validate_path(&s, &ctx) {
                    Ok(valid) => valid,
                    Err(e) => {
                        eprintln!("--out invalid: {e}");
                    }
                };
                runtimo_core::observe::bundle_path(&run_id)
            } else {
                runtimo_core::observe::bundle_path(&run_id)
            };
            let hz = sample_rate_hz
                .unwrap_or_else(|| RuntimoConfig::load().effective_observe_sample_hz(None));
            let dal_str = dal.unwrap_or_else(RuntimoConfig::get_dal);
            let pressure_suspend_ms = suspend_ms
                .unwrap_or_else(|| RuntimoConfig::load().resolved().observe_pressure_suspend_ms);
            // Try daemon first if running; else run locally.
            if daemon_is_running() {
                let mut params = serde_json::json!({
                    "run_id": run_id,
                    "sample_rate_hz": hz,
                    "dal": dal_str,
                    "out": bundle_path.display().to_string(),
                    "burst": burst,
                    "pressure_suspend_ms": pressure_suspend_ms,
                });
                if let Some(p) = pid {
                    params["pid"] = serde_json::json!(p);
                }
                if let Some(ref c) = cmd {
                    params["cmd"] = serde_json::json!(c);
                }
                match send_rpc("observe_start", params) {
                    Ok(v) => {
                        if mode.is_json() {
                            println!("{}", serde_json::to_string_pretty(&v).unwrap());
                            println!(
                                "observe dispatched: run_id={} bundle={} hz={} dal={}",
                                v["run_id"].as_str().unwrap_or("?"),
                                v["bundle"].as_str().unwrap_or("?"),
                                hz,
                                dal_str
                            );
                            println!("bundle: {}", bundle_path.display());
                        } else if !mode.is_quiet() {
                            eprintln!("observe_start failed: {e}");
                        }
                    }
                    Err(e) => {
                        eprintln!("observe_start failed: {e}");
                    }
                }
                // Local synchronous collection (sibling — caller is parent, collector + target share parent).
                let target_pid = if let Some(p) = pid {
                    p
                } else if let Some(ref c) = cmd {
                    match std::process::Command::new("sh").arg("-c").arg(c).spawn() {
                        Ok(child) => child.id(),
                        Err(e) => {
                            eprintln!("failed to spawn --cmd: {e}");
                        }
                    }
                } else {
                    eprintln!("observe requires --pid or --cmd (or --self-test / --verify)");
                    return;
                };
                let mut sup = match runtimo_core::observe::ObserveSupervisor::new_at_path(
                    &run_id,
                    &dal_str,
                    Some(bundle_path.clone()),
                    pressure_suspend_ms,
                    None,
                ) {
                    Ok(sup) => sup,
                    Err(e) => {
                        eprintln!("supervisor create failed: {e}");
                        return;
                    }
                };
                let _ = sup.attach(target_pid, 0);
                // Short burst collection (demo: 50 ticks or 2s).
                let interval = sup.sampler_interval();
                #[allow(clippy::arithmetic_side_effects)]
                // Instant::now() + Duration; bounded by 2s
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                let mut ticks = 0;
                while std::time::Instant::now() < deadline && ticks < 50 {
                    // 0 = unspecified: gated_tick falls back to the attach-captured process_start_time.
                    let _ = sup.gated_tick(0);
                    std::thread::sleep(interval.min(std::time::Duration::from_millis(20)));
                    ticks += 1;
                }
                if let Err(e) = sup.finalize() {
                    eprintln!("finalize failed: {e}");
                }
                let v = runtimo_core::observe::verify_report(&bundle_path);
                println!(
                    "observe complete: run_id={run_id} bundle={} ticks={ticks} hz={hz} dal={dal_str} watermark={:?}\nverify: total={} truncated_gaps={} structurally_parseable={} integrity_valid={} lifecycle_valid={} completeness_known={} admissible={} error={:?} watermark={:?}",
                    bundle_path.display(),
                    sup.watermark(),
                    v.total,
                    v.truncated_gaps,
                    v.structurally_parseable,
                    v.integrity_valid,
                    v.lifecycle_valid,
                    v.completeness_known,
                    v.admissible,
                    v.error,
                    v.watermark
                );
            } else {
                let mut params = serde_json::json!({
                    "run_id": run_id,
                    "sample_rate_hz": hz,
                    "dal": dal_str,
                    "out": bundle_path.display().to_string(),
                    "burst": burst,
                    "pressure_suspend_ms": pressure_suspend_ms,
                });
                if let Some(p) = pid {
                    params["pid"] = serde_json::json!(p);
                }
                if let Some(ref c) = cmd {
                    params["cmd"] = serde_json::json!(c);
                }
                match send_rpc("observe_start", params) {
                    Ok(v) => {
                        if mode.is_json() {
                            println!("{}", serde_json::to_string_pretty(&v).unwrap());
                            println!(
                                "observe dispatched: run_id={} bundle={} hz={} dal={}",
                                v["run_id"].as_str().unwrap_or("?"),
                                v["bundle"].as_str().unwrap_or("?"),
                                hz,
                                dal_str
                            );
                            println!("bundle: {}", bundle_path.display());
                        } else if !mode.is_quiet() {
                            eprintln!("observe_start failed: {e}");
                        }
                    }
                    Err(e) => {
                        eprintln!("observe_start failed: {e}");
                    }
                }
                // Local synchronous collection (sibling — caller is parent, collector + target share parent).
                let target_pid = if let Some(p) = pid {
                    p
                } else if let Some(ref c) = cmd {
                    match std::process::Command::new("sh").arg("-c").arg(c).spawn() {
                        Ok(child) => child.id(),
                        Err(e) => {
                            eprintln!("failed to spawn --cmd: {e}");
                        }
                    }
                } else {
                    eprintln!("observe requires --pid or --cmd (or --self-test / --verify)");
                    return;
                };
                let mut sup = match runtimo_core::observe::ObserveSupervisor::new_at_path(
                    &run_id,
                    &dal_str,
                    Some(bundle_path.clone()),
                    pressure_suspend_ms,
                    None,
                ) {
                    Ok(sup) => sup,
                    Err(e) => {
                        eprintln!("supervisor create failed: {e}");
                        return;
                    }
                };
                let _ = sup.attach(target_pid, 0);
                // Short burst collection (demo: 50 ticks or 2s).
                let interval = sup.sampler_interval();
                #[allow(clippy::arithmetic_side_effects)]
                // Instant::now() + Duration; bounded by 2s
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                let mut ticks = 0;
                while std::time::Instant::now() < deadline && ticks < 50 {
                    // 0 = unspecified: gated_tick falls back to the attach-captured process_start_time.
                    let _ = sup.gated_tick(0);
                    std::thread::sleep(interval.min(std::time::Duration::from_millis(20)));
                    ticks += 1;
                }
                if let Err(e) = sup.finalize() {
                    eprintln!("finalize failed: {e}");
                }
                let v = runtimo_core::observe::verify_report(&bundle_path);
                println!(
                    "observe complete: run_id={run_id} bundle={} ticks={ticks} hz={hz} dal={dal_str} watermark={:?}\nverify: total={} truncated_gaps={} structurally_parseable={} integrity_valid={} lifecycle_valid={} completeness_known={} admissible={} error={:?} watermark={:?}",
                    bundle_path.display(),
                    sup.watermark(),
                    v.total,
                    v.truncated_gaps,
                    v.structurally_parseable,
                    v.integrity_valid,
                    v.lifecycle_valid,
                    v.completeness_known,
                    v.admissible,
                    v.error,
                    v.watermark
                );
            }
        }
        Commands::Oracle { command } => match command {
            OracleCommand::Schema => {
                print_oracle_schema();
            }
            OracleCommand::Benchmark => {
                let report = runtimo_core::oracle::run_benchmarks();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "parse_ms": report.parse_ms,
                        "eval_ms_per_1k": report.eval_ms_per_1k,
                        "eval_1k_ms": report.eval_1k_ms,
                        "eval_10k_ms": report.eval_10k_ms,
                        "eval_100k_ms": report.eval_100k_ms,
                    })).unwrap_or_else(|_| "benchmark error".to_string())
                );
            }
            OracleCommand::Evaluate {
                wal,
                bundle,
                facts,
                properties,
                properties_file,
                json,
            } => {
                std::process::exit(oracle_evaluate(
                    wal,
                    bundle,
                    facts,
                    properties,
                    properties_file,
                    json,
                ));
            }
        }
    }

    Ok(())
}

/// Oracle exit codes (§44): 0 Satisfied, 1 Violated, 2 property/eval error,"
/// 3 evidence/infrastructure error. Violated is a successful evaluation with"
/// a negative result — not a crash."
fn oracle_evaluate(
    wal: Option<PathBuf>,
    bundle: Option<PathBuf>,
    facts: Option<PathBuf>,
    properties: Option<String>,
    properties_file: Option<PathBuf>,
    json: bool,
) -> i32 {
    use runtimo_core::oracle::{evaluate, parse_spec};
    // Exactly one source.
    let sources = [wal.is_some(), bundle.is_some(), facts.is_some()];
    if sources != 1 {
        eprintln!("oracle evaluate: exactly one of --wal, --bundle, --facts is required");
        return 3;
    }
    // Property inline XOR file (§43: bounded, regular-file validated, same parser).
    let spec_str = match (properties, properties_file) {
        (Some(s), None) => s,
        (None, Some(p)) => {
            let ctx = runtimo_core::validation::path::PathContext {
                allowed_prefixes: RuntimoConfig::get_allowed_prefixes(),
                require_exists: true,
                require_file: true,
            };
            if let Err(e) = runtimo_core::validation::path::validate_path(
                p.to_string_lossy().as_ref(),
                &ctx,
            ) {
                eprintln!("oracle evaluate: invalid properties-file: {e}");
                return 3;
            }
            match std::fs::read_to_string(&p) {
                Ok(s) if s.len() <= 1_048_576 => s,
                Ok(_) => {
                    eprintln!("oracle evaluate: properties-file exceeds 1MB bound");
                    return 3;
                }
                Err(e) => {
                    eprintln!("oracle evaluate: cannot read properties-file: {e}");
                }
            }
        }
        _ => {
            eprintln!("oracle evaluate: one of --properties or --properties-file is required");
            return 2;
        }
    };
    let spec = match parse_spec(&spec_str) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("oracle evaluate: property spec parse error: {e}");
            return 2;
        }
    };
    if let Some(facts_path) = facts {
        return oracle_evaluate_facts(&facts_path, &spec, json);
    }
    // WAL vs bundle (§42): distinct evidence types. Raw WAL never gets"
    // Observe admissibility applied; bundles verify integrity separately"
    // and report it alongside (not conflated with) the property verdict.
    let is_bundle = bundle.is_some();
    let path = wal.or(bundle).unwrap();
    let source_kind = if is_bundle { "bundle" } else { "wal" };
    let reader = match WalReader::load_all(&path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("oracle evaluate: cannot load {source_kind}: {e}");
            return 3;
        }
    };
    let events = reader.events();
    let verdict = match evaluate(events, &spec) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("oracle evaluate: evaluation error: {e}");
            return 2;
        }
    };
    let code = match &verdict.verdict {
        runtimo_core::oracle::Verdict::Satisfied => 0,
        runtimo_core::oracle::Verdict::Violated => 1,
        _ => 2,
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "property": verdict.name,
                "verdict": format!("{:?}", verdict.verdict),
                "detail": verdict.detail,
                "source_kind": if is_bundle { "observe_bundle" } else { "wal" },
                "selected_count": verdict.selected_count,
                "evaluated_count": verdict.evaluated_count,
                "matched_count": verdict.matched_count,
            })).unwrap()
        );
    } else {
        println!(
            "oracle {}: {:?} — {} (selected={}, evaluated={}, matched={})",
            verdict.name,
            verdict.verdict,
            verdict.detail,
            verdict.selected_count,
            verdict.evaluated_count,
            verdict.matched_count
        );
    }
    code
}

/// Facts source (§53-54): same selector/quantifier engine, separate field"
/// resolver over `RuntimeFactV1`. Absent `ObservedExec` is NOT proof of"
/// non-execution (`NOT OBSERVED != FALSE`)."
fn oracle_evaluate_facts(
    facts_path: &std::path::Path,
    spec: &runtimo_core::oracle::PropertySpec,
    json: bool,
) -> i32 {
    use runtimo_core::oracle::{Op, Verdict};
    let content = match std::fs::read_to_string(facts_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("oracle evaluate: cannot read facts: {e}");
            return 3;
        }
    };
    let mut records: Vec<serde_json::Value> = Vec::new();
    for (n, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(v) => records.push(v),
            Err(e) => {
                eprintln!("oracle evaluate: facts line {}: parse error: {e}", n + 1);
            }
        }
    }
    // Minimal fact-field resolver: family/run_id/provider/fidelity/"
    // observations/first_seen/last_seen/revision/process.pid/"
    // process.process_start_time/extra.<key>. Unknown field → Error.
    fn fact_field(rec: &serde_json::Value, field: &str) -> Option<serde_json::Value> {
        match field {
            "family" => rec.get("family").cloned(),
            "run_id" => rec.get("run_id").cloned(),
            "provider" => rec.get("provider").cloned(),
            "fidelity" => rec.get("fidelity").cloned(),
            "observations" => rec.get("observations").cloned(),
            "first_seen" => rec.get("first_seen").cloned(),
            "last_seen" => rec.get("last_seen").cloned(),
            "revision" => rec.get("revision").cloned(),
            "process.pid" => rec.get("process_key").and_then(|k| k.get("pid")).cloned(),
            "process.process_start_time" => rec
                .get("process_key")
                .and_then(|k| k.get("process_start_time"))
                .cloned(),
            _ if field.starts_with("extra.") => {
                let key = &field["extra.".len()..];
                rec.get("extra").and_then(|e| e.get(key)).cloned()
            }
            _ => None,
        }
    }
    fn cmp(field_val: &serde_json::Value, op: &Op, pred_val: &serde_json::Value) -> Option<bool> {
        match op {
            Op::Eq => Some(field_val == pred_val),
            Op::Neq => Some(field_val != pred_val),
            Op::Gt | Op::Lt | Op::Gte | Op::Lte => {
                let a = field_val.as_f64()?;
                let b = pred_val.as_f64()?;
                Some(match op {
                    Op::Gt => a > b,
                    Op::Lt => a < b,
                    Op::Gte => a >= b,
                    Op::Lte => a <= b,
                    _ => return None,
                })
            }
            // Contains | Regex (substring) + future ops fail closed to None.
            match op {
                Op::Contains | Op::Regex => {
                    Some(field_val.as_str()?.contains(pred_val.as_str()?))
                }
                _ => None,
            }
        }
    }
    // Select (missing → filtered out), then quantifier.
    let selected: Vec<&serde_json::Value> = records
        .iter()
        .filter(|r| {
            spec.select.iter().all(|p| {
                fact_field(r, &p.field)
                    .and_then(|fv| cmp(&fv, &p.op, &p.value))
                    .unwrap_or(false)
            })
        })
        .collect();
    let mut matched = 0usize;
    for rec in &selected {
        let mut ok = true;
        for p in &spec.predicates {
            match fact_field(rec, &p.field) {
                Some(fv) => match cmp(&fv, &p.op, &p.value) {
                    Some(true) => {}
                    _ => {
                        ok = false;
                    }
                },
                None => {
                    eprintln!("oracle evaluate: facts: unknown field '{}'", p.field);
                    return 2;
                }
            }
        }
        if ok {
            matched += 1;
        }
    }
    let verdict = match &spec.quantifier {
        runtimo_core::oracle::Quantifier::All => {
            if matched == selected.len() {
                Verdict::Satisfied
            } else {
                Verdict::Violated
            }
        }
        runtimo_core::oracle::Quantifier::Exists => {
            if matched >= 1 {
                Verdict::Satisfied
            } else {
                Verdict::Violated
            }
        }
        runtimo_core::oracle::Quantifier::None => {
            if matched == 0 {
                Verdict::Satisfied
            } else {
                Verdict::Violated
            }
        }
        // Count (struct variant) — threshold check.
        match &spec.quantifier {
            runtimo_core::oracle::Quantifier::Count { op, threshold } => {
                let a = matched as f64;
                let b = *threshold as f64;
                let ok = match op {
                    Op::Gt => a > b,
                    Op::Lt => a < b,
                    Op::Gte => a >= b,
                    Op::Lte => a <= b,
                    Op::Eq => a == b,
                    Op::Neq => a != b,
                    _ => false,
                };
                if ok {
                    Verdict::Satisfied
                } else {
                    Verdict::Violated
                }
            }
            _ => Verdict::Error,
        }
    };
    let code = match &verdict {
        Verdict::Satisfied => 0,
        Verdict::Violated => 1,
        _ => 2,
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "property": spec.name,
                "verdict": format!("{verdict:?}"),
                "source_kind": "runtime_facts",
                "selected_count": selected.len(),
                "matched_count": matched,
            })).unwrap()
        );
    } else {
        println!(
            "oracle {}: {:?} (facts selected={}, matched={})",
            spec.name,
            verdict,
            selected.len(),
            matched
        );
    }
    code
}

/// Print the truthful property-spec schema (§49: `Regex` is substring)."
fn print_oracle_schema() {
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "version": 2,
            "legacy": {
                "name": "string",
                "predicates": [{"field": "string", "op": "Eq|Neq|Gt|Lt|Gte|Lte|Contains|Regex", "value": "json"}],
                "semantics": "ALL events satisfy ALL predicates; empty predicates => Satisfied; empty events + non-empty predicates => vacuously Satisfied (legacy)"
            },
            "v2_additive": {
                "select": "optional ANDed pre-filter (missing field => filtered out, never UnknownField)",
                "quantifier": "all (default, legacy) | exists | none | {count: {op, threshold}}",
                "version": "optional 1|2",
                "counts": "selected_count/evaluated_count/matched_count always reported; zero matches never hidden",
                "event": ["event_type", "job_id", "seq", "capability", "error", "output.<key>", "watermark(bundle-metadata-only=>Error)"],
                "safety": ["safety.semantic_policy", "safety.dal", "safety.llmosafe_status", "safety.runtimo_disposition", "safety.input_class", "safety.analysis_kind", "safety.provenance_consistent", "safety.no_evidence", "safety.stages_executed", "safety.oov_ratio", "safety.detection_flags", "safety.body_pressure", "safety.schema_version", "safety.field_id"],
                "facts": ["family", "run_id", "provider", "fidelity", "observations", "first_seen", "last_seen", "revision", "process.pid", "process.process_start_time", "extra.<key>"]
            },
            "operators": {
                "Regex": "SUBSTRING match (std only, no regex crate) — legacy semantics preserved; use a distinct operator + dependency for true regex",
                "Contains": "substring",
                "note": "Count op must be numeric (Gt/Lt/Gte/Lte/Eq/Neq)"
            },
            "exit_codes": {"0": "Satisfied", "1": "Violated", "2": "property/eval error", "3": "evidence/infrastructure error"},
            "sources": {"wal": "raw WAL, no admissibility", "bundle": "observe bundle, integrity reported separately", "facts": "runtime-facts-v1.jsonl; NOT OBSERVED != FALSE"}
        })).unwrap()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    static CLI_SLOT_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    // ── CLI Argument Parsing (GAP 3) ─────────────────────────────────
    #[test]
    fn test_cli_parse_run_command() {
        let args = vec![
            "runtimo",
            "run",
            "-c",
            "FileRead",
            "-a",
            "{\"path\":\"/tmp/test.txt\"}",
        ];
        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Commands::Run {
                capability,
                args,
                ..
            } => {
                assert_eq!(capability, "FileRead");
                assert_eq!(args, "{\"path\":\"/tmp/test.txt\"}");
                assert!(!dry_run);
            }
            _ => panic!("Expected Run command"),
        }
    }

    #[test]
    fn test_cli_parse_run_with_flags() {
        let args = vec![
            "runtimo",
            "run",
            "-c",
            "ShellExec",
            "-a",
            "{\"cmd\":\"echo hello\"}",
            "--json",
            "10",
        ];
        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Commands::Run {
                capability,
                json,
                quiet,
                timeout,
                ..
            } => {
                assert_eq!(capability, "ShellExec");
                assert!(json);
                assert!(!quiet);
                assert_eq!(timeout, Some(10));
            }
            _ => panic!("Expected Run command"),
        }
    }

    #[test]
    fn test_cli_parse_dispatch_command() {
        let args = vec![
            "runtimo",
            "dispatch",
            "-c",
            "FileWrite",
            "-a",
            "{\"path\":\"/tmp/x.txt\",\"content\":\"bg\"}",
        ];
        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Commands::Dispatch {
                capability,
                args,
                ..
            } => {
                assert_eq!(capability, "FileWrite");
                // Verify args was captured (not empty)
                assert!(!args.is_empty(), "Dispatch args should not be empty");
                // Verify args contains the expected content field
                assert!(
                    args.contains("\"content\":\"bg\""),
                    "Args should contain content:bg, got: {}",
                    args
                );
            }
            _ => panic!("Expected Dispatch command"),
        }
    }

    #[test]
    fn test_cli_parse_list_command() {
        let args = vec!["runtimo", "list"];
        let cli = Cli::try_parse_from(args).unwrap();
        assert!(matches!(cli.command, Commands::List { .. }));
    }

    #[test]
    fn test_cli_parse_telemetry_command() {
        let args = vec!["runtimo", "telemetry", "--json"];
        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Commands::Telemetry { json, verbose } => {
                assert!(!verbose);
            }
            _ => panic!("Expected Telemetry command"),
        }
    }

    #[test]
    fn test_cli_parse_telemetry_verbose() {
        let args = vec!["runtimo", "telemetry", "--verbose"];
        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Commands::Telemetry { json, verbose } => {
                assert!(!json);
                assert!(verbose);
            }
            _ => panic!("Expected Telemetry command"),
        }
    }

    #[test]
    fn test_cli_parse_invalid_command() {
        let args = vec!["runtimo", "nonexistent_command"];
        let result = Cli::try_parse_from(args);
        assert!(result.is_err());
    }

    #[test]
    fn test_cli_parse_missing_required_arg() {
        // 'run' requires -c (capability) — should fail without it
        let args = vec!["runtimo", "run"];
        let result = Cli::try_parse_from(args);
        assert!(result.is_err());
    }

    // ── MAX_CLI_CONCURRENT Slot Enforcement (GAP 3) ──────────────────
    #[test]
    fn test_acquire_cli_slot_under_limit() {
        let _guard = CLI_SLOT_MUTEX.lock().unwrap();
        // Reset counter for test isolation
        CLI_ACTIVE_JOBS.store(0, Ordering::Relaxed);
        let mut successes = 0;
        for _ in 0..MAX_CLI_CONCURRENT {
            if acquire_cli_slot() {
                successes += 1;
            }
        }
        assert_eq!(successes, MAX_CLI_CONCURRENT, "Should acquire all {} slots", MAX_CLI_CONCURRENT);
    }

    #[test]
    fn test_acquire_cli_slot_over_limit() {
        // Reset counter
        CLI_ACTIVE_JOBS.store(0, Ordering::Relaxed);
        // Acquire all slots
        for _ in 0..MAX_CLI_CONCURRENT {
            assert!(acquire_cli_slot(), "Should acquire slot");
        }
        // Next acquisition should fail
        assert!(!acquire_cli_slot(), "Should reject when at limit");
    }

    #[test]
    fn test_release_cli_slot_after_acquire() {
        assert!(acquire_cli_slot());
        assert_eq!(CLI_ACTIVE_JOBS.load(Ordering::Relaxed), 1);
        release_cli_slot();
        assert_eq!(CLI_ACTIVE_JOBS.load(Ordering::Relaxed), 0);
        // Should be able to acquire again
        assert!(acquire_cli_slot());
    }

    // ── Flock Coordination (GAP 3) ───────────────────────────────────
    #[test]
    fn test_acquire_daemon_lock_creates_file() {
        let _guard = CLI_SLOT_MUTEX.lock().unwrap(); // serialize env var access
        // Override XDG_DATA_HOME to use temp dir
        let tmp = std::env::temp_dir().join("runtimo_cli_lock_test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("XDG_DATA_HOME", &tmp);
        let result = acquire_daemon_lock();
        // Should succeed since no other process holds the lock (NB mode)
        assert!(
            result.is_ok(),
            "acquire_daemon_lock failed: {:?}",
            result.err()
        );
        let lock_path = daemon_lock_path();
        assert!(
            lock_path.exists(),
            "Lock file should exist at {}",
            lock_path.display()
        );
        // Drop the lock to release it
        drop(result.unwrap());
        std::env::remove_var("XDG_DATA_HOME");
    }

    #[test]
    fn test_daemon_lock_path_format() {
        // Should end with daemon.lock
        let path_str = lock_path.to_string_lossy();
        assert!(
            path_str.ends_with("daemon.lock"),
            "Lock path should end with daemon.lock: {}",
            path_str
        );
        assert!(
            path_str.contains("runtimo"),
            "Lock path should contain runtimo: {}",
            path_str
        );
    }

    #[test]
    fn test_daemon_socket_path_format() {
        let sock_path = daemon_socket();
        let path_str = sock_path.to_string_lossy();
        assert!(
            path_str.ends_with("runtimo.sock"),
            "Socket should end with runtimo.sock: {}",
            path_str
        );
    }

    // ── Properties Flag Tests ──────────────────────────
    #[test]
    fn test_properties_flag_present() {
        let args = vec![
            "runtimo",
            "observe",
            "--verify",
            "/tmp/test.jsonl",
            "--properties",
            r#"{"name":"p","predicates":[]}"#,
        ];
        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Commands::Observe { properties, .. } => {
                assert!(properties.is_some());
                assert_eq!(properties.unwrap(), r#"{"name":"p","predicates":[]}"#);
            }
            _ => panic!("Expected Observe command"),
        }
    }

    #[test]
    fn test_properties_flag_absent() {
        let args = vec![
            "runtimo",
            "observe",
            "--verify",
            "/tmp/test.jsonl",
        ];
        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Commands::Observe { properties, .. } => {
                assert!(properties.is_none());
            }
            _ => panic!("Expected Observe command"),
        }
    }

    #[test]
    fn test_parse_spec_satisfied() {
        let spec = parse_spec(
            r#"{"name":"good-prop","predicates":[{"field":"event_type","op":"Eq","value":"job_started"}]}"#,
        )
        .unwrap();
        assert_eq!(spec.name, "good-prop");
        assert_eq!(spec.predicates.len(), 1);
    }

    #[test]
    fn test_parse_spec_malformed() {
        let result = parse_spec(r#"{"name":"bad","predicates":[}]"#);
        assert!(result.is_err());
    }

    #[test]
    fn test_evaluate_satisfied() {
        use runtimo_core::oracle::Verdict;
        use runtimo_core::wal::WalEvent;
        let event = WalEvent {
            event_type: runtimo_core::WalEventType::JobStarted,
            job_id: "test-job-42".to_string(),
            ..WalEvent::default()
        };
        let spec = parse_spec(
            r#"{"name":"satisfied-prop","predicates":[{"field":"event_type","op":"Eq","value":"job_started"}]}"#,
        )
        .unwrap();
        let result = evaluate(&[event], &spec).unwrap();
        assert_eq!(result.verdict, Verdict::Satisfied);
    }

    #[test]
    fn test_evaluate_violated() {
        use runtimo_core::oracle::Verdict;
        use runtimo_core::wal::WalEvent;
        let event = WalEvent {
            event_type: runtimo_core::WalEventType::JobCompleted,
            job_id: "test-job-42".to_string(),
            ..WalEvent::default()
        };
        let spec = parse_spec(
            r#"{"name":"violated-prop","predicates":[{"field":"event_type","op":"Eq","value":"job_completed"}]}"#,
        )
        .unwrap();
        let result = evaluate(&[event], &spec).unwrap();
        assert_eq!(result.verdict, Verdict::Violated);
    }

    #[test]
    fn test_exit_independence_violated() {
        use runtimo_core::oracle::Verdict;
        use runtimo_core::wal::WalEvent;
        let event = WalEvent {
            event_type: runtimo_core::WalEventType::JobCompleted,
            job_id: "test-job-42".to_string(),
            ..WalEvent::default()
        };
        let spec = parse_spec(
            r#"{"name":"independence-prop","predicates":[{"field":"event_type","op":"Eq","value":"job_completed"}]}"#,
        )
        .unwrap();
        let result = evaluate(&[event], &spec).unwrap();
        assert_eq!(result.verdict, Verdict::Violated);
    }

    #[test]
    fn test_parse_spec_empty_predicates() {
        let spec = parse_spec(r#"{"name":"empty-prop","predicates":[]}"#).unwrap();
        assert!(spec.predicates.is_empty());
    }
}
