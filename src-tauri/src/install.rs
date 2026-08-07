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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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
/// The base-game pipeline's cache, nested inside `CACHE_DIR` so it shares the volume but not the
/// namespace — the shim's prune and uninstall must never reach a 16 GB game download.
const BASE_CACHE_SUBDIR: &str = "base";
const WINMM_ORIG: &str = "game/bin/win64/winmm_orig.dll";
/// sha256 of zero bytes — the only hash an empty file can have (see `obtain_to_cache`).
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

#[derive(Debug, PartialEq, Eq)]
pub enum WinmmOrig {
    Created,
    Existed,
    NotNeeded,
}

#[derive(Debug)]
pub struct InstallReport {
    pub version: String,
    /// The release tag this install came from.
    pub tag: String,
    pub written: Vec<String>,
    pub removed: Vec<String>,
    pub up_to_date: usize,
    pub winmm_orig: WinmmOrig,
    /// The manifest this install applied — the freshest one there is (install fetches its own).
    /// The shell uses it to refresh the replan cache, so a release published between check and
    /// apply can't leave the UI re-diffing against a stale manifest.
    pub manifest: Manifest,
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
    /// Whether `prev_dests` can be believed. False when the state file is gone but the folder
    /// still shows a prior install — then nothing may be promoted to the vanilla store, because
    /// a wrong promotion makes uninstall restore our own shim as "stock".
    trust_prev: bool,
    /// Whether the updater lineage already created winmm_orig.dll.
    prev_winmm_created: bool,
    /// Dests where an earlier removal restored a preserved vanilla original (state.restored) —
    /// carried into the new state so `plan` keeps treating those files as stock, not ours.
    prev_restored: Vec<String>,
}

/// Everything phase 2 places, removes and records.
struct CommitJob<'a> {
    staged: &'a [(&'a FileEntry, PathBuf)],
    removals: &'a [String],
    resolved: &'a [FileEntry],
    manifest: &'a Manifest,
}

/// `cancel` aborts the download phase (phase 1) between chunks. Phase 2 is NOT cancellable by
/// design: a commit is the part that must either complete or roll back.
pub fn install(
    settings: &Settings,
    dl: &dyn Downloader,
    tag: Option<&str>,
    progress: engine::Progress,
    cancel: Option<&AtomicBool>,
) -> Result<InstallReport> {
    let game_dir = settings.resolve_game_dir()?;
    let (release, manifest) = engine::fetch(settings, dl, tag)?;

    // Prior state distinguishes our files from genuine pre-existing ones, and remembers whether we
    // already created winmm_orig.dll.
    let prev = InstalledState::load(&game_dir);
    let prev_dests: HashSet<String> = prev
        .as_ref()
        .map(|s| s.files.iter().map(|f| f.dest.clone()).collect())
        .unwrap_or_default();
    let prev_winmm_created = prev.as_ref().map(|s| s.winmm_orig_created).unwrap_or(false);
    let prev_restored: Vec<String> =
        prev.as_ref().map(|s| s.restored.clone()).unwrap_or_default();
    // No state file + evidence of a prior install (we created winmm_orig.dll, or a vanilla store
    // exists) = `prev_dests` is empty but WRONG. See `back_up`.
    let trust_prev = prev.is_some()
        || (!game_dir.join(WINMM_ORIG).exists() && !game_dir.join(VANILLA_DIR).exists());

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
                // carry the restored-original record (minus any dest the manifest ships again —
                // that file will be displaced normally next time and the record no longer applies)
                let restored: Vec<String> = prev_restored
                    .iter()
                    .filter(|d| !resolved.iter().any(|f| &f.dest == *d))
                    .cloned()
                    .collect();
                write_state(&game_dir, &manifest, &resolved, created, restored)
                    .map(|_| winmm_orig)
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
            tag: release.tag_name.clone(),
            written: Vec::new(),
            removed: Vec::new(),
            up_to_date,
            winmm_orig,
            manifest,
        });
    }

    // --- interlock: fail fast when the game is running (phase 2 would roll back anyway) ---
    probe_writable(&game_dir, to_write.iter().map(|fe| &fe.dest).chain(removals.iter()))?;

    // --- phase 1a: obtain (cache-first, else parallel streaming download) — game untouched ---
    let staging = game_dir.join(STAGING_DIR);
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).context("creating the staging directory")?;
    obtain_all(&cache, dl, &release, &to_write, progress, cancel)?;

    // --- phase 1b: stage locally (same volume, so the phase-2 move is atomic) ---
    let mut staged: Vec<(&FileEntry, PathBuf)> = Vec::new();
    for (i, fe) in to_write.iter().enumerate() {
        // staged name is positional — asset names may repeat across dests
        let sp = staging.join(format!("s{i}"));
        std::fs::copy(cache.join(&fe.sha256), &sp).with_context(|| format!("staging {}", fe.dest))?;
        staged.push((fe, sp));
    }

    // --- re-probe: the game may have started during a long phase 1 — fail typed and untouched
    // here rather than mid-commit into a best-effort rollback against locked files ---
    if let Err(e) = probe_writable(&game_dir, to_write.iter().map(|fe| &fe.dest).chain(removals.iter()))
    {
        // this is the likeliest failure of the whole run (the user started the game during a long
        // download), and it must not leave a full copy of the payload in the game folder
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }

    // --- phase 2: commit, with rollback on any failure ---
    let ctx = Ctx {
        game_dir: game_dir.clone(),
        backup_root: game_dir.join(BACKUP_DIR).join(&manifest.version),
        vanilla_root: game_dir.join(VANILLA_DIR),
        prev_dests,
        trust_prev,
        prev_winmm_created,
        prev_restored,
    };
    let job = CommitJob { staged: &staged, removals: &removals, resolved: &resolved, manifest: &manifest };
    let mut committed: Vec<Committed> = Vec::new();

    match commit(&ctx, &job, &mut committed) {
        Ok((written, removed, winmm_orig)) => {
            let _ = std::fs::remove_dir_all(&staging);
            // The commit succeeded, so nothing will ever roll back to these — they exist only as
            // rollback material for the run that just finished. Left behind they accumulated one
            // full copy of every replaced file PER RELEASE, forever, inside the game folder.
            // (Preserved vanilla originals live in VANILLA_DIR and are untouched by this.)
            let _ = std::fs::remove_dir_all(game_dir.join(BACKUP_DIR));
            // caching the remaining assets (unselected variants, disabled toggles) is NOT done
            // here — it can be hundreds of MB of optional content and must not hold the install
            // result hostage. The shell runs warm_cache detached after this returns.
            Ok(InstallReport {
                version: manifest.version.clone(),
                tag: release.tag_name.clone(),
                written,
                removed,
                up_to_date,
                winmm_orig,
                manifest,
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
        // Recorded BEFORE the fallible rename, not after: back_up has already MOVED the original
        // out of the game folder, so from here on the only record of where it went is this entry.
        // Pushing it after the rename meant a failed rename (an AV scanner holding the staged file
        // is the everyday cause) rolled back a list that did not mention the backup — and the
        // game was left with no file at that dest at all. Rollback tolerates a target that was
        // never placed: its remove_file is best-effort and the restore is what matters.
        committed.push(Committed::Placed { target: target.clone(), backup });
        std::fs::rename(sp, &target).with_context(|| format!("installing {}", fe.dest))?;
        written.push(fe.dest.clone());
    }

    let winmm_orig = ensure_winmm_orig(&ctx.game_dir, has_winmm(job.resolved), committed)?;

    // removals: back the file up (ours -> ephemeral, foreign -> vanilla store), and if a vanilla
    // original was preserved for the dest, put it back so the game returns to stock there
    let mut removed = Vec::new();
    // The restored-original record for the NEW state: prior entries carried forward (minus dests
    // the manifest ships or removes again — either way the file there stops being the restored
    // original), plus every restore this run performs. Without the record, `plan` re-flags the
    // restored stock file as Remove forever (see state.rs).
    let managed: HashSet<&str> = job.resolved.iter().map(|f| f.dest.as_str()).collect();
    let mut restored_dests: Vec<String> = ctx
        .prev_restored
        .iter()
        .filter(|d| !managed.contains(d.as_str()) && !job.removals.contains(d))
        .cloned()
        .collect();
    for dest in job.removals {
        let target = ctx.game_dir.join(dest);
        let vanilla = ctx.vanilla_root.join(dest);
        // decided BEFORE back_up: back_up may itself preserve a foreign target into the vanilla
        // store, and restoring that same copy right back would undo the removal — the file would
        // then re-flag as Remove on every future plan, forever. Only a copy that predates this
        // removal is a genuine original to restore.
        let restore_vanilla = vanilla.exists();
        // What we displace decides whether the preserved original goes back. Removing OUR file
        // should leave the vanilla original in its place (that is what revert-to-stock means).
        // Removing a file we did not place, when a copy is ALREADY preserved, must not: that copy
        // is the original from an earlier removal, and renaming it over the current file would
        // undo this very removal — the dest would still be occupied while the report claims it
        // was removed, and the next plan would flag it again.
        let displaced_ours = if target.exists() {
            let ours = ctx.prev_dests.contains(dest);
            let backup = back_up(ctx, dest, &target)?;
            committed.push(Committed::Removed { target: target.clone(), backup });
            removed.push(dest.clone());
            ours
        } else {
            true // nothing to displace — a preserved original still belongs back at the dest
        };
        if restore_vanilla && displaced_ours {
            if let Some(p) = target.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::rename(&vanilla, &target)
                .with_context(|| format!("restoring vanilla {dest}"))?;
            committed.push(Committed::VanillaRestored { target, vanilla });
            restored_dests.push(dest.clone());
        }
    }

    let winmm_orig_created = ctx.prev_winmm_created || matches!(winmm_orig, WinmmOrig::Created);
    write_state(&ctx.game_dir, job.manifest, job.resolved, winmm_orig_created, restored_dests)?;

    Ok((written, removed, winmm_orig))
}

/// Record the install: version + the resolved (effective) set (selected variants and enabled
/// toggles included) + the winmm_orig lineage + the restored-original record (see state.rs).
fn write_state(
    game_dir: &Path,
    manifest: &Manifest,
    resolved: &[FileEntry],
    winmm_orig_created: bool,
    restored: Vec<String>,
) -> Result<()> {
    let state = InstalledState {
        version: manifest.version.clone(),
        files: resolved
            .iter()
            .map(|f| InstalledFile { dest: f.dest.clone(), sha256: f.sha256.clone() })
            .collect(),
        winmm_orig_created,
        restored,
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

/// Fail fast when a file we must touch can't be written — before downloading a byte. The
/// diagnosis matters: a sharing violation means the game holds it open (loaded DLLs, mmapped
/// VPKs) -> typed GameRunning ("close the game"); any other can't-write (read-only attribute,
/// ACL) is a permissions problem — saying "close Dota 2" for those would be advice that can
/// never work.
fn probe_writable<'a>(game_dir: &Path, dests: impl Iterator<Item = &'a String>) -> Result<()> {
    for dest in dests {
        let target = game_dir.join(dest);
        match fslock::probe(&target) {
            fslock::Probe::Writable => {}
            fslock::Probe::Held => return Err(anyhow!(engine::GameRunning(target))),
            fslock::Probe::Denied(e) => {
                return Err(anyhow::Error::new(e).context(format!(
                    "no write access to {} — clear its read-only attribute or run the launcher \
                     with sufficient permissions",
                    target.display()
                )));
            }
        }
    }
    Ok(())
}

// ---- asset cache ----

/// How many files phase 1 fetches at once. Files are independent (content-addressed cache
/// entries), so this is embarrassingly parallel. 8, not 4: the base game is thousands of TINY
/// files (a real run showed 1,290 done at 0.37 GB — ~290 KB each), and for those the cost is the
/// request round trip, not the bytes — per-file overhead divides by the worker count while the
/// few multi-GB VPKs stream at link speed regardless. Bounded by the per-host idle pool in
/// github.rs (POOL_PER_HOST) — raise that alongside this, or the workers churn reconnects.
const DL_WORKERS: usize = 8;
/// Byte-progress ticks are throttled to this granularity, so a fast link doesn't flood the UI
/// with an event per 64 KiB chunk.
const PROGRESS_GRAIN: u64 = 256 * 1024;
/// Same idea for the per-FILE ticks of a plan/verify pass: one event per file across 4,635 files
/// is a burst of pure overhead, and the counter only has to move visibly.
const PLAN_GRAIN: u64 = 16;

/// Fetch every to-write file into the asset cache with a small worker pool. Errors fail the
/// phase: the first error wins — remaining workers stop early AND in-flight streams abort at
/// their next chunk (the .part they leave is resumed on the next run), so a dead asset never
/// waits minutes for a 500 MB neighbor to finish before surfacing. Progress `current` counts
/// COMPLETED files while `item`/`bytes` track whichever file ticked most recently.
fn obtain_all(
    cache: &Path,
    dl: &dyn Downloader,
    release: &Release,
    to_write: &[&FileEntry],
    progress: engine::Progress,
    cancel: Option<&AtomicBool>,
) -> Result<()> {
    obtain_all_tagged(cache, dl, release, to_write, progress, "install", cancel)
}

/// `obtain_all` with the progress `op` tag and an external cancel flag injected — the base-game
/// path reports as its own operation ("game") and is user-cancellable mid-download (a shim
/// install is seconds; a 9 GB base install is not).
fn obtain_all_tagged(
    cache: &Path,
    dl: &dyn Downloader,
    release: &Release,
    to_write: &[&FileEntry],
    progress: engine::Progress,
    op: &'static str,
    cancel: Option<&AtomicBool>,
) -> Result<()> {
    // unique by sha256: two dests sharing one asset download once (the cache entry is shared)
    let mut seen = HashSet::new();
    let mut jobs: Vec<&FileEntry> =
        to_write.iter().filter(|fe| seen.insert(fe.sha256.as_str())).copied().collect();
    // Largest first (LPT scheduling): the multi-GB VPKs start streaming immediately and run for
    // most of the download while the other workers chew through the small-file tail — in manifest
    // (alphabetical) order, a giant file picked up near the end ran ALONE long after every other
    // worker went idle, adding its whole transfer time to the wall clock. Also steadies the UI:
    // the byte rate reaches link speed in the first seconds, so the ETA is honest early.
    jobs.sort_unstable_by_key(|fe| std::cmp::Reverse(fe.size));
    let total = jobs.len() as u64;
    // every dest that shares a job's hash — ticks fan out to all of them, so each UI file row
    // gets its bar even when two dests share one asset
    let mut dests_of: HashMap<&str, Vec<&str>> = HashMap::new();
    for fe in to_write {
        dests_of.entry(fe.sha256.as_str()).or_default().push(fe.dest.as_str());
    }

    // one lookup table for the whole pool: the base game's 4,635 jobs against a merged release's
    // 4,636 assets would otherwise be a linear scan each
    let index = release.asset_index();
    let next = AtomicUsize::new(0);
    let done = AtomicU64::new(0);
    let abort = AtomicBool::new(false); // cheap flag mirrored from first_err, checked per chunk
    let cancelled = || cancel.is_some_and(|c| c.load(Ordering::Relaxed));
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
                if abort.load(Ordering::Relaxed) || cancelled() {
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
                            op,
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
                    if abort.load(Ordering::Relaxed) || cancelled() {
                        return false; // a sibling failed or the user cancelled — stop this stream
                    }
                    if d - last >= PROGRESS_GRAIN || t == Some(d) {
                        last = d;
                        tick(done.load(Ordering::Relaxed), d, t.unwrap_or(size), false);
                    }
                    true
                };
                match obtain_to_cache(cache, dl, &index, &fe.name, &fe.sha256, fe.size, &mut chunk) {
                    Ok(_) => {
                        let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                        tick(d, size, size, true);
                    }
                    Err(e) => {
                        let mut g = first_err.lock().unwrap();
                        if g.is_none() {
                            *g = Some(e); // aborted streams land here too, but the real error won
                        }
                        abort.store(true, Ordering::Relaxed);
                        return;
                    }
                }
            });
        }
    });
    if cancelled() {
        // the user's cancel outranks whatever error the aborted streams produced — their .parts
        // stay behind as resume sources, and the UI closes quietly on the typed marker
        return Err(anyhow!(engine::Cancelled));
    }
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
    index: &HashMap<&str, &crate::downloader::Asset>,
    name: &str,
    sha256: &str,
    size: u64,
    chunk: crate::downloader::ChunkProgress,
) -> Result<PathBuf> {
    let cpath = cache.join(sha256);
    if cache_ok(&cpath, sha256, size) {
        return Ok(cpath);
    }
    // An EMPTY file has nothing to transfer and exactly one possible hash, so materialize it
    // instead of fetching. This is not an optimization: GitHub refuses to host a zero-byte
    // release asset (422), so the publisher cannot upload one even though the game genuinely
    // contains such files — the manifest lists them and the reader creates them. A size of 0
    // paired with any other hash is a corrupt manifest, not an empty file.
    if size == 0 {
        if sha256 != EMPTY_SHA256 {
            bail!("manifest lists {name} as 0 bytes but with sha256 {sha256}, not the empty hash");
        }
        std::fs::write(&cpath, b"").with_context(|| format!("creating empty {name}"))?;
        return Ok(cpath);
    }
    let _guard = Inflight::acquire(sha256);
    // another thread may have finished this hash while we waited for the slot
    if cache_ok(&cpath, sha256, size) {
        return Ok(cpath);
    }
    let asset = index
        .get(name)
        .copied()
        .with_context(|| format!("the release has no asset named {name}"))?;
    let tmp = cache.join(format!("{sha256}.part"));
    // an interrupted attempt left a .part behind — resume from its length instead of restarting
    let mut resume_from = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
    // ...but a .part that already reached the full length can never be resumed: the Range would
    // start at EOF and the CDN answers 416, which is an error, which keeps the .part — leaving
    // the asset permanently undownloadable. That state is reachable without anything exotic (a
    // completed transfer whose rename into the cache failed, or a cancel landing on the last
    // chunk of a file), so treat an over-long .part as poison and start clean.
    if resume_from >= size {
        let _ = std::fs::remove_file(&tmp);
        resume_from = 0;
    }
    let got = dl
        .download_to(asset, &tmp, resume_from, chunk)
        .with_context(|| format!("downloading {name}"));
    let (got_size, got_sha) = match got {
        Ok(v) => v,
        Err(e) => {
            // keep the .part — the next run resumes from it — unless it is now full-length or
            // longer, which no future Range request could extend
            if std::fs::metadata(&tmp).map(|m| m.len() >= size).unwrap_or(false) {
                let _ = std::fs::remove_file(&tmp);
            }
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

/// Bumped by `cancel_warm` so a background `warm_cache` in flight stops instead of recreating the
/// cache dir an uninstall just deleted (checked between assets AND per chunk mid-stream — a
/// leftover `.part` at worst).
///
/// An EPOCH, not a boolean. A boolean had to be cleared by whoever legitimized warming again
/// (`install`), and that clear could un-cancel a warm the previous uninstall had stopped — the
/// zombie then finished against a stale manifest and pruned the entries the new install had just
/// seeded. A warm captures the epoch when it starts and exits as soon as it moves; nothing ever
/// has to reset anything.
static WARM_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Stop a background `warm_cache`. Callers that run `uninstall` while a warm may be in flight
/// (the GUI shell) call this first; the engine's `uninstall` itself stays flag-free so headless
/// runs and tests are unaffected by process-global state.
pub fn cancel_warm() {
    WARM_EPOCH.fetch_add(1, Ordering::Relaxed);
}

/// Warm the asset cache: download every manifest asset not yet cached — unselected variants,
/// disabled toggles — so flipping customization later never waits on the network, then prune
/// entries the manifest no longer references. Fetches the release itself (one API round trip)
/// so the shell can run it DETACHED after `install` returns — optional content, possibly
/// hundreds of MB, must never hold the install result hostage. Entirely best-effort: any
/// failure just means on-demand download later.
pub fn warm_cache(settings: &Settings, dl: &dyn Downloader) {
    // captured before any work: every check below asks "has anyone cancelled since I started",
    // so a cancel can never be lost and a later install never resurrects this run
    let epoch = WARM_EPOCH.load(Ordering::Relaxed);
    let cancelled = || WARM_EPOCH.load(Ordering::Relaxed) != epoch;
    let Ok(game_dir) = settings.resolve_game_dir() else { return };
    let Ok((release, manifest)) = engine::fetch(settings, dl, None) else { return };
    if cancelled() {
        return;
    }
    let cache = game_dir.join(CACHE_DIR);
    if std::fs::create_dir_all(&cache).is_err() {
        return;
    }
    prefetch_all(&cache, dl, &release, &manifest, &cancelled);
    if !cancelled() {
        prune_cache(&cache, &manifest);
    }
}

/// Download every not-yet-cached manifest asset. Best-effort: a failed asset is skipped (it will
/// download on demand when actually selected) so an optional extra can't fail the warm.
fn prefetch_all(
    cache: &Path,
    dl: &dyn Downloader,
    release: &Release,
    manifest: &Manifest,
    cancelled: &dyn Fn() -> bool,
) {
    let index = release.asset_index();
    let mut seen = HashSet::new();
    for (name, sha256, size) in all_assets(manifest) {
        if cancelled() {
            return;
        }
        // cache_ok (not a bare exists) so a corrupt entry is evicted and re-downloaded here
        // instead of blocking the prefetch until the asset is actually selected
        if !seen.insert(sha256) || cache_ok(&cache.join(sha256), sha256, size) {
            continue;
        }
        // the chunk callback doubles as the cancel line: an uninstall's cancel_warm aborts the
        // stream mid-file instead of letting a huge optional asset finish downloading first
        let _ = obtain_to_cache(cache, dl, &index, name, sha256, size, &mut |_, _| !cancelled());
    }
}

/// Drop cache entries the current manifest no longer references (stale hashes). A referenced
/// asset's leftover `.part` is KEPT — it's the resume source for an interrupted download.
///
/// FILES ONLY, and only at the top level: `keep` is built from the SHIM manifest, while the
/// base-game pipeline caches under `CACHE_DIR/BASE_CACHE_SUBDIR`. Recursing (or deleting
/// directory entries) would make a background warm delete a half-finished 16 GB game download.
fn prune_cache(cache: &Path, manifest: &Manifest) {
    let keep: HashSet<&str> = all_assets(manifest).into_iter().map(|(_, sha, _)| sha).collect();
    if let Ok(rd) = std::fs::read_dir(cache) {
        for e in rd.flatten() {
            if !e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            let base = name.strip_suffix(".part").unwrap_or(&name);
            if !keep.contains(base) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// Delete the files directly inside `dir`, leaving subdirectories alone, then the directory itself
/// if that emptied it. Used to clear the shim's cache without touching the base game's.
fn clear_dir_files(dir: &Path) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let _ = std::fs::remove_dir(dir); // only succeeds once nothing is left
}

/// Move an existing `target` aside and return where it went. Ours -> ephemeral rollback backup;
/// a genuine pre-existing file -> the permanent vanilla store (kept only the first time).
fn back_up(ctx: &Ctx, dest: &str, target: &Path) -> Result<PathBuf> {
    let ours = ctx.prev_dests.contains(dest);
    let vanilla = ctx.vanilla_root.join(dest);
    // Promoting a file to the vanilla store is IRREVERSIBLE in effect: uninstall restores whatever
    // is in there as "stock". So it only happens when `prev_dests` is trustworthy. Without the
    // state file we cannot tell our own previously-installed files from genuine originals, and
    // guessing wrong preserves the Phoenix shim as vanilla — uninstall would then dutifully put
    // the shim back and report the game as stock. `trust_prev` is false exactly when the state is
    // missing AND the folder shows evidence of a prior install; the cost is an ephemeral backup
    // instead of a preserved original, which is the safe direction to be wrong in.
    let to = if ctx.trust_prev && !ours && !vanilla.exists() {
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
    // Copy to a temp name and rename into place. A copy straight to `orig` that dies partway
    // (disk full, transient I/O) leaves a TRUNCATED winmm_orig.dll — and because every later run
    // early-returns on `orig.exists()`, including the no-op heal that exists to repair exactly
    // this file, the proxy would forward into a broken DLL forever while the launcher reported a
    // clean install. The path is recorded before the rename so a failure after it rolls back.
    let tmp = orig.with_extension("dll.tmp");
    let _ = std::fs::remove_file(&tmp);
    std::fs::copy(&src, &tmp)
        .with_context(|| format!("copying {} -> winmm_orig.dll", src.display()))?;
    committed.push(Committed::OrigCreated { path: orig.clone() });
    if let Err(e) = std::fs::rename(&tmp, &orig) {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow::Error::from(e).context("moving winmm_orig.dll into place"));
    }
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

    // whatever is still in the vanilla store was displaced by us but isn't in state.files —
    // e.g. a foreign file preserved when a manifest remove[] displaced it. Revert-to-stock
    // means putting those back too, not deleting them with the store below.
    restore_vanilla_tree(&vanilla_root, &vanilla_root, &game_dir, &mut restored)?;

    let _ = std::fs::remove_dir_all(game_dir.join(BACKUP_DIR));
    let _ = std::fs::remove_dir_all(game_dir.join(STAGING_DIR));
    // shim entries only — an interrupted base-game download lives in a subdirectory and is not
    // this operation's to discard
    clear_dir_files(&game_dir.join(CACHE_DIR));
    let _ = std::fs::remove_dir_all(&vanilla_root);
    let _ = std::fs::remove_file(InstalledState::path(&game_dir));

    Ok(UninstallReport { version: state.version, restored, deleted, winmm_orig_removed })
}

/// Restore every file remaining under the vanilla store to its game-relative path. Skips a dest
/// that is (unexpectedly) occupied — a file that exists there now is not ours to clobber.
fn restore_vanilla_tree(
    root: &Path,
    dir: &Path,
    game_dir: &Path,
    restored: &mut Vec<String>,
) -> Result<()> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Ok(()) };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            restore_vanilla_tree(root, &p, game_dir, restored)?;
            continue;
        }
        let rel = p.strip_prefix(root).expect("entry under the vanilla root");
        let target = game_dir.join(rel);
        if !target.exists() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(&p, &target)
                .with_context(|| format!("restoring vanilla {}", rel.display()))?;
            restored.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

// ---- base game (fresh install / verify / repair) ----
//
// The base game is the SAME per-file manifest model as the shim, from its own repo (Settings::
// game_repo) — but a deliberately different pipeline. The shim flow stages copies and commits
// with rollback because it mutates a live install that must never be left half-updated; the base
// flow writes files that either don't exist yet (fresh install) or are corrupt (repair), so
// there is nothing worth preserving and "resume by re-planning" IS the recovery story. That
// difference is what keeps the disk requirement at ~the game size instead of ~2x: files move
// cache -> final by rename, no staging copies, no backups.
//
// It also deliberately writes NO install state: the base game is not ours to uninstall.
// `uninstall` reverts the shim and must never delete the game.
//
// Coexistence with the shim is by redirection, not exclusion: a base dest whose live path the
// shim REMOVED is preserved under .phoenix-vanilla/<dest> (that is what the removal machinery
// does), so the base file is verified — and repaired — AT THE VANILLA PATH. Repairing it at the
// live path would undo the shim's removal and re-flag it as Remove on every future plan, forever.
// A dest the shim itself manages (recorded in .phoenix-state.json) with no vanilla copy is
// untouchable and reported as skipped.

/// What the base plan decided for one manifest file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseAction {
    /// The file (at its live path, or its preserved vanilla copy) matches the manifest hash.
    UpToDate,
    /// Missing or hash-mismatched — download and place at `target`.
    Write,
    /// The shim manages this dest and no vanilla copy exists — nothing here is ours to touch.
    Skipped,
}

/// One base-game file's verdict.
///
/// Carries the manifest `entry` itself rather than just a dest: every caller needs the size and
/// hash (to total bytes, to dedupe shared content, to download), and looking those back up meant
/// re-resolving the whole manifest and scanning it per status — O(n²) over 4,635 files.
#[derive(Debug)]
pub struct BaseStatus {
    pub action: BaseAction,
    pub entry: FileEntry,
    /// Where the file actually lives for us: the live path, or its preserved copy under
    /// .phoenix-vanilla when the shim removed the dest.
    target: PathBuf,
}

impl BaseStatus {
    pub fn dest(&self) -> &str {
        &self.entry.dest
    }
}

#[derive(Debug)]
pub struct BaseReport {
    pub version: String,
    /// Read by the debug-only CLI's report line; the GUI view doesn't carry it.
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    pub tag: String,
    pub written: usize,
    pub up_to_date: usize,
    /// Read by the debug-only CLI and the tests; the GUI view doesn't carry it.
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    pub skipped: usize,
    /// Bytes downloaded (the sum of written file sizes).
    pub bytes: u64,
}

/// Diff the base-game manifest against the folder. Read-only; hashing is (size,mtime)-memoized,
/// so re-verifying an intact install costs stats, not a re-read of 9 GB. Emits one `op` tick per
/// file (hashing a full install takes real time — the UI must not sit dead through it).
///
/// `cancel` stops it BETWEEN files: a cold verify reads ~15 GB and runs for minutes, and "wait it
/// out" is not an acceptable only option. Granularity is one file per worker — a thread already
/// inside a multi-GB VPK finishes that file first — so the stop lands within seconds rather than
/// instantly. Nothing is written here, so an abandoned run costs nothing and leaves nothing but a
/// warmer hash memo; the partial verdicts are dropped rather than returned, since a plan missing
/// the files nobody looked at is indistinguishable from one where they were all intact.
pub fn base_plan(
    game_dir: &Path,
    manifest: &Manifest,
    progress: engine::Progress,
    op: &'static str,
    cancel: Option<&AtomicBool>,
) -> Result<Vec<BaseStatus>> {
    // resolve with no selections: today's game manifests carry no options, and if one ever does,
    // installing its defaults is the right reading
    let entries = engine::resolve(manifest, &Default::default());
    let shim_managed: HashSet<String> = crate::state::InstalledState::load(game_dir)
        .map(|s| s.files.into_iter().map(|f| f.dest).collect())
        .unwrap_or_default();
    let vanilla_root = game_dir.join(VANILLA_DIR);
    let total = entries.len() as u64;

    // Hash in parallel. Verifying a full base install reads ~15 GB, and every file is
    // independent, so doing it on one thread leaves both the CPU and the drive queue idle most
    // of the time. Results are written back BY INDEX so the returned order still matches the
    // manifest regardless of completion order.
    let slots: Mutex<Vec<Option<BaseStatus>>> =
        Mutex::new((0..entries.len()).map(|_| None).collect());
    let next = AtomicUsize::new(0);
    let done = AtomicU64::new(0);
    let cancelled = || cancel.is_some_and(|c| c.load(Ordering::Relaxed));
    std::thread::scope(|s| {
        for _ in 0..HASH_WORKERS.min(entries.len().max(1)) {
            s.spawn(|| loop {
                // one relaxed load per file, against a file hash — free, and it is the only place
                // a worker can be stopped without abandoning a half-read file
                if cancelled() {
                    return;
                }
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= entries.len() {
                    return;
                }
                let fe = &entries[i];
                let st = plan_one(game_dir, &vanilla_root, &shim_managed, fe);
                let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                // Throttled like the byte ticks are. One event per file means 4,635 JSON
                // serializations + webview postMessages + JS handler calls in a burst — and a warm
                // re-verify (every hash a memo hit) fires them as fast as the loop can spin, for
                // no extra information. The last file always reports, so the bar still lands.
                if let Some(p) = progress {
                    if d.is_multiple_of(PLAN_GRAIN) || d == total {
                        p(engine::OpProgress {
                            op,
                            current: d,
                            total,
                            item: Some(fe.dest.clone()),
                            bytes_done: None,
                            bytes_total: None,
                            done: st.action != BaseAction::Write,
                        });
                    }
                }
                slots.lock().unwrap()[i] = Some(st);
            });
        }
    });
    if cancelled() {
        return Err(anyhow!(engine::Cancelled));
    }
    Ok(slots.into_inner().unwrap().into_iter().flatten().collect())
}

/// One file's verdict, and WHERE it lives for us — see `base_plan`'s coexistence rules.
fn plan_one(
    game_dir: &Path,
    vanilla_root: &Path,
    shim_managed: &HashSet<String>,
    fe: &FileEntry,
) -> BaseStatus {
    let live = game_dir.join(&fe.dest);
    let vanilla = vanilla_root.join(&fe.dest);
    let (target, checkable) = if shim_managed.contains(&fe.dest) {
        // the shim owns the live path; its preserved original (if any) is the base file
        (vanilla.clone(), vanilla.exists())
    } else if !live.exists() && vanilla.exists() {
        // shim remove[] relocated it — verify/repair the preserved copy, not the void
        (vanilla, true)
    } else {
        (live, true)
    };
    let action = if !checkable {
        BaseAction::Skipped
    } else if !target.exists() {
        BaseAction::Write
    } else {
        match verify::sha256_file_cached(&target) {
            Ok(h) if h == fe.sha256 => BaseAction::UpToDate,
            _ => BaseAction::Write,
        }
    };
    BaseStatus { action, entry: fe.clone(), target }
}

/// Does this folder hold a DIFFERENT game build than the manifest describes?
///
/// `game/dota/steam.inf` carries the build identity, so a local copy that EXISTS but does not
/// match the manifest's hash means the folder is some other Dota 2 installation — not a damaged
/// one. The distinction is the difference between a useful repair and a catastrophe: verify
/// would otherwise report nearly every file as "damaged", and accepting that repair would
/// overwrite a perfectly good unrelated install with build 1805. An ABSENT steam.inf is a fresh
/// or empty target, which is not foreign — that is exactly what a fresh install starts from.
pub fn foreign_build(game_dir: &Path, manifest: &Manifest) -> bool {
    const STEAM_INF: &str = "game/dota/steam.inf";
    let Some(fe) = manifest.files.iter().find(|f| f.dest == STEAM_INF) else { return false };
    let local = game_dir.join(STEAM_INF);
    if !local.exists() {
        return false;
    }
    !matches!(verify::sha256_file_cached(&local), Ok(h) if h == fe.sha256)
}

/// Free bytes available to this process on the volume holding `dir` (its deepest existing
/// ancestor — a fresh install target may not exist yet). None = could not determine.
pub fn free_space(dir: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let mut probe = dir;
    while !probe.exists() {
        probe = probe.parent()?;
    }
    let wide: Vec<u16> = probe.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut avail: u64 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, std::ptr::null_mut(), std::ptr::null_mut())
    };
    (ok != 0).then_some(avail)
}

/// How many files the base plan hashes at once. Verifying a full install reads ~15 GB and each
/// file is independent, so one thread leaves both the CPU and the drive queue mostly idle. Kept
/// modest on purpose: past a handful of readers a spinning disk starts seeking instead of
/// streaming, which is slower than the serial case it replaced.
const HASH_WORKERS: usize = 4;

/// Headroom demanded beyond the payload itself: filesystem overhead, the shim install that
/// follows a fresh download, and not painting the user into a zero-byte volume.
const DISK_MARGIN: u64 = 512 * 1024 * 1024;

/// Refuse before the first byte when the volume can't hold what we're about to write. The error
/// wraps ERROR_DISK_FULL so it crosses the wire as an `io` kind — a clear problem statement
/// ("free up space"), never a mysterious mid-download failure at 97%.
fn ensure_disk_space(need: u64, free: Option<u64>) -> Result<()> {
    // unknowable free space (odd volume, junction weirdness): proceed and let the writes speak
    let Some(free) = free else { return Ok(()) };
    if free < need + DISK_MARGIN {
        const ERROR_DISK_FULL: i32 = 112;
        return Err(anyhow::Error::new(std::io::Error::from_raw_os_error(ERROR_DISK_FULL))
            .context(format!(
                "not enough free space: this needs ~{} MB plus headroom, the volume has {} MB",
                need / (1024 * 1024),
                free / (1024 * 1024)
            )));
    }
    Ok(())
}

/// Download and place every missing/damaged base-game file. Serves both fresh installs (into an
/// empty folder) and repair (into a live one) — the plan diff makes them the same operation.
/// Interruption at ANY point is recoverable by running again: completed files hash-match and
/// skip, interrupted downloads resume from their .part, the cache survives.
pub fn install_base(
    game_dir: &Path,
    dl: &dyn Downloader,
    release: &Release,
    manifest: &Manifest,
    progress: engine::Progress,
    cancel: Option<&AtomicBool>,
) -> Result<BaseReport> {
    // cancellable from the first file: repairing a live folder hashes it before a byte is
    // downloaded, and a Cancel that only took effect once the download started sat inert for
    // minutes on exactly the screen that shows a Stop button
    let statuses = base_plan(game_dir, manifest, progress, "game", cancel)?;
    let to_write: Vec<(&FileEntry, &Path)> = statuses
        .iter()
        .filter(|s| s.action == BaseAction::Write)
        .map(|s| (&s.entry, s.target.as_path()))
        .collect();
    let up_to_date = statuses.iter().filter(|s| s.action == BaseAction::UpToDate).count();
    let skipped = statuses.iter().filter(|s| s.action == BaseAction::Skipped).count();

    let report = |written: usize, bytes: u64| BaseReport {
        version: manifest.version.clone(),
        tag: release.tag_name.clone(),
        written,
        up_to_date,
        skipped,
        bytes,
    };
    if to_write.is_empty() {
        // same reclaim as the success path below: everything is intact, so whatever an earlier
        // interrupted attempt cached is no longer a resume source for anything
        clear_dir_files(&game_dir.join(CACHE_DIR).join(BASE_CACHE_SUBDIR));
        return Ok(report(0, 0));
    }

    // Preflight the asset index. A name the (merged, sharded) release does not carry is a
    // permanent condition no retry can fix, and the lookup otherwise happens inside the download
    // worker — so a truncated shard array surfaced only after thousands of files and gigabytes,
    // dressed up as a transient download failure. Milliseconds here, hours saved there.
    let missing: Vec<&str> = {
        let mut seen = HashSet::new();
        to_write
            .iter()
            // a 0-byte entry is CREATED, never downloaded — it legitimately has no asset
            .filter(|(fe, _)| fe.size > 0)
            .map(|(fe, _)| fe.name.as_str())
            .filter(|name| seen.insert(*name))
            .filter(|name| release.asset(name).is_none())
            .collect()
    };
    if !missing.is_empty() {
        bail!(
            "the game release is incomplete: {} file(s) have no matching asset (first: {})",
            missing.len(),
            missing[0]
        );
    }

    let need: u64 = {
        // unique by hash — shared-content dests download once
        let mut seen = HashSet::new();
        to_write.iter().filter(|(fe, _)| seen.insert(fe.sha256.as_str())).map(|(fe, _)| fe.size).sum()
    };
    ensure_disk_space(need, free_space(game_dir))?;

    // interlock: a running game holds its VPKs/DLLs mmapped — say "close the game" NOW, not
    // after gigabytes. Targets may sit under .phoenix-vanilla, so probe the actual target paths.
    let rels: Vec<String> = to_write
        .iter()
        .map(|(_, t)| t.strip_prefix(game_dir).unwrap_or(t).to_string_lossy().into_owned())
        .collect();
    probe_writable(game_dir, rels.iter())?;

    // The base game gets its OWN cache subdirectory. It shared the shim's until a detached
    // warm_cache — which prunes against the shim manifest — was found deleting multi-GB base
    // entries and their `.part` resume sources behind an interrupted download.
    let cache = game_dir.join(CACHE_DIR).join(BASE_CACHE_SUBDIR);
    std::fs::create_dir_all(&cache).context("creating the asset cache")?;
    let fe_only: Vec<&FileEntry> = to_write.iter().map(|(fe, _)| *fe).collect();
    obtain_all_tagged(&cache, dl, release, &fe_only, progress, "game", cancel)?;

    // the game may have started during a multi-GB download — re-probe before touching anything
    probe_writable(game_dir, rels.iter())?;

    // place: rename cache -> target (same volume, no copies). Dests sharing one hash copy for
    // all but the last taker, which consumes the cache entry by rename.
    let mut takers: HashMap<&str, Vec<&Path>> = HashMap::new();
    for (fe, target) in &to_write {
        takers.entry(fe.sha256.as_str()).or_default().push(target);
    }
    let mut written = 0usize;
    let mut bytes = 0u64;
    for (fe, _) in &to_write {
        let Some(targets) = takers.remove(fe.sha256.as_str()) else { continue };
        let cpath = cache.join(&fe.sha256);
        for (i, target) in targets.iter().enumerate() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let last = i + 1 == targets.len();
            if last {
                // rename replaces an existing (corrupt) target on Windows
                std::fs::rename(&cpath, target)
                    .with_context(|| format!("placing {}", target.display()))?;
            } else {
                std::fs::copy(&cpath, target)
                    .with_context(|| format!("placing {}", target.display()))?;
            }
            written += 1;
            bytes += fe.size;
        }
    }
    // Every entry this run needed was just consumed (the last taker renames it out) — anything
    // still in the cache is stale: entries and `.part`s from an interrupted run against an OLDER
    // manifest, or a poisoned leftover. Nothing prunes this directory otherwise (the shim's prune
    // and uninstall both deliberately keep out), so a superseded 16 GB attempt would sit inside
    // the game folder forever. Only on SUCCESS: a cancelled or failed run keeps everything — its
    // `.part`s and finished entries are exactly what the next attempt resumes from.
    clear_dir_files(&cache);
    Ok(report(written, bytes))
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
        let r = install(&settings(&dir), &dl, None, None, None).unwrap();

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

    /// The original of a file we are replacing must survive a failed placement. `back_up` MOVES
    /// it out of the game folder, so if the record of where it went is written only after the
    /// (fallible) rename, a failure there erases the file: rollback replays a list that never
    /// mentioned the backup. Injected here by handing commit a staged path that does not exist,
    /// which is what an AV lock or a vanished temp file looks like to `fs::rename`.
    #[test]
    fn a_failed_placement_still_rolls_the_original_back() {
        let dir = tempdir("commit-rollback");
        let dest = "game/bin/win64/winmm.dll";
        std::fs::create_dir_all(dir.join("game/bin/win64")).unwrap();
        std::fs::write(dir.join(dest), b"VANILLA").unwrap();

        let manifest = Manifest::parse(basic_release().0.as_bytes()).unwrap();
        let fe = FileEntry {
            name: "winmm.dll".to_string(),
            dest: dest.to_string(),
            sha256: sha(b"new"),
            size: 3,
        };
        let ctx = Ctx {
            game_dir: dir.clone(),
            backup_root: dir.join(BACKUP_DIR).join("1.0.0"),
            vanilla_root: dir.join(VANILLA_DIR),
            prev_dests: HashSet::new(),
            trust_prev: true,
            prev_winmm_created: false,
            prev_restored: Vec::new(),
        };
        let staged = vec![(&fe, dir.join(STAGING_DIR).join("s0"))]; // never created
        let resolved = vec![fe.clone()];
        let job = CommitJob {
            staged: &staged,
            removals: &[],
            resolved: &resolved,
            manifest: &manifest,
        };

        let mut committed = Vec::new();
        assert!(commit(&ctx, &job, &mut committed).is_err(), "placement should fail");
        rollback(&committed);
        assert_eq!(
            std::fs::read(dir.join(dest)).unwrap(),
            b"VANILLA",
            "the original must be back at its dest, not stranded in the backup store"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `.part` that reached the asset's full length can never be resumed — the Range request
    /// would start at EOF and the CDN answers 416, an error, which keeps the `.part`: the asset
    /// becomes permanently undownloadable. It must be discarded and re-fetched instead.
    #[test]
    fn a_full_length_part_is_discarded_instead_of_resumed_forever() {
        let dir = tempdir("poisoned-part");
        let (m, assets) = basic_release();
        let dl = Fake::new("v1.0.0", &m, assets);
        let cache = dir.join(CACHE_DIR);
        std::fs::create_dir_all(&cache).unwrap();
        // a leftover .part as long as the finished asset (a completed transfer whose rename into
        // the cache failed, or a cancel landing on the last chunk)
        let part = cache.join(format!("{}.part", sha(b"dll")));
        std::fs::write(&part, b"XXX").unwrap();

        let r = install(&settings(&dir), &dl, None, None, None);
        assert!(r.is_ok(), "install must recover from a full-length .part: {r:?}");
        assert_eq!(std::fs::read(dir.join("game/bin/win64/winmm.dll")).unwrap(), b"dll");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Without the state file we cannot tell our own installed files from vanilla originals. If
    /// one of ours is promoted into the vanilla store, uninstall "restores" the shim and reports
    /// the game as stock — so nothing may be promoted when a prior install is evident.
    #[test]
    fn a_lost_state_file_does_not_turn_our_own_file_into_a_vanilla_original() {
        let dir = tempdir("lost-state");
        let (m1, assets1) = basic_release();
        install(&settings(&dir), &Fake::new("v1.0.0", &m1, assets1), None, None, None).unwrap();
        // the state file is lost (AV cleanup, a tidy-up, or the corrupt-state quarantine)
        std::fs::remove_file(InstalledState::path(&dir)).unwrap();

        // v2 ships a different winmm.dll, so the v1 file we placed gets displaced
        let m2 = serde_json::json!({
            "version": "2.0.0",
            "files": [
                file_json("winmm.dll", "game/bin/win64/winmm.dll", b"dll2"),
                file_json("a.vpk", "game/dota/a.vpk", b"vpk"),
            ]
        })
        .to_string();
        let dl2 = Fake::new("v2.0.0", &m2, vec![("winmm.dll", b"dll2"), ("a.vpk", b"vpk")]);
        install(&settings(&dir), &dl2, None, None, None).unwrap();

        assert!(
            !dir.join(VANILLA_DIR).join("game/bin/win64/winmm.dll").exists(),
            "our own v1 shim must not be preserved as a vanilla original"
        );
        uninstall(&settings(&dir)).unwrap();
        assert!(
            !dir.join("game/bin/win64/winmm.dll").exists(),
            "uninstall must leave the dest empty, not restore the shim it mistook for stock"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn noop_install_heals_a_missing_state_file() {
        let dir = tempdir("heal");
        let (m, assets) = basic_release();
        let dl = Fake::new("v1.0.0", &m, assets);
        install(&settings(&dir), &dl, None, None, None).unwrap();
        // lose the state file — the folder is now "up to date but not installed"
        std::fs::remove_file(InstalledState::path(&dir)).unwrap();
        assert!(InstalledState::load(&dir).is_none());

        let r = install(&settings(&dir), &dl, None, None, None).unwrap();
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
        install(&s, &dl, None, None, None).unwrap();
        assert!(dir.join("game/dota/fx.vpk").exists());

        s.selections.insert("fx".into(), serde_json::json!(false));
        let r = install(&s, &dl, None, None, None).unwrap();
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
        install(&s, &dl, None, None, None).unwrap();

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
        install(&settings(&dir), &dl, None, None, None).unwrap();

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

        assert!(install(&settings(&dir), &dl, None, None, None).is_err());
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

        let r = install(&settings(&dir), &dl, None, None, None).unwrap();
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
        assert!(install(&settings(&dir), &dl, None, None, None).is_err());
        assert!(!dir.join("game/dota/big.vpk").exists());
        assert!(dir.join(CACHE_DIR).join(format!("{}.part", sha(&big))).exists());

        // second run: resumes the .part (asserted inside CutOnce) and completes
        let r = install(&settings(&dir), &dl, None, None, None).unwrap();
        assert_eq!(r.written, vec!["game/dota/big.vpk".to_string()]);
        assert_eq!(std::fs::read(dir.join("game/dota/big.vpk")).unwrap(), big);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_foreign_file_at_a_remove_dest_is_removed_preserved_and_stays_removed() {
        let dir = tempdir("remove-foreign");
        // a file WE never placed (no prior state) sits at the manifest's remove[] dest
        std::fs::create_dir_all(dir.join("game/dota")).unwrap();
        std::fs::write(dir.join("game/dota/stale.vpk"), b"old").unwrap();
        let m = serde_json::json!({
            "version": "1.0.0",
            "files": [ file_json("a.vpk", "game/dota/a.vpk", b"vpk") ],
            "remove": [ { "dest": "game/dota/stale.vpk" } ]
        })
        .to_string();
        let dl = Fake::new("v1.0.0", &m, vec![("a.vpk", b"vpk")]);

        let r = install(&settings(&dir), &dl, None, None, None).unwrap();
        assert_eq!(r.removed, vec!["game/dota/stale.vpk".to_string()]);
        // the removal must STICK (the old bug preserved-then-restored it in one breath,
        // re-flagging it as Remove on every future plan, forever)...
        assert!(!dir.join("game/dota/stale.vpk").exists());
        // ...while the foreign file is preserved, not destroyed
        assert_eq!(std::fs::read(dir.join(VANILLA_DIR).join("game/dota/stale.vpk")).unwrap(), b"old");
        // and the next plan is clean — no permanent pending-remove loop
        let chk = crate::engine::check(&settings(&dir), &dl, None).unwrap();
        assert_eq!(chk.changes(), 0, "plan must not re-flag the removed file");

        // uninstall reverts to what was there before us: the preserved file comes back
        let u = uninstall(&settings(&dir)).unwrap();
        assert!(u.restored.contains(&"game/dota/stale.vpk".to_string()));
        assert_eq!(std::fs::read(dir.join("game/dota/stale.vpk")).unwrap(), b"old");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The OTHER half of the removal story: v1 shipped a file that displaced a genuine loose
    /// stock file (preserved to the vanilla store); v2 lists that dest in remove[]. The removal
    /// displaces OUR file and restores the stock original — and that restore must STICK: without
    /// the state record, the next plan saw a file at a remove[] dest, flagged it Remove, and the
    /// following apply displaced the restored original right back into the vanilla store — the
    /// restore undone one release later, with a bogus "1 to change" shown in between.
    #[test]
    fn a_restored_vanilla_original_at_a_remove_dest_stays_restored() {
        let dir = tempdir("remove-restore");
        // a genuine loose stock file predates us
        std::fs::create_dir_all(dir.join("game/dota")).unwrap();
        std::fs::write(dir.join("game/dota/sound.vpk"), b"STOCK").unwrap();

        // v1 ships a file at that dest — the stock original is preserved
        let m1 = serde_json::json!({
            "version": "1.0.0",
            "files": [
                file_json("a.vpk", "game/dota/a.vpk", b"vpk"),
                file_json("sound.vpk", "game/dota/sound.vpk", b"PHOENIX"),
            ]
        })
        .to_string();
        let dl1 = Fake::new("v1.0.0", &m1, vec![("a.vpk", b"vpk"), ("sound.vpk", b"PHOENIX")]);
        install(&settings(&dir), &dl1, None, None, None).unwrap();
        assert_eq!(std::fs::read(dir.join(VANILLA_DIR).join("game/dota/sound.vpk")).unwrap(), b"STOCK");

        // v2 stops shipping it and removes the dest — our file goes, the original comes back
        let m2 = serde_json::json!({
            "version": "2.0.0",
            "files": [ file_json("a.vpk", "game/dota/a.vpk", b"vpk") ],
            "remove": [ { "dest": "game/dota/sound.vpk" } ]
        })
        .to_string();
        let dl2 = Fake::new("v2.0.0", &m2, vec![("a.vpk", b"vpk")]);
        let r = install(&settings(&dir), &dl2, None, None, None).unwrap();
        assert_eq!(r.removed, vec!["game/dota/sound.vpk".to_string()]);
        assert_eq!(std::fs::read(dir.join("game/dota/sound.vpk")).unwrap(), b"STOCK");
        let st = InstalledState::load(&dir).unwrap();
        assert_eq!(st.restored, vec!["game/dota/sound.vpk".to_string()]);

        // the next plan is CLEAN — the restored original must not re-flag as Remove
        let chk = crate::engine::check(&settings(&dir), &dl2, None).unwrap();
        assert_eq!(chk.changes(), 0, "restored original re-flagged: {:?}", chk.files);

        // a no-op re-install (the heal path) carries the record instead of dropping it
        install(&settings(&dir), &dl2, None, None, None).unwrap();
        assert_eq!(
            InstalledState::load(&dir).unwrap().restored,
            vec!["game/dota/sound.vpk".to_string()]
        );
        let chk = crate::engine::check(&settings(&dir), &dl2, None).unwrap();
        assert_eq!(chk.changes(), 0);

        // uninstall leaves the stock file exactly where the restore put it
        uninstall(&settings(&dir)).unwrap();
        assert_eq!(std::fs::read(dir.join("game/dota/sound.vpk")).unwrap(), b"STOCK");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A release that SHIPS a file at a previously-restored dest again drops the record: the
    /// stock file there is displaced (and re-preserved) like any other genuine original.
    #[test]
    fn a_reshipped_dest_drops_the_restored_record_and_represerves_the_original() {
        let dir = tempdir("remove-restore-reship");
        std::fs::create_dir_all(dir.join("game/dota")).unwrap();
        std::fs::write(dir.join("game/dota/sound.vpk"), b"STOCK").unwrap();
        let ship = |ver: &str, with_sound: bool, removes: bool| {
            let mut files = vec![file_json("a.vpk", "game/dota/a.vpk", b"vpk")];
            if with_sound {
                files.push(file_json("sound.vpk", "game/dota/sound.vpk", b"PHOENIX"));
            }
            let mut m = serde_json::json!({ "version": ver, "files": files });
            if removes {
                m["remove"] = serde_json::json!([{ "dest": "game/dota/sound.vpk" }]);
            }
            m.to_string()
        };

        let dl1 = Fake::new("v1", &ship("1.0.0", true, false), vec![("a.vpk", b"vpk"), ("sound.vpk", b"PHOENIX")]);
        install(&settings(&dir), &dl1, None, None, None).unwrap();
        let dl2 = Fake::new("v2", &ship("2.0.0", false, true), vec![("a.vpk", b"vpk")]);
        install(&settings(&dir), &dl2, None, None, None).unwrap(); // restored
        let dl3 = Fake::new("v3", &ship("3.0.0", true, false), vec![("a.vpk", b"vpk"), ("sound.vpk", b"PHOENIX")]);
        install(&settings(&dir), &dl3, None, None, None).unwrap(); // ships it again

        let st = InstalledState::load(&dir).unwrap();
        assert!(st.restored.is_empty(), "a shipped dest must not stay recorded: {:?}", st.restored);
        assert_eq!(std::fs::read(dir.join("game/dota/sound.vpk")).unwrap(), b"PHOENIX");
        // the stock original is preserved AGAIN, so uninstall still reverts to it
        uninstall(&settings(&dir)).unwrap();
        assert_eq!(std::fs::read(dir.join("game/dota/sound.vpk")).unwrap(), b"STOCK");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    // set_readonly(false) is fine here: Windows-only test file, deleted right after
    #[allow(clippy::permissions_set_readonly_false)]
    fn a_read_only_target_fails_with_a_permission_error_not_game_running() {
        let dir = tempdir("denied");
        let (m, assets) = basic_release();
        let dl = Fake::new("v1.0.0", &m, assets);
        install(&settings(&dir), &dl, None, None, None).unwrap();

        // a read-only attribute denies write with ERROR_ACCESS_DENIED — not a live process
        let target = dir.join("game/bin/win64/winmm.dll");
        let mut perm = std::fs::metadata(&target).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&target, perm.clone()).unwrap();

        let m2 = serde_json::json!({
            "version": "1.0.1",
            "files": [ file_json("winmm.dll", "game/bin/win64/winmm.dll", b"dll2") ]
        })
        .to_string();
        let dl2 = Fake::new("v1.0.1", &m2, vec![("winmm.dll", b"dll2")]);
        let err = install(&settings(&dir), &dl2, None, None, None).unwrap_err();
        assert!(
            !err.chain().any(|c| c.downcast_ref::<engine::GameRunning>().is_some()),
            "a permissions problem must not be diagnosed as the game running: {err:#}"
        );
        assert!(format!("{err:#}").contains("write access"), "got: {err:#}");

        perm.set_readonly(false);
        std::fs::set_permissions(&target, perm).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_locked_target_fails_with_game_running_before_downloading() {
        let dir = tempdir("locked");
        let (m, assets) = basic_release();
        let dl = Fake::new("v1.0.0", &m, assets);
        install(&settings(&dir), &dl, None, None, None).unwrap();

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
        let err = install(&settings(&dir), &dl2, None, None, None).unwrap_err();
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

    // ---- base game ----

    /// A little base-game release: two content files sharing bytes with nothing, plus one
    /// duplicated-content dest (two cfgs with identical bytes → one asset download, two places).
    fn base_release() -> (String, Vec<(&'static str, &'static [u8])>) {
        let m = serde_json::json!({
            "schema": 2,
            "version": "1805",
            "files": [
                file_json("game__bin__dota2.exe", "game/bin/win64/dota2.exe", b"EXE"),
                file_json("game__dota__pak01.vpk", "game/dota/pak01_dir.vpk", b"PAK"),
                file_json("cfg_a", "game/dota/cfg/a.cfg", b"CFG"),
                file_json("cfg_b", "game/core/cfg/b.cfg", b"CFG"),
            ]
        })
        .to_string();
        (
            m,
            vec![
                ("game__bin__dota2.exe", b"EXE"),
                ("game__dota__pak01.vpk", b"PAK"),
                ("cfg_a", b"CFG"),
                ("cfg_b", b"CFG"),
            ],
        )
    }

    fn base_fetch(dl: &Fake) -> (Release, Manifest) {
        let release = dl.fetch_release("r", None).unwrap();
        let manifest = engine::manifest_of(dl, &release).unwrap();
        (release, manifest)
    }

    #[test]
    fn base_install_into_empty_dir_writes_everything_and_no_state() {
        let dir = tempdir("base-fresh");
        let (m, assets) = base_release();
        let dl = Fake::new("v1805", &m, assets);
        let (release, manifest) = base_fetch(&dl);

        let r = install_base(&dir, &dl, &release, &manifest, None, None).unwrap();
        assert_eq!(r.written, 4);
        assert_eq!((r.up_to_date, r.skipped), (0, 0));
        assert_eq!(r.bytes, 12); // 4 files × 3 bytes — shared content still counts per placed file
        assert_eq!(std::fs::read(dir.join("game/bin/win64/dota2.exe")).unwrap(), b"EXE");
        assert_eq!(std::fs::read(dir.join("game/dota/cfg/a.cfg")).unwrap(), b"CFG");
        assert_eq!(std::fs::read(dir.join("game/core/cfg/b.cfg")).unwrap(), b"CFG");
        // the base game is NOT ours to uninstall — no install state may appear
        assert!(InstalledState::load(&dir).is_none());
        // everything moved out of the cache — a fresh install must not leave a 9 GB duplicate.
        // The base pipeline caches in its own subdirectory (kept clear of the shim's prune), so
        // that is where the leftovers would be. The success-path reclaim removes the emptied
        // directory itself, so "gone entirely" is the expected shape of "no leftovers".
        let leftovers: Vec<_> = std::fs::read_dir(dir.join(CACHE_DIR).join(BASE_CACHE_SUBDIR))
            .map(|rd| rd.flatten().collect())
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "cache entries left behind: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_file_installs_without_an_asset_to_download() {
        // The real 1805 tree contains 4 zero-byte files, and GitHub refuses to host a zero-byte
        // release asset (422) — so the publisher CANNOT upload one. The manifest still lists it
        // and the reader must materialize it. The fake release below deliberately omits the
        // asset, exactly as the real one does.
        let dir = tempdir("base-empty");
        let m = serde_json::json!({
            "schema": 2,
            "version": "1805",
            "files": [
                file_json("real", "game/dota/real.vpk", b"DATA"),
                file_json("empty", "game/core/scripts/vscripts/game/gameinit.lua", b""),
            ]
        })
        .to_string();
        // note: no "empty" asset — an upload of it would have been rejected
        let dl = Fake::new("v1805", &m, vec![("real", b"DATA")]);
        let (release, manifest) = base_fetch(&dl);

        let r = install_base(&dir, &dl, &release, &manifest, None, None).unwrap();
        assert_eq!(r.written, 2);
        let empty = dir.join("game/core/scripts/vscripts/game/gameinit.lua");
        assert!(empty.exists(), "the empty file must exist");
        assert_eq!(std::fs::metadata(&empty).unwrap().len(), 0);

        // and it verifies as UpToDate on the next pass, so it never re-flags as damaged
        let statuses = base_plan(&dir, &manifest, None, "verify", None).unwrap();
        assert!(statuses.iter().all(|s| s.action == BaseAction::UpToDate), "{statuses:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_zero_size_entry_with_a_wrong_hash_is_refused() {
        // size 0 is self-describing; pairing it with any other hash means a corrupt manifest,
        // and silently writing an empty file would satisfy a check that should have failed
        let dir = tempdir("base-empty-badhash");
        let m = serde_json::json!({
            "schema": 2, "version": "1805",
            "files": [{ "name": "e", "dest": "game/x.bin", "sha256": sha(b"not empty"), "size": 0 }]
        })
        .to_string();
        let dl = Fake::new("v1805", &m, vec![]);
        let (release, manifest) = base_fetch(&dl);
        let err = install_base(&dir, &dl, &release, &manifest, None, None).unwrap_err();
        assert!(format!("{err:#}").contains("not the empty hash"), "got: {err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn base_repair_touches_only_damaged_files() {
        let dir = tempdir("base-repair");
        let (m, assets) = base_release();
        let dl = Fake::new("v1805", &m, assets);
        let (release, manifest) = base_fetch(&dl);
        install_base(&dir, &dl, &release, &manifest, None, None).unwrap();

        // corrupt one file, delete another, leave the rest alone
        std::fs::write(dir.join("game/dota/pak01_dir.vpk"), b"CORRUPT").unwrap();
        std::fs::remove_file(dir.join("game/dota/cfg/a.cfg")).unwrap();

        let r = install_base(&dir, &dl, &release, &manifest, None, None).unwrap();
        assert_eq!(r.written, 2, "only the corrupt + missing files");
        assert_eq!(r.up_to_date, 2);
        assert_eq!(std::fs::read(dir.join("game/dota/pak01_dir.vpk")).unwrap(), b"PAK");
        assert_eq!(std::fs::read(dir.join("game/dota/cfg/a.cfg")).unwrap(), b"CFG");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn base_plan_redirects_to_vanilla_store_and_skips_shim_dests() {
        let dir = tempdir("base-coexist");
        let (m, _) = base_release();
        let manifest = crate::manifest::Manifest::parse(m.as_bytes()).unwrap();

        // the shim REMOVED a.cfg — its original is preserved in the vanilla store, intact
        std::fs::create_dir_all(dir.join(".phoenix-vanilla/game/dota/cfg")).unwrap();
        std::fs::write(dir.join(".phoenix-vanilla/game/dota/cfg/a.cfg"), b"CFG").unwrap();
        // the shim MANAGES dota2.exe (hypothetically) — state says so, no vanilla copy
        let st = InstalledState {
            version: "1.0.0".into(),
            files: vec![crate::state::InstalledFile {
                dest: "game/bin/win64/dota2.exe".into(),
                sha256: sha(b"SHIM"),
            }],
            winmm_orig_created: false,
            restored: Vec::new(),
        };
        st.save(&dir).unwrap();
        std::fs::create_dir_all(dir.join("game/bin/win64")).unwrap();
        std::fs::write(dir.join("game/bin/win64/dota2.exe"), b"SHIM").unwrap();
        // the remaining two dests are simply present and intact
        std::fs::create_dir_all(dir.join("game/dota")).unwrap();
        std::fs::write(dir.join("game/dota/pak01_dir.vpk"), b"PAK").unwrap();
        std::fs::create_dir_all(dir.join("game/core/cfg")).unwrap();
        std::fs::write(dir.join("game/core/cfg/b.cfg"), b"CFG").unwrap();

        let statuses = base_plan(&dir, &manifest, None, "verify", None).unwrap();
        let action_of = |dest: &str| {
            statuses.iter().find(|s| s.dest() == dest).map(|s| s.action).unwrap()
        };
        // preserved original counts as the base file — NOT damaged, NOT restored to the live path
        assert_eq!(action_of("game/dota/cfg/a.cfg"), BaseAction::UpToDate);
        // the shim's own file at a base dest with no preserved original: untouchable
        assert_eq!(action_of("game/bin/win64/dota2.exe"), BaseAction::Skipped);
        assert_eq!(action_of("game/dota/pak01_dir.vpk"), BaseAction::UpToDate);

        // a CORRUPT preserved original repairs INTO the vanilla store, never onto the live path
        // (placing it live would undo the shim's removal and re-flag it on every plan, forever).
        // The corruption must differ in LENGTH: Windows gives two writes microseconds apart the
        // same mtime ~90% of the time, so a same-size overwrite is invisible to the (size,mtime)
        // hash memo and the file would still read as intact.
        std::fs::write(dir.join(".phoenix-vanilla/game/dota/cfg/a.cfg"), b"ROTTEN").unwrap();
        let (mm, assets) = base_release();
        let dl = Fake::new("v1805", &mm, assets);
        let (release, manifest) = base_fetch(&dl);
        let r = install_base(&dir, &dl, &release, &manifest, None, None).unwrap();
        assert_eq!(r.written, 1);
        assert_eq!(r.skipped, 1);
        assert_eq!(std::fs::read(dir.join(".phoenix-vanilla/game/dota/cfg/a.cfg")).unwrap(), b"CFG");
        assert!(!dir.join("game/dota/cfg/a.cfg").exists(), "the removal must stick");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn base_install_success_clears_stale_cache_leftovers() {
        // An interrupted attempt against an OLDER manifest leaves entries and .parts under hashes
        // the current release never references; nothing else prunes this directory (the shim's
        // prune and uninstall both deliberately keep out), so a completed run must reclaim them —
        // a superseded multi-GB attempt otherwise sits inside the game folder forever.
        let dir = tempdir("base-cache-stale");
        let cache = dir.join(CACHE_DIR).join(BASE_CACHE_SUBDIR);
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join(sha(b"old-release-bytes")), b"old-release-bytes").unwrap();
        std::fs::write(cache.join(format!("{}.part", sha(b"other"))), b"par").unwrap();

        let (m, assets) = base_release();
        let dl = Fake::new("v1805", &m, assets);
        let (release, manifest) = base_fetch(&dl);
        install_base(&dir, &dl, &release, &manifest, None, None).unwrap();
        let leftover = std::fs::read_dir(&cache).map(|rd| rd.count()).unwrap_or(0);
        assert_eq!(leftover, 0, "stale cache entries must be reclaimed on success");

        // and the nothing-to-do path (everything intact) reclaims too
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join(sha(b"junk2")), b"junk2").unwrap();
        install_base(&dir, &dl, &release, &manifest, None, None).unwrap();
        let leftover = std::fs::read_dir(&cache).map(|rd| rd.count()).unwrap_or(0);
        assert_eq!(leftover, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn base_plan_cancel_is_typed_and_returns_no_partial_verdicts() {
        // Verify is minutes of hashing with nothing to show for an abandoned run, so it must be
        // stoppable — and a stopped plan must NOT come back as a Vec of the files that happened to
        // finish: every caller reads "not in the Write list" as "intact".
        let dir = tempdir("base-plan-cancel");
        let (m, _) = base_release();
        let manifest = crate::manifest::Manifest::parse(m.as_bytes()).unwrap();
        let cancel = AtomicBool::new(true);
        let err = base_plan(&dir, &manifest, None, "verify", Some(&cancel)).unwrap_err();
        assert!(
            err.chain().any(|c| c.downcast_ref::<engine::Cancelled>().is_some()),
            "expected the Cancelled marker, got: {err:#}"
        );
        // and an uncancelled plan of the same folder still reports (everything missing here)
        let statuses = base_plan(&dir, &manifest, None, "verify", None).unwrap();
        assert!(!statuses.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn base_install_cancel_stops_before_placing_and_is_typed() {
        let dir = tempdir("base-cancel");
        let (m, assets) = base_release();
        let dl = Fake::new("v1805", &m, assets);
        let (release, manifest) = base_fetch(&dl);

        let cancel = AtomicBool::new(true); // cancelled before the first chunk lands
        let err = install_base(&dir, &dl, &release, &manifest, None, Some(&cancel)).unwrap_err();
        assert!(
            err.chain().any(|c| c.downcast_ref::<engine::Cancelled>().is_some()),
            "expected the Cancelled marker, got: {err:#}"
        );
        assert!(!dir.join("game/bin/win64/dota2.exe").exists(), "nothing may be placed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_preflight_math() {
        // plenty of room / unknowable — allowed
        assert!(ensure_disk_space(1_000, Some(10 * 1024 * 1024 * 1024)).is_ok());
        assert!(ensure_disk_space(u64::MAX / 2, None).is_ok());
        // short of need + margin — refused, with an io error (ERROR_DISK_FULL) in the chain
        let err = ensure_disk_space(2 * 1024 * 1024 * 1024, Some(1024 * 1024 * 1024)).unwrap_err();
        assert!(err.chain().any(|c| c.downcast_ref::<std::io::Error>().is_some()));
        assert!(format!("{err:#}").contains("not enough free space"));
    }

    #[test]
    fn free_space_reports_something_sane() {
        // the deepest-existing-ancestor walk: a target that does not exist yet still resolves
        let missing = std::env::temp_dir().join("phoenix-nonexistent").join("deeper");
        let free = free_space(&missing);
        assert!(free.is_some_and(|b| b > 0), "temp volume free space unknowable: {free:?}");
    }
}
