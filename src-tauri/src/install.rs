//! The mutating install + uninstall.
//!
//! Install runs in two phases so a real game folder is never left half-updated:
//!   1. obtain every changed file — from the local asset cache when its hash matches, else a
//!      streaming download that is verified (sha256 + size) — and stage it on the same volume;
//!      nothing under the game is touched yet;
//!   2. commit: back up each existing target, atomically move the staged file into place, create
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

use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::config::Settings;
use crate::manifest::{FileEntry, Manifest};
use crate::state::{InstalledFile, InstalledState};
use crate::{engine, github, verify};

const STAGING_DIR: &str = ".phoenix-staging";
const BACKUP_DIR: &str = ".phoenix-backup";
const VANILLA_DIR: &str = ".phoenix-vanilla";
/// Content-addressed asset cache (file name = sha256). Every manifest asset — including unselected
/// variants and disabled toggles — is prefetched here after a successful install, so a later
/// customization change never re-downloads. Pruned to the current manifest, deleted on uninstall.
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

pub fn install(settings: &Settings, tag: Option<&str>) -> Result<InstallReport> {
    let game_dir = settings.resolve_game_dir()?;
    let (release, manifest) = engine::fetch(settings, tag)?;

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
        // nothing to change — still make sure every asset is cached for instant customization
        prefetch_all(&cache, &release, &manifest, settings.token.as_deref());
        prune_cache(&cache, &manifest);
        return Ok(InstallReport {
            version: manifest.version.clone(),
            written: Vec::new(),
            removed: Vec::new(),
            up_to_date,
            winmm_orig: WinmmOrig::NotNeeded,
        });
    }

    // --- phase 1: obtain (cache-first, else streaming download) + stage (game untouched) ---
    let staging = game_dir.join(STAGING_DIR);
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).context("creating the staging directory")?;

    let mut staged: Vec<(&FileEntry, PathBuf)> = Vec::new();
    for (i, fe) in to_write.iter().enumerate() {
        let cpath =
            obtain_to_cache(&cache, &release, settings.token.as_deref(), &fe.name, &fe.sha256, fe.size)?;
        // staged name is positional — asset names may repeat across dests
        let sp = staging.join(format!("s{i}"));
        std::fs::copy(&cpath, &sp).with_context(|| format!("staging {}", fe.dest))?;
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
            // cache the remaining assets (unselected variants, disabled toggles) so flipping
            // customization later never waits on the network; failures fall back to on-demand
            prefetch_all(&cache, &release, &manifest, settings.token.as_deref());
            prune_cache(&cache, &manifest);
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

    let winmm_orig = ensure_winmm_orig(&ctx.game_dir, job.staged, committed)?;

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
    let state = InstalledState {
        version: job.manifest.version.clone(),
        // the resolved (effective) set — includes selected variants and enabled toggles
        files: job
            .resolved
            .iter()
            .map(|f| InstalledFile { dest: f.dest.clone(), sha256: f.sha256.clone() })
            .collect(),
        winmm_orig_created,
    };
    state.save(&ctx.game_dir).context("writing install state")?;

    Ok((written, removed, winmm_orig))
}

// ---- asset cache ----

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

/// Path to a verified cache entry for an asset: cache hit, else streaming download + verify.
fn obtain_to_cache(
    cache: &Path,
    release: &github::Release,
    token: Option<&str>,
    name: &str,
    sha256: &str,
    size: u64,
) -> Result<PathBuf> {
    let cpath = cache.join(sha256);
    if cache_ok(&cpath, sha256, size) {
        return Ok(cpath);
    }
    let asset = release
        .asset(name)
        .with_context(|| format!("the release has no asset named {name}"))?;
    let tmp = cache.join(format!("{sha256}.part"));
    let dl = github::download_asset_to(asset, token, &tmp)
        .with_context(|| format!("downloading {name}"));
    let (got_size, got_sha) = match dl {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };
    if got_size != size || got_sha != sha256 {
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

/// Download every not-yet-cached manifest asset. Best-effort: a failed asset is skipped (it will
/// download on demand when actually selected) so an optional extra can't fail the install.
fn prefetch_all(cache: &Path, release: &github::Release, manifest: &Manifest, token: Option<&str>) {
    let mut seen = HashSet::new();
    for (name, sha256, size) in all_assets(manifest) {
        if !seen.insert(sha256) || cache.join(sha256).exists() {
            continue;
        }
        let _ = obtain_to_cache(cache, release, token, name, sha256, size);
    }
}

/// Drop cache entries the current manifest no longer references (stale hashes, leftover .part).
fn prune_cache(cache: &Path, manifest: &Manifest) {
    let keep: HashSet<&str> = all_assets(manifest).into_iter().map(|(_, sha, _)| sha).collect();
    if let Ok(rd) = std::fs::read_dir(cache) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !keep.contains(name.as_str()) {
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

/// If a winmm.dll was placed and winmm_orig.dll is absent, create it by COPYING the system winmm.dll.
/// Never overwrite an existing winmm_orig.dll — overwriting it with our proxy would make the proxy's
/// forwarders point at themselves.
fn ensure_winmm_orig(
    game_dir: &Path,
    staged: &[(&FileEntry, PathBuf)],
    committed: &mut Vec<Committed>,
) -> Result<WinmmOrig> {
    let placed_winmm = staged.iter().any(|(fe, _)| {
        Path::new(&fe.dest)
            .file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case("winmm.dll"))
    });
    if !placed_winmm {
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
