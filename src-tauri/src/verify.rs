//! SHA-256 of local files and byte buffers, for diffing against the manifest and verifying
//! downloads. `sha256_file_cached` memoizes by (size, mtime), so repeated plans (every check /
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
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    // 256 KiB reads (vs io::copy's 8 KiB): managed content includes multi-hundred-MB VPKs
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// `sha256_file` with a (size, mtime)-keyed memo.
pub fn sha256_file_cached(path: &Path) -> Result<String> {
    let md = std::fs::metadata(path)?;
    let (size, mtime) = (md.len(), md.modified()?);
    if let Some((s, m, h)) = FILE_HASHES.lock().unwrap().get(path) {
        if *s == size && *m == mtime {
            return Ok(h.clone());
        }
    }
    let h = sha256_file(path)?;
    FILE_HASHES.lock().unwrap().insert(path.to_path_buf(), (size, mtime, h.clone()));
    Ok(h)
}
