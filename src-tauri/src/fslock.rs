//! Shared file-lock probing with Windows sharing-violation semantics. Used by the
//! install/uninstall game-running interlock and by game-running detection.
//!
//! Two predicates on purpose: the interlock refuses on ANY can't-write condition (access-denied
//! included — install couldn't write such a file anyway), while game *detection* counts only
//! sharing/lock violations. A read-only attribute or a restrictive ACL (game under Program
//! Files, launcher unelevated) also denies write — calling that "running" would lock the UI
//! into "In game" forever.

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

/// Sharing/lock violation ONLY — the signature of a live process holding the file open (a
/// running image, an mmapped VPK). Deliberately excludes access-denied (see module docs).
pub fn sharing_violation(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION))
}

/// Interlock probe: is `path` unwritable right now? (Write-probe: an open without truncating
/// touches nothing.) A missing file is not "locked".
pub fn locked(path: &Path) -> bool {
    match std::fs::OpenOptions::new().write(true).open(path) {
        Ok(_) => false,
        Err(e) => is_in_use(&e),
    }
}

/// Detection probe: is `path` held open by a live process? Only sharing/lock violations count —
/// a read-only or ACL-denied file is unwritable but NOT held.
pub fn held_by_process(path: &Path) -> bool {
    match std::fs::OpenOptions::new().write(true).open(path) {
        Ok(_) => false,
        Err(e) => sharing_violation(&e),
    }
}
