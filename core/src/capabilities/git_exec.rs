//! GitExec capability — git operations with state tracking and undo support.
//!
//! Provides git operations (clone, pull, commit, revert, clean, status) with:
//! - State tracking (commit sha, branch, remote URL)
//! - Backup-before-mutate for undo support
//! - WAL logging for audit trail
//! - Path traversal protection
//! - Timeout enforcement on all git subprocesses
//! - URL validation (HTTPS/SSH only, SSRF blocking)
//! - Credential sanitization from output and stderr
//! - Secret file detection for git add
//! - Telemetry and process tracking before/after execution
//!
//! # Network capability
//!
//! **Git operations ARE inherently network-capable.** `git clone`, `git pull`,
//! and `git fetch` make outbound connections to remote repositories.
//! This is by design — denying network access would make GitExec useless.
//!
//! The network isolation is at the transport/protocol level:
//! - Only HTTPS (`https://`) and SSH (`git@`) URLs are accepted
//! - SSRF targets (metadata services, localhost, private ranges) are blocked
//! - Credentials are sanitized from all output, stderr, and telemetry
//!
//! **Note on ShellExec interaction:** GitExec spawns `git` subprocesses which
//! internally invoke `git-remote-https` (a git helper, NOT the system `curl`).
//! The ShellExec network blocklist (`curl`, `wget`, etc.) does NOT affect
//! GitExec — git uses its own transport layer. However, `RUNTIMO_ENABLE_NETWORK`
//! does NOT gate GitExec; GitExec's network access is controlled by its own
//! URL validation and SSRF blocking.
//!
//! # Example
//!
//! ```rust,ignore
//! use runtimo_core::capabilities::GitExec;
//! use runtimo_core::capability::{Capability, Context};
//! use serde_json::json;
//! use std::path::PathBuf;
//!
//! let cap = GitExec::new(PathBuf::from("/tmp/backups"));
//! let result = cap.execute(
//!     &json!({"operation": "clone", "url": "https://github.com/user/repo.git", "path": "/tmp/repo"}),
//!     &Context { dry_run: false, job_id: "job1".into(), working_dir: PathBuf::from("/tmp") }
//! ).unwrap();
//!
//! assert!(result.status == "ok");
//! ```

use crate::backup::BackupManager;
use crate::capability::{CapabilityError, Context, Output, TypedCapability};
use crate::config::RuntimoConfig;
use crate::processes::ProcessSnapshot;
use crate::telemetry::Telemetry;
use crate::validation::path::{validate_path, PathContext};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Arguments for the [`GitExec`] capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)] // args struct — fields are the contract
pub struct GitExecArgs {
    /// Git operation to perform (clone, pull, commit, revert, clean, status).
    pub operation: String,
    /// Repository URL (for clone/pull).
    pub url: Option<String>,
    /// Local path to repository (for clone/commit/revert/clean/status).
    pub path: Option<String>,
    /// Branch name (for checkout/clone).
    pub branch: Option<String>,
    /// Commit message (for commit).
    pub message: Option<String>,
    /// Files to commit (for commit).
    pub files: Option<Vec<String>>,
    /// Commit SHA to revert to (for revert).
    pub commit_sha: Option<String>,
    /// Timeout in seconds (default: 300).
    /// Accepts `timeout` as an alias — `{"operation": "...", "timeout": 60}`.
    #[serde(alias = "timeout")]
    pub timeout_secs: Option<u64>,
}

/// Resolves the effective git timeout: explicit arg → config
/// `capability_timeouts["GitExec"]` → 300s built-in default.
#[must_use]
fn resolve_timeout(args_timeout: Option<u64>) -> u64 {
    args_timeout.unwrap_or_else(|| RuntimoConfig::get_capability_timeout("GitExec", 300))
}

/// Git state before/after operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitState {
    /// Current commit SHA (HEAD).
    pub commit_sha: Option<String>,
    /// Current branch name.
    pub branch: Option<String>,
    /// Remote URL (origin).
    pub remote_url: Option<String>,
    /// Repository path.
    pub repo_path: String,
    /// Working directory status (clean/dirty).
    pub is_clean: bool,
}

/// Known secret file patterns to exclude from `git add -A`.
const SECRET_PATTERNS: &[&str] = &[
    ".env",
    ".env.local",
    ".env.production",
    ".env.staging",
    "credentials.json",
    "credentials.yml",
    "credentials.yaml",
    "secrets.json",
    "secrets.yml",
    "secrets.yaml",
    ".ssh/id_rsa",
    ".ssh/id_ed25519",
    ".ssh/id_dsa",
    "id_rsa",
    "id_ed25519",
    "id_dsa",
    ".npmrc",
    ".pypirc",
    ".docker/config.json",
    "token",
    "api_key",
    "api_secret",
    ".aws/credentials",
    ".azure/credentials",
    "keystore.jks",
    "keystore.p12",
];

/// Extra line-content patterns redacted from git stderr by [`redact_stderr`].
///
/// Distinct from [`SECRET_PATTERNS`] (secret file names excluded from
/// `git add -A`) — these match content inside stderr lines.
const EXTRA_PATTERNS: &[&str] = &["password", "token", "secret", "key=", "auth="];

/// Maximum length (bytes) of the redacted git-stderr tail carried in error
/// strings. Longer output is cut at a character boundary and suffixed with
/// `"... (truncated)"`.
const MAX_STDERR_LEN: usize = 512;

/// Maximum number of untracked files allowed for `git clean -fd`.
const MAX_CLEAN_FILES: usize = 1000;

/// Capability that executes git operations with full state tracking.
///
/// Supports clone, pull, commit, revert, clean, and status operations.
/// Creates backups before mutable operations for undo support.
pub struct GitExec {
    backup_mgr: BackupManager,
}

impl GitExec {
    /// Creates a new GitExec capability with the given backup directory.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::BackupError`] if the backup
    /// directory cannot be created.
    pub fn new(backup_dir: PathBuf) -> Result<Self> {
        Ok(Self {
            backup_mgr: BackupManager::new(backup_dir)?,
        })
    }

    /// Runs a git command with timeout enforcement and returns the output.
    fn run_git_with_timeout(repo_path: &Path, args: &[&str], timeout_secs: u64) -> Result<String> {
        let mut child = Command::new("git")
            .current_dir(repo_path)
            .args(args)
            .stdin(std::process::Stdio::null())
            .spawn()
            .map_err(|e| Error::ExecutionFailed(format!("git command failed: {}", e)))?;

        let timeout = Duration::from_secs(timeout_secs);
        let start = Instant::now();

        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let output = child
                        .wait_with_output()
                        .map_err(|e| Error::ExecutionFailed(format!("git wait failed: {}", e)))?;
                    if !status.success() {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        // Sanitize stderr for safe JSON embedding: escape control chars
                        let sanitized_stderr = stderr
                            .chars()
                            .filter(|c| !c.is_control() || *c == '\n' || *c == '\r' || *c == '\t')
                            .collect::<String>();
                        return Err(Error::ExecutionFailed(format!(
                            "git {}: {}",
                            args.join(" "),
                            sanitized_stderr.trim()
                        )));
                    }
                    return Ok(String::from_utf8_lossy(&output.stdout).to_string());
                }
                Ok(None) => {
                    if start.elapsed() > timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(Error::ExecutionFailed(format!(
                            "git {} timed out after {}s",
                            args.join(" "),
                            timeout_secs
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Error::ExecutionFailed(format!("git wait error: {}", e)));
                }
            }
        }
    }

    /// Checks if the working tree is clean (no uncommitted changes).
    fn is_working_tree_clean(repo_path: &Path) -> bool {
        let output = Command::new("git")
            .current_dir(repo_path)
            .args(["status", "--porcelain"])
            .output();

        match output {
            Ok(out) => out.stdout.is_empty() && out.stderr.is_empty(),
            Err(_) => false,
        }
    }

    /// Validates a git URL format. Blocks http:// (MITM risk) and SSRF patterns.
    ///
    /// Accepts only `https://` and `git@` (SSH) schemes. For `https://` URLs the
    /// authority is reduced to its host before the SSRF check: a bracketed IPv6
    /// authority (`[::1]`, `[::1]:22`) is cut at the closing bracket so the
    /// address literal survives intact, while a plain authority is cut at the
    /// `:` port separator. An unterminated bracket falls back to the whole
    /// authority, which fails closed.
    ///
    /// Userinfo (`user:pass@host`) is stripped before host extraction: the
    /// authority is cut at its last `@` and the right side is taken, so a
    /// credentialed URL (`https://user:pass@127.0.0.1/`) cannot disguise the
    /// real host from the SSRF check. The cut is scoped to the authority
    /// (RFC 3986: the authority ends at the first `/`), so an `@` inside the
    /// path never moves the cut point.
    ///
    /// For `git@` scp-style URLs the same rule applies on the segment before
    /// the `:` path separator: git connects to the host after the last `@`
    /// (`git@u@127.0.0.5` targets 127.0.0.5, not `u@127.0.0.5`), and a
    /// bracketed IPv6 literal that opens before any `:` is the host itself
    /// (`git@a@b@[::1]` targets `::1`); an unterminated bracket falls back to
    /// the whole segment and fails closed.
    ///
    /// Every rejection message embeds the URL through [`Self::sanitize_url`]:
    /// the host stays visible for debugging while `user:pass` userinfo is
    /// redacted to `***@` before the message reaches stdout, the WAL, or
    /// bundles.
    ///
    /// # Parameters
    /// - `url`: Candidate git remote URL
    ///
    /// # Returns
    /// `Ok(())` when the scheme is allowed and the host is not an SSRF target
    ///
    /// # Errors
    /// Returns [`Error::SchemaValidationFailed`] for a disallowed scheme or when
    /// the extracted host matches a known SSRF target.
    // The index sum in the bracket branch is total: `bracket` is a byte index
    // into `s` and `i` an index into `s[bracket..]`, so `bracket + i + 1`
    // never exceeds `s.len()`.
    #[allow(clippy::arithmetic_side_effects)]
    fn validate_url(url: &str) -> Result<()> {
        let is_https = url.starts_with("https://");
        let is_ssh = url.starts_with("git@");
        if !is_https && !is_ssh {
            return Err(Error::SchemaValidationFailed(format!(
                "Insecure or unsupported URL scheme: {} (must use https:// or git@ SSH)",
                Self::sanitize_url(url)
            )));
        }

        if is_https {
            if let Some(host_part) = url
                .strip_prefix("https://")
                .and_then(|s| s.split('/').next())
                .map(|authority| {
                    authority
                        .rsplit_once('@')
                        .map_or(authority, |(_, host)| host)
                })
            {
                let host = if let Some(rest) = host_part.strip_prefix('[') {
                    match rest.split_once(']') {
                        Some((inside, _)) => inside,
                        None => rest,
                    }
                } else {
                    host_part.split(':').next().unwrap_or(host_part)
                };
                if Self::is_ssrf_host(host) {
                    return Err(Error::SchemaValidationFailed(format!(
                        "SSRF blocked: URL targets internal/metadata address: {}",
                        Self::sanitize_url(url)
                    )));
                }
            }
        } else if is_ssh {
            if let Some(s) = url.strip_prefix("git@") {
                // scp-style host extraction: the segment ends at the first
                // ':' — unless a '[' opens a bracketed IPv6 literal before
                // that ':', in which case it ends at the closing ']'
                // (inclusive); an unterminated bracket takes the whole
                // segment and fails closed, mirroring the https branch.
                let authority = match (s.find(':'), s.find('[')) {
                    (Some(colon), Some(bracket)) if bracket < colon => {
                        let end = s[bracket..].find(']').map_or(s.len(), |i| bracket + i + 1);
                        &s[..end]
                    }
                    _ => s.split(':').next().unwrap_or(s),
                };
                // The last '@' is the userinfo/host boundary: git connects to
                // the right side (git@u@127.0.0.5 → 127.0.0.5), exactly as
                // the https authority cut above.
                let host = authority
                    .rsplit_once('@')
                    .map_or(authority, |(_, host)| host);
                if Self::is_ssrf_host(host) {
                    return Err(Error::SchemaValidationFailed(format!(
                        "SSRF blocked: URL targets internal/metadata address: {}",
                        Self::sanitize_url(url)
                    )));
                }
            }
        }

        Ok(())
    }

    /// Checks if a host is a known SSRF target (cloud metadata, localhost, link-local).
    ///
    /// Normalizes the host before checking against the blocklist:
    /// - Converts to lowercase
    /// - Strips a single trailing dot (FQDN root label, e.g. `2130706433.`)
    /// - Strips surrounding brackets (IPv6 notation like `[::1]`)
    /// - Converts decimal integer IPs (e.g., `2130706433`) to dotted quad
    /// - Converts hex integer IPs (e.g., `0x7f000001`) to dotted quad
    /// - Converts octal / mixed-radix dotted quads (e.g., `0177.0.0.1`,
    ///   `0x7f.0.0.1`) to decimal dotted quad
    /// - Converts short-form (1-3 part) numeric hosts the way `inet_aton`
    ///   does (e.g., `0x7f.1`, `0x7f.0.1`, `0x0a.1`) to decimal dotted quad
    /// - Extracts IPv4 from IPv4-mapped IPv6 addresses (`::ffff:127.0.0.1`)
    /// - Blocks the all-zero IPv6 literal (`::`, `::0`, `0:0:0:0:0:0:0:0`)
    /// - Blocks short-form numeric hosts (≤2 dots, up to 3 all-numeric parts)
    /// - Blocks 127.0.0.0/8 and 10.0.0.0/8 by first octet for numeric dotted
    ///   quads (see [`Self::is_ssrf_blocklist`])
    ///
    /// # Parameters
    /// - `host`: Hostname or IP address extracted from a URL
    ///
    /// # Returns
    /// `true` if the host matches a known SSRF target pattern
    fn is_ssrf_host(host: &str) -> bool {
        let lower = host.to_lowercase();
        // A single trailing dot is the FQDN root label; dropping it keeps the
        // integer-IP and octal-quad normalizations below from missing the
        // target ("2130706433." and "0177.0.0.1." name the same addresses).
        let dotless = lower.strip_suffix('.').unwrap_or(lower.as_str());
        let unbracketed = dotless.trim_start_matches('[').trim_end_matches(']');

        // Handle IPv4-mapped IPv6: ::ffff:127.0.0.1 → check 127.0.0.1.
        // The remainder is not always a dotted quad: the last 32 bits of a
        // mapped address may arrive in pure hex colon form ("::ffff:7f00:1"
        // is 127.0.0.1), which no dot-gate below can parse — it reached the
        // blocklist verbatim and passed. check_mapped_ipv4 decodes that form
        // and otherwise defers to the full chain on the remainder.
        if let Some(mapped) = unbracketed.strip_prefix("::ffff:") {
            return Self::check_mapped_ipv4(mapped);
        }

        // The all-zero IPv6 literal is the IPv6 counterpart of 0.0.0.0:
        // connecting to it reaches the local host. The "::1" indicator
        // cannot match it — the spelling carries no 1 — so it is gated on
        // its shape instead of on a substring.
        if Self::is_all_zero_ipv6(unbracketed) {
            return true;
        }

        // Normalize decimal integer IP to dotted quad: 2130706433 → 127.0.0.1
        if let Ok(addr) = unbracketed.parse::<u32>() {
            return Self::is_ssrf_blocklist(&Self::u32_to_dotted_quad(addr));
        }

        // Normalize hex integer IP to dotted quad: 0x7f000001 → 127.0.0.1
        if let Some(hex) = unbracketed.strip_prefix("0x") {
            if let Ok(addr) = u32::from_str_radix(hex, 16) {
                return Self::is_ssrf_blocklist(&Self::u32_to_dotted_quad(addr));
            }
        }

        // Normalize octal / mixed-radix dotted quads to decimal dotted quad:
        // 0177.0.0.1 → 127.0.0.1, 0x7f.0.0.1 → 127.0.0.1. These carry dots, so
        // the integer-IP branch above never parses them and their obfuscated
        // spelling never matches a blocklist substring.
        if let Some(quad) = Self::normalize_octal_dotted_quad(unbracketed) {
            return Self::is_ssrf_blocklist(&quad);
        }

        // Normalize short-form (1-3 part) numeric hosts the way inet_aton
        // does: 0x7f.1 → 127.0.0.1, 0x7f.0.1 → 127.0.0.1, 0x0a.1 → 10.0.0.1.
        // These carry dots (the integer-IP branch never parses them), have
        // fewer than four parts (normalize_octal_dotted_quad rejects them),
        // and a 0x part is not all-ASCII-digit (is_short_form_numeric rejects
        // them) — without this gate they reach the blocklist verbatim.
        if let Some(quad) = Self::normalize_short_form_numeric_host(unbracketed) {
            return Self::is_ssrf_blocklist(&quad);
        }

        // Block short-form numeric hosts (≤2 dots, all parts numeric)
        // e.g., 127.1, 1.2.3 — ambiguous representations used for SSRF bypass
        if Self::is_short_form_numeric(unbracketed) {
            return true;
        }

        Self::is_ssrf_blocklist(unbracketed)
    }

    /// Checks the IPv4 payload of an IPv4-mapped IPv6 host for SSRF targets.
    ///
    /// Called with the remainder of a `::ffff:` authority (the prefix already
    /// stripped, host lowercased and bracket-stripped). Three shapes reach it:
    ///
    /// - A remainder whose trailing group after the last `:` carries a dot
    ///   (`0:127.0.0.1`) embeds a dotted quad; that quad is re-checked
    ///   through [`Self::is_ssrf_host`] so every normalization there applies
    ///   (`0:127.1.1.1` is loopback even though the `127.0.0.1` literal and
    ///   the first-octet guard both miss it on the raw text).
    /// - A pure hex colon remainder (`7f00:1`, `a9fe:a9fe`) spells the last
    ///   32 bits of the address as one or two hex groups; it is decoded by
    ///   [`Self::decode_mapped_ipv4_hex`] and checked against
    ///   [`Self::is_ssrf_blocklist`] as a dotted quad.
    /// - Anything else (a plain dotted or integer remainder, or a hex form
    ///   that does not decode) is handed to [`Self::is_ssrf_host`] unchanged.
    ///
    /// The decode is additive: when it does not block, the existing chain
    /// still runs on the remainder, so no spelling that was blocked before
    /// (`::ffff:fe80:1` via the `fe80:` indicator) becomes accepted.
    ///
    /// # Parameters
    /// - `mapped`: Host text after the `::ffff:` prefix
    ///
    /// # Returns
    /// `true` if the mapped IPv4 address is a known SSRF target
    fn check_mapped_ipv4(mapped: &str) -> bool {
        // An embedded dotted quad behind hex groups: check the quad itself.
        if let Some((_, trailing)) = mapped.rsplit_once(':') {
            if trailing.contains('.') {
                return Self::is_ssrf_host(trailing);
            }
        }

        // Pure hex colon form: decode the trailing 32 bits and check the
        // resulting dotted quad.
        if let Some(addr) = Self::decode_mapped_ipv4_hex(mapped) {
            if Self::is_ssrf_blocklist(&Self::u32_to_dotted_quad(addr)) {
                return true;
            }
        }

        Self::is_ssrf_host(mapped)
    }

    /// Decodes the trailing 32 bits of an IPv4-mapped IPv6 hex form to a u32.
    ///
    /// `::ffff:7f00:1` spells 127.0.0.1 and `::ffff:a9fe:a9fe` spells
    /// 169.254.169.254: the last 32 bits of the mapped address are the IPv4
    /// address, written as one or two hex groups. Two groups are read as
    /// `hi:lo` (16 + 16 bits); a single group holds all 32 bits and is
    /// capped at 8 hex digits. Groups wider than 16 bits are not legal IPv6
    /// groups, so such a spelling does not resolve and yields `None`.
    ///
    /// # Parameters
    /// - `mapped`: Host text after the `::ffff:` prefix
    ///
    /// # Returns
    /// `Some(addr)` when the trailing groups decode to a 32-bit address;
    /// `None` when the remainder is not that shape (dotted, empty, or
    /// carrying a non-hex group)
    // The shift and or below are total: parse_hex_u16 pins each group inside
    // 16 bits, so the shifted high half can never overflow the u32.
    #[allow(clippy::arithmetic_side_effects)]
    fn decode_mapped_ipv4_hex(mapped: &str) -> Option<u32> {
        // Only the trailing two groups carry the IPv4 payload; any earlier
        // groups of a longer spelling are the zero-padding of the mapping.
        let mut groups = mapped.rsplit(':');
        let lo = groups.next()?;
        if let Some(hi) = groups.next() {
            let hi = Self::parse_hex_u16(hi)?;
            let lo = Self::parse_hex_u16(lo)?;
            Some((u32::from(hi) << 16) | u32::from(lo))
        } else {
            // A single group holds all 32 bits.
            if lo.is_empty() || lo.len() > 8 {
                return None;
            }
            if !lo.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            u32::from_str_radix(lo, 16).ok()
        }
    }

    /// Parses one IPv6 hex group as a 16-bit value.
    ///
    /// Accepts 1-4 hex digits — the shape `inet_pton` requires of an IPv6
    /// group. A `+`/`-` sign, whitespace, an empty group, or a group wider
    /// than 16 bits yields `None`: the first three are not host spellings
    /// (`from_str_radix` alone would accept a leading `+`), and the last does
    /// not resolve.
    ///
    /// # Parameters
    /// - `group`: Single colon-separated group of an IPv6 literal
    ///
    /// # Returns
    /// `Some(value)` when the group is a legal 1-4 digit hex group
    fn parse_hex_u16(group: &str) -> Option<u16> {
        if group.is_empty() || group.len() > 4 {
            return None;
        }
        if !group.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        u16::from_str_radix(group, 16).ok()
    }

    /// Converts a u32 address to dotted quad notation (e.g., 2130706433 → "127.0.0.1").
    ///
    /// # Parameters
    /// - `addr`: 32-bit integer representation of an IPv4 address
    ///
    /// # Returns
    /// Dotted quad string (e.g., "127.0.0.1")
    fn u32_to_dotted_quad(addr: u32) -> String {
        format!(
            "{}.{}.{}.{}",
            (addr >> 24) & 0xFF,
            (addr >> 16) & 0xFF,
            (addr >> 8) & 0xFF,
            addr & 0xFF
        )
    }

    /// Checks if a host is a short-form numeric address (≤2 dots, all parts numeric).
    ///
    /// Short-form IPs like `127.1` or `1.2.3` are ambiguous representations
    /// that can bypass substring blocklists. These are treated as suspicious.
    ///
    /// # Parameters
    /// - `host`: Hostname to check (should already be bracket-stripped and lowercased)
    ///
    /// # Returns
    /// `true` if the host is a short-form numeric address
    fn is_short_form_numeric(host: &str) -> bool {
        let dot_count = host.matches('.').count();
        dot_count <= 2
            && !host.is_empty()
            && host
                .split('.')
                .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
    }

    /// Parses one dot-separated part of a numeric host in its own radix.
    ///
    /// A `0x` prefix selects hex, a leading zero on a multi-character part
    /// selects octal, and anything else is decimal — the same rule `git`
    /// and `curl` apply when they resolve a dotted host. Both the 4-part
    /// quad normalizer and the short-form normalizer parse parts through
    /// this one function so the two gates cannot drift apart and leave a
    /// spelling that falls between them.
    ///
    /// # Parameters
    /// - `part`: Single dot-separated label of a candidate numeric host
    ///
    /// # Returns
    /// `Some(value)` when the part is a valid number in its radix; `None`
    /// when it is empty or not parseable in the selected radix
    fn parse_numeric_part(part: &str) -> Option<u32> {
        let (radix, digits) = if let Some(hex) = part.strip_prefix("0x") {
            (16, hex)
        } else if part.len() > 1 && part.starts_with('0') {
            (8, part)
        } else {
            (10, part)
        };
        // Only plain digit/hex runs count as numeric parts: from_str_radix
        // accepts a leading '+', which is not a host spelling, so the
        // all-numeric gate must reject it explicitly.
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return None;
        }
        u32::from_str_radix(digits, radix).ok()
    }

    /// Checks whether a host is the all-zero IPv6 literal.
    ///
    /// The unspecified address (`::`, and its equivalent spellings `::0`
    /// and `0:0:0:0:0:0:0:0`) is the IPv6 counterpart of `0.0.0.0`:
    /// connecting to it reaches the local host. The blocklist covers
    /// `::1` as a substring, but the all-zero spelling carries no `1` to
    /// match, so it reached the blocklist verbatim and passed.
    ///
    /// The check is a shape test, not an address parse: `:` is not a legal
    /// character in a hostname, so its presence marks an IPv6 literal, and
    /// every group of the all-zero address is either empty (the `::`
    /// compression) or all zeroes. A public literal that merely compresses
    /// (`2606:4700::1111`) has a non-zero group and is left alone.
    ///
    /// # Parameters
    /// - `host`: Host to check (should already be bracket-stripped and lowercased)
    ///
    /// # Returns
    /// `true` if the host is an all-zero IPv6 literal
    fn is_all_zero_ipv6(host: &str) -> bool {
        host.contains(':')
            && host
                .split(':')
                .all(|group| group.is_empty() || group.bytes().all(|b| b == b'0'))
    }

    /// Normalizes an octal or mixed-radix dotted quad to a decimal dotted quad.
    ///
    /// `git` and `curl` resolve dotted quads whose octets carry a leading zero
    /// as octal (`0177.0.0.1` → `127.0.0.1`) and accept a `0x` prefix on an
    /// octet (`0x7f.0.0.1` → `127.0.0.1`). Such a host contains dots, so the
    /// integer-IP branch of [`Self::is_ssrf_host`] never parses it, and its
    /// obfuscated spelling never matches a blocklist substring — it reaches
    /// the blocklist verbatim and passes. Normalizing first closes that gap.
    ///
    /// Applies only when all four dot-separated parts are numeric and at
    /// least one part carries a leading zero. A host with any non-numeric
    /// part (`007.com`) yields `None` and is left untouched, so ordinary
    /// hostnames are never rewritten. A part that is not valid in its radix
    /// (`0189` is not octal) or exceeds 255 also yields `None`, leaving the
    /// host on its existing path — such a spelling does not resolve, so it
    /// is not an SSRF vector. Part radixes are decided by
    /// [`Self::parse_numeric_part`], shared with the short-form normalizer.
    ///
    /// # Parameters
    /// - `host`: Hostname to check (should already be lowercased,
    ///   trailing-dot-stripped, and bracket-stripped)
    ///
    /// # Returns
    /// `Some(dotted_quad)` when the host is a leading-zero numeric quad that
    /// normalizes cleanly; `None` when the host is not that shape
    fn normalize_octal_dotted_quad(host: &str) -> Option<String> {
        let parts: [&str; 4] = host.split('.').collect::<Vec<_>>().try_into().ok()?;
        let mut has_leading_zero = false;
        let mut octets = [0u8; 4];

        for (slot, part) in octets.iter_mut().zip(parts) {
            // A "0x" prefix is itself a leading-zero spelling, and so is a
            // multi-character part starting with '0'.
            if part.starts_with("0x") || (part.len() > 1 && part.starts_with('0')) {
                has_leading_zero = true;
            }
            let value = Self::parse_numeric_part(part)?;
            // try_from rejects any octet above 255 — the same out-of-range
            // guard an explicit `value > 255` check would perform.
            *slot = u8::try_from(value).ok()?;
        }

        if !has_leading_zero {
            return None;
        }

        Some(
            octets
                .iter()
                .map(|octet| octet.to_string())
                .collect::<Vec<_>>()
                .join("."),
        )
    }

    /// Normalizes a short-form (1-3 part) numeric host to a decimal dotted quad.
    ///
    /// `git` and `curl` resolve a dotted host with fewer than four parts the
    /// way `inet_aton` does: the trailing part absorbs the bits its position
    /// leaves over, so `0x7f.1` is `127.0.0.1` and `0x7f.0.1` is `127.0.0.1`.
    /// Such a host carries dots (the integer-IP branch of
    /// [`Self::is_ssrf_host`] never parses it), has fewer than four parts
    /// ([`Self::normalize_octal_dotted_quad`] rejects it), and a `0x` part is
    /// not all-ASCII-digit ([`Self::is_short_form_numeric`] rejects it) — so
    /// without this gate it reaches the blocklist verbatim and passes.
    ///
    /// Every part must be numeric in its own radix (see
    /// [`Self::parse_numeric_part`]) and within the ceiling `inet_aton`
    /// allows for its position: every part but the last is a whole octet,
    /// and the last may hold the remaining 24, 16, or 32 bits. A host with
    /// any non-numeric part (`007.com`) yields `None`, so ordinary hostnames
    /// are never rewritten. A part above its ceiling does not resolve, so it
    /// is not an SSRF vector and also yields `None`.
    ///
    /// # Parameters
    /// - `host`: Hostname to check (should already be lowercased,
    ///   trailing-dot-stripped, and bracket-stripped)
    ///
    /// # Returns
    /// `Some(dotted_quad)` when the host is a 1-3 part numeric form that
    /// normalizes cleanly; `None` when the host is not that shape
    // The shift and or below are total: the per-part ceiling check pins every
    // part inside the bits its position owns, so no combination can overflow.
    #[allow(clippy::arithmetic_side_effects)]
    fn normalize_short_form_numeric_host(host: &str) -> Option<String> {
        let parts: Vec<&str> = host.split('.').collect();
        // inet_aton's part-count rule: the trailing part absorbs the bits its
        // position leaves over, so its ceiling grows as the part count falls.
        // Four or more parts belong to normalize_octal_dotted_quad.
        let last_ceiling: u32 = match parts.len() {
            1 => u32::MAX,
            2 => 0x00FF_FFFF,
            3 => 0x0000_FFFF,
            _ => return None,
        };

        let mut addr: u32 = 0;
        for (idx, part) in parts.iter().enumerate() {
            let value = Self::parse_numeric_part(part)?;
            if idx + 1 == parts.len() {
                // The trailing part absorbs every bit the earlier parts
                // left over, so its ceiling is the widest of the three.
                if value > last_ceiling {
                    return None;
                }
                addr |= value;
            } else {
                // Every earlier part is a whole octet counted from the top
                // of the address, exactly as in a 4-part quad. With at most
                // three parts, a non-final part only ever sits at index 0
                // or 1, so `3 - idx` cannot underflow.
                if value > 0xFF {
                    return None;
                }
                // `idx` is at most 1 here (a non-final part of a 1-3 part
                // host), so the subtraction cannot underflow and the cast
                // cannot truncate.
                let shift = 8 * u32::try_from(3 - idx).ok()?;
                addr |= value << shift;
            }
        }

        Some(Self::u32_to_dotted_quad(addr))
    }

    /// Checks if a host matches the SSRF blocklist.
    ///
    /// A first-octet range guard runs before the substring scan so the
    /// blocklist covers whole private ranges, not single spellings:
    /// - A 4-part all-numeric host whose first octet is `127` is the whole
    ///   127.0.0.0/8 loopback range (`127.1.1.1`) — the literal
    ///   `127.0.0.1` indicator only matches one spelling of it.
    /// - A 4-part all-numeric host whose first octet is `10` is the whole
    ///   10.0.0.0/8 range (`10.9.9.9`), past the four enumerated /24s.
    ///
    /// The all-numeric gate keeps hostnames that merely contain digits on
    /// the substring path below, so a name like `host10.example.com` is
    /// never matched by the 10.0.0.0/8 guard.
    ///
    /// # Parameters
    /// - `host`: Normalized hostname or dotted-quad IP (bracket-stripped, lowercased)
    ///
    /// # Returns
    /// `true` if the host matches any SSRF indicator
    fn is_ssrf_blocklist(host: &str) -> bool {
        // 127.0.0.0/8 and 10.0.0.0/8 by first octet: a 4-part all-numeric
        // host starting at 127 or 10 is loopback or RFC1918 space
        // ("127.1.1.1", "10.9.9.9"), spellings the literal indicators miss.
        // The all-numeric gate leaves hostnames that merely contain digits
        // ("host10.example.com") on the substring path below.
        let parts: Vec<&str> = host.split('.').collect();
        if parts.len() == 4
            && matches!(parts.first(), Some(&("127" | "10")))
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        {
            return true;
        }
        let ssrf_indicators = [
            "169.254.169.254",
            "169.254.",
            "127.0.0.1",
            "localhost",
            "0.0.0.0",
            "::1",
            "10.0.0.",
            "10.0.1.",
            "10.0.2.",
            "10.0.3.",
            "172.16.",
            "172.17.",
            "172.18.",
            "172.19.",
            "172.20.",
            "172.21.",
            "172.22.",
            "172.23.",
            "172.24.",
            "172.25.",
            "172.26.",
            "172.27.",
            "172.28.",
            "172.29.",
            "172.30.",
            "172.31.",
            "192.168.",
            "metadata.google",
            "metadata.azure",
            "instance-data",
            "100.100.100.200",
            "fe80:",
        ];
        ssrf_indicators
            .iter()
            .any(|indicator| host.contains(indicator))
    }

    /// Validates a branch name against git's ref naming rules and option injection.
    ///
    /// Rejects: empty branches, `..` (range spec), `@{` (reflog), `--` prefix
    /// (option injection), `refs/` patterns (ref injection), control characters,
    /// whitespace, and shell/git metacharacters (`:`, `~`, `^`, `*`, `[`, `\\`,
    /// `.lock`, `?`).
    fn validate_branch_name(branch: &str) -> Result<()> {
        if branch.is_empty() {
            return Err(Error::SchemaValidationFailed("Branch name is empty".into()));
        }
        if branch.contains("..") || branch.contains("@{") {
            return Err(Error::SchemaValidationFailed(format!(
                "Invalid branch name: {}",
                branch
            )));
        }
        if branch.starts_with("--") {
            return Err(Error::SchemaValidationFailed(format!(
                "Branch name cannot start with '--': {}",
                branch
            )));
        }
        if branch.starts_with("refs/") || branch.contains("/refs/") {
            return Err(Error::SchemaValidationFailed(format!(
                "Ref injection detected in branch name: {}",
                branch
            )));
        }
        if branch.contains(|c: char| c.is_control() || c.is_whitespace()) {
            return Err(Error::SchemaValidationFailed(format!(
                "Branch name contains control or whitespace: {}",
                branch
            )));
        }
        if branch.contains([':', '~', '^', '*', '[', '\\', '?'])
            || std::path::Path::new(branch)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("lock"))
        {
            return Err(Error::SchemaValidationFailed(format!(
                "Branch name contains invalid character: {}",
                branch
            )));
        }
        Ok(())
    }

    /// Validates a commit SHA.
    fn validate_commit_sha(sha: &str) -> Result<()> {
        if sha.len() < 7 || sha.len() > 40 {
            return Err(Error::SchemaValidationFailed(format!(
                "Invalid commit SHA length: {}",
                sha
            )));
        }
        if !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(Error::SchemaValidationFailed(format!(
                "Invalid commit SHA: {}",
                sha
            )));
        }
        Ok(())
    }

    /// Sanitizes credentials from a URL string (redacts user:pass@).
    /// Preserves SSH-style URLs (git@host:path) unchanged.
    #[allow(clippy::arithmetic_side_effects)]
    fn sanitize_url(url: &str) -> String {
        if url.starts_with("git@") {
            return url.to_string();
        }
        if let Some(at_pos) = url.find('@') {
            if let Some(scheme_end) = url.find("://") {
                let scheme = &url[..scheme_end + 3];
                let after_at = &url[at_pos + 1..];
                return format!("{}***@{}", scheme, after_at);
            }
            return format!("***@{}", &url[at_pos + 1..]);
        }
        url.to_string()
    }

    /// Sanitizes git output to remove credential leakage.
    fn sanitize_output(output: &str) -> String {
        let re_pattern = |line: &str| -> String {
            let mut result = String::new();
            let mut chars = line.chars().peekable();
            while let Some(c) = chars.next() {
                if c == ':' && chars.peek() == Some(&'/') && chars.clone().nth(1) == Some('/') {
                    result.push_str("://");
                    chars.next();
                    chars.next();
                    let mut user_pass = String::new();
                    let mut found_at = false;
                    for nc in chars.by_ref() {
                        if nc == '@' {
                            found_at = true;
                            break;
                        }
                        user_pass.push(nc);
                    }
                    if found_at && !user_pass.is_empty() {
                        result.push_str("***@");
                    } else {
                        result.push_str(&user_pass);
                        if found_at {
                            result.push('@');
                        }
                    }
                } else {
                    result.push(c);
                }
            }
            result
        };

        output
            .lines()
            .map(re_pattern)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Checks if a file path looks like a secret file that should not be committed.
    fn is_secret_file(path: &str) -> bool {
        let lower = path.to_lowercase();
        SECRET_PATTERNS.iter().any(|pattern| {
            lower == *pattern
                || lower.ends_with(&format!("/{}", pattern))
                || lower.contains(&format!("/{}/", pattern))
        })
    }

    /// Validates a file path for git add (no traversal, no secrets).
    fn validate_add_file(file: &str, repo_path: &Path) -> Result<()> {
        if file.contains("..") {
            return Err(Error::SchemaValidationFailed(format!(
                "Path traversal in file path: {}",
                file
            )));
        }
        if Self::is_secret_file(file) {
            return Err(Error::SchemaValidationFailed(format!(
                "Secret file detected, refusing to add: {}",
                file
            )));
        }
        let full_path = repo_path.join(file);
        if full_path.exists() {
            let canonical = full_path.canonicalize().map_err(|e| {
                Error::SchemaValidationFailed(format!("Cannot resolve file {}: {}", file, e))
            })?;
            let canonical_repo = repo_path.canonicalize().map_err(|e| {
                Error::SchemaValidationFailed(format!("Cannot resolve repo: {}", e))
            })?;
            if !canonical.starts_with(&canonical_repo) {
                return Err(Error::SchemaValidationFailed(format!(
                    "File {} escapes repository boundary",
                    file
                )));
            }
        }
        Ok(())
    }

    /// Checks available disk space (returns free bytes, or None if unknown).
    fn disk_free_bytes(path: &Path) -> Option<u64> {
        let output = Command::new("df")
            .arg("--output=avail")
            .arg("-B1")
            .arg(path)
            .output()
            .ok()?;
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            stdout.lines().nth(1)?.trim().parse().ok()
        } else {
            None
        }
    }

    /// Counts untracked files that would be removed by git clean -fd.
    fn count_untracked_files(repo_path: &Path, timeout_secs: u64) -> Result<usize> {
        let output = Self::run_git_with_timeout(
            repo_path,
            &["ls-files", "--others", "--exclude-standard"],
            timeout_secs,
        )?;
        Ok(output.lines().filter(|l| !l.is_empty()).count())
    }

    /// Sanitizes a commit message (strips control chars, ensures non-empty).
    fn sanitize_commit_message(msg: &str) -> Result<String> {
        let sanitized: String = msg
            .chars()
            .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
            .collect();
        let trimmed = sanitized.trim();
        if trimmed.is_empty() {
            return Err(Error::SchemaValidationFailed(
                "Commit message is empty after sanitization".into(),
            ));
        }
        Ok(trimmed.to_string())
    }

    /// Creates a backup unconditionally before any mutating operation.
    fn backup_before_mutation(&self, repo_path: &Path, job_id: &str) -> Result<PathBuf> {
        self.backup_mgr.create_backup(repo_path, job_id)
    }

    /// Captures the current git state for a repository.
    fn capture_state(repo_path: &Path, timeout_secs: u64) -> Result<GitState> {
        let commit_sha =
            Self::run_git_with_timeout(repo_path, &["rev-parse", "HEAD"], timeout_secs)
                .map(|s| s.trim().to_string())
                .ok();

        let branch = Self::run_git_with_timeout(
            repo_path,
            &["rev-parse", "--abbrev-ref", "HEAD"],
            timeout_secs,
        )
        .map(|s| s.trim().to_string())
        .ok();

        let remote_url =
            Self::run_git_with_timeout(repo_path, &["remote", "get-url", "origin"], timeout_secs)
                .ok()
                .and_then(|s| {
                    let trimmed = s.trim().to_string();
                    let sanitized = Self::sanitize_url(&trimmed);
                    if sanitized.is_empty() {
                        None
                    } else {
                        Some(sanitized)
                    }
                });

        let is_clean = Self::is_working_tree_clean(repo_path);

        Ok(GitState {
            commit_sha,
            branch,
            remote_url,
            repo_path: repo_path.to_string_lossy().to_string(),
            is_clean,
        })
    }

    /// Executes git clone operation.
    ///
    /// # Parameters
    /// - `args`: Git operation arguments (URL, branch, timeout)
    /// - `ctx`: Execution context (dry_run, job_id, working_dir)
    /// - `validated_path`: Pre-validated path from [`validate_path`] — must be used
    ///   as the clone target to prevent path traversal via the original `args.path`
    ///
    /// # Errors
    /// Returns [`Error::ExecutionFailed`] if the path already exists, disk space
    /// is insufficient, or the git clone subprocess fails.
    /// Returns [`Error::SchemaValidationFailed`] if the URL fails validation.
    fn op_clone(&self, args: &GitExecArgs, ctx: &Context, validated_path: &Path) -> Result<Output> {
        let _ = self;
        let timeout_secs = resolve_timeout(args.timeout_secs);
        let url = args
            .url
            .as_ref()
            .ok_or_else(|| Error::ExecutionFailed("URL required for clone".into()))?;

        Self::validate_url(url)?;

        let path = validated_path;
        if path.exists() {
            return Err(Error::ExecutionFailed(format!(
                "Path already exists: {}",
                path.display()
            )));
        }

        if let Some(free) = Self::disk_free_bytes(path.parent().unwrap_or_else(|| Path::new("/"))) {
            if free < 100 * 1024 * 1024 {
                return Err(Error::ExecutionFailed(
                    "Insufficient disk space for clone (need at least 100MB)".into(),
                ));
            }
        }

        if ctx.dry_run {
            let mut out = Output::ok(format!(
                "DRY RUN: would clone {} to {}",
                Self::sanitize_url(url),
                path.display()
            ));
            out.data = Some(serde_json::json!({
                "operation": "clone",
                "url": Self::sanitize_url(url),
                "path": path.display().to_string(),
                "dry_run": true
            }));
            return Ok(out);
        }

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                Error::ExecutionFailed(format!("mkdir {}: {}", parent.display(), e))
            })?;
        }

        let mut cmd = Command::new("git");
        cmd.arg("clone").arg(url).arg(path);

        if let Some(branch) = &args.branch {
            cmd.arg("-b").arg(branch);
        }

        let mut child = cmd
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| Error::ExecutionFailed(format!("git clone spawn failed: {}", e)))?;

        let timeout = Duration::from_secs(timeout_secs);
        let start = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) => {
                    if start.elapsed() > timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(Error::ExecutionFailed(format!(
                            "git clone timed out after {}s",
                            timeout_secs
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Error::ExecutionFailed(format!(
                        "git clone wait error: {}",
                        e
                    )));
                }
            }
        };

        if !status.success() {
            let output = child
                .wait_with_output()
                .map_err(|e| Error::ExecutionFailed(format!("git wait failed: {}", e)))?;
            let stderr_raw = String::from_utf8_lossy(&output.stderr).to_string();
            let stderr_text = redact_stderr(&stderr_raw);
            let msg = if stderr_text.trim().is_empty() {
                "unknown error".to_string()
            } else {
                stderr_text
            };
            return Err(Error::ExecutionFailed(format!("git clone failed: {msg}")));
        }
        let _ = child.wait_with_output();

        let state = Self::capture_state(path, timeout_secs)?;

        let mut out = Output::ok(format!(
            "Cloned {} to {}",
            Self::sanitize_url(url),
            path.display()
        ));
        out.data = Some(serde_json::json!({
            "operation": "clone",
            "url": Self::sanitize_url(url),
            "path": path.display().to_string(),
            "commit_sha": state.commit_sha,
            "branch": state.branch,
            "remote_url": state.remote_url
        }));
        Ok(out)
    }

    /// Executes git pull operation.
    fn op_pull(&self, args: &GitExecArgs, ctx: &Context, repo_path: &Path) -> Result<Output> {
        let timeout_secs = resolve_timeout(args.timeout_secs);

        if !repo_path.exists() {
            return Err(Error::ExecutionFailed(format!(
                "Repository not found: {}",
                repo_path.display()
            )));
        }

        let state_before = Self::capture_state(repo_path, timeout_secs)?;

        if ctx.dry_run {
            let mut out = Output::ok("DRY RUN: would pull".into());
            out.data = Some(serde_json::json!({
                "operation": "pull",
                "path": repo_path.display().to_string(),
                "dry_run": true
            }));
            return Ok(out);
        }

        let backup_path = Some(self.backup_before_mutation(repo_path, &ctx.job_id)?);

        let output = Self::run_git_with_timeout(repo_path, &["pull", "--rebase"], timeout_secs)
            .map_err(|e| Error::ExecutionFailed(format!("git pull failed: {}", e)))?;

        let state_after = Self::capture_state(repo_path, timeout_secs)?;

        let mut out = Output::ok("Pulled successfully".into());
        out.data = Some(serde_json::json!({
            "operation": "pull",
            "path": repo_path.display().to_string(),
            "commit_sha_before": state_before.commit_sha,
            "commit_sha_after": state_after.commit_sha,
            "branch": state_after.branch,
            "backup_path": backup_path.map(|p| p.to_string_lossy().to_string()),
            "git_output": Self::sanitize_output(&output)
        }));
        Ok(out)
    }

    /// Executes git commit operation.
    fn op_commit(&self, args: &GitExecArgs, ctx: &Context, repo_path: &Path) -> Result<Output> {
        let timeout_secs = resolve_timeout(args.timeout_secs);

        if !repo_path.exists() {
            return Err(Error::ExecutionFailed(format!(
                "Repository not found: {}",
                repo_path.display()
            )));
        }

        let message = args
            .message
            .as_ref()
            .ok_or_else(|| Error::ExecutionFailed("Commit message required".into()))?;
        let message = Self::sanitize_commit_message(message)?;

        let state_before = Self::capture_state(repo_path, timeout_secs)?;

        if ctx.dry_run {
            let mut out = Output::ok("DRY RUN: would commit".into());
            out.data = Some(serde_json::json!({
                "operation": "commit",
                "path": repo_path.display().to_string(),
                "message": &message,
                "dry_run": true
            }));
            return Ok(out);
        }

        let backup_path = Some(self.backup_before_mutation(repo_path, &ctx.job_id)?);

        if let Some(files) = &args.files {
            for file in files {
                Self::validate_add_file(file, repo_path)?;
                let output = Self::run_git_with_timeout(repo_path, &["add", file], timeout_secs)
                    .map_err(|e| Error::ExecutionFailed(format!("git add failed: {}", e)))?;
                let _ = output;
            }
        } else {
            let untracked = Self::run_git_with_timeout(
                repo_path,
                &["ls-files", "--others", "--exclude-standard"],
                timeout_secs,
            )?;
            for line in untracked.lines() {
                let file = line.trim();
                if file.is_empty() {
                    continue;
                }
                if Self::is_secret_file(file) {
                    eprintln!("[runtimo] Skipping secret file from git add: {}", file);
                    continue;
                }
                Self::run_git_with_timeout(repo_path, &["add", file], timeout_secs).map_err(
                    |e| Error::ExecutionFailed(format!("git add {} failed: {}", file, e)),
                )?;
            }
        }

        let output =
            Self::run_git_with_timeout(repo_path, &["commit", "-m", &message], timeout_secs)
                .map_err(|e| Error::ExecutionFailed(format!("git commit failed: {}", e)))?;
        let _ = output;

        let state_after = Self::capture_state(repo_path, timeout_secs)?;

        let mut out = Output::ok(format!("Committed: {}", message));
        out.data = Some(serde_json::json!({
            "operation": "commit",
            "path": repo_path.display().to_string(),
            "message": message,
            "commit_sha_before": state_before.commit_sha,
            "commit_sha_after": state_after.commit_sha,
            "branch": state_after.branch,
            "backup_path": backup_path.map(|p| p.to_string_lossy().to_string())
        }));
        Ok(out)
    }

    /// Executes git revert operation.
    fn op_revert(&self, args: &GitExecArgs, ctx: &Context, repo_path: &Path) -> Result<Output> {
        let timeout_secs = resolve_timeout(args.timeout_secs);

        if !repo_path.exists() {
            return Err(Error::ExecutionFailed(format!(
                "Repository not found: {}",
                repo_path.display()
            )));
        }

        let commit_sha = args
            .commit_sha
            .as_ref()
            .ok_or_else(|| Error::ExecutionFailed("Commit SHA required for revert".into()))?;

        Self::validate_commit_sha(commit_sha)?;

        let state_before = Self::capture_state(repo_path, timeout_secs)?;

        if ctx.dry_run {
            let mut out = Output::ok(format!("DRY RUN: would revert {}", commit_sha));
            out.data = Some(serde_json::json!({
                "operation": "revert",
                "path": repo_path.display().to_string(),
                "commit_sha": commit_sha,
                "dry_run": true
            }));
            return Ok(out);
        }

        let backup_path = Some(self.backup_before_mutation(repo_path, &ctx.job_id)?);

        let output = Self::run_git_with_timeout(
            repo_path,
            &["revert", "--no-edit", commit_sha],
            timeout_secs,
        )
        .map_err(|e| Error::ExecutionFailed(format!("git revert failed: {}", e)))?;
        let _ = output;

        let state_after = Self::capture_state(repo_path, timeout_secs)?;

        let mut out = Output::ok(format!("Reverted {}", commit_sha));
        out.data = Some(serde_json::json!({
            "operation": "revert",
            "path": repo_path.display().to_string(),
            "commit_sha": commit_sha,
            "commit_sha_before": state_before.commit_sha,
            "commit_sha_after": state_after.commit_sha,
            "branch": state_after.branch,
            "backup_path": backup_path.map(|p| p.to_string_lossy().to_string())
        }));
        Ok(out)
    }

    /// Executes git clean operation.
    fn op_clean(&self, args: &GitExecArgs, ctx: &Context, repo_path: &Path) -> Result<Output> {
        let timeout_secs = resolve_timeout(args.timeout_secs);

        if !repo_path.exists() {
            return Err(Error::ExecutionFailed(format!(
                "Repository not found: {}",
                repo_path.display()
            )));
        }

        let state_before = Self::capture_state(repo_path, timeout_secs)?;

        if ctx.dry_run {
            let untracked_count = Self::count_untracked_files(repo_path, timeout_secs).unwrap_or(0);
            let preview =
                Self::run_git_with_timeout(repo_path, &["clean", "-fd", "--dry-run"], timeout_secs)
                    .map(|s| Self::sanitize_output(&s))
                    .unwrap_or_default();
            let mut out = Output::ok(format!(
                "DRY RUN: would clean {} untracked files",
                untracked_count
            ));
            out.data = Some(serde_json::json!({
                "operation": "clean",
                "path": repo_path.display().to_string(),
                "dry_run": true,
                "untracked_count": untracked_count,
                "preview": preview
            }));
            return Ok(out);
        }

        let untracked_count = Self::count_untracked_files(repo_path, timeout_secs)?;
        if untracked_count > MAX_CLEAN_FILES {
            return Err(Error::ExecutionFailed(format!(
                "Too many untracked files to clean safely: {} (limit: {})",
                untracked_count, MAX_CLEAN_FILES
            )));
        }

        let backup_path = Some(self.backup_before_mutation(repo_path, &ctx.job_id)?);

        let output = Self::run_git_with_timeout(repo_path, &["clean", "-fd"], timeout_secs)
            .map_err(|e| Error::ExecutionFailed(format!("git clean failed: {}", e)))?;
        let _ = output;

        let state_after = Self::capture_state(repo_path, timeout_secs)?;

        let mut out = Output::ok(format!("Cleaned {} untracked files", untracked_count));
        out.data = Some(serde_json::json!({
            "operation": "clean",
            "path": repo_path.display().to_string(),
            "was_clean": state_before.is_clean,
            "is_clean": state_after.is_clean,
            "untracked_files_removed": untracked_count,
            "backup_path": backup_path.map(|p| p.to_string_lossy().to_string())
        }));
        Ok(out)
    }

    /// Executes git status operation.
    #[allow(clippy::unused_self, clippy::used_underscore_binding)]
    fn op_status(&self, _args: &GitExecArgs, _ctx: &Context, repo_path: &Path) -> Result<Output> {
        let timeout_secs = resolve_timeout(_args.timeout_secs);

        if !repo_path.exists() {
            return Err(Error::ExecutionFailed(format!(
                "Repository not found: {}",
                repo_path.display()
            )));
        }

        let state = Self::capture_state(repo_path, timeout_secs)?;

        let status_output =
            Self::run_git_with_timeout(repo_path, &["status", "--porcelain"], timeout_secs)
                .unwrap_or_default();

        let branch = state.branch.clone().unwrap_or_default();
        let remote_url = state.remote_url.clone().unwrap_or_default();

        let mut out = Output::ok(format!(
            "On branch {}: {}",
            branch,
            if state.is_clean { "clean" } else { "dirty" }
        ));
        out.data = Some(serde_json::json!({
            "operation": "status",
            "path": repo_path.display().to_string(),
            "branch": branch,
            "remote_url": remote_url,
            "commit_sha": state.commit_sha,
            "is_clean": state.is_clean,
            "status": status_output
        }));
        Ok(out)
    }
}

impl TypedCapability for GitExec {
    type Args = GitExecArgs;

    fn name(&self) -> &'static str {
        "GitExec"
    }

    fn description(&self) -> &'static str {
        "git operations: clone, pull, commit, revert, clean, status. state tracking (sha, branch, remote), SSRF-blocked URLs, secret detection, timeout, undo via backup."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "operation": { "type": "string", "enum": ["clone", "pull", "commit", "revert", "clean", "status"] },
                "url": { "type": "string" },
                "path": { "type": "string" },
                "branch": { "type": "string" },
                "message": { "type": "string" },
                "files": { "type": "array", "items": { "type": "string" } },
                "commit_sha": { "type": "string" },
                "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 600 }
            },
            "required": ["operation"]
        })
    }

    fn execute(
        &self,
        args: GitExecArgs,
        ctx: &Context,
    ) -> std::result::Result<Output, CapabilityError> {
        let valid_ops = ["clone", "pull", "commit", "revert", "clean", "status"];
        if !valid_ops.contains(&args.operation.as_str()) {
            return Err(CapabilityError::InvalidArgs(format!(
                "Invalid operation: {}. Must be one of: {}",
                args.operation,
                valid_ops.join(", ")
            )));
        }

        // Clone target validated once here and reused by the `clone` arm
        // below — the canonicalized path is what op_clone must operate on.
        let mut validated_clone_path: Option<PathBuf> = None;
        if args.operation == "clone" {
            if let Some(url) = &args.url {
                Self::validate_url(url)
                    .map_err(|e| CapabilityError::PermissionDenied(e.to_string()))?;
            } else {
                return Err(CapabilityError::InvalidArgs(
                    "URL required for clone".into(),
                ));
            }
            if let Some(path) = &args.path {
                let ctx = PathContext {
                    require_exists: false,
                    require_file: false,
                    ..Default::default()
                };
                validated_clone_path =
                    Some(validate_path(path, &ctx).map_err(CapabilityError::PermissionDenied)?);
            }
        }

        if args.operation != "clone" {
            if let Some(path) = &args.path {
                let ctx = PathContext {
                    require_exists: true,
                    require_file: false,
                    ..Default::default()
                };
                validate_path(path, &ctx).map_err(CapabilityError::PermissionDenied)?;
            }
        }

        if let Some(branch) = &args.branch {
            Self::validate_branch_name(branch)
                .map_err(|e| CapabilityError::InvalidArgs(e.to_string()))?;
        }

        if let Some(sha) = &args.commit_sha {
            Self::validate_commit_sha(sha)
                .map_err(|e| CapabilityError::InvalidArgs(e.to_string()))?;
        }

        let telemetry_before = Telemetry::capture();
        let process_before = ProcessSnapshot::capture();

        let result = match args.operation.as_str() {
            "clone" => {
                let validated = validated_clone_path.as_deref().ok_or_else(|| {
                    CapabilityError::InvalidArgs("Path required for clone".into())
                })?;
                self.op_clone(&args, ctx, validated)
            }
            "pull" => {
                let path = args
                    .path
                    .as_ref()
                    .ok_or_else(|| CapabilityError::InvalidArgs("Path required for pull".into()))?;
                self.op_pull(&args, ctx, Path::new(path))
            }
            "commit" => {
                let path = args.path.as_ref().ok_or_else(|| {
                    CapabilityError::InvalidArgs("Path required for commit".into())
                })?;
                self.op_commit(&args, ctx, Path::new(path))
            }
            "revert" => {
                let path = args.path.as_ref().ok_or_else(|| {
                    CapabilityError::InvalidArgs("Path required for revert".into())
                })?;
                self.op_revert(&args, ctx, Path::new(path))
            }
            "clean" => {
                let path = args.path.as_ref().ok_or_else(|| {
                    CapabilityError::InvalidArgs("Path required for clean".into())
                })?;
                self.op_clean(&args, ctx, Path::new(path))
            }
            "status" => {
                let path = args.path.as_ref().ok_or_else(|| {
                    CapabilityError::InvalidArgs("Path required for status".into())
                })?;
                self.op_status(&args, ctx, Path::new(path))
            }
            _ => Err(Error::ExecutionFailed(format!(
                "Unknown operation: {}",
                args.operation
            ))),
        };

        let telemetry_after = Telemetry::capture();
        let process_after = ProcessSnapshot::capture();

        let mut output = result.map_err(|e| CapabilityError::Internal(e.to_string()))?;
        if let Some(obj) = output.data.as_mut().and_then(|d| d.as_object_mut()) {
            obj.insert(
                "telemetry_before".to_string(),
                serde_json::to_value(&telemetry_before).unwrap_or(Value::Null),
            );
            obj.insert(
                "telemetry_after".to_string(),
                serde_json::to_value(&telemetry_after).unwrap_or(Value::Null),
            );
            obj.insert(
                "process_before".to_string(),
                serde_json::to_value(&process_before.summary).unwrap_or(Value::Null),
            );
            obj.insert(
                "process_after".to_string(),
                serde_json::to_value(&process_after.summary).unwrap_or(Value::Null),
            );
        }

        Ok(output)
    }
}

/// Redacts secret-bearing lines from git stderr before it reaches error
/// strings (which surface in WAL `error` fields, CLI output, and daemon logs).
///
/// Two pattern passes run line-by-line: [`SECRET_PATTERNS`] (secret file
/// names) plus [`EXTRA_PATTERNS`] (line-content markers). Any line
/// containing a pattern is replaced wholesale with
/// `"[REDACTED - secret pattern detected]"`; lines that already carry the
/// redaction marker are never re-processed, so the marker itself (which
/// contains "secret") is stable under repeated passes.
///
/// The result is truncated to [`MAX_STDERR_LEN`] bytes (character-boundary
/// safe, then suffixed with `"... (truncated)"`) so a chatty clone failure
/// cannot bloat the error string.
///
/// # Inputs
/// - `s`: raw git stderr (UTF-8 after `String::from_utf8_lossy`)
///
/// # Returns
/// - Redacted, truncated, always-valid-UTF-8 text. Never panics on
///   multi-byte input (truncation backs off to a character boundary).
///
/// # Errors
/// - None. This is a pure string transform.
fn redact_stderr(s: &str) -> String {
    let mut result = s.to_string();
    for pattern in SECRET_PATTERNS {
        if result.to_lowercase().contains(&pattern.to_lowercase()) {
            result = result
                .lines()
                .map(|line| {
                    // Skip lines already redacted by a prior pass
                    if line.contains("[REDACTED") {
                        line
                    } else if line.to_lowercase().contains(&pattern.to_lowercase()) {
                        "[REDACTED - secret pattern detected]"
                    } else {
                        line
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
        }
    }
    for pattern in EXTRA_PATTERNS {
        if result.to_lowercase().contains(pattern) {
            result = result
                .lines()
                .map(|line| {
                    if line.contains("[REDACTED") {
                        line.to_string()
                    } else if line.to_lowercase().contains(pattern) {
                        "[REDACTED - secret pattern detected]".to_string()
                    } else {
                        line.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
        }
    }
    if result.len() > MAX_STDERR_LEN {
        // Back off to a character boundary — git stderr can embed
        // user-controlled paths/URLs with multi-byte UTF-8, and a raw
        // byte slice would panic mid-character on the error path.
        let mut end = MAX_STDERR_LEN;
        while !result.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        format!("{}... (truncated)", &result[..end])
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::Capability;

    fn test_backup_dir() -> PathBuf {
        std::env::temp_dir().join("runtimo_git_test")
    }

    #[test]
    fn validates_git_url_https_only() {
        assert!(GitExec::validate_url("https://github.com/user/repo.git").is_ok());
        assert!(GitExec::validate_url("git@github.com:user/repo.git").is_ok());

        assert!(GitExec::validate_url("http://example.com/repo.git").is_err());
        assert!(GitExec::validate_url("not-a-url").is_err());
        assert!(GitExec::validate_url("").is_err());

        std::fs::remove_dir_all(test_backup_dir()).ok();
    }

    #[test]
    fn timeout_alias_is_accepted() {
        let args: GitExecArgs = serde_json::from_value(serde_json::json!({
            "operation": "status",
            "timeout": 120
        }))
        .expect("timeout alias should deserialize");
        assert_eq!(args.timeout_secs, Some(120));
    }

    #[test]
    fn resolve_timeout_prefers_explicit_arg() {
        let _guard = crate::config::CONFIG_TEST_MUTEX.lock().unwrap();
        // Isolate from any operator config: an empty XDG_CONFIG_HOME makes
        // RuntimoConfig::load() fall back to built-in defaults, so the
        // built-in-default branch is what is under test.
        let iso = std::env::temp_dir().join("runtimo_git_timeout_iso");
        let _ = std::fs::remove_dir_all(&iso);
        std::fs::create_dir_all(iso.join("runtimo")).unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &iso);

        let explicit = resolve_timeout(Some(50));
        let fallback = resolve_timeout(None);

        std::env::remove_var("XDG_CONFIG_HOME");
        let _ = std::fs::remove_dir_all(&iso);

        assert_eq!(explicit, 50, "explicit arg must win");
        assert_eq!(fallback, 300, "built-in default must apply without config");
    }

    #[test]
    fn resolve_timeout_prefers_config_over_builtin_default() {
        let _guard = crate::config::CONFIG_TEST_MUTEX.lock().unwrap();
        // Real config file carrying a GitExec timeout: proves the full
        // precedence chain explicit arg > config > built-in default.
        let iso = std::env::temp_dir().join("runtimo_git_timeout_cfg");
        let _ = std::fs::remove_dir_all(&iso);
        let cfg_dir = iso.join("runtimo");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join("config.toml"),
            "[capability_timeouts]\nGitExec = 120\n",
        )
        .unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &iso);

        let from_config = resolve_timeout(None);
        let explicit_wins = resolve_timeout(Some(50));

        std::env::remove_var("XDG_CONFIG_HOME");
        let _ = std::fs::remove_dir_all(&iso);

        assert_eq!(
            from_config, 120,
            "config capability_timeouts must override the built-in default"
        );
        assert_eq!(
            explicit_wins, 50,
            "explicit arg must override the config timeout"
        );
    }

    #[test]
    fn blocks_ssrf_urls() {
        assert!(GitExec::validate_url("https://169.254.169.254/latest/meta-data/").is_err());
        assert!(GitExec::validate_url("https://127.0.0.1/repo.git").is_err());
        assert!(GitExec::validate_url("https://localhost/repo.git").is_err());
        assert!(GitExec::validate_url("https://192.168.1.1/repo.git").is_err());
        assert!(GitExec::validate_url("https://metadata.google.internal/computeMetadata").is_err());

        std::fs::remove_dir_all(test_backup_dir()).ok();
    }

    #[test]
    fn blocks_ssrf_normalized_ip_forms() {
        // Normalization level: every obfuscated loopback representation
        // must resolve to a blocked host.
        for host in [
            "2130706433",       // decimal 127.0.0.1
            "0x7f000001",       // hex 127.0.0.1
            "127.1",            // short form
            "::ffff:127.0.0.1", // IPv4-mapped IPv6
            "[::1]",            // bracketed IPv6 loopback
        ] {
            assert!(
                GitExec::is_ssrf_host(host),
                "SSRF normalization must block host '{}'",
                host
            );
        }
        // URL level for the forms that survive host extraction
        // (bracketed IPv6 authorities are covered by the dedicated test below).
        for url in [
            "https://2130706433/repo.git",
            "https://0x7f000001/repo.git",
            "https://127.1/repo.git",
        ] {
            assert!(
                GitExec::validate_url(url).is_err(),
                "validate_url must block '{}'",
                url
            );
        }
        // Valid remotes still pass.
        assert!(GitExec::validate_url("https://github.com/user/repo.git").is_ok());
        assert!(GitExec::validate_url("git@github.com:user/repo.git").is_ok());
    }

    #[test]
    fn blocks_ssrf_bracketed_ipv6_urls() {
        // Bracketed IPv6 authorities carry their own ':' separators, so the
        // host must be cut at the closing bracket — never at the first ':' —
        // or the address literal is mangled before the SSRF check runs.
        for url in [
            "https://[::1]/repo.git",
            "https://[::ffff:127.0.0.1]/repo.git",
            "https://[::1]:22/repo.git",
            "https://[::1/repo.git",
        ] {
            assert!(
                GitExec::validate_url(url).is_err(),
                "validate_url must block '{}'",
                url
            );
        }
        // Non-bracket hosts keep their existing accept/reject behavior.
        for url in [
            "https://github.com/user/repo.git",
            "https://github.com:443/user/repo.git",
            "git@github.com:user/repo.git",
        ] {
            assert!(
                GitExec::validate_url(url).is_ok(),
                "validate_url must accept '{}'",
                url
            );
        }
    }

    #[test]
    fn blocks_ssrf_userinfo_urls() {
        // Userinfo must never disguise the host: the authority is cut at its
        // last '@' before host extraction, so the SSRF check sees the real
        // target — not the "user" fragment a naive ':' split would yield.
        for url in [
            "https://user:pass@127.0.0.1/repo.git",
            "https://user:pass@169.254.169.254/latest/meta-data/",
            "https://token@localhost/repo.git",
            "https://user:pass@192.168.1.1/repo.git",
            // Failure-class guard: an '@' in the PATH must not move the cut
            // point — the authority still ends at the first '/'.
            "https://user:pass@127.0.0.1/x@y",
        ] {
            assert!(
                GitExec::validate_url(url).is_err(),
                "validate_url must block credentialed SSRF URL '{}'",
                url
            );
        }
    }

    #[test]
    fn blocks_ssrf_scp_userinfo_urls() {
        // scp-style userinfo: git resolves the host as the text after the
        // last '@' before the ':' separator, so the userinfo fragment must
        // never disguise the real target, and a bracketed IPv6 literal is
        // the host itself.
        for url in [
            "git@u@127.0.0.5:repo.git",
            "git@u@10.9.9.9:repo.git",
            "git@a@b@[::1]:repo.git",
        ] {
            assert!(
                GitExec::validate_url(url).is_err(),
                "validate_url must block scp-style userinfo URL '{}'",
                url
            );
        }
        // The public-host verdict is untouched by the userinfo cut.
        assert!(
            GitExec::validate_url("git@github.com:user/repo.git").is_ok(),
            "validate_url must accept plain scp-style public URL"
        );
    }

    #[test]
    fn accepts_userinfo_public_urls() {
        // Credentialed URLs to public hosts stay valid — stripping userinfo
        // must not reject legitimate remotes.
        for url in [
            "https://user:pass@github.com/user/repo.git",
            "https://oauth2:token@github.com:443/org/repo.git",
            "https://user@github.com/user/repo.git",
            // An '@' in the path is not userinfo: host extraction must still
            // land on github.com, not on the path segment.
            "https://github.com/user@org/repo.git",
        ] {
            assert!(
                GitExec::validate_url(url).is_ok(),
                "validate_url must accept '{}'",
                url
            );
        }
    }

    #[test]
    fn no_userinfo_behavior_unchanged() {
        // Regression fence: URLs without userinfo keep their exact prior
        // accept/reject behavior after the userinfo cut was introduced.
        for url in [
            "https://github.com/user/repo.git",
            "https://github.com:443/user/repo.git",
            "git@github.com:user/repo.git",
        ] {
            assert!(
                GitExec::validate_url(url).is_ok(),
                "validate_url must accept '{}'",
                url
            );
        }
        for url in [
            "https://127.0.0.1/repo.git",
            "https://localhost/repo.git",
            "https://[::1]/repo.git",
            "http://example.com/repo.git",
        ] {
            assert!(
                GitExec::validate_url(url).is_err(),
                "validate_url must block '{}'",
                url
            );
        }
    }

    #[test]
    fn blocks_ssrf_octal_dotted_quad_hosts() {
        // Octal and mixed-radix dotted quads resolve to blocked addresses but
        // carry dots, so the integer-IP branch never parses them and their
        // obfuscated spelling never matches a blocklist substring. Every entry
        // below was verified against the system resolver to land on a target
        // the blocklist already covers.
        for host in [
            "0177.0.0.1",          // octal 127.0.0.1
            "0177.0.0.1.",         // same target, FQDN root label
            "0x7f.0.0.1",          // hex first octet, octal remainder
            "0251.0376.0251.0376", // octal 169.254.169.254 (metadata)
            "0300.0250.1.1",       // octal 192.168.1.1
            "012.0.0.1",           // octal 10.0.0.1
            "0000.0000.0000.0000", // octal 0.0.0.0
        ] {
            assert!(
                GitExec::is_ssrf_host(host),
                "SSRF normalization must block octal host '{}'",
                host
            );
        }
        // URL level: the obfuscated host must not survive host extraction.
        for url in [
            "https://0177.0.0.1/repo.git",
            "https://0x7f.0.0.1/repo.git",
            "git@0177.0.0.1:user/repo.git",
        ] {
            assert!(
                GitExec::validate_url(url).is_err(),
                "validate_url must block '{}'",
                url
            );
        }
        // Pass-through fence: a leading-zero label inside a real hostname is
        // not an IP literal and must stay valid.
        for host in ["007.com", "01.example.com", "github.com", "8.8.8.8"] {
            assert!(
                !GitExec::is_ssrf_host(host),
                "host '{}' is not an SSRF target and must pass",
                host
            );
        }
        // Normal dotted quads keep their exact prior verdict.
        assert!(GitExec::is_ssrf_host("127.0.0.1"));
        assert!(GitExec::is_ssrf_host("169.254.169.254"));
        assert!(!GitExec::is_ssrf_host("1.2.3.4"));
    }

    #[test]
    fn blocks_ssrf_loopback_and_ten_ranges() {
        // Failure class: the blocklist matched "127.0.0.1" and four 10/24s
        // literally, so the rest of 127.0.0.0/8 ("127.1.1.1") and 10.0.0.0/8
        // ("10.9.9.9") passed — including the octal spelling "0177.1.1.1",
        // which normalizes to 127.1.1.1 before the blocklist runs.
        for host in [
            "127.1.1.1",       // loopback, non-.0.0.1 spelling
            "127.0.0.1",       // the literal spelling stays blocked
            "127.255.255.255", // far end of 127/8 — range, not one instance
            "0177.1.1.1",      // octal → normalizes to 127.1.1.1
            "10.9.9.9",        // 10/8 past the four enumerated /24s
            "10.255.255.255",  // far end of 10/8
        ] {
            assert!(
                GitExec::is_ssrf_host(host),
                "SSRF check must block host '{}'",
                host
            );
        }
        // URL level: the obfuscated host must not survive host extraction.
        for url in [
            "https://127.1.1.1/repo.git",
            "https://0177.1.1.1/repo.git",
            "https://10.9.9.9/repo.git",
            "git@127.1.1.1:user/repo.git",
        ] {
            assert!(
                GitExec::validate_url(url).is_err(),
                "validate_url must block '{}'",
                url
            );
        }
        // Boundary and hostname-safety fence: first octet 128 is not
        // loopback, and the all-numeric gate leaves non-numeric hosts
        // (hostnames that merely contain digits) untouched.
        for host in [
            "128.1.1.1",
            "8.8.8.8",
            "supervisor.example.com",
            "github.com",
        ] {
            assert!(
                !GitExec::is_ssrf_host(host),
                "host '{}' is not an SSRF target and must pass",
                host
            );
        }
        for url in [
            "https://128.1.1.1/repo.git",
            "https://supervisor.example.com/repo.git",
        ] {
            assert!(
                GitExec::validate_url(url).is_ok(),
                "validate_url must accept '{}'",
                url
            );
        }
    }

    #[test]
    fn blocks_ssrf_short_form_hex_hosts() {
        // Failure class: a dotted host with fewer than four parts whose
        // parts carry a `0x` prefix falls between every gate — it has dots
        // (the integer-IP branch never parses it), fewer than four parts
        // (`normalize_octal_dotted_quad` rejects it), and a `0x` part is
        // not all-ASCII-digit (`is_short_form_numeric` rejects it). It
        // reaches the blocklist verbatim and passes, while `inet_aton`
        // resolves each spelling below to a blocked address.
        for host in [
            "0x7f.1",   // 2 parts → 127.0.0.1
            "0x7f.0.1", // 3 parts → 127.0.0.1
            "0x0a.1",   // 2 parts → 10.0.0.1
            "0x7f.1.1", // 3 parts → 127.0.1.1
        ] {
            assert!(
                GitExec::is_ssrf_host(host),
                "SSRF check must block short-form hex host '{}'",
                host
            );
        }
        // URL level: the obfuscated host must not survive host extraction.
        for url in [
            "https://0x7f.1/repo.git",
            "https://0x7f.0.1/repo.git",
            "https://0x0a.1/repo.git",
            "git@0x7f.1:user/repo.git",
        ] {
            assert!(
                GitExec::validate_url(url).is_err(),
                "validate_url must block '{}'",
                url
            );
        }
        // Pass-through fence: one non-numeric label leaves the host on its
        // existing path, and a 4-part form belongs to the octal-quad gate.
        for host in ["0x7f.example.com", "007.com", "8.8.8.8", "1.2.3.4"] {
            assert!(
                !GitExec::is_ssrf_host(host),
                "host '{}' is not an SSRF target and must pass",
                host
            );
        }
    }

    #[test]
    fn blocks_ssrf_all_zero_ipv6_hosts() {
        // Failure class: the bare all-zero IPv6 literal is the IPv6
        // counterpart of 0.0.0.0 — connecting to it reaches the local
        // host. The "::1" indicator cannot match it: the spelling carries
        // no 1, so it reached the blocklist verbatim and passed.
        for host in ["::", "::0", "0:0:0:0:0:0:0:0"] {
            assert!(
                GitExec::is_ssrf_host(host),
                "SSRF check must block all-zero IPv6 host '{}'",
                host
            );
        }
        // URL level: bracket extraction must yield the bare literal for
        // both the portless and the ported authority.
        for url in [
            "https://[::]/repo.git",
            "https://[::]:9443/repo.git",
            "https://[0:0:0:0:0:0:0:0]/repo.git",
        ] {
            assert!(
                GitExec::validate_url(url).is_err(),
                "validate_url must block '{}'",
                url
            );
        }
        // Pass-through fence: a public IPv6 literal compresses with `::`
        // but is not all-zero, and the loopback spelling stays blocked.
        // (The literal chosen here deliberately avoids a "::1" run: the
        // pre-existing "::1" substring indicator still over-blocks public
        // addresses like 2606:4700::1111 — a separate defect, out of scope
        // for this gate.)
        assert!(!GitExec::is_ssrf_host("2001:4860:4860::8888"));
        assert!(GitExec::is_ssrf_host("::1"));
    }

    #[test]
    fn blocks_ssrf_ipv4_mapped_hex_hosts() {
        // Failure class: the last 32 bits of an IPv4-mapped IPv6 address can
        // be written in pure hex colon form ("::ffff:7f00:1" is 127.0.0.1).
        // That remainder carries no dot, so no dot-gate below parses it and
        // the spelling reached the blocklist verbatim — and passed.
        for host in [
            "::ffff:7f00:1",      // hex 127.0.0.1
            "::ffff:a9fe:a9fe",   // hex 169.254.169.254 (metadata)
            "::ffff:0a00:0001",   // hex 10.0.0.1
            "::ffff:c0a8:0101",   // hex 192.168.1.1
            "::ffff:0:0:7f00:1",  // same target, extra leading zero groups
            "::ffff:0:127.0.0.1", // embedded dotted quad behind a zero group
            "::ffff:0:127.1.1.1", // embedded quad the "127.0.0.1" literal misses
            "::ffff:0:10.9.9.9",  // embedded quad past the four 10/24s
        ] {
            assert!(
                GitExec::is_ssrf_host(host),
                "SSRF check must block IPv4-mapped hex host '{}'",
                host
            );
        }
        // URL level: bracket extraction must hand the whole literal to the
        // SSRF check, and the block must fire before git ever spawns.
        for url in [
            "https://[::ffff:7f00:1]/repo.git",
            "https://[::ffff:a9fe:a9fe]/latest/meta-data/",
            "https://[::ffff:c0a8:0101]:443/repo.git",
        ] {
            assert!(
                GitExec::validate_url(url).is_err(),
                "validate_url must block '{}'",
                url
            );
        }
        // Pass-fence: a public mapped address is not an SSRF target, in
        // either the dotted or the hex spelling of the same 32 bits.
        for host in [
            "::ffff:8.8.8.8",   // dotted public
            "::ffff:0808:0808", // hex public, same 32 bits
            "::ffff:0:8.8.8.8", // dotted public behind a zero group
            "::ffff:1.2.3.4",   // dotted public, no zero group
        ] {
            assert!(
                !GitExec::is_ssrf_host(host),
                "host '{}' maps to a public address and must pass",
                host
            );
        }
        for url in [
            "https://[::ffff:8.8.8.8]/repo.git",
            "https://[::ffff:0808:0808]/repo.git",
        ] {
            assert!(
                GitExec::validate_url(url).is_ok(),
                "validate_url must accept '{}'",
                url
            );
        }
        // Verdict-preservation fence: spellings the pre-existing gates
        // already decided keep their exact verdict once the decode lands.
        assert!(GitExec::is_ssrf_host("::ffff:127.0.0.1"));
        assert!(GitExec::is_ssrf_host("::ffff:0x7f000001"));
        assert!(GitExec::is_ssrf_host("::ffff:2130706433"));
        // The "fe80:" indicator matches the raw mapped text even though the
        // decoded payload (254.128.0.1) is public — the decode only adds
        // blocks, it never removes one.
        assert!(GitExec::is_ssrf_host("::ffff:fe80:1"));
        assert!(!GitExec::is_ssrf_host("::ffff:github.com"));
    }

    #[test]
    fn accepts_hostnames_containing_ten_label() {
        // Failure class: the "10." substring indicator matched any host
        // whose text contains "10.", including ordinary hostnames. The
        // 10.0.0.0/8 range is gated on the first octet of a 4-part
        // all-numeric host instead, so a digit inside a name never matches.
        for host in [
            "host10.example.com",
            "storage10.example.net",
            "v10.example.org",
        ] {
            assert!(
                !GitExec::is_ssrf_host(host),
                "host '{}' is a hostname, not 10.0.0.0/8, and must pass",
                host
            );
        }
        for url in [
            "https://host10.example.com/repo.git",
            "https://storage10.example.net/repo.git",
        ] {
            assert!(
                GitExec::validate_url(url).is_ok(),
                "validate_url must accept '{}'",
                url
            );
        }
        // The range itself stays blocked, including the octal spelling
        // that normalizes into it and the short-form hex spelling.
        for host in [
            "10.0.0.1",
            "10.9.9.9",
            "10.255.255.255",
            "012.0.0.1",
            "0x0a.1",
        ] {
            assert!(
                GitExec::is_ssrf_host(host),
                "SSRF check must block 10.0.0.0/8 host '{}'",
                host
            );
        }
    }

    #[test]
    fn sanitizes_credentials_from_url() {
        assert_eq!(
            GitExec::sanitize_url("https://user:pass@github.com/repo.git"),
            "https://***@github.com/repo.git"
        );
        assert_eq!(
            GitExec::sanitize_url("https://github.com/repo.git"),
            "https://github.com/repo.git"
        );
        assert_eq!(
            GitExec::sanitize_url("git@github.com:user/repo.git"),
            "git@github.com:user/repo.git"
        );
    }

    #[test]
    fn detects_secret_files() {
        assert!(GitExec::is_secret_file(".env"));
        assert!(GitExec::is_secret_file("config/.env"));
        assert!(GitExec::is_secret_file("credentials.json"));
        assert!(GitExec::is_secret_file(".ssh/id_rsa"));
        assert!(GitExec::is_secret_file("src/.env.local"));

        assert!(!GitExec::is_secret_file("main.rs"));
        assert!(!GitExec::is_secret_file("Cargo.toml"));
        assert!(!GitExec::is_secret_file("README.md"));
    }

    #[test]
    fn validates_branch_name() {
        assert!(GitExec::validate_branch_name("main").is_ok());
        assert!(GitExec::validate_branch_name("feature/my-branch").is_ok());
        assert!(GitExec::validate_branch_name("v1.0").is_ok());

        assert!(GitExec::validate_branch_name("").is_err());
        assert!(GitExec::validate_branch_name("bad..name").is_err());
        assert!(GitExec::validate_branch_name("@{..}").is_err());
        // Option injection
        assert!(GitExec::validate_branch_name("--force").is_err());
        assert!(GitExec::validate_branch_name("--help").is_err());
        // Ref injection
        assert!(GitExec::validate_branch_name("refs/heads/main").is_err());
        // Control chars and whitespace
        assert!(GitExec::validate_branch_name("bad\nname").is_err());
        assert!(GitExec::validate_branch_name("bad\tname").is_err());
        // Metacharacters
        assert!(GitExec::validate_branch_name("bad:name").is_err());
        assert!(GitExec::validate_branch_name("bad~name").is_err());
        assert!(GitExec::validate_branch_name("bad^name").is_err());
        assert!(GitExec::validate_branch_name("bad*name").is_err());
        assert!(GitExec::validate_branch_name("bad[name").is_err());
        assert!(GitExec::validate_branch_name("bad\\name").is_err());
        assert!(GitExec::validate_branch_name("bad?name").is_err());
        assert!(GitExec::validate_branch_name("name.lock").is_err());
    }

    #[test]
    fn validates_commit_sha() {
        assert!(GitExec::validate_commit_sha("abc1234").is_ok());
        assert!(GitExec::validate_commit_sha("a1b2c3d4").is_ok());
        assert!(GitExec::validate_commit_sha("a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0").is_ok());

        assert!(GitExec::validate_commit_sha("abc123").is_err());
        assert!(GitExec::validate_commit_sha("").is_err());
        assert!(GitExec::validate_commit_sha("xyz123").is_err());
    }

    #[allow(clippy::expect_used)]
    #[test]
    fn rejects_path_traversal() {
        let cap = GitExec::new(test_backup_dir()).expect("Failed to create GitExec");

        let result = Capability::execute(
            &cap,
            &serde_json::json!({
                "operation": "clone",
                "url": "https://github.com/user/repo.git",
                "path": "../../../etc/passwd"
            }),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        );

        assert!(result.is_err() || !result.unwrap().status.is_empty());
        // The blanket impl's validate always returns Ok, so path traversal
        // is caught at execute time, not validate time.
        std::fs::remove_dir_all(test_backup_dir()).ok();
    }

    #[allow(clippy::expect_used)]
    #[test]
    fn rejects_invalid_operation() {
        let cap = GitExec::new(test_backup_dir()).expect("Failed to create GitExec");

        let result = Capability::execute(
            &cap,
            &serde_json::json!({
                "operation": "invalid_op"
            }),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        );

        assert!(result.is_err());
        std::fs::remove_dir_all(test_backup_dir()).ok();
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn status_on_nonexistent_repo() {
        let cap = GitExec::new(test_backup_dir()).expect("Failed to create GitExec");

        let result = Capability::execute(
            &cap,
            &serde_json::json!({
                "operation": "status",
                "path": "/tmp/nonexistent_repo"
            }),
            &Context {
                dry_run: false,
                job_id: "test".into(),
                working_dir: std::env::temp_dir(),
            },
        );

        assert!(result.is_err());
        std::fs::remove_dir_all(test_backup_dir()).ok();
    }

    #[test]
    fn sanitizes_commit_message() {
        assert!(GitExec::sanitize_commit_message("valid commit").is_ok());
        assert!(GitExec::sanitize_commit_message("  trimmed  ").is_ok());
        assert!(GitExec::sanitize_commit_message("").is_err());
        assert!(GitExec::sanitize_commit_message("   ").is_err());
        let result = GitExec::sanitize_commit_message("hello\x00world").unwrap();
        assert!(!result.contains('\x00'));
    }

    #[test]
    fn timeout_enforced_on_git_command() {
        // Start a TCP listener on localhost that accepts but never responds.
        // This creates a guaranteed-timeout scenario without depending on
        // external network behavior. The listener is dropped after the test.
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("failed to bind TCP listener");
        let port = listener.local_addr().unwrap().port();

        let tmp = std::env::temp_dir().join("runtimo_git_timeout_test");
        std::fs::create_dir_all(&tmp).ok();
        Command::new("git")
            .arg("init")
            .current_dir(&tmp)
            .output()
            .ok();

        // Spawn a thread that accepts one connection and hangs.
        // The git clone will connect and wait for a response that never comes.
        let _hang_handle = std::thread::spawn(move || {
            if let Ok((_stream, _addr)) = listener.accept() {
                // Hold the connection open indefinitely — never send a response
                std::thread::sleep(std::time::Duration::from_mins(5));
            }
        });

        // git clone to localhost times out after 2 seconds.
        let result = GitExec::run_git_with_timeout(
            &tmp,
            &["clone", &format!("http://127.0.0.1:{}/repo.git", port)],
            2,
        );

        // The operation should fail with a timeout (or a connection error
        // if git detects the protocol mismatch before the timeout fires).
        assert!(
            result.is_err(),
            "Expected timeout or connection error, got: {:?}",
            result
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn clone_failure_includes_stderr_message() {
        let tmp = std::env::temp_dir().join("runtimo_git_stderr_test");
        std::fs::remove_dir_all(&tmp).ok();
        std::fs::create_dir_all(&tmp).unwrap();

        let cap = GitExec::new(test_backup_dir()).expect("Failed to create GitExec");
        let ctx = Context {
            dry_run: false,
            job_id: "stderr-test".into(),
            working_dir: std::env::temp_dir(),
        };
        let result = Capability::execute(
            &cap,
            &serde_json::json!({
                "operation": "clone",
                "url": "https://invalid.example.com/no-such-repo.git",
                "path": tmp.join("target").to_str().unwrap()
            }),
            &ctx,
        );

        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("git clone failed"),
            "Error should contain 'git clone failed': {err_msg}"
        );
        assert!(
            !err_msg.contains("see stderr"),
            "Should not contain old opaque 'see stderr' message: {err_msg}"
        );
        // Proves the capture is non-empty: git's fatal line embeds the URL,
        // so a real stderr capture must carry the host. 'unknown error'
        // (the empty-capture fallback) would fail this assertion.
        assert!(
            err_msg.contains("invalid.example.com"),
            "Error should carry git's captured stderr (URL host), got: {err_msg}"
        );

        std::fs::remove_dir_all(&tmp).ok();
        std::fs::remove_dir_all(test_backup_dir()).ok();
    }

    #[test]
    fn redact_stderr_strips_secrets() {
        let input = "remote: Password: s3cret123\nremote: OK\n";
        let result = super::redact_stderr(input);
        assert!(
            !result.contains("s3cret123"),
            "Secret should be redacted, got: {result}"
        );
        assert!(
            result.contains("[REDACTED"),
            "Should contain [REDACTED], got: {result}"
        );
        assert!(
            result.contains("remote: OK"),
            "Non-secret line should be preserved: {result}"
        );
    }

    #[test]
    fn redact_stderr_truncates_long_output() {
        let long = "x".repeat(1000);
        let result = super::redact_stderr(&long);
        assert!(
            result.len() < 600,
            "Should truncate long output, got {} chars",
            result.len()
        );
        assert!(result.contains("(truncated)"));
    }

    #[test]
    fn redact_stderr_truncation_is_char_boundary_safe() {
        // 511 ASCII bytes + one 2-byte char (U+00E9 'é'): the 512-byte
        // cutoff lands INSIDE the multi-byte char. A raw byte slice
        // panics here; git stderr reaches this path with user-controlled
        // paths/URLs embedded in fatal lines.
        let input = format!("{}é", "x".repeat(511));
        assert_eq!(input.len(), 513);
        let result = super::redact_stderr(&input);
        assert!(
            result.len() < 600,
            "Should truncate at the boundary, got {} chars",
            result.len()
        );
        assert!(result.contains("(truncated)"));
        assert!(result.ends_with("... (truncated)"));
    }

    #[test]
    fn clone_rejects_path_outside_allowed_prefixes() {
        let _guard = crate::config::CONFIG_TEST_MUTEX.lock().unwrap();
        // Isolate config so the allowed-prefix whitelist is the built-in
        // default set regardless of any operator config.
        let iso = std::env::temp_dir().join("runtimo_git_clone_validate_iso");
        let _ = std::fs::remove_dir_all(&iso);
        std::fs::create_dir_all(iso.join("runtimo")).unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &iso);

        let tmp = std::env::temp_dir().join("runtimo_git_clone_validate");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let cap = GitExec::new(tmp.join("backups")).expect("GitExec");
        let err = Capability::execute(
            &cap,
            &serde_json::json!({
                "operation": "clone",
                "url": "https://github.com/example/repo.git",
                "path": "/etc/runtimo_clone_probe"
            }),
            &Context {
                dry_run: false,
                job_id: "clone-validate".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();

        std::env::remove_var("XDG_CONFIG_HOME");
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&iso);

        let msg = err.to_string();
        assert!(
            msg.contains("outside allowed directories"),
            "clone must reject paths outside allowed prefixes before spawning git, got: {}",
            msg
        );
    }

    #[test]
    #[cfg(unix)]
    fn clone_uses_validated_canonical_path() {
        let _guard = crate::config::CONFIG_TEST_MUTEX.lock().unwrap();
        let iso = std::env::temp_dir().join("runtimo_git_clone_canonical_iso");
        let _ = std::fs::remove_dir_all(&iso);
        std::fs::create_dir_all(iso.join("runtimo")).unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &iso);

        let tmp = std::env::temp_dir().join("runtimo_git_clone_canonical");
        let _ = std::fs::remove_dir_all(&tmp);
        let real = tmp.join("real");
        let link = tmp.join("link");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // Pre-create the canonical target so op_clone's existence check
        // fires before any git subprocess: the error message names the
        // path op_clone actually operated on.
        std::fs::create_dir_all(real.join("target")).unwrap();

        let cap = GitExec::new(tmp.join("backups")).expect("GitExec");
        let err = Capability::execute(
            &cap,
            &serde_json::json!({
                "operation": "clone",
                "url": "https://github.com/example/repo.git",
                "path": format!("{}/target", link.display())
            }),
            &Context {
                dry_run: false,
                job_id: "clone-canonical".into(),
                working_dir: std::env::temp_dir(),
            },
        )
        .unwrap_err();

        std::env::remove_var("XDG_CONFIG_HOME");
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&iso);

        let msg = err.to_string();
        let canonical = real.join("target");
        assert!(
            msg.contains(&*canonical.to_string_lossy()),
            "clone must operate on the validated canonical path {}, got: {}",
            canonical.display(),
            msg
        );
        assert!(
            !msg.contains("link/target"),
            "clone must not use the raw symlink path, got: {}",
            msg
        );
    }

    #[test]
    fn validate_url_errors_redact_credentials() {
        // G-SEC: validation error text flows to stdout, the WAL, and
        // bundles — the embedded URL must keep the host visible and drop
        // the password on every rejection path.
        let err =
            GitExec::validate_url("https://user:sekritPass99@127.0.0.1/repo.git").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("127.0.0.1"),
            "https SSRF error must show the host: {msg}"
        );
        assert!(
            !msg.contains("sekritPass99"),
            "https SSRF error must redact the password: {msg}"
        );

        let err = GitExec::validate_url("http://user:sekritPass99@10.0.0.1/repo.git").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("10.0.0.1"),
            "scheme error must show the host: {msg}"
        );
        assert!(
            !msg.contains("sekritPass99"),
            "scheme error must redact the password: {msg}"
        );

        // scp-style carries no password field, but the host must survive the
        // sanitize pass instead of being stripped (debuggability).
        let err = GitExec::validate_url("git@u@127.0.0.5:repo.git").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("127.0.0.5"),
            "ssh SSRF error must show the host: {msg}"
        );
    }

    #[test]
    fn consumer_dry_run_error_redacts_credentials() {
        // Consumer boundary (WAL/stdout/bundles): the capability error for a
        // blocked credentialed clone carries the host, never the password.
        let cap = GitExec::new(test_backup_dir()).expect("Failed to create GitExec");
        let result = Capability::execute(
            &cap,
            &serde_json::json!({
                "operation": "clone",
                "url": "https://user:sekritPass99@127.0.0.1/repo.git",
                "path": std::env::temp_dir().join("runtimo_git_probe_target").to_str().unwrap()
            }),
            &Context {
                dry_run: true,
                job_id: "probe".into(),
                working_dir: std::env::temp_dir(),
            },
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("127.0.0.1"),
            "consumer error must show the host: {msg}"
        );
        assert!(
            !msg.contains("sekritPass99"),
            "consumer error must redact the password: {msg}"
        );
        std::fs::remove_dir_all(test_backup_dir()).ok();
    }

    #[test]
    fn consumer_ssh_userinfo_clone_blocked() {
        // N7 probe: scp-style userinfo targeting loopback must be blocked
        // before git ever spawns, with the host still visible in the error.
        let cap = GitExec::new(test_backup_dir()).expect("Failed to create GitExec");
        let result = Capability::execute(
            &cap,
            &serde_json::json!({
                "operation": "clone",
                "url": "git@u@127.0.0.5:repo.git",
                "path": std::env::temp_dir().join("runtimo_git_probe_target").to_str().unwrap()
            }),
            &Context {
                dry_run: true,
                job_id: "probe".into(),
                working_dir: std::env::temp_dir(),
            },
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("blocked"),
            "N7 ssh probe must be blocked: {msg}"
        );
        assert!(
            msg.contains("127.0.0.5"),
            "ssh error must keep the host visible: {msg}"
        );
        std::fs::remove_dir_all(test_backup_dir()).ok();
    }
}
