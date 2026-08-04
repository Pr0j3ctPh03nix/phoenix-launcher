//! Finds candidate game folders on the machine (a folder containing `game/dota/steam.inf`).
//!
//! Two passes: a fast one over Steam's library folders (registry + libraryfolders.vdf), then a
//! deep bounded walk of every drive. Pure engine code — progress goes through a callback and
//! cancellation through an AtomicBool, so the shell (Tauri) decides how to surface both.
//! The engine attaches no meaning to the found ClientVersion — comparing it to the manifest's
//! required one is the caller's job (the gate value is data-driven, never hardcoded here).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::steaminf;

const MAX_DEPTH: usize = 6;
/// Directory names never worth descending into (case-insensitive).
const PRUNE: [&str; 9] = [
    "windows",
    "$recycle.bin",
    "system volume information",
    "programdata",
    "appdata",
    "node_modules",
    ".git",
    "winsxs",
    "$windows.~bt",
];

#[derive(Debug)]
pub struct Candidate {
    pub path: PathBuf,
    pub client_version: Option<String>,
}

#[derive(Debug)]
pub struct Progress {
    pub scanned: u64,
    pub current: String,
    pub found: usize,
}

/// A folder qualifies if it directly contains `game/dota/steam.inf`.
fn is_candidate(dir: &Path) -> bool {
    dir.join("game").join("dota").join("steam.inf").is_file()
}

struct Scan<'a, F: FnMut(&Progress)> {
    progress: F,
    cancel: &'a AtomicBool,
    scanned: u64,
    seen: HashSet<String>,
    found: Vec<Candidate>,
}

impl<'a, F: FnMut(&Progress)> Scan<'a, F> {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Record `dir` if it qualifies and was not seen yet. Returns true if it was a candidate
    /// (callers stop descending into it either way).
    fn consider(&mut self, dir: &Path) -> bool {
        if !is_candidate(dir) {
            return false;
        }
        let key = dir.to_string_lossy().to_ascii_lowercase();
        if self.seen.insert(key) {
            self.found.push(Candidate {
                path: dir.to_path_buf(),
                client_version: steaminf::client_version(dir),
            });
            self.report(dir, true);
        }
        true
    }

    fn report(&mut self, current: &Path, force: bool) {
        self.scanned += 1;
        if force || self.scanned.is_multiple_of(128) {
            (self.progress)(&Progress {
                scanned: self.scanned,
                current: current.display().to_string(),
                found: self.found.len(),
            });
        }
    }

    fn walk(&mut self, dir: &Path, depth: usize) {
        if self.cancelled() || depth > MAX_DEPTH {
            return;
        }
        self.report(dir, false);
        if self.consider(dir) {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            if self.cancelled() {
                return;
            }
            let Ok(ft) = e.file_type() else { continue };
            if !ft.is_dir() {
                continue;
            }
            // skip all reparse points — symlinks AND junctions (is_symlink misses junctions),
            // which otherwise create walk cycles (e.g. the legacy C:\Users junctions)
            if let Ok(md) = e.metadata() {
                use std::os::windows::fs::MetadataExt;
                const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
                if md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    continue;
                }
            }
            let name = e.file_name().to_string_lossy().to_ascii_lowercase();
            if PRUNE.contains(&name.as_str()) {
                continue;
            }
            self.walk(&e.path(), depth + 1);
        }
    }
}

/// Steam install dir from the registry, if present.
fn steam_path() -> Option<PathBuf> {
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    let key = hkcu.open_subkey("SOFTWARE\\Valve\\Steam").ok()?;
    let p: String = key.get_value("SteamPath").ok()?;
    Some(PathBuf::from(p.replace('/', "\\")))
}

/// All Steam library roots: the install dir plus every `"path"` in libraryfolders.vdf
/// (naive line parse — the value is the second quoted string on a `"path"` line).
fn steam_libraries() -> Vec<PathBuf> {
    let Some(root) = steam_path() else { return Vec::new() };
    let mut libs = vec![root.clone()];
    if let Ok(text) = std::fs::read_to_string(root.join("steamapps").join("libraryfolders.vdf")) {
        for line in text.lines() {
            let mut parts = line.split('"').filter(|s| !s.trim().is_empty());
            if parts.next() == Some("path") {
                if let Some(v) = parts.next() {
                    libs.push(PathBuf::from(v.replace("\\\\", "\\")));
                }
            }
        }
    }
    libs
}

/// Present fixed/available drive roots (`C:\`, `D:\`, …).
fn drives() -> Vec<PathBuf> {
    (b'A'..=b'Z')
        .map(|c| PathBuf::from(format!("{}:\\", c as char)))
        .filter(|p| std::fs::read_dir(p).is_ok())
        .collect()
}

/// Scan the machine for game-folder candidates. Long-running; check `cancel` to abort early —
/// whatever was found so far is still returned.
pub fn autofind(progress: impl FnMut(&Progress), cancel: &AtomicBool) -> Vec<Candidate> {
    let mut scan = Scan { progress, cancel, scanned: 0, seen: HashSet::new(), found: Vec::new() };

    // pass 1: Steam libraries — cheap and where most installs live
    for lib in steam_libraries() {
        if scan.cancelled() {
            return scan.found;
        }
        let common = lib.join("steamapps").join("common");
        if let Ok(entries) = std::fs::read_dir(&common) {
            for e in entries.flatten() {
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    scan.consider(&e.path());
                }
            }
        }
    }

    // pass 2: deep bounded walk of every drive
    for drive in drives() {
        if scan.cancelled() {
            break;
        }
        scan.walk(&drive, 0);
    }
    scan.found
}
