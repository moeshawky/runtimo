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

use std::os::unix::io::AsRawFd;

/// Authenticate a Unix stream connection via SO_PEERCRED.
///
/// Reads the peer's UID from the socket and compares it against the daemon's
/// own UID. Only same-UID connections are permitted — this is a same-user
/// access control, not a cryptographic authentication.
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

    Ok(())
}
