//! The mutating install + uninstall.
//!
//! Install runs in two phases so a real game folder is never left half-updated:
//!   0. interlock: if any file we're about to touch is locked (the game holds its loaded DLLs /
//!      mmapped VPKs open), refuse with a typed GameRunning error — before downloading a byte;
//!   1. obtain every changed file — from the local asset cache when its hash matches, else a
//!      streaming download (a small pool fetches files in parallel; an interrupted .part is
//!      resumed, never restarted) that is verified (sha256 + size) — and stage it on the same
//!      volume; nothing under the game is touched yet;
//!   2. commit: back up each existing target, atomically move the staged file in, create
//!      winmm_orig.dll if needed, apply removals (manifest remove[] + orphaned option files),
//!      write state. Any failure in phase 2 rolls back every step already taken.
//!
//! Backups distinguish two cases so uninstall is a clean revert to stock:
//!   * a target that is OURS (a previous phoenix version, present in the prior state) is backed up
//!     ephemerally under .phoenix-backup/<version>/ purely for rollback of this install;
//!   * a target that is NOT ours (a genuine pre-existing file we would shadow) is preserved once
//!     under .phoenix-vanilla/, and uninstall restores it. Today every shipped file is a net-new
//!     loose override (its stock form lives in the VPK / System32, untouched), so .phoenix-vanilla
//!     stays empty and uninstall is a pure delete — but the machinery keeps it correct if that ever
//!     changes.

use anyhow::{anyhow, bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};

use crate::config::Settings;
use crate::downloader::{Downloader, Release};
use crate::manifest::{FileEntry, Manifest};
use crate::state::{InstalledFile, InstalledState};
use crate::{engine, fslock, verify};

const STAGING_DIR: &str = ".phoenix-staging";
const BACKUP_DIR: &str = ".phoenix-backup";
const VANILLA_DIR: &str = ".phoenix-vanilla";
/// Content-addressed asset cache (file name = sha256). Every manifest asset — including unselected
/// variants and disabled toggles — lands here via `warm_cache` (run detached by the shell after a
/// successful install), so a later customization change never re-downloads. Pruned to the current
/// manifest, deleted on uninstall.
const CACHE_DIR: &str = ".phoenix-cache";
const WINMM_ORIG: &str = "game/bin/win64/winmm_orig.dll";

#[derive(Debug, PartialEq, Eq)]
pub enum WinmmOrig {
    Created,
    Existed,
    NotNeeded,
}

#[derive(Debug)]
pub struct InstallReport {
    pub version: String,
    pub written: Vec<String>,
    pub removed: Vec<String>,
    pub up_to_date: usize,
    pub winmm_orig: WinmmOrig,
}

#[derive(Debug)]
pub struct UninstallReport {
    pub version: String,
    pub restored: Vec<String>,
    pub deleted: Vec<String>,
    pub winmm_orig_removed: bool,
}

/// A committed filesystem step, kept so phase-2 failures can be undone in reverse.
enum Committed {
    /// Moved a staged file to `target`; if the target had a prior file it was moved to `backup`.
    Placed { target: PathBuf, backup: Option<PathBuf> },
    /// Deleted `target` (moved to `backup`) — manifest remove[] or an orphaned option file.
    Removed { target: PathBuf, backup: PathBuf },
    /// Moved a preserved vanilla original from `vanilla` back to `target` (removal restore).
    VanillaRestored { target: PathBuf, vanilla: PathBuf },
    /// Created winmm_orig.dll at `path`.
    OrigCreated { path: PathBuf },
}

struct Ctx {
    game_dir: PathBuf,
    backup_root: PathBuf,
    vanilla_root: PathBuf,
    /// Dests the previous install managed (so an existing target is ours, not a vanilla original).
    prev_dests: HashSet<String>,
    /// Whether the updater lineage already created winmm_orig.dll.
    prev_winmm_created: bool,
}

/// Everything phase 2 places, removes and records.
struct CommitJob<'a> {
    staged: &'a [(&'a FileEntry, PathBuf)],
    removals: &'a [String],
    resolved: &'a [FileEntry],
    manifest: &'a Manifest,
}

pub fn install(
    settings: &Settings,
    dl: &dyn Downloader,
    tag: Option<&str>,
    progress: engine::Progress,
) -> Result<InstallReport> {
    let game_dir = settings.resolve_game_dir()?;
    // a (re)install legitimizes cache warming again after an uninstall cancelled it — cleared
    // here (not in warm_cache) so an uninstall racing a just-spawned warm still wins
    WARM_CANCEL.store(false, Ordering::Relaxed);
    let (release, manifest) = engine::fetch(settings, dl, tag)?;

    // Prior state distinguishes our files from genuine pre-existing ones, and remembers whether we
    // already created winmm_orig.dll.
    let prev = InstalledState::load(&game_dir);
    let prev_dests: HashSet<String> = prev
        .as_ref()
        .map(|s| s.files.iter().map(|f| f.dest.clone()).collect())
        .unwrap_or_default();
    let prev_winmm_created = prev.as_ref().map(|s| s.winmm_orig_created).unwrap_or(false);

    // --- what changes ---
    let resolved = engine::resolve(&manifest, &settings.selections);
    let statuses = engine::plan(&game_dir, &resolved, prev.as_ref(), &manifest.remove);
    let up_to_date = statuses.iter().filter(|s| s.action == engine::Action::UpToDate).count();
    let to_write: Vec<&FileEntry> = resolved
        .iter()
        .filter(|fe| {
            statuses.iter().any(|s| {
                s.dest == fe.dest
                    && s.action != engine::Action::UpToDate
                    && s.action != engine::Action::Remove
            })
        })
        .collect();
    // everything plan wants gone: manifest remove[] entries + orphaned option files
    let removals: Vec<String> = statuses
        .iter()
        .filter(|s| s.action == engine::Action::Remove)
        .map(|s| s.dest.clone())
        .collect();

    // --- asset cache: seed from files already installed at their manifest hash ---
    let cache = game_dir.join(CACHE_DIR);
    std::fs::create_dir_all(&cache).context("creating the asset cache")?;
    seed_cache(&cache, &game_dir, &resolved, &statuses);

    if to_write.is_empty() && removals.is_empty() {
        // Nothing to change — but a missing/corrupt state file or a missing winmm_orig.dll must
        // not lock this folder into "up to date yet not installed" forever: a no-op install
        // still heals both (every resolved file hash-matches, so the set is provably ours to
        // record). A heal failure rolls back whatever was created.
        let mut committed = Vec::new();
        let heal = ensure_winmm_orig(&game_dir, has_winmm(&resolved), &mut committed).and_then(
            |winmm_orig| {
                let created = prev_winmm_created || matches!(winmm_orig, WinmmOrig::Created);
                write_state(&game_dir, &manifest, &resolved, created).map(|_| winmm_orig)
            },
        );
        let winmm_orig = match heal {
            Ok(w) => w,
            Err(e) => {
                rollback(&committed);
                return Err(e.context("could not record the install state"));
            }
        };
        // cache warming is the caller's affair (warm_cache, backgroundable) — a heal must
        // return as fast as it healed
        return Ok(InstallReport {
            version: manifest.version.clone(),
            written: Vec::new(),
            removed: Vec::new(),
            up_to_date,
            winmm_orig,
        });
    }

    // --- interlock: fail fast when the game is running (phase 2 would roll back anyway) ---
    probe_writable(&game_dir, to_write.iter().map(|fe| &fe.dest).chain(removals.iter()))?;

    // --- phase 1a: obtain (cache-first, else parallel streaming download) — game untouched ---
    let staging = game_dir.join(STAGING_DIR);
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).context("creating the staging directory")?;
    obtain_all(&cache, dl, &release, &to_write, progress)?;

    // --- phase 1b: stage locally (same volume, so the phase-2 move is atomic) ---
    let mut staged: Vec<(&FileEntry, PathBuf)> = Vec::new();
    for (i, fe) in to_write.iter().enumerate() {
        // staged name is positional — asset names may repeat across dests
        let sp = staging.join(format!("s{i}"));
        std::fs::copy(cache.join(&fe.sha256), &sp).with_context(|| format!("staging {}", fe.dest))?;
        staged.push((fe, sp));
    }

    // --- phase 2: commit, with rollback on any failure ---
    let ctx = Ctx {
        game_dir: game_dir.clone(),
        backup_root: game_dir.join(BACKUP_DIR).join(&manifest.version),
        vanilla_root: game_dir.join(VANILLA_DIR),
        prev_dests,
        prev_winmm_created,
    };
    let job = CommitJob { staged: &staged, removals: &removals, resolved: &resolved, manifest: &manifest };
    let mut committed: Vec<Committed> = Vec::new();

    match commit(&ctx, &job, &mut committed) {
        Ok((written, removed, winmm_orig)) => {
            let _ = std::fs::remove_dir_all(&staging);
            // caching the remaining assets (unselected variants, disabled toggles) is NOT done
            // here — it can be hundreds of MB of optional content and must not hold the install
            // result hostage. The shell runs warm_cache detached after this returns.
            Ok(InstallReport {
                version: manifest.version.clone(),
                written,
                removed,
                up_to_date,
                winmm_orig,
            })
        }
        Err(e) => {
            rollback(&committed);
            let _ = std::fs::remove_dir_all(&staging);
            Err(e.context("install failed and was rolled back"))
        }
    }
}

fn commit(
    ctx: &Ctx,
    job: &CommitJob,
    committed: &mut Vec<Committed>,
) -> Result<(Vec<String>, Vec<String>, WinmmOrig)> {
    let mut written = Vec::new();

    for (fe, sp) in job.staged {
        let target = ctx.game_dir.join(&fe.dest);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let backup = if target.exists() {
            Some(back_up(ctx, &fe.dest, &target)?)
        } else {
            None
        };
        std::fs::rename(sp, &target).with_context(|| format!("installing {}", fe.dest))?;
        committed.push(Committed::Placed { target, backup });
        written.push(fe.dest.clone());
    }

    let winmm_orig = ensure_winmm_orig(&ctx.game_dir, has_winmm(job.resolved), committed)?;

    // removals: back the file up (ours -> ephemeral, foreign -> vanilla store), and if a vanilla
    // original was preserved for the dest, put it back so the game returns to stock there
    let mut removed = Vec::new();
    for dest in job.removals {
        let target = ctx.game_dir.join(dest);
        if target.exists() {
            let backup = back_up(ctx, dest, &target)?;
            committed.push(Committed::Removed { target: target.clone(), backup });
            removed.push(dest.clone());
        }
        let vanilla = ctx.vanilla_root.join(dest);
        if vanilla.exists() {
            if let Some(p) = target.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::rename(&vanilla, &target)
                .with_context(|| format!("restoring vanilla {dest}"))?;
            committed.push(Committed::VanillaRestored { target, vanilla });
        }
    }

    let winmm_orig_created = ctx.prev_winmm_created || matches!(winmm_orig, WinmmOrig::Created);
    write_state(&ctx.game_dir, job.manifest, job.resolved, winmm_orig_created)?;

    Ok((written, removed, winmm_orig))
}

/// Record the install: version + the resolved (effective) set (selected variants and enabled
/// toggles included) + the winmm_orig lineage.
fn write_state(
    game_dir: &Path,
    manifest: &Manifest,
    resolved: &[FileEntry],
    winmm_orig_created: bool,
) -> Result<()> {
    let state = InstalledState {
        version: manifest.version.clone(),
        files: resolved
            .iter()
            .map(|f| InstalledFile { dest: f.dest.clone(), sha256: f.sha256.clone() })
            .collect(),
        winmm_orig_created,
    };
    state.save(game_dir).context("writing install state")
}

/// Does the effective file set manage a winmm.dll (at any dest)?
fn has_winmm(resolved: &[FileEntry]) -> bool {
    resolved.iter().any(|fe| {
        Path::new(&fe.dest)
            .file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case("winmm.dll"))
    })
}

/// Fail fast when the game is running: it holds managed files open (loaded DLLs, mmapped VPKs),
/// so phase 2 would only roll back anyway — say "close the game" before downloading a byte.
fn probe_writable<'a>(game_dir: &Path, dests: impl Iterator<Item = &'a String>) -> Result<()> {
    for dest in dests {
        let target = game_dir.join(dest);
        if fslock::locked(&target) {
            return Err(anyhow!(engine::GameRunning(target)));
        }
    }
    Ok(())
}

// ---- asset cache ----

/// How many files phase 1 fetches at once. Files are independent (content-addressed cache
/// entries), so this is embarrassingly parallel; 4 keeps a slow link busy without hammering it.
const DL_WORKERS: usize = 4;
/// Byte-progress ticks are throttled to this granularity, so a fast link doesn't flood the UI
/// with an event per 64 KiB chunk.
const PROGRESS_GRAIN: u64 = 256 * 1024;

/// Fetch every to-write file into the asset cache with a small worker pool. Errors fail the
/// phase: the first error wins (remaining workers stop early; in-flight downloads finish — the
/// .part they leave is resumed on the next run). Progress `current` counts COMPLETED files
/// while `item`/`bytes` track whichever file ticked most recently.
fn obtain_all(
    cache: &Path,
    dl: &dyn Downloader,
    release: &Release,
    to_write: &[&FileEntry],
    progress: engine::Progress,
) -> Result<()> {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Mutex;

    // unique by sha256: two dests sharing one asset download once (the cache entry is shared)
    let mut seen = HashSet::new();
    let jobs: Vec<&FileEntry> =
        to_write.iter().filter(|fe| seen.insert(fe.sha256.as_str())).copied().collect();
    let total = jobs.len() as u64;
    // every dest that shares a job's hash — ticks fan out to all of them, so each UI file row
    // gets its bar even when two dests share one asset
    let mut dests_of: HashMap<&str, Vec<&str>> = HashMap::new();
    for fe in to_write {
        dests_of.entry(fe.sha256.as_str()).or_default().push(fe.dest.as_str());
    }

    let next = AtomicUsize::new(0);
    let done = AtomicU64::new(0);
    let first_err: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    let sink = progress.map(Mutex::new);
    let report = |p: engine::OpProgress| {
        if let Some(m) = &sink {
            (m.lock().unwrap())(p);
        }
    };

    std::thread::scope(|s| {
        for _ in 0..DL_WORKERS.min(jobs.len()) {
            s.spawn(|| loop {
                if first_err.lock().unwrap().is_some() {
                    return;
                }
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= jobs.len() {
                    return;
                }
                let fe = jobs[i];
                let dests = &dests_of[fe.sha256.as_str()];
                // size is known from the manifest, so a file's bar has its full extent from the
                // very first tick — even before the transport reports Content-Length.
                let size = fe.size;
                let tick = |current: u64, bytes_done: u64, bytes_total: u64, is_done: bool| {
                    for dest in dests {
                        report(engine::OpProgress {
                            op: "install",
                            current,
                            total,
                            item: Some((*dest).to_string()),
                            bytes_done: Some(bytes_done),
                            bytes_total: Some(bytes_total),
                            done: is_done,
                        });
                    }
                };
                tick(done.load(Ordering::Relaxed), 0, size, false);
                let mut last = 0u64;
                let mut chunk = |d: u64, t: Option<u64>| {
                    if d - last >= PROGRESS_GRAIN || t == Some(d) {
                        last = d;
                        tick(done.load(Ordering::Relaxed), d, t.unwrap_or(size), false);
                    }
                };
                match obtain_to_cache(cache, dl, release, &fe.name, &fe.sha256, fe.size, &mut chunk) {
                    Ok(_) => {
                        let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                        tick(d, size, size, true);
                    }
                    Err(e) => {
                        let mut g = first_err.lock().unwrap();
                        if g.is_none() {
                            *g = Some(e);
                        }
                        return;
                    }
                }
            });
        }
    });
    if let Some(e) = first_err.lock().unwrap().take() {
        return Err(e);
    }
    Ok(())
}

/// Every downloadable asset in the manifest — core files, all choice variants, all toggle files —
/// as (asset name, sha256, size).
fn all_assets(m: &Manifest) -> Vec<(&str, &str, u64)> {
    let mut v: Vec<(&str, &str, u64)> =
        m.files.iter().map(|f| (f.name.as_str(), f.sha256.as_str(), f.size)).collect();
    for o in &m.options {
        for var in &o.variants {
            v.push((var.name.as_str(), var.sha256.as_str(), var.size));
        }
        for f in &o.files {
            v.push((f.name.as_str(), f.sha256.as_str(), f.size));
        }
    }
    v
}

/// Does this cache entry exist and verify? A corrupt entry is deleted and treated as a miss.
/// Verification goes through the (size, mtime) hash memo, so a warm cache costs one stat.
fn cache_ok(cpath: &Path, sha256: &str, size: u64) -> bool {
    match std::fs::metadata(cpath) {
        Ok(md) if md.len() == size => {}
        Ok(_) => {
            let _ = std::fs::remove_file(cpath);
            return false;
        }
        Err(_) => return false,
    }
    match verify::sha256_file_cached(cpath) {
        Ok(h) if h == sha256 => true,
        _ => {
            let _ = std::fs::remove_file(cpath);
            false
        }
    }
}

/// Hashes with a download in flight, process-wide: an apply and the background cache warm share
/// the same `.part` path per hash — two concurrent writers would corrupt it.
static INFLIGHT: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

/// Holds a hash's in-flight slot; blocks until it is free. Waiting is right for both callers:
/// whoever got there first is downloading exactly the bytes the waiter wants — after the wait,
/// the cache re-check hits.
struct Inflight(String);

impl Inflight {
    fn acquire(sha256: &str) -> Self {
        loop {
            if INFLIGHT.lock().unwrap().insert(sha256.to_string()) {
                return Self(sha256.to_string());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}

impl Drop for Inflight {
    fn drop(&mut self) {
        INFLIGHT.lock().unwrap().remove(&self.0);
    }
}

/// Path to a verified cache entry for an asset: cache hit, else streaming download + verify.
fn obtain_to_cache(
    cache: &Path,
    dl: &dyn Downloader,
    release: &Release,
    name: &str,
    sha256: &str,
    size: u64,
    chunk: crate::downloader::ChunkProgress,
) -> Result<PathBuf> {
    let cpath = cache.join(sha256);
    if cache_ok(&cpath, sha256, size) {
        return Ok(cpath);
    }
    let _guard = Inflight::acquire(sha256);
    // another thread may have finished this hash while we waited for the slot
    if cache_ok(&cpath, sha256, size) {
        return Ok(cpath);
    }
    let asset = release
        .asset(name)
        .with_context(|| format!("the release has no asset named {name}"))?;
    let tmp = cache.join(format!("{sha256}.part"));
    // an interrupted attempt left a .part behind — resume from its length instead of restarting
    let resume_from = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
    let got = dl
        .download_to(asset, &tmp, resume_from, chunk)
        .with_context(|| format!("downloading {name}"));
    let (got_size, got_sha) = match got {
        Ok(v) => v,
        Err(e) => {
            // keep the .part — the next run resumes from it
            return Err(e);
        }
    };
    if got_size != size || got_sha != sha256 {
        // wrong bytes (corrupt source or a poisoned .part): resume can't help — start over
        let _ = std::fs::remove_file(&tmp);
        bail!("verification failed for {name}: manifest {size}b/{sha256} got {got_size}b/{got_sha}");
    }
    std::fs::rename(&tmp, &cpath).with_context(|| format!("caching {name}"))?;
    Ok(cpath)
}

/// Installed files already matching their manifest hash are verified byte sources — copy them into
/// the cache so switching a variant away and back never re-downloads.
fn seed_cache(
    cache: &Path,
    game_dir: &Path,
    resolved: &[FileEntry],
    statuses: &[engine::FileStatus],
) {
    for s in statuses.iter().filter(|s| s.action == engine::Action::UpToDate) {
        if let Some(fe) = resolved.iter().find(|f| f.dest == s.dest) {
            let cpath = cache.join(&fe.sha256);
            if !cpath.exists() {
                let _ = std::fs::copy(game_dir.join(&fe.dest), cpath);
            }
        }
    }
}

/// Set via `cancel_warm` so a background `warm_cache` in flight stops instead of recreating the
/// cache dir an uninstall just deleted (checked between assets — one in-flight asset may still
/// land; harmless residue at worst). Cleared by `install` (a (re)install legitimizes warming
/// again).
static WARM_CANCEL: AtomicBool = AtomicBool::new(false);

/// Stop a background `warm_cache`. Callers that run `uninstall` while a warm may be in flight
/// (the GUI shell) call this first; the engine's `uninstall` itself stays flag-free so headless
/// runs and tests are unaffected by process-global state.
pub fn cancel_warm() {
    WARM_CANCEL.store(true, Ordering::Relaxed);
}

/// Warm the asset cache: download every manifest asset not yet cached — unselected variants,
/// disabled toggles — so flipping customization later never waits on the network, then prune
/// entries the manifest no longer references. Fetches the release itself (one API round trip)
/// so the shell can run it DETACHED after `install` returns — optional content, possibly
/// hundreds of MB, must never hold the install result hostage. Entirely best-effort: any
/// failure just means on-demand download later.
pub fn warm_cache(settings: &Settings, dl: &dyn Downloader) {
    let Ok(game_dir) = settings.resolve_game_dir() else { return };
    let Ok((release, manifest)) = engine::fetch(settings, dl, None) else { return };
    if WARM_CANCEL.load(Ordering::Relaxed) {
        return;
    }
    let cache = game_dir.join(CACHE_DIR);
    if std::fs::create_dir_all(&cache).is_err() {
        return;
    }
    prefetch_all(&cache, dl, &release, &manifest);
    if !WARM_CANCEL.load(Ordering::Relaxed) {
        prune_cache(&cache, &manifest);
    }
}

/// Download every not-yet-cached manifest asset. Best-effort: a failed asset is skipped (it will
/// download on demand when actually selected) so an optional extra can't fail the warm.
fn prefetch_all(cache: &Path, dl: &dyn Downloader, release: &Release, manifest: &Manifest) {
    let mut seen = HashSet::new();
    for (name, sha256, size) in all_assets(manifest) {
        if WARM_CANCEL.load(Ordering::Relaxed) {
            return;
        }
        // cache_ok (not a bare exists) so a corrupt entry is evicted and re-downloaded here
        // instead of blocking the prefetch until the asset is actually selected
        if !seen.insert(sha256) || cache_ok(&cache.join(sha256), sha256, size) {
            continue;
        }
        let _ = obtain_to_cache(cache, dl, release, name, sha256, size, &mut |_, _| {});
    }
}

/// Drop cache entries the current manifest no longer references (stale hashes). A referenced
/// asset's leftover `.part` is KEPT — it's the resume source for an interrupted download.
fn prune_cache(cache: &Path, manifest: &Manifest) {
    let keep: HashSet<&str> = all_assets(manifest).into_iter().map(|(_, sha, _)| sha).collect();
    if let Ok(rd) = std::fs::read_dir(cache) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let base = name.strip_suffix(".part").unwrap_or(&name);
            if !keep.contains(base) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// Move an existing `target` aside and return where it went. Ours -> ephemeral rollback backup;
/// a genuine pre-existing file -> the permanent vanilla store (kept only the first time).
fn back_up(ctx: &Ctx, dest: &str, target: &Path) -> Result<PathBuf> {
    let ours = ctx.prev_dests.contains(dest);
    let vanilla = ctx.vanilla_root.join(dest);
    let to = if !ours && !vanilla.exists() {
        vanilla
    } else {
        ctx.backup_root.join(dest)
    };
    if let Some(p) = to.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::rename(target, &to).with_context(|| format!("backing up {dest}"))?;
    Ok(to)
}

/// If a winmm.dll is managed and winmm_orig.dll is absent, create it by COPYING the system
/// winmm.dll. Never overwrite an existing winmm_orig.dll — overwriting it with our proxy would
/// make the proxy's forwarders point at themselves. Decided from the resolved set (not just the
/// files written this run) so a no-op or partial install still heals a deleted winmm_orig.
fn ensure_winmm_orig(
    game_dir: &Path,
    winmm_managed: bool,
    committed: &mut Vec<Committed>,
) -> Result<WinmmOrig> {
    if !winmm_managed {
        return Ok(WinmmOrig::NotNeeded);
    }

    let orig = game_dir.join(WINMM_ORIG);
    if orig.exists() {
        return Ok(WinmmOrig::Existed);
    }
    let sysroot = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
    let src = Path::new(&sysroot).join("System32").join("winmm.dll");
    if let Some(p) = orig.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::copy(&src, &orig)
        .with_context(|| format!("copying {} -> winmm_orig.dll", src.display()))?;
    committed.push(Committed::OrigCreated { path: orig });
    Ok(WinmmOrig::Created)
}

fn rollback(committed: &[Committed]) {
    for op in committed.iter().rev() {
        match op {
            Committed::Placed { target, backup } => {
                let _ = std::fs::remove_file(target);
                if let Some(b) = backup {
                    let _ = std::fs::rename(b, target);
                }
            }
            Committed::Removed { target, backup } => {
                let _ = std::fs::rename(backup, target);
            }
            Committed::VanillaRestored { target, vanilla } => {
                let _ = std::fs::rename(target, vanilla);
            }
            Committed::OrigCreated { path } => {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// Revert the game to stock: for each managed file restore its preserved vanilla original if one was
/// kept, else delete it; delete winmm_orig.dll only if we created it; then remove our own scratch
/// dirs and the state file. Game dirs (scripts/, cfg/, bin/win64/) are left alone.
pub fn uninstall(settings: &Settings) -> Result<UninstallReport> {
    let game_dir = settings.resolve_game_dir()?;
    let state = InstalledState::load(&game_dir)
        .context("nothing to uninstall (no .phoenix-state.json in the game folder)")?;

    // same interlock as install — without it a locked file fails the delete loop halfway and
    // leaves a half-reverted install (the game holds winmm_orig.dll open too, via the proxy)
    let winmm: Vec<String> =
        if state.winmm_orig_created { vec![WINMM_ORIG.to_string()] } else { Vec::new() };
    probe_writable(&game_dir, state.files.iter().map(|f| &f.dest).chain(winmm.iter()))?;

    let vanilla_root = game_dir.join(VANILLA_DIR);
    let mut restored = Vec::new();
    let mut deleted = Vec::new();

    for f in &state.files {
        let target = game_dir.join(&f.dest);
        let vanilla = vanilla_root.join(&f.dest);
        if vanilla.exists() {
            if let Some(p) = target.parent() {
                std::fs::create_dir_all(p)?;
            }
            let _ = std::fs::remove_file(&target);
            std::fs::rename(&vanilla, &target)
                .with_context(|| format!("restoring vanilla {}", f.dest))?;
            restored.push(f.dest.clone());
        } else if target.exists() {
            std::fs::remove_file(&target).with_context(|| format!("deleting {}", f.dest))?;
            deleted.push(f.dest.clone());
        }
    }

    let mut winmm_orig_removed = false;
    if state.winmm_orig_created {
        let orig = game_dir.join(WINMM_ORIG);
        if orig.exists() {
            std::fs::remove_file(&orig).context("deleting winmm_orig.dll")?;
            winmm_orig_removed = true;
        }
    }

    let _ = std::fs::remove_dir_all(game_dir.join(BACKUP_DIR));
    let _ = std::fs::remove_dir_all(game_dir.join(STAGING_DIR));
    let _ = std::fs::remove_dir_all(game_dir.join(CACHE_DIR));
    let _ = std::fs::remove_dir_all(&vanilla_root);
    let _ = std::fs::remove_file(InstalledState::path(&game_dir));

    Ok(UninstallReport { version: state.version, restored, deleted, winmm_orig_removed })
}

#[cfg(test)]
mod tests {
    //! Golden-path install state-machine tests against temp dirs, served by the in-memory
    //! downloader fake — no network, no real game folder.
    use super::*;
    use crate::downloader::fake::Fake;
    use sha2::Digest;

    fn sha(b: &[u8]) -> String {
        hex::encode(sha2::Sha256::digest(b))
    }

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("phoenix-install-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn settings(dir: &Path) -> Settings {
        Settings { game_dir: Some(dir.to_path_buf()), ..Default::default() }
    }

    fn file_json(name: &str, dest: &str, bytes: &[u8]) -> serde_json::Value {
        serde_json::json!({ "name": name, "dest": dest, "sha256": sha(bytes), "size": bytes.len() })
    }

    /// The canonical test release: a winmm.dll + one content file.
    fn basic_release() -> (String, Vec<(&'static str, &'static [u8])>) {
        let m = serde_json::json!({
            "version": "1.0.0",
            "files": [
                file_json("winmm.dll", "game/bin/win64/winmm.dll", b"dll"),
                file_json("a.vpk", "game/dota/a.vpk", b"vpk"),
            ]
        })
        .to_string();
        (m, vec![("winmm.dll", b"dll"), ("a.vpk", b"vpk")])
    }

    #[test]
    fn fresh_install_writes_files_state_and_winmm_orig() {
        let dir = tempdir("fresh");
        let (m, assets) = basic_release();
        let dl = Fake::new("v1.0.0", &m, assets);
        let r = install(&settings(&dir), &dl, None, None).unwrap();

        assert_eq!(r.written.len(), 2);
        assert_eq!(r.winmm_orig, WinmmOrig::Created);
        assert_eq!(std::fs::read(dir.join("game/bin/win64/winmm.dll")).unwrap(), b"dll");
        assert_eq!(std::fs::read(dir.join("game/dota/a.vpk")).unwrap(), b"vpk");
        assert!(dir.join(WINMM_ORIG).exists());
        let st = InstalledState::load(&dir).unwrap();
        assert_eq!(st.version, "1.0.0");
        assert_eq!(st.files.len(), 2);
        assert!(st.winmm_orig_created);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn noop_install_heals_a_missing_state_file() {
        let dir = tempdir("heal");
        let (m, assets) = basic_release();
        let dl = Fake::new("v1.0.0", &m, assets);
        install(&settings(&dir), &dl, None, None).unwrap();
        // lose the state file — the folder is now "up to date but not installed"
        std::fs::remove_file(InstalledState::path(&dir)).unwrap();
        assert!(InstalledState::load(&dir).is_none());

        let r = install(&settings(&dir), &dl, None, None).unwrap();
        assert!(r.written.is_empty() && r.removed.is_empty());
        assert_eq!(r.up_to_date, 2);
        // healed: state rewritten. winmm_orig already existed, and with the state lost we can no
        // longer prove WE created it — so the lineage conservatively records false: a later
        // uninstall leaves it in place rather than risk deleting a user's own winmm_orig.dll.
        let st = InstalledState::load(&dir).unwrap();
        assert_eq!(st.files.len(), 2);
        assert!(!st.winmm_orig_created);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deselected_toggle_file_is_removed_on_next_install() {
        let dir = tempdir("orphan");
        let m = serde_json::json!({
            "version": "1.0.0",
            "files": [ file_json("a.vpk", "game/dota/a.vpk", b"vpk") ],
            "options": [
                { "id": "fx", "kind": "toggle", "label": "FX", "default": false,
                  "files": [ file_json("fx.vpk", "game/dota/fx.vpk", b"fx") ] }
            ]
        })
        .to_string();
        let dl = Fake::new("v1.0.0", &m, vec![("a.vpk", b"vpk"), ("fx.vpk", b"fx")]);

        let mut s = settings(&dir);
        s.selections.insert("fx".into(), serde_json::json!(true));
        install(&s, &dl, None, None).unwrap();
        assert!(dir.join("game/dota/fx.vpk").exists());

        s.selections.insert("fx".into(), serde_json::json!(false));
        let r = install(&s, &dl, None, None).unwrap();
        assert_eq!(r.removed, vec!["game/dota/fx.vpk".to_string()]);
        assert!(!dir.join("game/dota/fx.vpk").exists());
        let st = InstalledState::load(&dir).unwrap();
        assert!(!st.files.iter().any(|f| f.dest == "game/dota/fx.vpk"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn warm_cache_prefetches_unselected_assets() {
        let dir = tempdir("warm");
        let m = serde_json::json!({
            "version": "1.0.0",
            "files": [ file_json("a.vpk", "game/dota/a.vpk", b"vpk") ],
            "options": [
                { "id": "fx", "kind": "toggle", "label": "FX", "default": false,
                  "files": [ file_json("fx.vpk", "game/dota/fx.vpk", b"fx") ] }
            ]
        })
        .to_string();
        let dl = Fake::new("v1.0.0", &m, vec![("a.vpk", b"vpk"), ("fx.vpk", b"fx")]);
        let s = settings(&dir);
        install(&s, &dl, None, None).unwrap();

        // install itself no longer prefetches the disabled toggle's asset...
        assert!(!dir.join(CACHE_DIR).join(sha(b"fx")).exists());
        // ...the detached warm does
        warm_cache(&s, &dl);
        assert!(dir.join(CACHE_DIR).join(sha(b"fx")).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn uninstall_reverts_to_stock() {
        let dir = tempdir("uninstall");
        let (m, assets) = basic_release();
        let dl = Fake::new("v1.0.0", &m, assets);
        install(&settings(&dir), &dl, None, None).unwrap();

        let r = uninstall(&settings(&dir)).unwrap();
        assert_eq!(r.deleted.len(), 2);
        assert!(r.winmm_orig_removed);
        assert!(!dir.join("game/bin/win64/winmm.dll").exists());
        assert!(!dir.join("game/dota/a.vpk").exists());
        assert!(!dir.join(WINMM_ORIG).exists());
        assert!(InstalledState::load(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_download_fails_and_touches_nothing() {
        let dir = tempdir("corrupt");
        // manifest claims a hash the served bytes don't have
        let m = serde_json::json!({
            "version": "1.0.0",
            "files": [ { "name": "a.vpk", "dest": "game/dota/a.vpk",
                         "sha256": "ff".repeat(32), "size": 3 } ]
        })
        .to_string();
        let dl = Fake::new("v1.0.0", &m, vec![("a.vpk", b"vpk")]);

        assert!(install(&settings(&dir), &dl, None, None).is_err());
        assert!(!dir.join("game/dota/a.vpk").exists());
        assert!(InstalledState::load(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn many_files_install_through_the_worker_pool() {
        let dir = tempdir("pool");
        let names = ["f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8"];
        let files: Vec<serde_json::Value> = names
            .iter()
            .map(|n| file_json(&format!("{n}.vpk"), &format!("game/dota/{n}.vpk"), n.as_bytes()))
            .collect();
        let m = serde_json::json!({ "version": "1.0.0", "files": files }).to_string();
        let owned: Vec<(String, Vec<u8>)> =
            names.iter().map(|n| (format!("{n}.vpk"), n.as_bytes().to_vec())).collect();
        let assets: Vec<(&str, &[u8])> =
            owned.iter().map(|(n, b)| (n.as_str(), b.as_slice())).collect();
        let dl = Fake::new("v1.0.0", &m, assets);

        let r = install(&settings(&dir), &dl, None, None).unwrap();
        assert_eq!(r.written.len(), 8);
        for n in names {
            assert_eq!(std::fs::read(dir.join(format!("game/dota/{n}.vpk"))).unwrap(), n.as_bytes());
        }
        assert_eq!(InstalledState::load(&dir).unwrap().files.len(), 8);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A downloader whose first download_to dies mid-stream; the next call must resume from the
    /// .part it left behind (asserted inside).
    struct CutOnce {
        inner: Fake,
        cut: usize,
        failed: std::sync::atomic::AtomicBool,
    }

    impl crate::downloader::Downloader for CutOnce {
        fn fetch_release(&self, repo: &str, tag: Option<&str>) -> Result<crate::downloader::Release> {
            self.inner.fetch_release(repo, tag)
        }
        fn fetch_releases(&self, repo: &str) -> Result<Vec<crate::downloader::Release>> {
            self.inner.fetch_releases(repo)
        }
        fn download(&self, asset: &crate::downloader::Asset) -> Result<Vec<u8>> {
            self.inner.download(asset)
        }
        fn download_to(
            &self,
            asset: &crate::downloader::Asset,
            dest: &Path,
            resume_from: u64,
            _progress: crate::downloader::ChunkProgress,
        ) -> Result<(u64, String)> {
            use std::sync::atomic::Ordering;
            let bytes = self.download(asset)?;
            if !self.failed.swap(true, Ordering::SeqCst) {
                assert_eq!(resume_from, 0, "first attempt must start fresh");
                std::fs::write(dest, &bytes[..self.cut])?;
                anyhow::bail!("simulated dropped connection");
            }
            // the engine must resume from exactly what the interrupted attempt wrote
            assert_eq!(resume_from as usize, self.cut);
            let mut out = std::fs::read(dest)?;
            out.extend_from_slice(&bytes[out.len()..]);
            std::fs::write(dest, &out)?;
            Ok((out.len() as u64, sha(&out)))
        }
    }

    #[test]
    fn an_interrupted_download_resumes_instead_of_restarting() {
        let dir = tempdir("resume");
        let big: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let m = serde_json::json!({
            "version": "1.0.0",
            "files": [ file_json("big.vpk", "game/dota/big.vpk", &big) ]
        })
        .to_string();
        let dl = CutOnce {
            inner: Fake::new("v1.0.0", &m, vec![("big.vpk", &big)]),
            cut: 40_000,
            failed: false.into(),
        };

        // first run: dies mid-download, touches nothing but the resumable .part
        assert!(install(&settings(&dir), &dl, None, None).is_err());
        assert!(!dir.join("game/dota/big.vpk").exists());
        assert!(dir.join(CACHE_DIR).join(format!("{}.part", sha(&big))).exists());

        // second run: resumes the .part (asserted inside CutOnce) and completes
        let r = install(&settings(&dir), &dl, None, None).unwrap();
        assert_eq!(r.written, vec!["game/dota/big.vpk".to_string()]);
        assert_eq!(std::fs::read(dir.join("game/dota/big.vpk")).unwrap(), big);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_locked_target_fails_with_game_running_before_downloading() {
        let dir = tempdir("locked");
        let (m, assets) = basic_release();
        let dl = Fake::new("v1.0.0", &m, assets);
        install(&settings(&dir), &dl, None, None).unwrap();

        // simulate the game holding winmm.dll open: no sharing allowed on our handle
        use std::os::windows::fs::OpenOptionsExt;
        let mut lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(dir.join("game/bin/win64/winmm.dll"))
            .unwrap();

        // a new release changes winmm.dll -> it becomes a write target -> the probe must refuse
        let m2 = serde_json::json!({
            "version": "1.0.1",
            "files": [
                file_json("winmm.dll", "game/bin/win64/winmm.dll", b"dll2"),
                file_json("a.vpk", "game/dota/a.vpk", b"vpk"),
            ]
        })
        .to_string();
        let dl2 = Fake::new("v1.0.1", &m2, vec![("winmm.dll", b"dll2"), ("a.vpk", b"vpk")]);
        let err = install(&settings(&dir), &dl2, None, None).unwrap_err();
        assert!(
            err.chain().any(|c| c.downcast_ref::<engine::GameRunning>().is_some()),
            "expected GameRunning in the error chain, got: {err:#}"
        );
        // and nothing was replaced while the "game" held the file — verified through the lock
        // handle itself (a second open would violate our own share(0))
        use std::io::Read as _;
        let mut buf = Vec::new();
        lock.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"dll");

        drop(lock);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
