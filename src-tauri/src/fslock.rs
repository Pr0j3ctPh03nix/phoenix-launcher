//! Shared file-lock probing with Windows sharing-violation semantics. Used by the
//! install/uninstall game-running interlock and by game-running detection.
//!
//! The probe is three-way on purpose: only a sharing/lock violation is the signature of a live
//! process holding the file open (a running image, an mmapped VPK). A read-only attribute or a
//! restrictive ACL (game under Program Files, launcher unelevated) also denies write — but
//! calling that "the game is running" would send the user chasing a process that isn't there
//! (interlock) or lock the UI into "In game" forever (detection).

use std::path::Path;

const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_SHARING_VIOLATION: i32 = 32;
const ERROR_LOCK_VIOLATION: i32 = 33;

/// Is this error "the file can't be written right now"? std doesn't map sharing/lock violations
/// to PermissionDenied reliably, so match the raw Windows codes too.
pub fn is_in_use(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::PermissionDenied
        || matches!(e.raw_os_error(), Some(ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION))
}

/// Sharing/lock violation ONLY — the signature of a live process holding the file open.
pub fn sharing_violation(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION))
}

/// Result of a write-probe (an open without truncating — touches nothing).
pub enum Probe {
    /// Writable right now. A missing file is Writable too — nothing holds it.
    Writable,
    /// Sharing/lock violation: a live process holds the file open.
    Held,
    /// Unwritable for another reason (read-only attribute, ACL). NOT a live-process signature —
    /// the caller should talk about permissions, not about closing the game.
    Denied(std::io::Error),
}

pub fn probe(path: &Path) -> Probe {
    match std::fs::OpenOptions::new().write(true).open(path) {
        Ok(_) => Probe::Writable,
        Err(e) if sharing_violation(&e) => Probe::Held,
        Err(e) if is_in_use(&e) => Probe::Denied(e),
        // missing file / missing parent / anything else: not a lock — let the actual
        // operation surface whatever the real problem is
        Err(_) => Probe::Writable,
    }
}

/// Detection probe: is `path` held open by a live process? Only sharing/lock violations count.
pub fn held_by_process(path: &Path) -> bool {
    matches!(probe(path), Probe::Held)
}
