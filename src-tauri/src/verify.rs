//! SHA-256 of local files and byte buffers, for diffing against the manifest and verifying
//! downloads. `sha256_file_cached` memoizes by (size, mtime), so repeated plans (every check /
//! selection change re-diffs the whole file set) don't re-read unchanged files — a rewrite
//! changes the mtime and naturally invalidates the entry.

use anyhow::Result;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

static FILE_HASHES: LazyLock<Mutex<HashMap<PathBuf, (u64, SystemTime, String)>>> =
    LazyLock::new(Default::default);

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut f, &mut hasher)?;
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
