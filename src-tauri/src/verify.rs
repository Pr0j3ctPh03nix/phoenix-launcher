//! SHA-256 of local files, for diffing against the manifest and verifying downloads.
//! `sha256_file_cached` memoizes by (size, mtime), so repeated plans (every check /
//! selection change re-diffs the whole file set) don't re-read unchanged files — a rewrite
//! changes the mtime and naturally invalidates the entry.
//!
//! GRANULARITY CAVEAT: Windows resolves file times coarsely (measured: two writes microseconds
//! apart share an mtime ~90% of the time), so a SAME-SIZE rewrite within the same tick as the
//! last hash is invisible to the memo. Harmless in practice — every real path puts UI, network
//! or user time between a hash and an edit — but tests that corrupt a file immediately after
//! planning must change its LENGTH, or the memo will keep reporting the file as intact.

use anyhow::Result;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

/// Memo entry: (size, mtime, hex sha256).
type HashMemo = HashMap<PathBuf, (u64, SystemTime, String)>;

static FILE_HASHES: LazyLock<Mutex<HashMemo>> = LazyLock::new(Default::default);

pub fn sha256_file(path: &Path) -> Result<String> {
    sha256_file_with(path, 0, None)
}

/// `sha256_file`, reporting bytes read as it goes.
///
/// `on_read` is called with the running total at most once per `report_every` bytes (0 = never).
/// It exists because a verify's progress is counted in FILES, and the game ships single VPKs of
/// several hundred megabytes: landing on one makes a counter sit still for a minute while the
/// disk and CPU are working flat out. A stalled counter over a pegged CPU is indistinguishable
/// from a hang, and that is the report this answers.
pub fn sha256_file_with(
    path: &Path,
    report_every: u64,
    on_read: Option<&dyn Fn(u64)>,
) -> Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    // 256 KiB reads (vs io::copy's 8 KiB): managed content includes multi-hundred-MB VPKs
    let mut buf = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    let mut next = report_every;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
        if let (Some(cb), true) = (on_read, report_every > 0 && total >= next) {
            next = total + report_every;
            cb(total);
        }
    }
    Ok(hex::encode(hasher.finalize()))
}

/// `sha256_file` with a (size, mtime)-keyed memo.
pub fn sha256_file_cached(path: &Path) -> Result<String> {
    sha256_file_cached_with(path, 0, None)
}

/// `sha256_file_cached`, reporting progress on the reads it actually performs. A memo HIT reports
/// nothing and returns instantly — there is no work to narrate, which is exactly why a warm
/// re-verify must not be made to look like a cold one.
pub fn sha256_file_cached_with(
    path: &Path,
    report_every: u64,
    on_read: Option<&dyn Fn(u64)>,
) -> Result<String> {
    let md = std::fs::metadata(path)?;
    let (size, mtime) = (md.len(), md.modified()?);
    if let Some((s, m, h)) = FILE_HASHES.lock().unwrap().get(path) {
        if *s == size && *m == mtime {
            return Ok(h.clone());
        }
    }
    let h = sha256_file_with(path, report_every, on_read)?;
    FILE_HASHES.lock().unwrap().insert(path.to_path_buf(), (size, mtime, h.clone()));
    Ok(h)
}
