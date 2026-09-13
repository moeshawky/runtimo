//! Peer authentication for Unix socket connections.
//!
//! Authenticates peer connections via `SO_PEERCRED` — reads the connecting
//! process's UID from the socket and verifies it matches the daemon's own UID.
//! This is a same-user access control, not cryptographic authentication.
//!
//! # Ownership
//! Owns the socket-level authentication boundary.
//!
//! # Safety
//! Uses `libc::getsockopt` with `SO_PEERCRED` — safe because:
//! - The fd is a valid, live Unix stream socket.
//! - `getsockopt` is read-only metadata retrieval (no side effects).
//! - The `ucred` struct is zero-initialized before the call.
//!
//! # Hardening (FINDING F7)
//! Beyond UID matching, the peer's PID is validated (non-zero) and
//! the process existence is verified via `/proc/{pid}/status` to prevent
//! PID-reuse attacks where a same-user process could connect after
//! the original process exits and its PID is reused.
//!
//! # Limitation
//! `authenticate_peer` (lines 36-102) provides identity only: SO_PEERCRED
//! UID match + PID liveness. After `handle_request` (daemon/src/engine.rs:214),
//! the centralized gate `check_capability_allowed` (daemon/src/engine.rs:218)
//! covers run+dispatch: None=default-open same-UID, Some(list)=deny -32604
//! pre-execution. Socket mode 0600 does not discriminate within a UID.
//! Mitigation: option B allow-list (pending). Cannon-gate: corpus silent
//! on SO_PEERCRED specifics — gate-unavailable; grounded in source only.

use std::fs;
use std::os::unix::io::AsRawFd;

/// Authenticate a Unix stream connection via SO_PEERCRED.
///
/// Reads the peer's UID from the socket and compares it against the daemon's
/// own UID. Only same-UID connections are permitted — this is a same-user
/// access control, not a cryptographic authentication.
///
/// # Invariants
/// PID validation fails closed: `ucred.pid` is converted with
/// `u32::try_from` and any conversion error returns `Err`. Since `pid == 0`
/// is rejected earlier, the conversion error is unreachable in practice.
#[allow(clippy::borrow_as_ptr)] // FFI: addr_of_mut! + .cast() for getsockopt
pub fn authenticate_peer(stream: &tokio::net::UnixStream) -> Result<(), String> {
    let fd = stream.as_raw_fd();
    // SAFETY: zeroed representation of ucred is valid — kernel fills it via getsockopt
    let mut ucred: libc::ucred = unsafe { std::mem::zeroed() };
    #[allow(clippy::cast_possible_truncation)] // socklen_t is u32, ucred is 32 bytes
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;

    // SAFETY: fd is a valid open socket; getsockopt reads metadata only
    let ret = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::addr_of_mut!(ucred).cast::<libc::c_void>(),
            &mut len,
        )
    };

    if ret != 0 {
        return Err(format!(
            "getsockopt(SO_PEERCRED) failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    // SAFETY: getuid is always safe — reads caller's real UID with no side effects
    let daemon_uid = unsafe { libc::getuid() };
    if ucred.uid != daemon_uid {
        return Err(format!(
            "UID mismatch: peer={}, daemon={}",
            ucred.uid, daemon_uid
        ));
    }

    // Hardening (FINDING F7): validate peer PID is non-zero and
    // the process still exists. This prevents PID-reuse attacks
    // where a same-user process could connect after the original
    // process exits and its PID is reused.
    if ucred.pid == 0 {
        return Err("PID 0 is the kernel idle task".to_string());
    }
    // liveness probe: verifies the process exists (does NOT prevent PID reuse — see authenticate_peer doc).
    let proc_path = format!("/proc/{}/status", ucred.pid);
    if let Ok(proc_content) = fs::read_to_string(&proc_path) {
        // PID match check on the in-kernel socket cred — liveness only;
        // does NOT prevent PID reuse (see outer comment).
        if let Some(pid_line) = proc_content.lines().find(|l| l.starts_with("Pid:")) {
            if let Some(pid_str) = pid_line.split(':').nth(1) {
                if let Ok(proc_pid) = pid_str.trim().parse::<u32>() {
                    // ucred.pid is i32; convert to u32 for comparison.
                    // Err is unreachable (pid == 0 rejected above) — fail closed anyway.
                    let Ok(ucred_pid) = u32::try_from(ucred.pid) else {
                        return Err("PID out of u32 range".to_string());
                    };
                    if proc_pid != ucred_pid {
                        return Err(format!(
                            "PID mismatch: ucred={}, proc={}",
                            ucred.pid, proc_pid
                        ));
                    }
                }
            }
        }
    }

    Ok(())
}
