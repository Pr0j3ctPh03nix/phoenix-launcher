//! The mutating install + uninstall.
//!
//! Install runs in two phases so a real game folder is never left half-updated:
//!   0. interlock: if any file we're about to touch is locked (the game holds its loaded DLLs /
//!      mmapped VPKs open), refuse with a typed GameRunning error — before downloading a byte;
//!   1. obtain every changed file — from the local asset cache when its hash matches, else a
//!      streaming download (a small pool fetches files in parallel; an interrupted .part is
//!      resumed, never restarted) that is verified (sha256 + size) — and stage it on the same
//!      volume; nothing under the game is touched yet;
//!   2. commit: back up each existing target, atomically move the staged file in, apply removals
//!      (manifest remove[] + orphaned option files), write state. Any failure in phase 2 rolls
//!      back every step already taken.
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
use crate::downloader::NetKind;
use crate::manifest::{Bundle, FileEntry, Manifest};
use crate::source::{Resolved, Wire};
use crate::state::{InstalledFile, InstalledState};
use crate::{engine, fslock, verify};

const STAGING_DIR: &str = ".phoenix-staging";
const BACKUP_DIR: &str = ".phoenix-backup";
const VANILLA_DIR: &str = ".phoenix-vanilla";
/// Where a displaced file the USER wrote goes when the vanilla store's one slot for that dest is
/// already holding the genuine stock file. Never restored automatically and never cleaned up: it
/// is the only copy of bytes this launcher did not write, and the alternative was the ephemeral
/// backup, which is deleted on success. `.phoenix*` is already invisible to the extras scan.
const USER_DIR: &str = ".phoenix-yours";
/// Content-addressed asset cache (file name = sha256). Every manifest asset — including unselected
/// variants and disabled toggles — lands here via `warm_cache` (run detached by the shell after a
/// successful install), so a later customization change never re-downloads. Pruned to the current
/// manifest, deleted on uninstall.
const CACHE_DIR: &str = ".phoenix-cache";
/// The base-game pipeline's cache, nested inside `CACHE_DIR` so it shares the volume but not the
/// namespace — the shim's prune and uninstall must never reach a 16 GB game download.
const BASE_CACHE_SUBDIR: &str = "base";
/// LEGACY. Launchers up to 1.4.0 copied `%SystemRoot%\System32\winmm.dll` here, because the shim's
/// winmm.dll proxy forwarded through a local copy. The shim now resolves the system DLL itself at
/// load time, so **nothing creates this any more** — the copy was the single hardcoded exception to
/// this being a data-driven installer, and it was also the launcher's loudest antivirus signal (an
/// unsigned process reading System32 and dropping a Microsoft-signed binary under a
/// non-Microsoft name beside a game exe).
///
/// The name stays because folders installed by an older launcher still hold the file: uninstall
/// must still collect it (`state.winmm_orig_created`), `trust_prev` still reads it as evidence of a
/// prior install, and the extras scan must still not offer it up as an unclaimed file. Clearing it
/// from an existing install is the DIST REPO's job, via the manifest's `remove[]` — that path
/// already backs up, rolls back and refuses to delete bytes the user changed, none of which a
/// hardcoded delete here would do.
const WINMM_ORIG: &str = "game/bin/win64/winmm_orig.dll";
/// sha256 of zero bytes — the only hash an empty file can have (see `obtain_to_cache`).
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

#[derive(Debug)]
pub struct InstallReport {
    pub version: String,
    /// The release tag this install came from.
    pub tag: String,
    pub written: Vec<String>,
    pub removed: Vec<String>,
    pub up_to_date: usize,
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
    /// Dests left in place because what is there is no longer what we installed. Reported, never
    /// silently skipped — "reverted to stock" would be a false statement about a folder that
    /// still holds these.
    pub kept: Vec<String>,
    /// `.phoenix-vanilla/` still holds preserved originals that had nowhere to go (their dests are
    /// occupied by files in `kept`). The folder survives the uninstall so those originals do.
    pub vanilla_kept: bool,
    /// A legacy winmm_orig.dll was collected. Only ever true for folders an older launcher
    /// installed — see WINMM_ORIG.
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
}

struct Ctx {
    game_dir: PathBuf,
    backup_root: PathBuf,
    vanilla_root: PathBuf,
    /// Durable home for a displaced user edit that the vanilla store cannot take — see USER_DIR.
    user_root: PathBuf,
    /// Dests the previous install managed (so an existing target is ours, not a vanilla original).
    prev_dests: HashSet<String>,
    /// Whether `prev_dests` can be believed. False when the state file is gone but the folder
    /// still shows a prior install — then nothing may be promoted to the vanilla store, because
    /// a wrong promotion makes uninstall restore our own shim as "stock".
    trust_prev: bool,
    /// Whether the updater lineage created the legacy winmm_orig.dll. Nothing sets this true any
    /// more (see WINMM_ORIG); it is CARRIED FORWARD so that updating an old install does not make
    /// its uninstall forget to collect the file that install left behind.
    prev_winmm_created: bool,
    /// Dests where an earlier removal restored a preserved vanilla original (state.restored) —
    /// carried into the new state so `plan` keeps treating those files as stock, not ours.
    prev_restored: Vec<String>,
    /// Dests we recorded writing whose bytes are no longer the bytes we wrote. `back_up` stops
    /// calling these ours: the ephemeral backup it would otherwise pick is deleted on success.
    user_changed: HashSet<String>,
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
/// `only`: restrict the run to these dests — the files view's "restore these Phoenix files"
/// selection, and the ONLY way a `Modified`/`Kept` dest is written or removed. An ordinary apply
/// passes `None` and never touches them. Same contract as `install_base`'s parameter, and for the
/// same reason: the plan is recomputed here regardless, so the selection filters a fresh verdict
/// rather than standing in for one.
pub fn install(
    settings: &Settings,
    wire: &Wire,
    progress: engine::Progress,
    cancel: Option<&AtomicBool>,
    only: Option<&HashSet<String>>,
) -> Result<InstallReport> {
    let game_dir = settings.resolve_game_dir()?;
    // The wire opened a release and pinned its tag; the manifest is read THROUGH it, so a source
    // that refuses the trust gate fails over instead of ending the install.
    let release = wire.release();
    let manifest = wire.manifest()?;

    // Prior state distinguishes our files from genuine pre-existing ones, and carries the legacy
    // winmm_orig.dll lineage forward (see WINMM_ORIG).
    let prev = InstalledState::load(&game_dir);
    let prev_dests: HashSet<String> = prev
        .as_ref()
        .map(|s| s.files.iter().map(|f| f.dest.clone()).collect())
        .unwrap_or_default();
    let prev_winmm_created = prev.as_ref().map(|s| s.winmm_orig_created).unwrap_or(false);
    let prev_restored: Vec<String> =
        prev.as_ref().map(|s| s.restored.clone()).unwrap_or_default();
    // No state file + evidence that an install happened here anyway = `prev_dests` is empty but
    // WRONG, and believing it makes `back_up` promote OUR OWN shim into the vanilla store, so
    // uninstall then "restores" Phoenix as if it were stock. See `back_up`.
    //
    // The evidence is every artifact only an install leaves: the asset cache, a vanilla store, and
    // — for folders an older launcher set up — winmm_orig.dll. That last one used to be the whole
    // test; it stopped being created (see WINMM_ORIG), which silently made a state-less folder look
    // pristine. `shim_cache_used` is the durable replacement and a strictly better tombstone: the
    // cache is written by EVERY shim install, where winmm_orig.dll only appeared if the manifest
    // happened to ship a winmm.dll.
    let trust_prev = prev.is_some()
        || (!shim_cache_used(&game_dir)
            && !game_dir.join(WINMM_ORIG).exists()
            && !game_dir.join(VANILLA_DIR).exists());

    // --- what changes ---
    let resolved = engine::resolve(&manifest, &settings.selections);
    let statuses = engine::plan(&game_dir, &resolved, prev.as_ref(), &manifest.remove);
    let up_to_date = statuses.iter().filter(|s| s.action == engine::Action::UpToDate).count();
    // An apply acts on the unattended verdicts. A dest the user changed is acted on only when
    // `only` names it — that naming IS the user checking it back on in the files view, and the
    // caller drops its pin afterwards.
    let acts_on = |s: &engine::FileStatus| match only {
        Some(sel) => sel.contains(&s.dest) && s.action != engine::Action::UpToDate,
        None => s.action.is_unattended(),
    };
    let to_write: Vec<&FileEntry> = resolved
        .iter()
        .filter(|fe| {
            statuses
                .iter()
                .any(|s| s.dest == fe.dest && s.action != engine::Action::Remove && acts_on(s))
        })
        .collect();
    // everything plan wants gone: manifest remove[] entries + orphaned option files
    let removals: Vec<String> = statuses
        .iter()
        .filter(|s| s.action == engine::Action::Remove && acts_on(s))
        .map(|s| s.dest.clone())
        .collect();

    // --- asset cache: seed from files already installed at their manifest hash ---
    let cache = game_dir.join(CACHE_DIR);
    std::fs::create_dir_all(&cache).context("creating the asset cache")?;
    seed_cache(&cache, &game_dir, &resolved, &statuses);

    if to_write.is_empty() && removals.is_empty() {
        // Nothing to change — but a missing/corrupt state file must not lock this folder into
        // "up to date yet not installed" forever: a no-op install still heals it (every resolved
        // file hash-matches, so the set is provably ours to record). Nothing is created here any
        // more, so there is nothing to roll back either.
        //
        // carry the restored-original record (minus any dest the manifest ships again — that file
        // will be displaced normally next time and the record no longer applies)
        let restored: Vec<String> = prev_restored
            .iter()
            .filter(|d| !resolved.iter().any(|f| &f.dest == *d))
            .cloned()
            .collect();
        write_state(&game_dir, &manifest, &resolved, prev_winmm_created, restored)
            .map_err(|e| e.context("could not record the install state"))?;
        // cache warming is the caller's affair (warm_cache, backgroundable) — a heal must
        // return as fast as it healed
        return Ok(InstallReport {
            version: manifest.version.clone(),
            tag: release.tag_name.clone(),
            written: Vec::new(),
            removed: Vec::new(),
            up_to_date,
            manifest,
        });
    }

    // --- interlock: fail fast when the game is running (phase 2 would roll back anyway) ---
    probe_writable(&game_dir, to_write.iter().map(|fe| &fe.dest).chain(removals.iter()))?;

    // --- phase 1a: obtain (cache-first, else parallel streaming download) — game untouched ---
    let staging = game_dir.join(STAGING_DIR);
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).context("creating the staging directory")?;
    obtain_all(&cache, wire, &to_write, &manifest, progress, cancel)?;

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
        user_root: game_dir.join(USER_DIR),
        // Derived from the same plan the write set came from, so `back_up` and `plan` can never
        // disagree about whose bytes a file holds — computed once here rather than re-hashed
        // per displaced file inside the commit.
        // `plan` visits every resolved dest and every still-present orphan, which between them is
        // every recorded dest that could be displaced — so its verdicts are complete here and
        // nothing needs re-hashing.
        user_changed: statuses
            .iter()
            .filter(|s| s.action.is_users())
            .map(|s| s.dest.clone())
            .collect(),
        prev_dests,
        trust_prev,
        prev_winmm_created,
        prev_restored,
    };
    let job = CommitJob { staged: &staged, removals: &removals, resolved: &resolved, manifest: &manifest };
    let mut committed: Vec<Committed> = Vec::new();

    match commit(&ctx, &job, &mut committed) {
        Ok((written, removed)) => {
            let _ = std::fs::remove_dir_all(&staging);
            // The commit succeeded, so nothing will ever roll back to these — they exist only as
            // rollback material for the run that just finished. Left behind they accumulated one
            // full copy of every replaced file PER RELEASE, forever, inside the game folder.
            // (Preserved vanilla originals live in VANILLA_DIR and are untouched by this.)
            let _ = std::fs::remove_dir_all(game_dir.join(BACKUP_DIR));
            // caching the remaining assets (unselected variants, disabled toggles) is NOT done
            // here — it can be hundreds of MB of optional content and must not hold the install
            // result hostage. The shell runs warm_cache detached after this returns.
            //
            // The anti-rollback floor advances HERE, on a committed install, not when the manifest
            // was merely read. Ratcheting on a check meant a release the user only looked at — or
            // was offered and declined — floored them permanently, so yanking a bad release left
            // every client that had polled once unable to accept the good one that preceded it.
            engine::ratchet_installed(settings, crate::trust::Payload::Mod, &manifest);
            Ok(InstallReport {
                version: manifest.version.clone(),
                tag: release.tag_name.clone(),
                written,
                removed,
                up_to_date,
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
) -> Result<(Vec<String>, Vec<String>)> {
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

    write_state(&ctx.game_dir, job.manifest, job.resolved, ctx.prev_winmm_created, restored_dests)?;

    Ok((written, removed))
}

/// Record the install: version + the resolved (effective) set (selected variants and enabled
/// toggles included) + the legacy winmm_orig lineage + the restored-original record (see state.rs).
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
/// A file big enough that hashing it visibly stalls a per-FILE counter, and the tick spacing used
/// while reading one. The game ships several VPKs in the hundreds of megabytes; on a cold verify
/// each is tens of seconds during which the only honest thing to show is bytes.
const BIG_FILE_BYTES: u64 = 32 * 1024 * 1024;
const BIG_FILE_TICK_BYTES: u64 = 8 * 1024 * 1024;

/// Transient-failure retries per asset before the run fails. The base game is 4,600+ requests
/// against GitHub's CDN, which throws sporadic 5xxs — without retries the odds of one hiccup
/// killing a 15 GB run are terrible, and the user becomes the retry loop (clicking Resume for
/// every blip). Retries cost nothing wrong: the `.part` resume machinery continues each attempt
/// from the bytes already fetched.
const DL_RETRIES: u32 = 3;
/// First retry delay; doubles per attempt (1 s, 2 s, 4 s live). Milliseconds in tests — a unit
/// test must not sleep out a real backoff schedule.
#[cfg(not(test))]
const RETRY_BACKOFF_MS: u64 = 1000;
#[cfg(test)]
const RETRY_BACKOFF_MS: u64 = 1;
/// Backoff sleeps in slices this long, polling the chunk callback between them — a Stop pressed
/// during a 4-second backoff must land now, not after the nap.
const RETRY_SLICE_MS: u64 = 100;

/// Worth retrying? Only failures the next attempt can plausibly not repeat: transport drops and
/// server-side errors (5xx, 429 rate limit, 408 timeout). A 4xx is a fact about the request, a
/// verification failure is a fact about the source, and a callback abort (cancel / sibling
/// failure) is an instruction — retrying any of those fights the truth or the user.
fn transient_net_failure(e: &anyhow::Error) -> bool {
    e.chain().any(|c| match c.downcast_ref::<NetKind>() {
        Some(NetKind::Transport) => true,
        Some(NetKind::Status(s)) => *s >= 500 || *s == 429 || *s == 408,
        None => false,
    })
}

/// How one source's attempt at an asset ended.
///
/// `Rejected` and `Unreachable` both advance to the next source and differ in exactly one thing:
/// what happens to the `.part`. That distinction is the whole reason this is an enum rather than a
/// `Result` — see `obtain_to_cache`.
enum SourceEnd {
    /// Fetched and verified: the `.part` holds the asset.
    Done,
    /// The source CONTRADICTED the signed manifest — wrong bytes, or more bytes than the manifest
    /// promised. A fact about the source, so advance; and the prefix it wrote is poison, never a
    /// resume source for the next one.
    Rejected(anyhow::Error),
    /// The exchange never completed: transport drops, a refusal, an asset this source does not
    /// carry. Says nothing about bytes already on disk, so the `.part` survives.
    Unreachable(anyhow::Error),
    /// The CALLER told us to stop — a user cancel, or a sibling worker's failure. Not a source
    /// failure at all: an instruction. It must never advance, or cancelling a 7.9 GiB install
    /// would restart it against the next mirror.
    Aborted(anyhow::Error),
}

/// One unit of network acquisition — an ASSET, not a file. Since manifest schema 3 the two are
/// no longer the same thing: a raw entry maps 1:1, but a bundle is one asset carrying up to
/// thousands of members. Jobs are what the download pool schedules, so members needed from one
/// bundle are grouped into ONE job — without that, needing N files from a bundle would download
/// the same multi-GB asset N times.
enum Acq<'a> {
    /// A named release asset, fetched into `cache/<sha256>` (route 2 — schema-2 behaviour).
    Raw { name: &'a str, sha256: &'a str, size: u64 },
    /// A zero-byte entry — materialized locally, never on the wire (route 1; GitHub refuses to
    /// host zero-byte assets, so none exists even when the entry carries a leftover `name`).
    Empty { sha256: &'a str },
    /// A packed bundle (route 3): fetch `cache/<psha256>`, decode, split by member sizes, keep
    /// the `wanted` members as ordinary content-addressed cache entries.
    Bundle { bundle: &'a Bundle, wanted: Vec<(&'a str, u64)> },
}

impl Acq<'_> {
    /// Bytes this job puts on the WIRE — the LPT sort key, the progress currency, and one half
    /// of every user-facing byte total (R7: `psize` is what crosses the network, `size` is what
    /// lands on disk; they differ per bundle and are never interchangeable).
    fn wire_cost(&self) -> u64 {
        match self {
            Acq::Raw { size, .. } => *size,
            Acq::Empty { .. } => 0,
            Acq::Bundle { bundle, .. } => bundle.psize,
        }
    }

    /// The release asset this job needs, if any — the preflight checks these against the
    /// release's asset list before a byte moves.
    fn asset_name(&self) -> Option<&str> {
        match self {
            Acq::Raw { name, .. } => Some(name),
            Acq::Empty { .. } => None,
            Acq::Bundle { bundle, .. } => Some(&bundle.name),
        }
    }
}

/// Group `wants` (deduplicated by content hash — two dests sharing one hash acquire once) into
/// acquisition jobs, resolving each entry by the spec's route order: empty → named asset →
/// bundle. The B3 guarantee (validated at parse) makes the bundle lookup total; the bail is the
/// belt to that suspender.
fn build_acqs<'a>(
    bundles: &'a [Bundle],
    wants: impl Iterator<Item = (Option<&'a str>, &'a str, u64)>,
) -> Result<Vec<Acq<'a>>> {
    let member_of: HashMap<&str, usize> = bundles
        .iter()
        .enumerate()
        .flat_map(|(i, b)| b.members.iter().map(move |m| (m.as_str(), i)))
        .collect();
    let mut seen = HashSet::new();
    let mut acqs = Vec::new();
    let mut wanted_of: HashMap<usize, Vec<(&str, u64)>> = HashMap::new();
    for (name, sha256, size) in wants {
        if !seen.insert(sha256) {
            continue;
        }
        if size == 0 {
            acqs.push(Acq::Empty { sha256 });
        } else if let Some(name) = name {
            acqs.push(Acq::Raw { name, sha256, size });
        } else if let Some(&i) = member_of.get(sha256) {
            wanted_of.entry(i).or_default().push((sha256, size));
        } else {
            bail!("entry {sha256} has no asset name and is in no bundle");
        }
    }
    acqs.extend(
        wanted_of.into_iter().map(|(i, wanted)| Acq::Bundle { bundle: &bundles[i], wanted }),
    );
    Ok(acqs)
}

/// The byte totals of an acquisition set, in R7's two currencies plus the preflight demand:
/// (wire, disk, need).
///   wire  what crosses the network — raw assets by size, each needed bundle's `psize` ONCE
///         (needing one member costs the whole bundle). Bars, ETAs, "downloaded so far".
///   disk  the decoded content that lands, unique by hash. The installed footprint.
///   need  what the disk preflight demands (sans margin): `disk` plus the packed transient —
///         a download worker holds at most one packed bundle at a time (it decodes and deletes
///         before taking its next job), so at worst the DL_WORKERS largest packed assets are
///         alive in the cache on top of the decoded content.
fn costs_of(acqs: &[Acq]) -> (u64, u64, u64) {
    let wire = acqs.iter().map(Acq::wire_cost).sum();
    let disk = acqs
        .iter()
        .map(|a| match a {
            Acq::Raw { size, .. } => *size,
            Acq::Empty { .. } => 0,
            Acq::Bundle { wanted, .. } => wanted.iter().map(|(_, s)| s).sum(),
        })
        .sum::<u64>();
    let mut psizes: Vec<u64> = acqs
        .iter()
        .filter_map(|a| match a {
            Acq::Bundle { bundle, .. } => Some(bundle.psize),
            _ => None,
        })
        .collect();
    psizes.sort_unstable_by_key(|p| std::cmp::Reverse(*p));
    let transient: u64 = psizes.iter().take(DL_WORKERS).sum();
    (wire, disk, disk + transient)
}

/// `costs_of` over a base plan's to-write set — the numbers behind the download/repair confirms,
/// computed with the exact math `install_base`'s own preflight uses.
pub fn base_costs(manifest: &Manifest, statuses: &[BaseStatus]) -> Result<(u64, u64, u64)> {
    base_costs_for(manifest, statuses.iter().filter(|s| s.action.writes()))
}

/// `base_costs` over an arbitrary subset — what a PARTIAL repair costs. Bundles make this
/// non-additive (needing one member costs the whole packed asset, needing a second member of the
/// same bundle costs nothing more), which is why a caller cannot get this by summing per-file
/// numbers and must hand the whole subset over at once.
pub fn base_costs_for<'a>(
    manifest: &Manifest,
    statuses: impl Iterator<Item = &'a BaseStatus>,
) -> Result<(u64, u64, u64)> {
    let acqs = build_acqs(
        &manifest.bundles,
        statuses.map(|s| (s.entry.name.as_deref(), s.entry.sha256.as_str(), s.entry.size)),
    )?;
    Ok(costs_of(&acqs))
}

/// Which download a file rides in, and what that download costs on the wire.
///
/// Exists so the files view can total a live selection WITHOUT a round trip per checkbox: two
/// entries sharing a key are one fetch, so "N selected · X GB" is a sum over DISTINCT keys — the
/// same rule `costs_of` applies backend-side, expressed as data the UI can carry. Built once and
/// reused; the member index alone is O(every bundle member).
pub struct WireIndex<'a> {
    member_of: HashMap<&'a str, &'a Bundle>,
}

impl<'a> WireIndex<'a> {
    pub fn new(manifest: &'a Manifest) -> Self {
        Self {
            member_of: manifest
                .bundles
                .iter()
                .flat_map(|b| b.members.iter().map(move |m| (m.as_str(), b)))
                .collect(),
        }
    }

    /// `(key, wire bytes)`. `None` = costs nothing to obtain (a zero-byte entry is materialized
    /// locally, never fetched — see `Acq::Empty`). Raw entries key by CONTENT hash rather than
    /// asset name because that is what `build_acqs` deduplicates on: two dests sharing bytes
    /// download once.
    pub fn of(&self, fe: &FileEntry) -> (Option<String>, u64) {
        if fe.size == 0 {
            (None, 0)
        } else if fe.name.is_some() {
            (Some(fe.sha256.clone()), fe.size)
        } else if let Some(b) = self.member_of.get(fe.sha256.as_str()) {
            (Some(format!("bundle:{}", b.name)), b.psize)
        } else {
            // B3 makes this unreachable for a validated manifest; reporting it as free is the
            // harmless reading, and `install_base`'s own preflight bails on it properly.
            (None, 0)
        }
    }
}

/// Fetch every to-write file into the asset cache with a small worker pool. Errors fail the
/// phase: the first error wins — remaining workers stop early AND in-flight streams abort at
/// their next chunk (the .part they leave is resumed on the next run), so a dead asset never
/// waits minutes for a 500 MB neighbor to finish before surfacing. Progress `current` counts
/// COMPLETED jobs while `item`/`bytes` track whichever asset ticked most recently.
fn obtain_all(
    cache: &Path,
    wire: &Wire,
    to_write: &[&FileEntry],
    manifest: &Manifest,
    progress: engine::Progress,
    cancel: Option<&AtomicBool>,
) -> Result<()> {
    obtain_all_tagged(cache, wire, to_write, manifest, progress, "install", cancel)
}

/// `obtain_all` with the progress `op` tag and an external cancel flag injected — the base-game
/// path reports as its own operation ("game") and is user-cancellable mid-download (a shim
/// install is seconds; a 9 GB base install is not).
// two internal callers; a params struct would be ceremony without reuse
#[allow(clippy::too_many_arguments)]
fn obtain_all_tagged(
    cache: &Path,
    wire: &Wire,
    to_write: &[&FileEntry],
    manifest: &Manifest,
    progress: engine::Progress,
    op: &'static str,
    cancel: Option<&AtomicBool>,
) -> Result<()> {
    let mut jobs = build_acqs(
        &manifest.bundles,
        to_write.iter().map(|fe| (fe.name.as_deref(), fe.sha256.as_str(), fe.size)),
    )?;
    // Largest WIRE cost first (LPT scheduling): the multi-GB assets start streaming immediately
    // and run for most of the download while the other workers chew through the small tail — in
    // manifest (alphabetical) order, a giant asset picked up near the end ran ALONE long after
    // every other worker went idle, adding its whole transfer time to the wall clock. Also
    // steadies the UI: the byte rate reaches link speed in the first seconds, so the ETA is
    // honest early.
    jobs.sort_unstable_by_key(|a| std::cmp::Reverse(a.wire_cost()));
    let total = jobs.len() as u64;
    // every dest that shares a hash — completion ticks fan out to all of them, so each UI file
    // row settles even when two dests share one asset (or one bundle member)
    let mut dests_of: HashMap<&str, Vec<&str>> = HashMap::new();
    for fe in to_write {
        dests_of.entry(fe.sha256.as_str()).or_default().push(fe.dest.as_str());
    }
    // split boundaries for every bundle member, from the WHOLE manifest — a repair may want two
    // members of a bundle whose other two thousand still have to be sized to be skipped over
    let sizes: HashMap<&str, u64> = manifest.payload_entries().map(|(_, s, z)| (s, z)).collect();

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
                let job = &jobs[i];
                // What a byte tick names, and its full extent (known from the manifest, so the
                // bar has its span from the very first tick). A raw asset ticks per dest; a
                // bundle ticks as ONE item under its asset name — fanning every packed chunk to
                // two thousand member dests would multiply the event stream by the member count
                // for no information.
                let (items, wire_cost): (Vec<&str>, u64) = match job {
                    Acq::Raw { sha256, size, .. } => (dests_of[sha256].clone(), *size),
                    Acq::Bundle { bundle, .. } => (vec![bundle.name.as_str()], bundle.psize),
                    Acq::Empty { .. } => (Vec::new(), 0),
                };
                let tick = |current: u64, bytes_done: u64, bytes_total: u64, is_done: bool| {
                    for item in &items {
                        report(engine::OpProgress {
                            op,
                            phase: "fetch",
                            current,
                            total,
                            item: Some((*item).to_string()),
                            bytes_done: Some(bytes_done),
                            bytes_total: Some(bytes_total),
                            done: is_done,
                        });
                    }
                };
                if !items.is_empty() {
                    tick(done.load(Ordering::Relaxed), 0, wire_cost, false);
                }
                let mut last = 0u64;
                let mut chunk = |d: u64, t: Option<u64>| {
                    if abort.load(Ordering::Relaxed) || cancelled() {
                        return false; // a sibling failed or the user cancelled — stop this stream
                    }
                    // abs_diff, NOT d - last: this closure spans every retry attempt of the
                    // job, and an attempt can restart BELOW the previous high-water mark (a
                    // poisoned .part discarded, a server declining the Range) — plain
                    // subtraction underflows (a debug-build panic), and a monotonic check
                    // would mute the bar until the old mark was re-passed.
                    if d.abs_diff(last) >= PROGRESS_GRAIN || t == Some(d) {
                        last = d;
                        tick(done.load(Ordering::Relaxed), d, t.unwrap_or(wire_cost), false);
                    }
                    true
                };
                let mut keep_going = || !(abort.load(Ordering::Relaxed) || cancelled());
                match obtain_acq(cache, wire, job, &sizes, &mut chunk, &mut keep_going) {
                    Ok(()) => {
                        let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                        match job {
                            Acq::Raw { size, .. } => tick(d, *size, *size, true),
                            // dests of a zero-byte entry still owe the UI their completion
                            Acq::Empty { sha256 } => {
                                for dest in &dests_of[*sha256] {
                                    report(engine::OpProgress {
                                        op,
                                        phase: "fetch",
                                        current: d,
                                        total,
                                        item: Some((*dest).to_string()),
                                        bytes_done: Some(0),
                                        bytes_total: Some(0),
                                        done: true,
                                    });
                                }
                            }
                            Acq::Bundle { bundle, wanted } => {
                                // settle the bundle's own bar — done stays FALSE on this item:
                                // file counters count DESTS, and the bundle is not a dest
                                tick(d, bundle.psize, bundle.psize, false);
                                // …then complete every dest the bundle just satisfied. Zero
                                // bytes on purpose: the wire bytes were already accounted under
                                // the bundle item, and double-counting them per member would
                                // run the aggregate bar past its total.
                                for (sha, _) in wanted {
                                    for dest in &dests_of[*sha] {
                                        report(engine::OpProgress {
                                            op,
                                            phase: "fetch",
                                            current: d,
                                            total,
                                            item: Some((*dest).to_string()),
                                            bytes_done: Some(0),
                                            bytes_total: Some(0),
                                            done: true,
                                        });
                                    }
                                }
                            }
                        }
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

/// Obtain one acquisition job into the cache. Raw assets verify and land at `cache/<sha256>`;
/// bundles land packed at `cache/<psha256>` (same resume/retry machinery), are decoded into
/// their wanted members, and the packed entry is deleted — it held wire-sized bytes with no
/// further use, inside the game folder.
///
/// `chunk` is the byte-tick/abort line for the download; `keep_going` is the abort line for the
/// decode (which must stay off the byte accounting — its progress is not wire progress).
fn obtain_acq(
    cache: &Path,
    wire: &Wire,
    acq: &Acq,
    sizes: &HashMap<&str, u64>,
    chunk: crate::downloader::ChunkProgress,
    keep_going: &mut dyn FnMut() -> bool,
) -> Result<()> {
    match acq {
        Acq::Empty { sha256 } => materialize_empty(cache, sha256).map(drop),
        Acq::Raw { name, sha256, size } => {
            obtain_to_cache(cache, wire, name, sha256, *size, chunk, keep_going).map(drop)
        }
        Acq::Bundle { bundle, wanted } => {
            // One flow decodes a given bundle at a time, process-wide: obtain_to_cache guards
            // the packed DOWNLOAD, but a background warm and an apply could otherwise both
            // finish (or cache-hit) the same packed entry and then write the same member
            // entries concurrently. Keyed distinctly from the download guard — same key would
            // self-deadlock on the nested acquire.
            let _guard = Inflight::acquire(&format!("bundle:{}", bundle.psha256));
            // decided under the guard: whoever held it before us may have extracted everything
            let missing: HashSet<&str> = wanted
                .iter()
                .filter(|(sha, size)| !cache_ok(&cache.join(sha), sha, *size))
                .map(|(sha, _)| *sha)
                .collect();
            if missing.is_empty() {
                return Ok(());
            }
            // R3 rides on the existing verify: obtain_to_cache renames the packed asset into
            // the cache only after psize + psha256 check out — nothing unverified is decoded
            let packed = obtain_to_cache(
                cache,
                wire,
                &bundle.name,
                &bundle.psha256,
                bundle.psize,
                chunk,
                keep_going,
            )?;
            extract_members(cache, &packed, bundle, &missing, sizes, keep_going)?;
            let _ = std::fs::remove_file(&packed);
            Ok(())
        }
    }
}

/// Split a verified packed bundle into its members (R4): stream-decode, count bytes against
/// each member's manifest size, keep the wanted ones as content-addressed cache entries. Decode
/// is strictly sequential — reaching member 900 means consuming members 0–899 — so unneeded
/// members are passed through and discarded (zstd decodes >1 GB/s; there is no seeking).
///
/// Any defect found here — a member hash mismatch, a stream that runs short or long (B4) — is a
/// PRODUCER defect (R5): `psha256` already verified, so the wire carried exactly the bytes the
/// manifest asked for and refetching would reproduce them. Fail loudly, never retry; the errors
/// carry no NetKind on purpose, which is what keeps the retry loop away from them. The packed
/// cache entry is left in place so the next attempt fails fast instead of re-downloading
/// gigabytes toward the same wall.
fn extract_members(
    cache: &Path,
    packed: &Path,
    bundle: &Bundle,
    wanted: &HashSet<&str>,
    sizes: &HashMap<&str, u64>,
    keep_going: &mut dyn FnMut() -> bool,
) -> Result<()> {
    use sha2::Digest;
    use std::io::{Read, Write};
    let file = std::fs::File::open(packed)
        .with_context(|| format!("opening the downloaded bundle {}", bundle.name))?;
    let mut dec = zstd::stream::read::Decoder::new(file)
        .with_context(|| format!("starting to decode bundle {}", bundle.name))?;
    // 256 KiB like every other streaming path in this codebase (github.rs, verify.rs)
    let mut buf = vec![0u8; 256 * 1024];
    for m in &bundle.members {
        let size = *sizes
            .get(m.as_str())
            .with_context(|| format!("bundle {} member {m} matches no entry", bundle.name))?;
        // a member already in the cache is skipped like an unwanted one — its bytes still have
        // to be consumed to reach the members behind it
        let keep = wanted.contains(m.as_str());
        let mut out = if keep {
            let tmp = cache.join(format!("{m}.tmp"));
            Some((
                std::fs::File::create(&tmp)
                    .with_context(|| format!("creating a cache entry for {m}"))?,
                tmp,
                sha2::Sha256::new(),
            ))
        } else {
            None
        };
        let mut left = size;
        while left > 0 {
            if !keep_going() {
                // the `_` pattern drops the open handle before the delete — Windows insists
                if let Some((_, tmp, _)) = out {
                    let _ = std::fs::remove_file(&tmp);
                }
                bail!("bundle decode aborted");
            }
            let want = buf.len().min(left as usize);
            let n = dec
                .read(&mut buf[..want])
                .with_context(|| format!("decoding bundle {}", bundle.name))?;
            if n == 0 {
                if let Some((_, tmp, _)) = out {
                    let _ = std::fs::remove_file(&tmp);
                }
                bail!(
                    "bundle {} ran out mid-member: the decoded stream is shorter than the \
                     manifest declares — a broken release, refetching cannot fix it",
                    bundle.name
                );
            }
            if let Some((f, _, hasher)) = &mut out {
                f.write_all(&buf[..n])
                    .with_context(|| format!("writing a cache entry for {m}"))?;
                hasher.update(&buf[..n]);
            }
            left -= n as u64;
        }
        if let Some((f, tmp, hasher)) = out {
            drop(f);
            let got = hex::encode(hasher.finalize());
            if got != *m {
                let _ = std::fs::remove_file(&tmp);
                bail!(
                    "bundle {} member decoded with hash {got}, manifest says {m} — the packed \
                     asset verified clean, so this is a broken release; not retrying",
                    bundle.name
                );
            }
            std::fs::rename(&tmp, cache.join(m.as_str()))
                .with_context(|| format!("caching bundle member {m}"))?;
        }
    }
    // B4: nothing after the last member — trailing bytes mean the split above was built on a lie
    if dec.read(&mut buf[..1]).unwrap_or(1) != 0 {
        bail!(
            "bundle {} decodes past its declared members — a broken release, refetching cannot \
             fix it",
            bundle.name
        );
    }
    Ok(())
}

/// Materialize the one file that never crosses the wire: a zero-byte entry. Not an optimization
/// — GitHub refuses to host a zero-byte release asset (422), so the publisher COULD not upload
/// one even though the game genuinely contains such files. A size of 0 paired with any other
/// hash is a corrupt manifest, not an empty file.
fn materialize_empty(cache: &Path, sha256: &str) -> Result<PathBuf> {
    let cpath = cache.join(sha256);
    if sha256 != EMPTY_SHA256 {
        bail!("manifest lists a 0-byte entry with sha256 {sha256}, not the empty hash");
    }
    if !cpath.exists() {
        std::fs::write(&cpath, b"").context("creating an empty cache entry")?;
    }
    Ok(cpath)
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

/// Path to a verified cache entry for an asset: cache hit, else streaming download + verify, from
/// whichever source the wire is on — and the next one if that fails.
fn obtain_to_cache(
    cache: &Path,
    wire: &Wire,
    name: &str,
    sha256: &str,
    size: u64,
    chunk: crate::downloader::ChunkProgress,
    keep_going: &mut dyn FnMut() -> bool,
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
    let tmp = cache.join(format!("{sha256}.part"));

    // Walk the wire. An asset the current source cannot deliver FAILS THAT SOURCE OVER and is
    // asked of the next; only when the ranking is spent does the run fail. That is the difference
    // between a 7.9 GiB base install dying on one bad asset and finishing from a mirror.
    //
    // The generation is what keeps eight workers from causing eight failovers: it is read with the
    // source and handed back with the failure, and `Wire::fail` ignores a report against a
    // generation somebody has already moved past — that worker simply re-reads and retries against
    // whatever is current now.
    let mut last;
    loop {
        // An abort landing between sources is still an abort: advancing here would restart a
        // cancelled install against the next mirror.
        if !keep_going() {
            return Err(anyhow!("download aborted"));
        }
        let (gen, source, _release) = wire.current();
        match attempt_source(&source, &tmp, name, sha256, size, chunk, keep_going) {
            SourceEnd::Done => {
                return std::fs::rename(&tmp, &cpath)
                    .map(|()| cpath)
                    .with_context(|| format!("caching {name}"));
            }
            // An instruction, not a failure — nothing to fall through to.
            SourceEnd::Aborted(e) => return Err(e),
            SourceEnd::Rejected(e) => {
                // These bytes came from a source that has just been shown to contradict the signed
                // manifest. Resume is per-ASSET, so leaving them would hand the next source a
                // prefix written by a broken (or hostile) one and let that corruption survive the
                // failover — the whole-file hash would catch it, but only after the next source
                // had re-fetched the entire remainder to reach the same wall.
                let _ = std::fs::remove_file(&tmp);
                last = Some(e);
            }
            SourceEnd::Unreachable(e) => {
                // The opposite case, and the reason the two are distinguished at all: a transport
                // failure says nothing about the bytes already written, and on the links this
                // feature exists for those bytes can be gigabytes. They are kept as the next
                // source's resume prefix — safely, because the final whole-file hash still covers
                // them, and `attempt_source`'s clean-restart budget (reset per source) spends one
                // from-zero retry before an inherited prefix is allowed to indict the source that
                // merely finished it.
                last = Some(e);
            }
        }
        if wire.fail(gen).is_err() {
            return Err(match last {
                Some(e) => e.context(format!("downloading {name}: every download source failed")),
                None => anyhow!("no download source is configured"),
            });
        }
    }
}

/// One source's whole attempt at one asset: resume, retry with backoff, verify. Returns HOW it
/// ended, because the caller's next move — advance, stop, or keep the `.part` — depends on which.
fn attempt_source(
    source: &Resolved,
    tmp: &Path,
    name: &str,
    sha256: &str,
    size: u64,
    chunk: crate::downloader::ChunkProgress,
    keep_going: &mut dyn FnMut() -> bool,
) -> SourceEnd {
    let dl = source.dl();
    let Some(asset) = source.asset_for(name, sha256, size) else {
        // Nothing this source can even address — a permanent fact about IT, not about the release.
        return SourceEnd::Unreachable(anyhow!("the release has no asset named {name}"));
    };
    let mut attempt = 0u32;
    // A verification failure that followed a RESUME indicts the prefix we inherited, not the
    // source — spend one clean restart before calling the release broken. See the bail below.
    // PER SOURCE, deliberately: the prefix a failover inherits was written by the PREVIOUS source,
    // so a budget already spent elsewhere would let one source's corruption condemn the next one
    // that is merely finishing its file.
    let mut may_restart_clean = true;
    loop {
        // an interrupted attempt (an earlier run's, or this loop's previous try) left a .part —
        // resume from its length instead of restarting.
        // ...but a .part that already reached the full length can never be resumed: the Range
        // would start at EOF and the CDN answers 416, which is an error, which keeps the .part —
        // leaving the asset permanently undownloadable. That state is reachable without anything
        // exotic (a completed transfer whose rename into the cache failed, or a cancel landing
        // on the last chunk of a file), so treat an over-long .part as poison and start clean.
        let mut resume_from = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
        if resume_from >= size {
            let _ = std::fs::remove_file(&tmp);
            resume_from = 0;
        }
        // The signed size is a trust input like the hash: a source that keeps sending past it is
        // lying about what this asset is, and an endless body has to be cut off DURING the
        // transfer — not discovered after it has already filled the disk. Piggybacks on the
        // existing abort line rather than growing the `Downloader` trait: `written` already
        // includes any resumed prefix (every impl counts it that way), so this needs no extra
        // bookkeeping to stay correct across a resume.
        let mut capped = false;
        let result = {
            let mut guarded = |written: u64, total: Option<u64>| -> bool {
                if written > size {
                    capped = true;
                    return false;
                }
                chunk(written, total)
            };
            dl.download_to(&asset, tmp, resume_from, &mut guarded)
        };
        match result {
            Ok((got_size, got_sha)) => {
                if got_size == size && got_sha == sha256 {
                    return SourceEnd::Done;
                }
                // Wrong bytes. Resume cannot help either way, so the .part goes.
                let _ = std::fs::remove_file(tmp);
                // Whose fault is it? If this attempt RESUMED, the hash covers a prefix we did not
                // fetch and did not verify — and that prefix has a mundane way of being wrong that
                // has nothing to do with the release: NTFS journals metadata, not data, so a
                // power loss can leave a `.part` whose LENGTH persisted while its tail never made
                // it out of the cache. Resuming past that hashes zeros, and the mismatch only
                // surfaces after the whole remainder has been re-fetched — a 16 GB run spent to
                // reach a wall the next run would walk straight back into. So spend one restart
                // from zero before blaming the source. (A server that DECLINED the Range answered
                // from zero already and this restart is redundant; it costs one extra download in
                // a case that was failing anyway, and there is no way to tell from here.)
                if resume_from > 0 && may_restart_clean {
                    may_restart_clean = false;
                    continue;
                }
                // Fetched whole and still wrong: that is a fact about the source, not about us.
                // Not retried against THIS source — `transient_net_failure` deliberately excludes
                // verification, and re-asking a settled question just burns the link. The next
                // source is asked instead, from zero.
                return SourceEnd::Rejected(anyhow!(
                    "verification failed for {name}: manifest {size}b/{sha256} got {got_size}b/{got_sha}"
                ));
            }
            Err(e) => {
                // An abort is an INSTRUCTION (Stop, or a sibling worker's failure), not a
                // transport problem, and it can land mid-attempt — inside `download_to`, which
                // fails the transfer the moment the chunk callback says stop. Asked of the abort
                // line itself rather than sniffed out of the error, so no impl's wording can
                // silently turn a cancel into a failover. `capped` is checked first: it rides the
                // same callback but is a fact about the SOURCE, not an instruction from us.
                if !capped && !keep_going() {
                    return SourceEnd::Aborted(e);
                }
                // keep the .part — the next attempt (or run) resumes from it — unless it is now
                // full-length or longer, which no future Range request could extend
                if std::fs::metadata(tmp).map(|m| m.len() >= size).unwrap_or(false) {
                    let _ = std::fs::remove_file(tmp);
                }
                // A cap violation is not an ordinary abort even though it rides the same
                // callback (Stop, a sibling's failure) — it is a distinct fact, "the source sent
                // more bytes than the signed manifest promised", and must say so rather than
                // read as a cancel, and must never be retried as though it were a network hiccup
                // (it isn't: no `NetKind` is in this chain, so `transient_net_failure` already
                // says no — this bails explicitly so the reason is not left to fall out of that).
                if capped {
                    return SourceEnd::Rejected(anyhow!(
                        "{name}: the source sent more than the signed {size} bytes — refusing \
                         (the host is misbehaving or hostile)"
                    ));
                }
                attempt += 1;
                if attempt > DL_RETRIES || !transient_net_failure(&e) {
                    // Spent, or a failure no retry can fix (a 4xx, an unreadable release). Either
                    // way the next SOURCE may still have it, and the bytes on disk are untouched
                    // by this verdict — `Unreachable` keeps them.
                    let n = attempt;
                    return SourceEnd::Unreachable(e.context(if n > 1 {
                        format!("downloading {name} (after {n} attempts)")
                    } else {
                        format!("downloading {name}")
                    }));
                }
                // Exponential backoff, slept in slices with the chunk callback polled between
                // them: the callback is the cancel line (Stop, a sibling's failure), and a
                // cancel during a multi-second nap must land now — the sleeper reports the
                // bytes it already has, which the grain check keeps out of the UI.
                let written = std::fs::metadata(tmp).map(|m| m.len()).unwrap_or(0);
                let delay = RETRY_BACKOFF_MS << (attempt - 1);
                for _ in 0..delay.div_ceil(RETRY_SLICE_MS) {
                    if !chunk(written, None) {
                        return SourceEnd::Aborted(anyhow!("download aborted"));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(
                        RETRY_SLICE_MS.min(delay),
                    ));
                }
            }
        }
    }
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
pub fn warm_cache(settings: &Settings, wire: &Wire) {
    // captured before any work: every check below asks "has anyone cancelled since I started",
    // so a cancel can never be lost and a later install never resurrects this run
    let epoch = WARM_EPOCH.load(Ordering::Relaxed);
    let cancelled = || WARM_EPOCH.load(Ordering::Relaxed) != epoch;
    let Ok(game_dir) = settings.resolve_game_dir() else { return };
    let Ok(manifest) = wire.manifest() else { return };
    if cancelled() {
        return;
    }
    let cache = game_dir.join(CACHE_DIR);
    if std::fs::create_dir_all(&cache).is_err() {
        return;
    }
    prefetch_all(&cache, wire, &manifest, &cancelled);
    if !cancelled() {
        prune_cache(&cache, &manifest);
    }
}

/// Download every not-yet-cached manifest asset. Best-effort: a failed asset is skipped (it will
/// download on demand when actually selected) so an optional extra can't fail the warm. Bundled
/// entries warm by the bundle, through the same job builder the install uses.
fn prefetch_all(
    cache: &Path,
    wire: &Wire,
    manifest: &Manifest,
    cancelled: &dyn Fn() -> bool,
) {
    let sizes: HashMap<&str, u64> = manifest.payload_entries().map(|(_, s, z)| (s, z)).collect();
    let Ok(acqs) = build_acqs(&manifest.bundles, manifest.payload_entries()) else { return };
    for acq in acqs {
        if cancelled() {
            return;
        }
        // obtain_acq itself starts from the cache check (evicting a corrupt entry), so a warm
        // over an already-warm cache costs stats. The chunk callback doubles as the cancel
        // line: an uninstall's cancel_warm aborts the stream mid-file instead of letting a
        // huge optional asset finish downloading first.
        let _ = obtain_acq(
            cache,
            wire,
            &acq,
            &sizes,
            &mut |_, _| !cancelled(),
            &mut || !cancelled(),
        );
    }
}

/// Drop cache entries the current manifest no longer references (stale hashes). A referenced
/// asset's leftover `.part` is KEPT — it's the resume source for an interrupted download.
///
/// FILES ONLY, and only at the top level: `keep` is built from the SHIM manifest, while the
/// base-game pipeline caches under `CACHE_DIR/BASE_CACHE_SUBDIR`. Recursing (or deleting
/// directory entries) would make a background warm delete a half-finished 16 GB game download.
fn prune_cache(cache: &Path, manifest: &Manifest) {
    // entry hashes AND bundle psha256s: a packed bundle (or its `.part`) mid-warm is a resume
    // source exactly like an asset `.part`, and must survive the prune that follows it
    let keep: HashSet<&str> = manifest
        .payload_entries()
        .map(|(_, sha, _)| sha)
        .chain(manifest.bundles.iter().map(|b| b.psha256.as_str()))
        .collect();
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

/// Has a SHIM install ever run in this folder? Answered by the asset cache holding at least one
/// entry: cache files are content-addressed and sit flat at the top level, while the base game's
/// cache is a SUBDIRECTORY (`base/`) — so this cannot be fooled by someone who only ever
/// downloaded the game. Uninstall clears those files (`clear_dir_files`), which is what makes a
/// reverted folder correctly read as pristine again.
///
/// Used only as a tombstone for `trust_prev`, so the failure directions are asymmetric on purpose:
/// a false NO promotes our own shim to the vanilla store (uninstall then restores Phoenix as
/// "stock"), a false YES merely declines to preserve a genuine original. Read errors therefore
/// answer YES.
fn shim_cache_used(game_dir: &Path) -> bool {
    // NotFound is the ONE error that genuinely means no: there is no cache, so nothing was ever
    // staged through it. Anything else (ACL, I/O) means we cannot tell, and per the asymmetry
    // above "cannot tell" has to answer YES. Treating every error as `false` did exactly the
    // damaging thing this doc comment says is avoided.
    let rd = match std::fs::read_dir(game_dir.join(CACHE_DIR)) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    rd.flatten().any(|e| e.file_type().map(|t| t.is_file()).unwrap_or(true))
}

/// Move an existing `target` aside and return where it went. Ours -> ephemeral rollback backup;
/// a genuine pre-existing file -> the permanent vanilla store (kept only the first time).
fn back_up(ctx: &Ctx, dest: &str, target: &Path) -> Result<PathBuf> {
    // "Ours" is a claim about BYTES, not about a dest. The record says we wrote specific content
    // here; if the content changed, the file we wrote is gone and what sits there was put there by
    // somebody else. Treating that as ours would send it to the ephemeral backup, which is deleted
    // on success — so a user's edit to a Phoenix file evaporated the next time the file was
    // displaced, with no copy anywhere. Routed to the vanilla store instead it survives, and
    // uninstall puts it back, which is the only reading of "revert" that does not throw away work
    // the launcher did not do.
    //
    // The legacy winmm_orig.dll is ours by LINEAGE rather than by record: we created it, but it
    // was never a manifest entry, so it is absent from `prev_dests` and no hash of it was ever
    // written down. Without this clause it reads as a genuine pre-existing original and is
    // promoted to the vanilla store — and uninstall restores that store AFTER collecting
    // winmm_orig.dll, so the file would be put straight back and a manifest `remove[]` could
    // never actually retire it. Uninstall already deletes this file on the strength of the same
    // flag and no content check; this only makes the two paths agree about whose it is.
    let ours = (ctx.prev_dests.contains(dest) && !ctx.user_changed.contains(dest))
        || (ctx.prev_winmm_created && dest == WINMM_ORIG);
    let vanilla = ctx.vanilla_root.join(dest);
    // Promoting a file to the vanilla store is IRREVERSIBLE in effect: uninstall restores whatever
    // is in there as "stock". So it only happens when `prev_dests` is trustworthy. Without the
    // state file we cannot tell our own previously-installed files from genuine originals, and
    // guessing wrong preserves the Phoenix shim as vanilla — uninstall would then dutifully put
    // the shim back and report the game as stock. `trust_prev` is false exactly when the state is
    // missing AND the folder shows evidence of a prior install; the cost is an ephemeral backup
    // instead of a preserved original, which is the safe direction to be wrong in.
    let to = if !ctx.trust_prev || ours {
        ctx.backup_root.join(dest)
    } else if !vanilla.exists() {
        vanilla
    } else {
        // The vanilla slot for this dest is already holding the genuine stock file — the one
        // uninstall has to put back — so what is being displaced here is the user's edit of OUR
        // file, and it needs a home of its own. It used to fall through to the ephemeral backup,
        // which is deleted on success: an edit to a Phoenix file at a dest that once had a stock
        // original evaporated on the next update, with no copy anywhere. One slot, two files that
        // both matter, so the second one gets its own store.
        ctx.user_root.join(dest)
    };
    if let Some(p) = to.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::rename(target, &to).with_context(|| format!("backing up {dest}"))?;
    Ok(to)
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
        }
    }
}

/// Revert the game to stock: for each managed file restore its preserved vanilla original if one was
/// kept, else delete it; delete the legacy winmm_orig.dll only if our lineage created it; then
/// remove our own scratch dirs and the state file. Game dirs (scripts/, cfg/, bin/win64/) are left
/// alone.
///
/// Nothing creates winmm_orig.dll any more (see WINMM_ORIG), but folders installed by a launcher
/// that did still hold one, and their state still says so — collecting it is the difference
/// between a clean revert and leaving a stray copy of a system DLL in somebody's game folder.
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

    let mut kept = Vec::new();
    for f in &state.files {
        let target = game_dir.join(&f.dest);
        let vanilla = vanilla_root.join(&f.dest);
        // Is this still the file we installed? Uninstall deletes on the strength of the record
        // saying "we put this here", and that record stops being true the moment somebody edits
        // the file. Deleting it then destroys their work while reporting a clean revert — so a
        // changed file is left exactly where it is and named in the report instead.
        //
        // Note which way the unreadable case falls: a file we cannot hash is NOT assumed changed.
        // The record is the only evidence available and it says the file is ours; refusing to
        // remove it would leave the shim's own winmm.dll behind whenever an antivirus held it for
        // a second, which is a half-uninstalled game that still loads Phoenix.
        let changed = verify::sha256_file_cached(&target).is_ok_and(|h| h != f.sha256);
        if changed && !vanilla.exists() {
            kept.push(f.dest.clone());
            continue;
        }
        if vanilla.exists() {
            // A preserved original outranks the current file only when the current file is still
            // ours. If the user changed it, moving the original back would bury their version
            // under the very "restore" that is supposed to be doing them a favour.
            if changed {
                kept.push(f.dest.clone());
                continue;
            }
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
    // The store goes only when everything in it went home. What `restore_vanilla_tree` could not
    // place is an original whose dest is occupied by a file the user put there — the one copy of
    // a file they may well want back. Deleting the store on the way out would take it with us, so
    // an un-emptied store survives the uninstall and is reported rather than silently discarded.
    let vanilla_kept = tree_has_files(&vanilla_root);
    if !vanilla_kept {
        let _ = std::fs::remove_dir_all(&vanilla_root);
    }
    let _ = std::fs::remove_file(InstalledState::path(&game_dir));

    Ok(UninstallReport {
        version: state.version,
        restored,
        deleted,
        kept,
        vanilla_kept,
        winmm_orig_removed,
    })
}

/// Does this tree hold any FILE (at any depth)? Empty directories do not count — they are the
/// residue of a store whose contents all went home.
fn tree_has_files(dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else { return false };
    for e in rd.flatten() {
        match e.metadata() {
            Ok(md) if md.is_dir() => {
                if tree_has_files(&e.path()) {
                    return true;
                }
            }
            Ok(_) => return true,
            Err(_) => return true, // cannot tell: assume something is there rather than delete it
        }
    }
    false
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

/// What the base plan decided for one manifest file — ONE axis, deliberately.
///
/// This used to be three variants plus an `unreadable` bool, which forced every reader to
/// reconstruct the real question ("what am I looking at?") from two fields. The distinctions that
/// matter to a user are all differences from vanilla; what separates them is *what we know about
/// the difference*, and that is exactly one thing per file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseAction {
    /// The file (at its live path, or its preserved vanilla copy) matches the manifest hash.
    UpToDate,
    /// Not there at all. The one unambiguous verdict: nothing is lost by writing it back.
    Missing,
    /// Present, read in full, and NOT the manifest's bytes. Note what this does NOT say: it is a
    /// difference, not damage. A mod that replaces a stock file lands here identically to a
    /// corrupted one, and only the user can tell them apart — which is why the files view shows
    /// the evidence (size delta, when it changed) instead of calling it "damaged".
    Differs,
    /// Present but could not be READ — a lock, an ACL, a bad sector. Not a statement about its
    /// contents; the cure is usually antivirus or permissions, not a download. Still written by a
    /// repair (rewriting IS the cure when the cause is the disk) and `probe_writable` names a lock
    /// or an ACL before a byte downloads.
    Unreadable,
    /// Differs, and the user pinned exactly these bytes as intentional (see keep.rs). Never
    /// written unless a caller names it explicitly — a pin is an instruction, not a hint.
    Kept,
    /// The shim manages this dest and no vanilla copy exists — nothing here is ours to touch.
    Skipped,
}

impl BaseAction {
    /// Does the pipeline write this of its own accord? The three unapproved differences do; a
    /// `Kept` file takes an explicit selection, and the other two are nothing to do.
    pub fn writes(self) -> bool {
        matches!(self, BaseAction::Missing | BaseAction::Differs | BaseAction::Unreadable)
    }

    /// The wire name for this verdict — one word, shared by the view layer and the debug CLI so
    /// they can never drift into describing the same state differently.
    pub fn word(self) -> &'static str {
        match self {
            BaseAction::UpToDate => "intact",
            BaseAction::Missing => "missing",
            BaseAction::Differs => "modified",
            BaseAction::Unreadable => "unreadable",
            BaseAction::Kept => "kept",
            BaseAction::Skipped => "skipped",
        }
    }
}

/// One base-game file's verdict, plus the evidence a user needs to judge it.
///
/// Carries the manifest `entry` itself rather than just a dest: every caller needs the size and
/// hash (to total bytes, to dedupe shared content, to download), and looking those back up meant
/// re-resolving the whole manifest and scanning it per status — O(n²) over 4,635 files.
#[derive(Debug)]
pub struct BaseStatus {
    pub action: BaseAction,
    pub entry: FileEntry,
    /// Size on disk. Together with `entry.size` this is the one piece of hard evidence about a
    /// difference we can state as fact — "2.1 MB of an expected 340 MB" is a truncated download,
    /// and no mod looks like that.
    pub local_size: Option<u64>,
    /// Last-modified, unix seconds. The other half of the evidence: vanilla files all carry the
    /// install date, so a file touched months later is a change someone made on purpose. Read for
    /// free — the hash memo stats every file anyway.
    pub mtime: Option<u64>,
    /// `Differs` reached by a pin EXPIRING because the manifest changed this file, rather than a
    /// difference nobody has ruled on. See `engine::FileStatus::superseded`.
    pub superseded: bool,
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
    base_plan_of(game_dir, manifest, progress, op, cancel, None)
}

/// `base_plan` over a SUBSET of the manifest — the verdicts for named dests and nothing else.
///
/// The cost of a base plan *is* the hashing, so filtering the entry list is what makes a narrow
/// question cheap: `your_files` needs the state of the handful of dests the user has pinned, and
/// reading 15 GB to answer that would be the very thing that screen exists to avoid. `only` names
/// dests in manifest form; one the manifest does not carry is simply absent from the result,
/// because there is no authority verdict to give about it.
///
/// Every other rule is `base_plan`'s, unchanged — pins, the shim's claim on a dest, and the
/// preserved-vanilla redirect all still apply, because a subset must not answer differently from
/// the whole for the files it does cover.
pub fn base_plan_of(
    game_dir: &Path,
    manifest: &Manifest,
    progress: engine::Progress,
    op: &'static str,
    cancel: Option<&AtomicBool>,
    only: Option<&HashSet<String>>,
) -> Result<Vec<BaseStatus>> {
    // resolve with no selections: today's game manifests carry no options, and if one ever does,
    // installing its defaults is the right reading
    let entries = engine::resolve(manifest, &Default::default());
    let entries: Vec<_> = match only {
        Some(want) => entries.into_iter().filter(|f| want.contains(&f.dest)).collect(),
        None => entries,
    };
    let shim_managed: HashSet<String> = crate::state::InstalledState::load(game_dir)
        .map(|s| s.files.into_iter().map(|f| f.dest).collect())
        .unwrap_or_default();
    // Loaded here rather than passed in, exactly like the install record above: the pins describe
    // THIS folder, so every caller that plans it wants the same answer and none of them should be
    // able to forget to ask. A plan that ignored pins would report the user's mods as differences
    // again, and `install_base` re-plans before writing — that re-plan is the one that must not.
    let keep = crate::keep::KeepList::load(game_dir);
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
        for _ in 0..hash_workers().min(entries.len().max(1)) {
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
                // Narrate the read itself for the few files big enough to stall the counter. The
                // closure is only built (and only fires) for those — see plan_one.
                let tick = |read: u64| {
                    if let Some(p) = progress {
                        p(engine::OpProgress {
                            op,
                            phase: "plan",
                            current: done.load(Ordering::Relaxed),
                            total,
                            item: Some(fe.dest.clone()),
                            bytes_done: Some(read),
                            bytes_total: Some(fe.size),
                            done: false,
                        });
                    }
                };
                let on_read: Option<&dyn Fn(u64)> = progress.map(|_| &tick as &dyn Fn(u64));
                let st = plan_one(game_dir, &vanilla_root, &shim_managed, &keep, fe, on_read);
                let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                // Throttled like the byte ticks are. One event per file means 4,635 JSON
                // serializations + webview postMessages + JS handler calls in a burst — and a warm
                // re-verify (every hash a memo hit) fires them as fast as the loop can spin, for
                // no extra information. The last file always reports, so the bar still lands.
                if let Some(p) = progress {
                    if d.is_multiple_of(PLAN_GRAIN) || d == total {
                        p(engine::OpProgress {
                            op,
                            phase: "plan",
                            current: d,
                            total,
                            item: Some(fe.dest.clone()),
                            bytes_done: None,
                            bytes_total: None,
                            done: !st.action.writes(),
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

/// WHICH FILE a base dest's verdict is about, and whether there is one to give.
///
/// Not always `game_dir/dest`: the shim can occupy that path, or have relocated the stock file, in
/// which case the base file is its preserved original under `.phoenix-vanilla/`. Everything that
/// forms an opinion about a base dest — the plan, and the pin recorded from the plan's answer —
/// has to look at the same file, or a pin records an approval of bytes nobody compared.
pub fn base_target(
    game_dir: &Path,
    vanilla_root: &Path,
    shim_managed: &HashSet<String>,
    dest: &str,
) -> (PathBuf, bool) {
    let live = game_dir.join(dest);
    let vanilla = vanilla_root.join(dest);
    if shim_managed.contains(dest) {
        // the shim owns the live path; its preserved original (if any) is the base file
        let exists = vanilla.exists();
        (vanilla, exists)
    } else if !live.exists() && vanilla.exists() {
        // shim remove[] relocated it — verify/repair the preserved copy, not the void
        (vanilla, true)
    } else {
        (live, true)
    }
}

/// `base_target`'s path, resolved for a caller that has only the game folder — the shape
/// `keep::pin_all` wants. Loads the install record once; the returned closure is cheap per dest.
pub fn base_paths(game_dir: &Path) -> impl Fn(&str) -> PathBuf + '_ {
    let shim_managed: HashSet<String> = crate::state::InstalledState::load(game_dir)
        .map(|s| s.files.into_iter().map(|f| f.dest).collect())
        .unwrap_or_default();
    let vanilla_root = game_dir.join(VANILLA_DIR);
    move |dest| base_target(game_dir, &vanilla_root, &shim_managed, dest).0
}

/// One file's verdict, and WHERE it lives for us — see `base_plan`'s coexistence rules.
fn plan_one(
    game_dir: &Path,
    vanilla_root: &Path,
    shim_managed: &HashSet<String>,
    keep: &crate::keep::KeepList,
    fe: &FileEntry,
    on_read: Option<&dyn Fn(u64)>,
) -> BaseStatus {
    let (target, checkable) = base_target(game_dir, vanilla_root, shim_managed, &fe.dest);
    // Whether this dest carries a pin decides how hard we look below, so it is read once up front.
    let pinned = keep.files.contains_key(&fe.dest);
    let mut local_size = None;
    let mut mtime = None;
    let mut superseded = false;

    let action = if !checkable {
        BaseAction::Skipped
    } else {
        match std::fs::metadata(&target) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BaseAction::Missing,
            // there, but we cannot even stat it — as unreadable as a file whose bytes won't come
            Err(_) => BaseAction::Unreadable,
            Ok(md) => {
                local_size = Some(md.len());
                mtime = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs());
                // The LENGTH settles it without reading a byte: a content hash implies a content
                // length, so a size mismatch cannot possibly hash clean. This is the common damage
                // — a file truncated by a killed download or a full disk — and it used to be read
                // in full (these include multi-hundred-MB VPKs) to conclude what the metadata
                // already proved. The memo fetches this metadata anyway, so the check is free.
                //
                // A PIN suspends the shortcut, and only for the pinned dests. A kept file is
                // normally a different SIZE as well as different bytes (that is what replacing a
                // file means), so short-circuiting here would make it impossible for a pin to ever
                // match — the feature would be dead on arrival for exactly the files it exists
                // for. The extra reads are bounded by the number of pins, which is a handful.
                if md.len() != fe.size && !pinned {
                    BaseAction::Differs
                } else {
                    // Only the big ones narrate: below the threshold a file is gone before a tick
                    // could mean anything, and the events would be pure noise on the wire.
                    let (every, cb) = if fe.size >= BIG_FILE_BYTES {
                        (BIG_FILE_TICK_BYTES, on_read)
                    } else {
                        (0, None)
                    };
                    match verify::sha256_file_cached_with(&target, every, cb) {
                        Ok(h) if h == fe.sha256 => BaseAction::UpToDate,
                        Ok(h) if keep.is_kept(&fe.dest, &h, Some(&fe.sha256)) => {
                            BaseAction::Kept
                        }
                        Ok(h) if keep.superseded(&fe.dest, &h, Some(&fe.sha256)) => {
                            superseded = true;
                            BaseAction::Differs
                        }
                        // includes a pin that no longer matches: the bytes the user approved are
                        // not the bytes that are there now, so the approval does not carry to them
                        Ok(_) => BaseAction::Differs,
                        Err(_) => BaseAction::Unreadable,
                    }
                }
            }
        }
    };
    BaseStatus { action, entry: fe.clone(), local_size, mtime, superseded, target }
}

/// Does this folder hold a game at all — `game/dota` on disk, or a shim install record (a shim
/// install is evidence a game was here even if the folder is damaged)? The check view derives
/// its "no game here" state from this, the apply command refuses on it, and game_verify words
/// its refusal with it. NOT a build gate (that stays removed by decision): it asks "is there
/// anything to update INTO", never "is it the right version".
pub fn game_present(game_dir: &Path) -> bool {
    game_dir.join("game").join("dota").exists()
        || crate::state::InstalledState::load(game_dir).is_some()
}

/// Bytes an interrupted base-game download left in the cache (complete entries + `.part`s).
/// Metadata only — this is the "a download is waiting here" signal for the UI, not a verification
/// (obtain re-verifies every entry it reuses). 0 when the cache is absent.
pub fn pending_base_bytes(game_dir: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(game_dir.join(CACHE_DIR).join(BASE_CACHE_SUBDIR)) else {
        return 0;
    };
    rd.flatten()
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

/// Is there a game here, or the beginnings of one? `game_present` widened by an interrupted
/// download's cache — together, the two states a fresh download would CONTINUE rather than fill an
/// empty folder. The download dialog asks this about both the folder the user picked and the
/// destination it composed, because the answer changes what that dialog is offering.
pub fn game_started(dir: &Path) -> bool {
    game_present(dir) || pending_base_bytes(dir) > 0
}

/// Top-level entries in `dir` that are not the launcher's own bookkeeping. 0 when it does not
/// exist, which is the ordinary case for a fresh destination.
///
/// This is the extras scan's premise, counted before a byte is downloaded: everything in the game
/// folder that no manifest describes is reported as a file nothing claims — so a destination that
/// already holds somebody's documents will report them, with a delete control on them. The count
/// is what lets the dialog say that in a number instead of in the abstract. `.phoenix*` is skipped
/// on the same reasoning `scan_extras` skips it: those are ours, and they are not what the
/// sentence is about.
///
/// Top level only. This runs on every keystroke in the name field; walking a folder someone might
/// have pointed at `D:\` is not something to do between two keys, and "is there other stuff in
/// here" is answered by the first level.
pub fn foreign_entry_count(dir: &Path) -> u32 {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    rd.flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with(".phoenix"))
        .count()
        .min(u32::MAX as usize) as u32
}

/// The folder a fresh download creates inside the one the user picks, unless they say otherwise.
///
/// A DEFAULT, not a rule: the dialog lets it be renamed or switched off entirely, and nothing else
/// in the launcher ever looks for this name — whatever is chosen is adopted as `game_dir` and every
/// later read goes through that setting. So this is the one place to change it.
pub const GAME_SUBDIR: &str = "dota2_688f";

/// Longest destination folder name the dialog accepts. Launcher policy, not a filesystem limit
/// (NTFS allows 255): every name here is a segment added to paths that already run deep — the
/// game ships files nine levels down — and nothing legible needs more than this.
const GAME_SUBDIR_MAX: usize = 64;

/// Why a typed destination folder name cannot be used. A reason, not a sentence: the shell owns
/// no language (see main.rs), so this crosses to the webview as a code and is worded there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubdirIssue {
    /// Nothing typed. Distinct from "no subfolder wanted", which is `None` at the call site.
    Empty,
    /// A path separator: this names ONE folder, and the rest of the path is the picker's job.
    Separator,
    /// A character Windows refuses in a file name — including `:`, which would also make the
    /// composed string mean a different drive entirely.
    Chars,
    /// Leading/trailing space or a trailing dot. Win32 strips those before it resolves the path,
    /// so the folder created would not be the folder named — the dialog would be showing a path
    /// that does not exist.
    Edge,
    /// A reserved device name (`NUL`, `COM1`, `con.txt`): writing there vanishes bytes.
    Reserved,
    /// Longer than `GAME_SUBDIR_MAX`.
    TooLong,
}

/// Vet a destination folder NAME — one path component, typed by the user.
///
/// Deliberately stricter than the filesystem, and in one direction only: everything refused here
/// either cannot become a folder at all, or would become a folder with a different name than the
/// one on screen. Being refused costs a keystroke; the alternative costs a multi-gigabyte download
/// into a place the user was not shown.
pub fn subdir_issue(name: &str) -> Option<SubdirIssue> {
    if name.is_empty() {
        return Some(SubdirIssue::Empty);
    }
    if name.chars().count() > GAME_SUBDIR_MAX {
        return Some(SubdirIssue::TooLong);
    }
    if name.contains(['/', '\\']) {
        return Some(SubdirIssue::Separator);
    }
    if name.contains([':', '*', '?', '"', '<', '>', '|']) || name.chars().any(|c| c.is_control()) {
        return Some(SubdirIssue::Chars);
    }
    // Win32 resolves `dota2 ` and `dota2.` to `dota2`, so a name that only differs by an edge
    // space or dot is a name the user cannot actually have. `..` and `.` fall in here too.
    if name != name.trim() || name.ends_with('.') {
        return Some(SubdirIssue::Edge);
    }
    if crate::manifest::is_reserved_device(name) {
        return Some(SubdirIssue::Reserved);
    }
    None
}

/// Where a fresh download would write, split into the head the user cannot edit and the whole.
///
/// Returns `(prefix, target)`: `prefix` is `base` plus the separator a subfolder would be joined
/// with (just `base` when there is none), and `target` is the destination itself. They are
/// produced together, by one rule, because the download dialog SHOWS the prefix and SENDS the
/// target — join either of them frontend-side and the path on screen stops being the path on disk
/// the first time somebody picks a drive root, which already ends in a separator and must not be
/// given a second one.
///
/// `sub` is assumed vetted (`subdir_issue`); nothing here can make a bad name safe, and the caller
/// that ignores the verdict gets exactly the folder it asked for.
pub fn target_of(base: &str, sub: Option<&str>) -> (String, String) {
    let Some(sub) = sub else { return (base.to_string(), base.to_string()) };
    let mut prefix = base.to_string();
    if !prefix.ends_with(['/', '\\']) {
        prefix.push(std::path::MAIN_SEPARATOR);
    }
    let target = format!("{prefix}{sub}");
    (prefix, target)
}

/// What the plan's to-download set ALREADY has in the base cache: (WIRE bytes, fully-fetched
/// files). Accounted per ASSET, not per file (R6): a partly-fetched bundle is progress toward
/// all of its members and toward none of them individually — without asset tracking, a bundle
/// killed at 90% read as "0 bytes downloaded" with gigabytes sitting on disk.
///
/// Bytes are wire currency, capped at each asset's wire size (an over-long leftover must not
/// report more than the plan asked for): a raw entry or `.part` counts its length; a bundle
/// counts its full `psize` once nothing more of it must cross the network (packed asset
/// present, or every wanted member already extracted), else its packed `.part`'s length.
/// FILES count dests (that is what every visible counter counts), and a dest is "fetched" only
/// when its bytes are obtainable with no network — a complete raw entry, an extracted member,
/// or any member of a fully-present packed bundle. Metadata only. Drives the resume confirm's
/// "X of Y GB · N of M files already downloaded".
pub fn base_cached(game_dir: &Path, manifest: &Manifest, statuses: &[BaseStatus]) -> (u64, usize) {
    let cache = game_dir.join(CACHE_DIR).join(BASE_CACHE_SUBDIR);
    let writes: Vec<&FileEntry> = statuses
        .iter()
        .filter(|s| s.action.writes())
        .map(|s| &s.entry)
        .collect();
    let Ok(acqs) = build_acqs(
        &manifest.bundles,
        writes.iter().map(|fe| (fe.name.as_deref(), fe.sha256.as_str(), fe.size)),
    ) else {
        return (0, 0);
    };
    let entry_len = |name: String| std::fs::metadata(cache.join(name)).ok().map(|m| m.len());
    let complete = |sha: &str, size: u64| entry_len(sha.to_string()).is_some_and(|l| l >= size);

    let mut bytes = 0u64;
    // hashes whose bytes need no further network — dests holding them count as fetched below
    let mut fetched: HashSet<&str> = HashSet::new();
    for acq in &acqs {
        match acq {
            Acq::Raw { sha256, size, .. } => {
                if let Some(len) = entry_len(sha256.to_string()) {
                    bytes += len.min(*size);
                    if len >= *size {
                        fetched.insert(sha256);
                    }
                } else if let Some(len) = entry_len(format!("{sha256}.part")) {
                    bytes += len.min(*size);
                }
            }
            // costs nothing to download; "fetched" once its entry was materialized
            Acq::Empty { sha256 } => {
                if entry_len(sha256.to_string()).is_some() {
                    fetched.insert(sha256);
                }
            }
            Acq::Bundle { bundle, wanted } => {
                let have: Vec<&str> = wanted
                    .iter()
                    .filter(|(sha, size)| complete(sha, *size))
                    .map(|(sha, _)| *sha)
                    .collect();
                if complete(&bundle.psha256, bundle.psize) || have.len() == wanted.len() {
                    // nothing more of this bundle crosses the network — and a present packed
                    // asset makes every wanted member obtainable offline
                    bytes += bundle.psize;
                    fetched.extend(wanted.iter().map(|(sha, _)| *sha));
                } else {
                    // extracted members do NOT discount the bytes: the packed asset still has
                    // to cross whole for the members that are missing. They do count as
                    // fetched FILES — their dests need no further network.
                    fetched.extend(have);
                    if let Some(len) = entry_len(format!("{}.part", bundle.psha256)) {
                        bytes += len.min(bundle.psize);
                    }
                }
            }
        }
    }
    let files = writes.iter().filter(|fe| fetched.contains(fe.sha256.as_str())).count();
    (bytes, files)
}

/// What `build_identity` concluded about a folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildIdentity {
    /// Nothing contradicts the manifest: steam.inf matches it, or is absent (a fresh target), or
    /// the manifest states none.
    Same,
    /// steam.inf is there and is a DIFFERENT build.
    Foreign,
    /// steam.inf is there and could not be read. Neither of the other two answers — and saying
    /// either would be a guess.
    Unknown,
}

/// Which Dota 2 build does this folder hold, relative to the manifest?
///
/// `game/dota/steam.inf` carries the build identity, so a local copy that exists but does not
/// match the manifest's hash means the folder is some other Dota 2 installation — not a damaged
/// one. The distinction is the difference between a useful repair and a catastrophe: verify would
/// otherwise report nearly every file as "damaged", and accepting that repair would overwrite a
/// perfectly good unrelated install with build 1805. An ABSENT steam.inf is a fresh or empty
/// target, which is not foreign — that is exactly what a fresh install starts from.
///
/// `Unknown` exists because the question is answered by READING a file, and a read can fail for
/// reasons that say nothing about which build this is. Folding that into "foreign" — as a bare
/// bool must — meant an antivirus holding steam.inf for a second was enough to tell the user
/// their 6.88 folder was some other build and to offer them the one irreversible action in the
/// app. The caller decides what to do with not-knowing; it must not be spelled as knowing.
pub fn build_identity(game_dir: &Path, manifest: &Manifest) -> BuildIdentity {
    const STEAM_INF: &str = "game/dota/steam.inf";
    let Some(fe) = manifest.files.iter().find(|f| f.dest == STEAM_INF) else {
        return BuildIdentity::Same;
    };
    let local = game_dir.join(STEAM_INF);
    match std::fs::metadata(&local) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => BuildIdentity::Same,
        Err(_) => BuildIdentity::Unknown,
        Ok(_) => match verify::sha256_file_cached(&local) {
            Ok(h) if h == fe.sha256 => BuildIdentity::Same,
            Ok(_) => BuildIdentity::Foreign,
            Err(_) => BuildIdentity::Unknown,
        },
    }
}

/// A file in the game folder that no manifest entry and no install record claims.
///
/// Never at risk from anything this app does — a repair can only write manifest dests, so these
/// are reported for one reason: a mod is usually some replaced files plus some ADDED ones, and
/// showing only the replaced half makes the other half look like it vanished. Deleting them is a
/// separate, explicit act (`delete_extras`).
#[derive(Debug, Clone)]
pub struct ExtraEntry {
    /// Relative to the game folder, `/`-separated like a manifest dest.
    pub path: String,
    /// Bytes: the file's own, or a summarized directory's total.
    pub size: u64,
    pub mtime: Option<u64>,
    /// 0 = a file. Otherwise the recursive file count of a DIRECTORY the manifest knows nothing
    /// about, reported as one row instead of thousands — a custom-game addon can hold tens of
    /// thousands of files, and enumerating them would bury the handful that mean anything.
    pub files: u32,
}

/// Ceiling on directory entries the extras scan will look at. A game folder can contain an
/// arbitrary user-made tree; this is metadata-only work (no hashing) so the cap is generous, but
/// "walk whatever is there" is not a bound, and the UI is told when it was hit rather than
/// quietly showing a short list as if it were the whole truth.
const EXTRA_SCAN_CAP: usize = 200_000;

/// Does the manifest's `ignore` list quiet this path? Three rules, no glob engine:
///   * exact match — one named file;
///   * a pattern ending in `/` — that directory and everything under it;
///   * a pattern starting with `*.` — that extension, anywhere.
///
/// Anything else matches nothing. A tiny, total language beats a glob dialect nobody can predict:
/// the cost of a pattern that silently matches too much is a file the user never gets told about.
fn ignores_extra(ignore: &[String], path: &str) -> bool {
    ignore.iter().any(|p| {
        if let Some(ext) = p.strip_prefix("*.") {
            path.rsplit_once('.').is_some_and(|(_, e)| e.eq_ignore_ascii_case(ext))
        } else if p.ends_with('/') {
            path.starts_with(p.as_str())
        } else {
            path == p
        }
    })
}

/// Why an extras walk ended. "The user stopped it" and "there are more than we will list" are
/// different facts, and reporting one as the other was a real misstatement: pressing Stop produced
/// "There are more extra files than could be listed — this is a partial view of them", which
/// describes a scan ceiling nobody hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtrasEnd {
    Complete,
    /// Hit `EXTRA_SCAN_CAP` — the entries are a prefix of the truth and the UI must say so.
    Capped,
    Cancelled,
}

/// Everything in the game folder that the manifest does not describe.
///
/// Bounded by the VANILLA footprint, not by the folder: it walks the directories the manifest
/// itself populates, and an unknown directory found inside one is summarized as a single row
/// (recursive count + bytes) rather than descended into as a listing. That keeps the common case
/// — a mod dropping files among stock ones — precise, while a 40,000-file custom game reads as
/// one line saying so.
///
/// Excluded outright, at every level: anything named `.phoenix*` (our staging, cache, backups,
/// preserved originals and state files). Offering to delete our own machinery would be a bug
/// dressed as a feature.
///
/// `winmm_orig.dll` used to be excluded alongside them, on exactly that reasoning — it WAS ours.
/// It is not any more (see WINMM_ORIG): nothing creates it, nothing reads it, and the shim no
/// longer forwards through it. On a folder an older launcher set up it is simply a file nothing
/// claims, which is the definition of an extra. Hiding it made the one leftover this launcher is
/// responsible for the only file in the folder its owner could neither see nor remove.
///
/// `claimed` is every dest the CALLER is already reporting under some other authority — the game
/// manifest's own dests plus whatever the shim plan accounted for. It is a parameter rather than
/// something derived here because the install record alone is the wrong answer to "is this file
/// spoken for": a dest we recorded, no longer manage, and whose bytes are not even the ones we
/// wrote describes a file that does not exist any more. What is actually there belongs to whoever
/// put it there, and hiding it behind a stale record is how a user's file becomes invisible to the
/// one view built to show it. Callers that cannot compute a shim plan (no network, no manifest)
/// pass the record's dests and get the old, conservative behaviour.
///
/// Returns the entries and WHY the walk ended — see `ExtrasEnd`.
pub fn scan_extras(
    game_dir: &Path,
    manifest: &Manifest,
    claimed: &HashSet<String>,
    cancel: Option<&AtomicBool>,
) -> (Vec<ExtraEntry>, ExtrasEnd) {
    let entries = engine::resolve(manifest, &Default::default());
    let mut known: HashSet<&str> = entries.iter().map(|f| f.dest.as_str()).collect();
    known.extend(claimed.iter().map(String::as_str));
    // Every ancestor of every KNOWN dest — the tree we walk file by file, as opposed to the
    // unknown subtrees we summarize whole. Membership answers "does anything here belong to
    // somebody", and it must be derived from `known`, NOT from the game manifest alone.
    //
    // This was a real, serious bug: Phoenix installs into directories vanilla does not have
    // (`game/dota_phoenix/`), so a set built from the game manifest classified the ENTIRE shim as
    // one foreign subtree — reported to the user as "not part of the game", with a delete control
    // on it. `summarize_dir` never consults `known` (it cannot: it exists precisely to avoid
    // enumerating what it walks), so a directory holding claimed files must never reach it. It
    // cannot now: a claimed file puts every one of its ancestors in here.
    let mut dirs: HashSet<&str> = HashSet::from([""]);
    for dest in &known {
        let mut d = *dest;
        while let Some((parent, _)) = d.rsplit_once('/') {
            if !dirs.insert(parent) {
                break; // this ancestor chain is already recorded
            }
            d = parent;
        }
    }

    let mut out = Vec::new();
    let mut budget = EXTRA_SCAN_CAP;
    let mut truncated = false;
    let mut stack = vec![String::new()];
    while let Some(rel) = stack.pop() {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return (out, ExtrasEnd::Cancelled);
        }
        let abs = if rel.is_empty() { game_dir.to_path_buf() } else { game_dir.join(&rel) };
        let Ok(rd) = std::fs::read_dir(&abs) else { continue };
        for e in rd.flatten() {
            if budget == 0 {
                truncated = true;
                break;
            }
            budget -= 1;
            let name = e.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with(".phoenix") {
                continue;
            }
            let child = if rel.is_empty() { name.to_string() } else { format!("{rel}/{name}") };
            let Ok(md) = e.metadata() else { continue };
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs());
            if md.is_dir() {
                if dirs.contains(child.as_str()) {
                    stack.push(child);
                } else if !ignores_extra(&manifest.ignore, &format!("{child}/")) {
                    let (files, size, cut) = summarize_dir(&e.path(), &mut budget);
                    truncated |= cut;
                    // an empty unknown directory is not something the user "has" — reporting it
                    // as a row with nothing in it is noise, and deleting it changes nothing
                    if files > 0 {
                        out.push(ExtraEntry { path: child, size, mtime, files });
                    }
                }
            } else if !known.contains(child.as_str()) && !ignores_extra(&manifest.ignore, &child) {
                out.push(ExtraEntry { path: child, size: md.len(), mtime, files: 0 });
            }
        }
        // Spent budget ends the WALK, not just this directory. Without this the stack kept being
        // popped and every remaining directory paid for a read_dir it would immediately abandon.
        if budget == 0 {
            break;
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    (out, if truncated { ExtrasEnd::Capped } else { ExtrasEnd::Complete })
}

/// Recursive file count + byte total of an unknown subtree, spending from the shared scan budget.
fn summarize_dir(dir: &Path, budget: &mut usize) -> (u32, u64, bool) {
    let (mut files, mut size, mut truncated) = (0u32, 0u64, false);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            if *budget == 0 {
                truncated = true;
                break;
            }
            *budget -= 1;
            match e.metadata() {
                Ok(md) if md.is_dir() => stack.push(e.path()),
                Ok(md) => {
                    files += 1;
                    size += md.len();
                }
                Err(_) => {}
            }
        }
    }
    (files, size, truncated)
}

/// Delete named extras. Irreversible, so it re-derives the legal set instead of trusting the
/// caller: only paths `scan_extras` reports right now can be removed, which makes a stale UI list,
/// a renamed folder, or a crafted path incapable of reaching a manifest file, the shim's files, or
/// anything under `.phoenix*`. Returns how many entries were removed.
///
/// Symlinks and junctions are refused rather than followed — `remove_dir_all` through a junction
/// would delete the target's contents, and a game folder is exactly where someone parks one.
pub fn delete_extras(
    game_dir: &Path,
    manifest: &Manifest,
    claimed: &HashSet<String>,
    paths: &[String],
) -> Result<u32> {
    let (extras, _) = scan_extras(game_dir, manifest, claimed, None);
    let legal: HashMap<&str, &ExtraEntry> =
        extras.iter().map(|e| (e.path.as_str(), e)).collect();
    let mut removed = 0;
    for p in paths {
        let Some(e) = legal.get(p.as_str()) else { continue };
        let target = game_dir.join(p);
        let md = std::fs::symlink_metadata(&target)
            .with_context(|| format!("reading {}", target.display()))?;
        if md.file_type().is_symlink() {
            bail!("{p} is a link, not a file — refusing to follow it");
        }
        if e.files > 0 {
            std::fs::remove_dir_all(&target)
                .with_context(|| format!("removing {}", target.display()))?;
        } else {
            std::fs::remove_file(&target)
                .with_context(|| format!("removing {}", target.display()))?;
        }
        removed += 1;
    }
    Ok(removed)
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
///
/// LEAVES A CORE. This was a flat 4, which on a 4-core machine means every core hashing: sha256
/// is CPU-bound (only some CPUs have SHA extensions; without them it is a few hundred MB/s per
/// core), so the webview's own thread ended up competing with the work it was trying to report on,
/// and the whole app went sluggish exactly while the user was watching a progress line. One core
/// held back costs a fraction of the throughput and buys a UI that still moves — and the run is
/// disk-bound as often as not anyway.
fn hash_workers() -> usize {
    std::thread::available_parallelism().map_or(2, |n| n.get().saturating_sub(1)).clamp(1, 4)
}

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

/// Download and place base-game files that are not what the manifest says they should be. Serves
/// both fresh installs (into an empty folder) and repair (into a live one) — the plan diff makes
/// them the same operation. Interruption at ANY point is recoverable by running again: completed
/// files hash-match and skip, interrupted downloads resume from their .part, the cache survives.
///
/// `wire` is the source this run is pulling from, swappable under the pool: an asset the current
/// source will not serve fails it over and is asked of the next, pinned to the same release. This
/// is where failover earns its keep — the base game is ~136 bundles and 7.9 GiB, so one asset one
/// host will not serve used to end the whole run.
///
/// `only` restricts the write set to those dests — a PARTIAL repair, which is what the files view
/// sends when the user has spared some files. Two things about it are load-bearing:
///
///   * the plan is still RECOMPUTED here, for those dests. The selection says what to look at; it
///     never stands in for the verdict. A dest the user picked minutes ago that is intact by the
///     time we get here is not rewritten, and one that broke in between is not written just
///     because it was in a stale list. It is scoped to the selection rather than the whole
///     manifest because the cost of a base plan IS the hashing: restoring one edited file used to
///     read all ~15 GB first, which is a full verification nobody asked for — and its file counter
///     then fought the download's own for the one progress line the screen has.
///   * it is the ONLY way a `Kept` file gets written. Pins are an instruction, so the default
///     (`None`) write set skips them; naming one explicitly is the user checking it back on, and
///     the caller is expected to drop the pin afterwards.
pub fn install_base(
    game_dir: &Path,
    wire: &Wire,
    manifest: &Manifest,
    progress: engine::Progress,
    cancel: Option<&AtomicBool>,
    only: Option<&HashSet<String>>,
) -> Result<BaseReport> {
    // The wire's release NAMES this run; every source it later swaps to is opened for that same
    // tag, so identity comes from the manifest already verified and only the bytes move.
    let release = wire.release();
    // cancellable from the first file: repairing a live folder hashes it before a byte is
    // downloaded, and a Cancel that only took effect once the download started sat inert for
    // minutes on exactly the screen that shows a Stop button
    let statuses = base_plan_of(game_dir, manifest, progress, "game", cancel, only)?;
    // `sel.contains` stays even though the plan is already scoped to it: `wanted` is the write
    // gate, and it must not depend on the plan having been narrowed to be correct.
    let wanted = |s: &BaseStatus| match only {
        Some(sel) => sel.contains(s.dest()) && (s.action.writes() || s.action == BaseAction::Kept),
        None => s.action.writes(),
    };
    let to_write: Vec<(&FileEntry, &Path)> = statuses
        .iter()
        .filter(|s| wanted(s))
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
        if only.is_none() {
            clear_dir_files(&game_dir.join(CACHE_DIR).join(BASE_CACHE_SUBDIR));
        }
        return Ok(report(0, 0));
    }

    // The acquisition set — raw assets, needed bundles (grouped), materialized empties — is what
    // both preflights below reason about: assets are what downloads, not files.
    let acqs = build_acqs(
        &manifest.bundles,
        to_write.iter().map(|(fe, _)| (fe.name.as_deref(), fe.sha256.as_str(), fe.size)),
    )?;

    // Preflight the asset index. A name NO source carries is a permanent condition no retry can
    // fix, and the lookup otherwise happens inside the download worker — so a truncated asset
    // array surfaced only after thousands of files and gigabytes, dressed up as a transient
    // download failure. Milliseconds here, hours saved there.
    //
    // The CURRENT source, and only it: a later one is opened for the same release, so an asset
    // this one omits is exactly what failover exists to survive — refusing the run up front on its
    // behalf would refuse a run the next source could finish. And a CONTENT-ADDRESSED source
    // (`content_addressed`) has no release index at all, so it can only ever answer "addressable";
    // whether the blob is really there is learned when it is fetched. That is the honest limit of
    // a preflight against a backend that publishes no index.
    let addressable = wire.current().1;
    let missing: Vec<&str> = {
        let mut seen = HashSet::new();
        acqs.iter()
            .filter_map(Acq::asset_name)
            .filter(|name| seen.insert(*name))
            .filter(|name| !addressable.carries(name))
            .collect()
    };
    if !missing.is_empty() {
        bail!(
            "the game release is incomplete: {} asset(s) are missing (first: {})",
            missing.len(),
            missing[0]
        );
    }

    // Disk preflight: the decoded content that lands plus the packed transient (see `costs_of`)
    // — refused BEFORE any bytes, never discovered as a mysterious failure at 97%.
    let (_wire, _disk, need) = costs_of(&acqs);
    ensure_disk_space(need, free_space(game_dir))?;

    // interlock: a running game holds its VPKs/DLLs mmapped — say "close the game" NOW, not
    // after gigabytes. Targets may sit under .phoenix-vanilla, so probe the actual target paths.
    let rels: Vec<String> = to_write
        .iter()
        .map(|(_, t)| t.strip_prefix(game_dir).unwrap_or(t).to_string_lossy().into_owned())
        .collect();
    probe_writable(game_dir, rels.iter())?;

    // The destination may not exist yet: a fresh download composes one INSIDE the folder the user
    // picked (see `target_of`), so this is the first thing that ever touches it. Creating it in its
    // own step is purely about the error — `create_dir_all(cache)` below would create it too, and
    // then report a folder nobody can write to as "creating the asset cache", which names our
    // machinery instead of the path the user chose. A no-op for repair, where it exists.
    std::fs::create_dir_all(game_dir)
        .with_context(|| format!("creating {}", game_dir.display()))?;

    // The base game gets its OWN cache subdirectory. It shared the shim's until a detached
    // warm_cache — which prunes against the shim manifest — was found deleting multi-GB base
    // entries and their `.part` resume sources behind an interrupted download.
    let cache = game_dir.join(CACHE_DIR).join(BASE_CACHE_SUBDIR);
    std::fs::create_dir_all(&cache).context("creating the asset cache")?;
    let fe_only: Vec<&FileEntry> = to_write.iter().map(|(fe, _)| *fe).collect();
    obtain_all_tagged(&cache, wire, &fe_only, manifest, progress, "game", cancel)?;

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
    //
    // And only on a WHOLE pass. "Everything needed was consumed" is a statement about a run that
    // considered every file; a partial repair deliberately looked at three of them, so what is
    // left over is not stale, it is the other 4,632 — including an interrupted multi-gigabyte
    // download that a three-file repair has no business reclaiming.
    if only.is_none() {
        clear_dir_files(&cache);
    }
    Ok(report(written, bytes))
}

#[cfg(test)]
mod tests {
    //! Golden-path install state-machine tests against temp dirs, served by the in-memory
    //! downloader fake — no network, no real game folder.
    use super::*;
    use crate::downloader::fake::Fake;
    use crate::downloader::{Asset, Downloader, Release};
    use sha2::Digest;
    use std::sync::Arc;

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

    /// A test double as the download path holds one: SHARED, so the test can still read its
    /// counters after a `Wire` has taken it. The concrete type survives — a `Wire` coerces it at
    /// the call, and a test that wants `dl.calls` still has something to ask.
    fn arc<D: Downloader + 'static>(dl: D) -> Arc<D> {
        Arc::new(dl)
    }

    /// A wire over the given backends, in that order — what these tests used to spell as a slice of
    /// `Origin`s.
    ///
    /// The ranking and the dial are both handed in — a `Wire` captures its ranking at open (see
    /// its field), so these tests need no process state and take no turns. The dial is injected
    /// because every production backend is an https-only agent no loopback listener can satisfy,
    /// and none of these tests is about transport.
    fn wire_over<D: Downloader + 'static>(
        peers: Vec<Arc<D>>,
        payload: crate::trust::Payload,
    ) -> Wire {
        use crate::config::Source;
        let sources: Vec<Source> = (0..peers.len())
            .map(|i| match i {
                0 => Source::default(),
                _ => Source::at(format!("https://s{i}.example")),
            })
            .collect();
        let by_key: HashMap<Option<String>, Arc<dyn Downloader>> = sources
            .iter()
            .map(|s| s.url.clone())
            .zip(peers.into_iter().map(|p| p as Arc<dyn Downloader>))
            .collect();
        Wire::with_dial(
            Box::new(move |s: &Source| by_key[&s.url].clone()),
            sources,
            &Settings::default(),
            "r",
            payload,
            None,
        )
        .expect("the first source opens")
    }

    fn mod_wire<D: Downloader + 'static>(dl: Arc<D>) -> Wire {
        wire_over(vec![dl], crate::trust::Payload::Mod)
    }

    fn game_wire<D: Downloader + 'static>(dl: Arc<D>) -> Wire {
        wire_over(vec![dl], crate::trust::Payload::Game)
    }

    fn game_wire2<D: Downloader + 'static>(a: Arc<D>, b: Arc<D>) -> Wire {
        wire_over(vec![a, b], crate::trust::Payload::Game)
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
    fn fresh_install_writes_only_the_manifests_files() {
        let dir = tempdir("fresh");
        let (m, assets) = basic_release();
        let dl = arc(Fake::new("v1.0.0", &m, assets));
        let r = install(&settings(&dir), &mod_wire(dl.clone()), None, None, None).unwrap();

        assert_eq!(r.written.len(), 2);
        assert_eq!(std::fs::read(dir.join("game/bin/win64/winmm.dll")).unwrap(), b"dll");
        assert_eq!(std::fs::read(dir.join("game/dota/a.vpk")).unwrap(), b"vpk");
        // The manifest ships a winmm.dll, which used to be the trigger for copying System32's
        // winmm.dll in beside it. Nothing does that now — an install writes the manifest's files
        // and NOTHING ELSE, which is the whole claim of a data-driven installer.
        assert!(!dir.join(WINMM_ORIG).exists(), "no system DLL is copied into the game folder");
        let st = InstalledState::load(&dir).unwrap();
        assert_eq!(st.version, "1.0.0");
        assert_eq!(st.files.len(), 2);
        assert!(!st.winmm_orig_created, "the legacy lineage flag is never newly set");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A folder an OLDER launcher set up: winmm_orig.dll on disk, `winmm_orig_created` in state.
    /// Updating it must carry that flag forward — dropping it would leave a stray copy of a system
    /// DLL in the game folder that no later uninstall would ever collect.
    #[test]
    fn a_legacy_winmm_orig_survives_an_update_and_is_collected_by_uninstall() {
        let dir = tempdir("legacy-winmm");
        let (m, assets) = basic_release();
        let dl = arc(Fake::new("v1.0.0", &m, assets));
        install(&settings(&dir), &mod_wire(dl.clone()), None, None, None).unwrap();

        // stage what an older launcher would have left behind
        std::fs::write(dir.join(WINMM_ORIG), b"SYSTEM WINMM").unwrap();
        let mut st = InstalledState::load(&dir).unwrap();
        st.winmm_orig_created = true;
        st.save(&dir).unwrap();

        // an update lands (v2 changes a.vpk), then a no-op install on top of it
        let m2 = m.replace("\"version\": \"1.0.0\"", "\"version\": \"1.0.1\"");
        let dl2 = arc(Fake::new("v1.0.1", &m2, vec![("winmm.dll", b"dll"), ("a.vpk", b"vpk")]));
        install(&settings(&dir), &mod_wire(dl2.clone()), None, None, None).unwrap();
        assert!(
            InstalledState::load(&dir).unwrap().winmm_orig_created,
            "the lineage flag must survive an update"
        );
        assert!(dir.join(WINMM_ORIG).exists(), "an update does not delete it either");

        let r = uninstall(&settings(&dir)).unwrap();
        assert!(r.winmm_orig_removed, "uninstall still collects a legacy winmm_orig.dll");
        assert!(!dir.join(WINMM_ORIG).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A manifest `remove[]` must be able to RETIRE the legacy winmm_orig.dll — that is how the
    /// dist repo clears it from existing installs when the shim stops needing it.
    ///
    /// It could not before: the file is not in `prev_dests` (it was never a manifest entry), so
    /// `back_up` read it as a genuine pre-existing original and preserved it into the vanilla
    /// store — from which uninstall's restore pass, which runs AFTER the winmm collection, put it
    /// straight back. The removal reported success and the file was still there afterwards.
    #[test]
    fn a_manifest_remove_retires_the_legacy_winmm_orig_for_good() {
        let dir = tempdir("winmm-remove");
        let (m, assets) = basic_release();
        let dl = arc(Fake::new("v1.0.0", &m, assets));
        install(&settings(&dir), &mod_wire(dl.clone()), None, None, None).unwrap();

        // what an older launcher left behind
        std::fs::write(dir.join(WINMM_ORIG), b"SYSTEM WINMM").unwrap();
        let mut st = InstalledState::load(&dir).unwrap();
        st.winmm_orig_created = true;
        st.save(&dir).unwrap();

        // the release that stops needing it says so in the manifest
        let m2 = serde_json::json!({
            "version": "1.0.1",
            "files": [
                file_json("winmm.dll", "game/bin/win64/winmm.dll", b"dll"),
                file_json("a.vpk", "game/dota/a.vpk", b"vpk"),
            ],
            "remove": [ { "dest": WINMM_ORIG } ]
        })
        .to_string();
        let dl2 = arc(Fake::new("v1.0.1", &m2, vec![("winmm.dll", b"dll"), ("a.vpk", b"vpk")]));
        let r = install(&settings(&dir), &mod_wire(dl2.clone()), None, None, None).unwrap();

        assert!(r.removed.contains(&WINMM_ORIG.to_string()), "the removal ran: {:?}", r.removed);
        assert!(!dir.join(WINMM_ORIG).exists(), "and the file is gone");
        assert!(
            !dir.join(VANILLA_DIR).join(WINMM_ORIG).exists(),
            "our own file must not be preserved as a vanilla original"
        );

        // the decisive half: it must not come back when the vanilla store is restored
        uninstall(&settings(&dir)).unwrap();
        assert!(!dir.join(WINMM_ORIG).exists(), "and it stays gone across an uninstall");
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
            name: Some("winmm.dll".to_string()),
            dest: dest.to_string(),
            sha256: sha(b"new"),
            size: 3,
        };
        let ctx = Ctx {
            game_dir: dir.clone(),
            backup_root: dir.join(BACKUP_DIR).join("1.0.0"),
            vanilla_root: dir.join(VANILLA_DIR),
            user_root: dir.join(USER_DIR),
            prev_dests: HashSet::new(),
            trust_prev: true,
            user_changed: HashSet::new(),
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
        let dl = arc(Fake::new("v1.0.0", &m, assets));
        let cache = dir.join(CACHE_DIR);
        std::fs::create_dir_all(&cache).unwrap();
        // a leftover .part as long as the finished asset (a completed transfer whose rename into
        // the cache failed, or a cancel landing on the last chunk)
        let part = cache.join(format!("{}.part", sha(b"dll")));
        std::fs::write(&part, b"XXX").unwrap();

        let r = install(&settings(&dir), &mod_wire(dl.clone()), None, None, None);
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
        install(&settings(&dir), &mod_wire(arc(Fake::new("v1.0.0", &m1, assets1))), None, None, None)
            .unwrap();
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
        let dl2 = arc(Fake::new("v2.0.0", &m2, vec![("winmm.dll", b"dll2"), ("a.vpk", b"vpk")]));
        install(&settings(&dir), &mod_wire(dl2.clone()), None, None, None).unwrap();

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
        let dl = arc(Fake::new("v1.0.0", &m, assets));
        install(&settings(&dir), &mod_wire(dl.clone()), None, None, None).unwrap();
        // lose the state file — the folder is now "up to date but not installed"
        std::fs::remove_file(InstalledState::path(&dir)).unwrap();
        assert!(InstalledState::load(&dir).is_none());

        let r = install(&settings(&dir), &mod_wire(dl.clone()), None, None, None).unwrap();
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
        let dl = arc(Fake::new("v1.0.0", &m, vec![("a.vpk", b"vpk"), ("fx.vpk", b"fx")]));

        let mut s = settings(&dir);
        s.selections.insert("fx".into(), serde_json::json!(true));
        install(&s, &mod_wire(dl.clone()), None, None, None).unwrap();
        assert!(dir.join("game/dota/fx.vpk").exists());

        s.selections.insert("fx".into(), serde_json::json!(false));
        let r = install(&s, &mod_wire(dl.clone()), None, None, None).unwrap();
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
        let dl = arc(Fake::new("v1.0.0", &m, vec![("a.vpk", b"vpk"), ("fx.vpk", b"fx")]));
        let s = settings(&dir);
        install(&s, &mod_wire(dl.clone()), None, None, None).unwrap();

        // install itself no longer prefetches the disabled toggle's asset...
        assert!(!dir.join(CACHE_DIR).join(sha(b"fx")).exists());
        // ...the detached warm does
        warm_cache(&s, &mod_wire(dl.clone()));
        assert!(dir.join(CACHE_DIR).join(sha(b"fx")).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn uninstall_reverts_to_stock() {
        let dir = tempdir("uninstall");
        let (m, assets) = basic_release();
        let dl = arc(Fake::new("v1.0.0", &m, assets));
        install(&settings(&dir), &mod_wire(dl.clone()), None, None, None).unwrap();

        let r = uninstall(&settings(&dir)).unwrap();
        assert_eq!(r.deleted.len(), 2);
        // nothing created one, so there is nothing to collect — the legacy path is covered by
        // a_legacy_winmm_orig_survives_an_update_and_is_collected_by_uninstall
        assert!(!r.winmm_orig_removed);
        assert!(!dir.join("game/bin/win64/winmm.dll").exists());
        assert!(!dir.join("game/dota/a.vpk").exists());
        assert!(!dir.join(WINMM_ORIG).exists());
        assert!(InstalledState::load(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_preexisting_loose_original_survives_install_and_uninstall() {
        // the gameinfo.gi shape: a genuine vanilla loose file the shim substitutes. Install must
        // preserve it in the vanilla store; uninstall must put it back, not delete the dest.
        let dir = tempdir("preexisting");
        std::fs::create_dir_all(dir.join("game/dota")).unwrap();
        std::fs::write(dir.join("game/dota/a.vpk"), b"STOCK ORIGINAL").unwrap();

        let (m, assets) = basic_release();
        let dl = arc(Fake::new("v1.0.0", &m, assets));
        install(&settings(&dir), &mod_wire(dl.clone()), None, None, None).unwrap();

        // shim in place, original preserved
        assert_eq!(std::fs::read(dir.join("game/dota/a.vpk")).unwrap(), b"vpk");
        assert_eq!(
            std::fs::read(dir.join(VANILLA_DIR).join("game/dota/a.vpk")).unwrap(),
            b"STOCK ORIGINAL"
        );

        let r = uninstall(&settings(&dir)).unwrap();
        assert!(r.restored.iter().any(|d| d == "game/dota/a.vpk"));
        assert_eq!(std::fs::read(dir.join("game/dota/a.vpk")).unwrap(), b"STOCK ORIGINAL");
        assert!(!dir.join(VANILLA_DIR).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_adopted_hash_matching_install_deletes_the_dest_on_uninstall() {
        // the OTHER history: the folder already contains the shim's bytes (a copied
        // already-patched install, no .phoenix-* metadata). The heal adopts them as ours with no
        // original ever preserved — uninstall then deletes the dest. Documents the accepted gap:
        // for a dest that substitutes a vanilla loose file, "revert to stock" leaves a hole.
        let dir = tempdir("adopted");
        std::fs::create_dir_all(dir.join("game/bin/win64")).unwrap();
        std::fs::create_dir_all(dir.join("game/dota")).unwrap();
        std::fs::write(dir.join("game/bin/win64/winmm.dll"), b"dll").unwrap();
        std::fs::write(dir.join("game/dota/a.vpk"), b"vpk").unwrap();

        let (m, assets) = basic_release();
        let dl = arc(Fake::new("v1.0.0", &m, assets));
        install(&settings(&dir), &mod_wire(dl.clone()), None, None, None).unwrap(); // no-op heal: adopts both

        let r = uninstall(&settings(&dir)).unwrap();
        assert!(r.deleted.iter().any(|d| d == "game/dota/a.vpk"));
        assert!(!dir.join("game/dota/a.vpk").exists());
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
        let dl = arc(Fake::new("v1.0.0", &m, vec![("a.vpk", b"vpk")]));

        assert!(install(&settings(&dir), &mod_wire(dl.clone()), None, None, None).is_err());
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
        let dl = arc(Fake::new("v1.0.0", &m, assets));

        let r = install(&settings(&dir), &mod_wire(dl.clone()), None, None, None).unwrap();
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
        let dl = arc(CutOnce {
            inner: Fake::new("v1.0.0", &m, vec![("big.vpk", &big)]),
            cut: 40_000,
            failed: false.into(),
        });

        // first run: dies mid-download, touches nothing but the resumable .part
        assert!(install(&settings(&dir), &mod_wire(dl.clone()), None, None, None).is_err());
        assert!(!dir.join("game/dota/big.vpk").exists());
        assert!(dir.join(CACHE_DIR).join(format!("{}.part", sha(&big))).exists());

        // second run: resumes the .part (asserted inside CutOnce) and completes
        let r = install(&settings(&dir), &mod_wire(dl.clone()), None, None, None).unwrap();
        assert_eq!(r.written, vec!["game/dota/big.vpk".to_string()]);
        assert_eq!(std::fs::read(dir.join("game/dota/big.vpk")).unwrap(), big);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Streams real, small chunks — unlike `Fake` and `CutOnce`, which each write a whole body in
    /// one call before the caller ever sees a progress tick. That is exactly the double this cap
    /// needs: a host that keeps sending well past the manifest's declared `size`, so the only way
    /// to prove the abort is MID-transfer (not a check that runs after the whole body already
    /// landed) is a downloader that can be caught in the act.
    struct Overflowing {
        inner: Fake,
        /// What the "host" actually sends — deliberately longer than the manifest's `size`.
        body: Vec<u8>,
        chunk: usize,
        calls: std::sync::atomic::AtomicU32,
        /// The last `written` value handed to the progress callback — read back after the call
        /// fails, since the resulting `.part` is swept by the existing over-long cleanup and can't
        /// be inspected afterward. This is the mid-transfer proof.
        last_written: std::sync::atomic::AtomicU64,
    }

    impl crate::downloader::Downloader for Overflowing {
        fn fetch_release(&self, r: &str, t: Option<&str>) -> Result<Release> {
            self.inner.fetch_release(r, t)
        }
        fn fetch_releases(&self, r: &str) -> Result<Vec<Release>> {
            self.inner.fetch_releases(r)
        }
        fn download(&self, a: &crate::downloader::Asset) -> Result<Vec<u8>> {
            self.inner.download(a)
        }
        fn download_to(
            &self,
            _asset: &crate::downloader::Asset,
            dest: &Path,
            resume_from: u64,
            progress: crate::downloader::ChunkProgress,
        ) -> Result<(u64, String)> {
            use std::sync::atomic::Ordering;
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(resume_from, 0, "the cap must fire on the first attempt, never a retry");
            let mut file = std::fs::File::create(dest)?;
            let mut written = 0u64;
            for piece in self.body.chunks(self.chunk) {
                std::io::Write::write_all(&mut file, piece)?;
                written += piece.len() as u64;
                self.last_written.store(written, Ordering::SeqCst);
                if !progress(written, Some(self.body.len() as u64)) {
                    anyhow::bail!("download aborted");
                }
            }
            Ok((written, sha(&self.body)))
        }
    }

    /// The signed `size` is a STREAMING ceiling, not a check that only runs once an unbounded
    /// body is already fully on disk — `obtain_to_cache` must cut a source off as soon as it
    /// sends more than the manifest promised.
    #[test]
    fn a_stream_longer_than_its_signed_size_is_aborted_mid_transfer() {
        use std::sync::atomic::Ordering;
        let dir = tempdir("overcap");
        let declared: Vec<u8> = (0..40u8).collect(); // what the manifest signs for
        let hostile: Vec<u8> =
            declared.iter().copied().chain(std::iter::repeat(b'X').take(100_000)).collect();
        let m = serde_json::json!({
            "version": "1.0.0",
            "files": [ { "name": "a.vpk", "dest": "game/dota/a.vpk",
                         "sha256": sha(&declared), "size": declared.len() } ]
        })
        .to_string();
        let dl = arc(Overflowing {
            inner: Fake::new("v1.0.0", &m, vec![("a.vpk", &declared)]),
            body: hostile,
            chunk: 16,
            calls: 0.into(),
            last_written: 0.into(),
        });

        let e = install(&settings(&dir), &mod_wire(dl.clone()), None, None, None).unwrap_err();
        let msg = format!("{e:#}");
        assert!(msg.contains("more than the signed"), "expected the size-cap refusal, got: {msg}");
        assert!(!msg.contains("aborted"), "must not read as an ordinary cancel: {msg}");
        assert_eq!(dl.calls.load(Ordering::SeqCst), 1, "a signed-size violation must not be retried");
        assert!(!dir.join("game/dota/a.vpk").exists());
        // the mid-transfer proof: writing stopped within one chunk of the signed size, nowhere
        // near the ~100,040-byte hostile body it would reach if the whole thing streamed through
        // before anything checked it
        let last = dl.last_written.load(Ordering::SeqCst);
        assert!(
            (declared.len() as u64..declared.len() as u64 + dl.chunk as u64).contains(&last),
            "expected the abort within one chunk past the {}-byte signed size, got {last} bytes written",
            declared.len()
        );
        // and the oversized .part left behind is swept, not kept holding hostile bytes
        assert!(!dir.join(CACHE_DIR).join(format!("{}.part", sha(&declared))).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Interrupted after `cut` bytes, then resumed with the real impls' accounting: `written`
    /// starts at the resumed prefix and grows by real chunks from there, exactly like github.rs.
    /// `Overflowing` above always starts at zero (it exists to prove the abort), and `CutOnce`
    /// never ticks progress at all on either attempt — neither exercises the cap guard on a
    /// resumed transfer, which is the one case the task calls out as easy to get silently wrong
    /// (counting from the prefix instead of from zero, or the reverse).
    struct ResumingToCap {
        inner: Fake,
        body: Vec<u8>,
        cut: usize,
        chunk: usize,
        calls: std::sync::atomic::AtomicU32,
        failed: std::sync::atomic::AtomicBool,
    }

    impl crate::downloader::Downloader for ResumingToCap {
        fn fetch_release(&self, r: &str, t: Option<&str>) -> Result<Release> {
            self.inner.fetch_release(r, t)
        }
        fn fetch_releases(&self, r: &str) -> Result<Vec<Release>> {
            self.inner.fetch_releases(r)
        }
        fn download(&self, a: &crate::downloader::Asset) -> Result<Vec<u8>> {
            self.inner.download(a)
        }
        fn download_to(
            &self,
            _asset: &crate::downloader::Asset,
            dest: &Path,
            resume_from: u64,
            progress: crate::downloader::ChunkProgress,
        ) -> Result<(u64, String)> {
            use std::io::{Seek, SeekFrom, Write};
            use std::sync::atomic::Ordering;
            self.calls.fetch_add(1, Ordering::SeqCst);
            if !self.failed.swap(true, Ordering::SeqCst) {
                assert_eq!(resume_from, 0, "first attempt must start fresh");
                std::fs::write(dest, &self.body[..self.cut])?;
                anyhow::bail!("simulated dropped connection");
            }
            assert_eq!(resume_from as usize, self.cut, "must resume from exactly what was written");
            let mut file = std::fs::OpenOptions::new().write(true).open(dest)?;
            file.seek(SeekFrom::Start(resume_from))?;
            let mut written = resume_from; // the prefix counts — same as every real impl
            for piece in self.body[self.cut..].chunks(self.chunk) {
                file.write_all(piece)?;
                written += piece.len() as u64;
                if !progress(written, Some(self.body.len() as u64)) {
                    anyhow::bail!("download aborted");
                }
            }
            Ok((written, sha(&self.body)))
        }
    }

    /// The companion the task explicitly asks for: a resume must not break under the new cap.
    /// `written` is seeded from the resumed prefix (not from zero), so a transfer that finishes
    /// EXACTLY at the signed size — however that size is split between the interrupted attempt
    /// and the resumed one — must complete, never get capped for bytes it already had on disk.
    #[test]
    fn a_resumed_transfer_that_completes_exactly_at_the_signed_size_is_not_capped() {
        use std::sync::atomic::Ordering;
        let dir = tempdir("resume-exact-cap");
        let content: Vec<u8> = (0..500u32).map(|i| (i % 251) as u8).collect();
        let m = serde_json::json!({
            "version": "1.0.0",
            "files": [ { "name": "a.vpk", "dest": "game/dota/a.vpk",
                         "sha256": sha(&content), "size": content.len() } ]
        })
        .to_string();
        let dl = arc(ResumingToCap {
            inner: Fake::new("v1.0.0", &m, vec![("a.vpk", &content)]),
            body: content.clone(),
            cut: 200,
            chunk: 37, // does not divide the remainder evenly, so the last tick lands off-grid
            calls: 0.into(),
            failed: false.into(),
        });

        // an abort with no NetKind is not retried within one run (same as `CutOnce`'s test), so
        // the interruption and the resume are two separate `install()` calls, exactly as a real
        // dropped connection and a later relaunch would be
        assert!(install(&settings(&dir), &mod_wire(dl.clone()), None, None, None).is_err());
        assert!(!dir.join("game/dota/a.vpk").exists());
        let r = install(&settings(&dir), &mod_wire(dl.clone()), None, None, None).unwrap();
        assert_eq!(r.written, vec!["game/dota/a.vpk".to_string()]);
        assert_eq!(std::fs::read(dir.join("game/dota/a.vpk")).unwrap(), content);
        assert_eq!(dl.calls.load(Ordering::SeqCst), 2, "attempt one drops, attempt two resumes and finishes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Fails `download_to` with a given error until `fail` runs out, then delegates to the Fake.
    struct Flaky {
        inner: Fake,
        fail: std::sync::atomic::AtomicU32,
        kind: Option<crate::downloader::NetKind>, // None = an error with no NetKind (an abort)
        calls: std::sync::atomic::AtomicU32,
    }

    impl crate::downloader::Downloader for Flaky {
        fn fetch_release(&self, r: &str, t: Option<&str>) -> Result<Release> {
            self.inner.fetch_release(r, t)
        }
        fn fetch_releases(&self, r: &str) -> Result<Vec<Release>> {
            self.inner.fetch_releases(r)
        }
        fn download(&self, a: &crate::downloader::Asset) -> Result<Vec<u8>> {
            self.inner.download(a)
        }
        fn download_to(
            &self,
            a: &crate::downloader::Asset,
            d: &Path,
            r: u64,
            p: crate::downloader::ChunkProgress,
        ) -> Result<(u64, String)> {
            use std::sync::atomic::Ordering;
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) > 0 {
                self.fail.fetch_sub(1, Ordering::SeqCst);
                return Err(match self.kind {
                    Some(k) => anyhow::Error::new(k).context("simulated server flake"),
                    None => anyhow!("download aborted"),
                });
            }
            self.inner.download_to(a, d, r, p)
        }
    }

    fn one_file_release() -> (String, Vec<(&'static str, &'static [u8])>) {
        let m = serde_json::json!({
            "version": "1.0.0",
            "files": [ file_json("a.vpk", "game/dota/a.vpk", b"vpk") ]
        })
        .to_string();
        (m, vec![("a.vpk", b"vpk")])
    }

    #[test]
    fn a_transient_500_is_retried_until_it_passes() {
        use crate::downloader::NetKind;
        let dir = tempdir("retry-500");
        let (m, assets) = one_file_release();
        let dl = arc(Flaky {
            inner: Fake::new("v1.0.0", &m, assets),
            fail: 2.into(),
            kind: Some(NetKind::Status(500)),
            calls: 0.into(),
        });
        // two flakes, then success — the user must never have seen an error
        install(&settings(&dir), &mod_wire(dl.clone()), None, None, None).unwrap();
        assert_eq!(dl.calls.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert_eq!(std::fs::read(dir.join("game/dota/a.vpk")).unwrap(), b"vpk");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn permanent_failures_and_aborts_are_not_retried() {
        use crate::downloader::NetKind;
        // a 404 is a fact about the request — retrying re-asks a settled question
        let dir = tempdir("retry-404");
        let (m, assets) = one_file_release();
        let dl = arc(Flaky {
            inner: Fake::new("v1.0.0", &m, assets),
            fail: 99.into(),
            kind: Some(NetKind::Status(404)),
            calls: 0.into(),
        });
        assert!(install(&settings(&dir), &mod_wire(dl.clone()), None, None, None).is_err());
        assert_eq!(dl.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(&dir);

        // an abort (cancel, sibling failure) is an instruction — retrying fights the user
        let dir = tempdir("retry-abort");
        let (m, assets) = one_file_release();
        let dl = arc(Flaky {
            inner: Fake::new("v1.0.0", &m, assets),
            fail: 99.into(),
            kind: None,
            calls: 0.into(),
        });
        assert!(install(&settings(&dir), &mod_wire(dl.clone()), None, None, None).is_err());
        assert_eq!(dl.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The power-loss shape: NTFS journals metadata, not data, so a `.part` can survive a crash
    /// with its LENGTH persisted and its tail never flushed. Resuming past that hashes garbage —
    /// and the mismatch only surfaces after the whole remainder has been re-fetched. Blaming the
    /// release there would spend a second full run to reach the same wall, so a verification
    /// failure that FOLLOWED a resume buys exactly one clean restart.
    #[test]
    fn a_poisoned_part_is_discarded_and_the_download_restarts_clean() {
        use std::sync::atomic::Ordering;
        let dir = tempdir("part-poison");
        let payload: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let m = serde_json::json!({
            "version": "1.0.0",
            "files": [ file_json("big.vpk", "game/dota/big.vpk", &payload) ]
        })
        .to_string();
        let dl = arc(Flaky {
            inner: Fake::new("v1.0.0", &m, vec![("big.vpk", &payload)]),
            fail: 0.into(),
            kind: None,
            calls: 0.into(),
        });
        // a resumable prefix of the right shape and the wrong bytes — zeros, as an unflushed
        // tail reads back
        let cache = dir.join(CACHE_DIR);
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join(format!("{}.part", sha(&payload))), vec![0u8; 400]).unwrap();

        install(&settings(&dir), &mod_wire(dl.clone()), None, None, None).unwrap();
        assert_eq!(dl.calls.load(Ordering::SeqCst), 2, "one resumed attempt, one clean restart");
        assert_eq!(std::fs::read(dir.join("game/dota/big.vpk")).unwrap(), payload);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other half of the same rule: bytes fetched WHOLE that still verify wrong are a fact
    /// about the source, and re-asking a settled question just burns the link.
    #[test]
    fn a_source_that_verifies_wrong_from_zero_is_not_retried() {
        use std::sync::atomic::Ordering;
        // the manifest describes GOOD; the release serves BAD at the same length, so it is the
        // hash — not the size — that refuses it
        let m = serde_json::json!({
            "version": "1.0.0",
            "files": [ file_json("a.vpk", "game/dota/a.vpk", b"GOOD") ]
        })
        .to_string();
        let bad = || Fake::new("v1.0.0", &m, vec![("a.vpk", b"BAD!")]);

        let dir = tempdir("bad-source");
        let dl = arc(Flaky { inner: bad(), fail: 0.into(), kind: None, calls: 0.into() });
        let err = install(&settings(&dir), &mod_wire(dl.clone()), None, None, None).unwrap_err();
        assert!(format!("{err:#}").contains("verification failed"), "got: {err:#}");
        assert_eq!(dl.calls.load(Ordering::SeqCst), 1, "nothing was resumed — one attempt settles it");
        let _ = std::fs::remove_dir_all(&dir);

        // and with a poisoned .part in front of it, the clean restart is spent exactly once
        // before the same verdict lands
        let dir = tempdir("bad-source-part");
        let cache = dir.join(CACHE_DIR);
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join(format!("{}.part", sha(b"GOOD"))), b"XX").unwrap();
        let dl = arc(Flaky { inner: bad(), fail: 0.into(), kind: None, calls: 0.into() });
        let err = install(&settings(&dir), &mod_wire(dl.clone()), None, None, None).unwrap_err();
        assert!(format!("{err:#}").contains("verification failed"), "got: {err:#}");
        assert_eq!(dl.calls.load(Ordering::SeqCst), 2, "the restart is spent once, not looped");
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
        let dl = arc(Fake::new("v1.0.0", &m, vec![("a.vpk", b"vpk")]));

        let r = install(&settings(&dir), &mod_wire(dl.clone()), None, None, None).unwrap();
        assert_eq!(r.removed, vec!["game/dota/stale.vpk".to_string()]);
        // the removal must STICK (the old bug preserved-then-restored it in one breath,
        // re-flagging it as Remove on every future plan, forever)...
        assert!(!dir.join("game/dota/stale.vpk").exists());
        // ...while the foreign file is preserved, not destroyed
        assert_eq!(std::fs::read(dir.join(VANILLA_DIR).join("game/dota/stale.vpk")).unwrap(), b"old");
        // and the next plan is clean — no permanent pending-remove loop
        let chk = crate::engine::check(&settings(&dir), dl.as_ref(), None).unwrap();
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
        let dl1 = arc(Fake::new("v1.0.0", &m1, vec![("a.vpk", b"vpk"), ("sound.vpk", b"PHOENIX")]));
        install(&settings(&dir), &mod_wire(dl1.clone()), None, None, None).unwrap();
        assert_eq!(std::fs::read(dir.join(VANILLA_DIR).join("game/dota/sound.vpk")).unwrap(), b"STOCK");

        // v2 stops shipping it and removes the dest — our file goes, the original comes back
        let m2 = serde_json::json!({
            "version": "2.0.0",
            "files": [ file_json("a.vpk", "game/dota/a.vpk", b"vpk") ],
            "remove": [ { "dest": "game/dota/sound.vpk" } ]
        })
        .to_string();
        let dl2 = arc(Fake::new("v2.0.0", &m2, vec![("a.vpk", b"vpk")]));
        let r = install(&settings(&dir), &mod_wire(dl2.clone()), None, None, None).unwrap();
        assert_eq!(r.removed, vec!["game/dota/sound.vpk".to_string()]);
        assert_eq!(std::fs::read(dir.join("game/dota/sound.vpk")).unwrap(), b"STOCK");
        let st = InstalledState::load(&dir).unwrap();
        assert_eq!(st.restored, vec!["game/dota/sound.vpk".to_string()]);

        // the next plan is CLEAN — the restored original must not re-flag as Remove
        let chk = crate::engine::check(&settings(&dir), dl2.as_ref(), None).unwrap();
        assert_eq!(chk.changes(), 0, "restored original re-flagged: {:?}", chk.files);

        // a no-op re-install (the heal path) carries the record instead of dropping it
        install(&settings(&dir), &mod_wire(dl2.clone()), None, None, None).unwrap();
        assert_eq!(
            InstalledState::load(&dir).unwrap().restored,
            vec!["game/dota/sound.vpk".to_string()]
        );
        let chk = crate::engine::check(&settings(&dir), dl2.as_ref(), None).unwrap();
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

        let dl1 = arc(Fake::new("v1", &ship("1.0.0", true, false), vec![("a.vpk", b"vpk"), ("sound.vpk", b"PHOENIX")]));
        install(&settings(&dir), &mod_wire(dl1.clone()), None, None, None).unwrap();
        let dl2 = arc(Fake::new("v2", &ship("2.0.0", false, true), vec![("a.vpk", b"vpk")]));
        install(&settings(&dir), &mod_wire(dl2.clone()), None, None, None).unwrap(); // restored
        let dl3 = arc(Fake::new("v3", &ship("3.0.0", true, false), vec![("a.vpk", b"vpk"), ("sound.vpk", b"PHOENIX")]));
        install(&settings(&dir), &mod_wire(dl3.clone()), None, None, None).unwrap(); // ships it again

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
        let dl = arc(Fake::new("v1.0.0", &m, assets));
        install(&settings(&dir), &mod_wire(dl.clone()), None, None, None).unwrap();

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
        let dl2 = arc(Fake::new("v1.0.1", &m2, vec![("winmm.dll", b"dll2")]));
        let err = install(&settings(&dir), &mod_wire(dl2.clone()), None, None, None).unwrap_err();
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
        let dl = arc(Fake::new("v1.0.0", &m, assets));
        install(&settings(&dir), &mod_wire(dl.clone()), None, None, None).unwrap();

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
        let dl2 = arc(Fake::new("v1.0.1", &m2, vec![("winmm.dll", b"dll2"), ("a.vpk", b"vpk")]));
        let err = install(&settings(&dir), &mod_wire(dl2.clone()), None, None, None).unwrap_err();
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

    // ---- selective repair, pins, and the extras scan ----

    /// A partial repair writes ONLY what it was given, and leaves everything else exactly as it
    /// found it — including files it would happily have rewritten had it been asked.
    #[test]
    fn base_repair_writes_only_the_selection() {
        let dir = tempdir("base-partial");
        let (m, assets) = base_release();
        let dl = arc(base_fake("v1805", &m, assets));
        let manifest = base_manifest(&dl);
        install_base(&dir, &game_wire(dl.clone()), &manifest, None, None, None).unwrap();

        // two differences: one the user picks, one they spare
        std::fs::write(dir.join("game/dota/cfg/a.cfg"), b"MOD").unwrap();
        std::fs::write(dir.join("game/core/cfg/b.cfg"), b"BAD").unwrap();

        let only: HashSet<String> = ["game/core/cfg/b.cfg".to_string()].into_iter().collect();
        let r = install_base(&dir, &game_wire(dl.clone()), &manifest, None, None, Some(&only)).unwrap();
        assert_eq!(r.written, 1);
        assert_eq!(std::fs::read(dir.join("game/core/cfg/b.cfg")).unwrap(), b"CFG", "restored");
        assert_eq!(
            std::fs::read(dir.join("game/dota/cfg/a.cfg")).unwrap(),
            b"MOD",
            "the spared file is untouched — this is the whole point of the selection"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A pin makes a modified file `Kept`: reported (never hidden), and skipped by an unrestricted
    /// run. Naming it explicitly still restores it — that is the user taking the approval back.
    #[test]
    fn a_pinned_file_survives_an_unrestricted_repair() {
        let dir = tempdir("base-pinned");
        let (m, assets) = base_release();
        let dl = arc(base_fake("v1805", &m, assets));
        let manifest = base_manifest(&dl);
        install_base(&dir, &game_wire(dl.clone()), &manifest, None, None, None).unwrap();

        let dest = "game/dota/cfg/a.cfg";
        std::fs::write(dir.join(dest), b"MY MOD").unwrap();
        let h = verify::sha256_file_cached(&dir.join(dest)).unwrap();
        let mut k = crate::keep::KeepList::default();
        k.pin(dest, &h, Some(fe_sha(&manifest, dest)));
        k.save(&dir).unwrap();

        let statuses = base_plan(&dir, &manifest, None, "verify", None).unwrap();
        let st = statuses.iter().find(|s| s.dest() == dest).unwrap();
        assert_eq!(st.action, BaseAction::Kept);
        assert!(!st.action.writes(), "a pin is not a difference to be fixed");

        // the sweeping repair leaves it alone
        install_base(&dir, &game_wire(dl.clone()), &manifest, None, None, None).unwrap();
        assert_eq!(std::fs::read(dir.join(dest)).unwrap(), b"MY MOD");

        // ...but naming it does not
        let only: HashSet<String> = [dest.to_string()].into_iter().collect();
        install_base(&dir, &game_wire(dl.clone()), &manifest, None, None, Some(&only)).unwrap();
        assert_eq!(std::fs::read(dir.join(dest)).unwrap(), b"CFG");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A PARTIAL repair must not read the whole install to write one file.
    ///
    /// It used to: `install_base` planned the entire manifest before filtering to the selection,
    /// so restoring a single edited file ran a full integrity pass over ~15 GB first — and the
    /// plan's file counter then fought the download's own ticks for the one progress line that
    /// screen has, because both are emitted under op "game" and `emit` drains asynchronously.
    /// The plan tick count is the observable proof of what got read.
    #[test]
    fn a_partial_repair_plans_only_the_selection() {
        let dir = tempdir("base-partial-plan");
        let (m, assets) = base_release();
        let dl = arc(base_fake("v1805", &m, assets));
        let manifest = base_manifest(&dl);
        install_base(&dir, &game_wire(dl.clone()), &manifest, None, None, None).unwrap();

        let dest = "game/dota/cfg/a.cfg";
        std::fs::write(dir.join(dest), b"MY MOD").unwrap();

        // The plan's `total` is the entry count it is working through — the one signal that is
        // exact whatever the tick throttle does (`d == total` always reports), so it says how many
        // files were about to be hashed rather than how many happened to be narrated.
        //
        // Selected by `phase`, NOT by "carries no bytes": the plan narrates bytes as well, for
        // files big enough that hashing them would freeze the counter. This test read them apart
        // the wrong way and only passed because its fixtures are three bytes long.
        let planned_total = std::sync::Mutex::new(None);
        let emit = |p: engine::OpProgress| {
            if p.op == "game" && p.phase == "plan" {
                *planned_total.lock().unwrap() = Some(p.total);
            }
        };
        let only: HashSet<String> = [dest.to_string()].into_iter().collect();
        let r = install_base(&dir, &game_wire(dl.clone()), &manifest, Some(&emit), None, Some(&only)).unwrap();

        assert_eq!(r.written, 1, "the one file the user picked is restored");
        assert_eq!(std::fs::read(dir.join(dest)).unwrap(), b"CFG");
        assert_eq!(
            planned_total.into_inner().unwrap(),
            Some(1),
            "a one-file repair planned more than the one file — that is a full verification the \
             user never asked for, racing the download for the same progress line"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A subset plan must answer for exactly the dests it was asked about, and give the SAME
    /// verdict the whole-install plan gives — the Your-files screen is built on it, and a cheaper
    /// answer that disagreed with the expensive one would be worse than no answer.
    #[test]
    fn a_subset_plan_reads_only_what_it_was_asked_about() {
        let dir = tempdir("base-plan-subset");
        let (m, assets) = base_release();
        let dl = arc(base_fake("v1805", &m, assets));
        let manifest = base_manifest(&dl);
        install_base(&dir, &game_wire(dl.clone()), &manifest, None, None, None).unwrap();

        let dest = "game/dota/cfg/a.cfg";
        std::fs::write(dir.join(dest), b"MY MOD").unwrap();

        let whole = base_plan(&dir, &manifest, None, "verify", None).unwrap();
        assert!(whole.len() > 1, "the fixture has more than the one dest under test");

        let only: HashSet<String> = [dest.to_string()].into_iter().collect();
        let subset = base_plan_of(&dir, &manifest, None, "yours", None, Some(&only)).unwrap();
        assert_eq!(subset.len(), 1, "nothing outside the selection is planned — or read");
        assert_eq!(subset[0].dest(), dest);
        assert_eq!(
            subset[0].action,
            whole.iter().find(|s| s.dest() == dest).unwrap().action,
            "the cheap answer must be the same answer"
        );

        // a dest the manifest does not carry has no verdict to give, and asking for one is not an
        // error — the keep list can name paths no authority claims
        let stranger: HashSet<String> = ["game/dota/nobody.txt".to_string()].into_iter().collect();
        assert!(base_plan_of(&dir, &manifest, None, "yours", None, Some(&stranger)).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The pin is on the BYTES. Change the file again and the approval does not carry over — the
    /// thing the user looked at is not the thing that is there now.
    #[test]
    fn a_pin_expires_when_the_content_changes_again() {
        let dir = tempdir("base-pin-expiry");
        let (m, assets) = base_release();
        let dl = arc(base_fake("v1805", &m, assets));
        let manifest = base_manifest(&dl);
        install_base(&dir, &game_wire(dl.clone()), &manifest, None, None, None).unwrap();

        let dest = "game/dota/cfg/a.cfg";
        std::fs::write(dir.join(dest), b"MOD v1").unwrap();
        let mut k = crate::keep::KeepList::default();
        k.pin(dest, &verify::sha256_file_cached(&dir.join(dest)).unwrap(), Some(fe_sha(&manifest, dest)));
        k.save(&dir).unwrap();

        std::fs::write(dir.join(dest), b"MOD v2 - a different length entirely").unwrap();
        let statuses = base_plan(&dir, &manifest, None, "verify", None).unwrap();
        let st = statuses.iter().find(|s| s.dest() == dest).unwrap();
        assert_eq!(
            st.action,
            BaseAction::Differs,
            "a size mismatch must not short-circuit past a pinned dest, or a pin could never \
             expire and could never have matched in the first place"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The evidence a user judges a difference by: expected vs actual length, and when it changed.
    #[test]
    fn a_plan_carries_the_evidence_for_each_difference() {
        let dir = tempdir("base-evidence");
        let (m, assets) = base_release();
        let dl = arc(base_fake("v1805", &m, assets));
        let manifest = base_manifest(&dl);
        install_base(&dir, &game_wire(dl.clone()), &manifest, None, None, None).unwrap();
        std::fs::write(dir.join("game/dota/cfg/a.cfg"), b"much longer than three").unwrap();
        std::fs::remove_file(dir.join("game/core/cfg/b.cfg")).unwrap();

        let statuses = base_plan(&dir, &manifest, None, "verify", None).unwrap();
        let st = |d: &str| statuses.iter().find(|s| s.dest() == d).unwrap();
        let a = st("game/dota/cfg/a.cfg");
        assert_eq!(a.entry.size, 3);
        assert_eq!(a.local_size, Some(22));
        assert!(a.mtime.is_some());
        let b = st("game/core/cfg/b.cfg");
        assert_eq!(b.action, BaseAction::Missing);
        assert_eq!(b.local_size, None, "nothing there to measure");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The extras scan finds what nobody claims, summarizes an unknown subtree as ONE row, and
    /// never offers our own machinery.
    #[test]
    fn extras_report_unclaimed_files_and_summarize_unknown_trees() {
        let dir = tempdir("base-extras");
        let (m, assets) = base_release();
        let dl = arc(base_fake("v1805", &m, assets));
        let manifest = base_manifest(&dl);
        install_base(&dir, &game_wire(dl.clone()), &manifest, None, None, None).unwrap();

        // a mod dropping a loose file among stock ones
        std::fs::write(dir.join("game/dota/cfg/mymod.cfg"), b"hello").unwrap();
        // ...and a whole subtree of its own
        std::fs::create_dir_all(dir.join("game/dota/addons/big/sub")).unwrap();
        std::fs::write(dir.join("game/dota/addons/big/one.txt"), b"1").unwrap();
        std::fs::write(dir.join("game/dota/addons/big/sub/two.txt"), b"22").unwrap();
        // our own scratch must never be offered for deletion
        std::fs::create_dir_all(dir.join(".phoenix-cache/base")).unwrap();
        std::fs::write(dir.join(".phoenix-cache/base/junk"), b"x").unwrap();

        let claimed: HashSet<String> = manifest.files.iter().map(|f| f.dest.clone()).collect();
        let (extras, end) = scan_extras(&dir, &manifest, &claimed, None);
        let truncated = end == ExtrasEnd::Capped;
        assert!(!truncated);
        let paths: Vec<&str> = extras.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"game/dota/cfg/mymod.cfg"));
        assert!(
            paths.contains(&"game/dota/addons"),
            "an unknown subtree is ONE row, not a listing: {paths:?}"
        );
        assert!(!paths.iter().any(|p| p.starts_with(".phoenix")), "ours: {paths:?}");
        let tree = extras.iter().find(|e| e.path == "game/dota/addons").unwrap();
        assert_eq!((tree.files, tree.size), (2, 3));

        // and deletion only reaches what the scan reports — a crafted path cannot touch the game
        let n = delete_extras(
            &dir,
            &manifest,
            &claimed,
            &["game/dota/cfg/mymod.cfg".into(), "game/dota/pak01_dir.vpk".into()],
        )
        .unwrap();
        assert_eq!(n, 1, "only the extra was legal");
        assert!(!dir.join("game/dota/cfg/mymod.cfg").exists());
        assert!(dir.join("game/dota/pak01_dir.vpk").exists(), "a manifest file is unreachable");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The legacy winmm_orig.dll is nobody's file now, so the view built to show unclaimed files
    /// has to show it and the delete control has to reach it. It used to be hardcoded into
    /// `known`, which made the one leftover this launcher is responsible for the only file in the
    /// folder its owner could neither see nor remove.
    #[test]
    fn a_legacy_winmm_orig_is_an_extra_the_user_can_delete() {
        let dir = tempdir("winmm-extra");
        let (m, assets) = base_release();
        let dl = arc(base_fake("v1805", &m, assets));
        let manifest = base_manifest(&dl);
        install_base(&dir, &game_wire(dl.clone()), &manifest, None, None, None).unwrap();
        std::fs::write(dir.join(WINMM_ORIG), b"SYSTEM WINMM").unwrap();

        let claimed: HashSet<String> = manifest.files.iter().map(|f| f.dest.clone()).collect();
        let (extras, _) = scan_extras(&dir, &manifest, &claimed, None);
        let paths: Vec<&str> = extras.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&WINMM_ORIG), "nothing claims it any more: {paths:?}");
        // its directory is still walked file by file, not summarized as one deletable subtree —
        // dota2.exe lives there and is claimed
        assert!(!paths.contains(&"game/bin/win64"), "not a summarized subtree: {paths:?}");
        assert!(!paths.contains(&"game/bin/win64/dota2.exe"));

        let n = delete_extras(&dir, &manifest, &claimed, &[WINMM_ORIG.into()]).unwrap();
        assert_eq!(n, 1);
        assert!(!dir.join(WINMM_ORIG).exists());
        assert!(dir.join("game/bin/win64/dota2.exe").exists(), "the game is untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The shim lives in directories the GAME manifest has never heard of. Nothing it owns may be
    /// reported as foreign, and above all its directory must not be summarized as one deletable
    /// subtree — that offered the user a single terracotta control that would erase their whole
    /// Phoenix install, under the label "not part of the game".
    #[test]
    fn a_phoenix_only_directory_is_never_foreign() {
        let dir = tempdir("base-extras-shim");
        let (m, assets) = base_release();
        let dl = arc(base_fake("v1805", &m, assets));
        let manifest = base_manifest(&dl);
        install_base(&dir, &game_wire(dl.clone()), &manifest, None, None, None).unwrap();

        // the shim's payload: its own top-level tree, plus a file among the game's own
        std::fs::create_dir_all(dir.join("game/dota_phoenix/pak01")).unwrap();
        std::fs::write(dir.join("game/dota_phoenix/pak01/textures.vpk"), b"phx").unwrap();
        std::fs::write(dir.join("game/dota_phoenix/hud.vpk"), b"hud").unwrap();
        std::fs::create_dir_all(dir.join("game/bin/win64")).unwrap();
        std::fs::write(dir.join("game/bin/win64/winmm.dll"), b"proxy").unwrap();
        // ...and a file genuinely nobody claims, sitting right beside them
        std::fs::write(dir.join("game/dota_phoenix/leftover.txt"), b"mine").unwrap();

        let claimed: HashSet<String> = manifest
            .files
            .iter()
            .map(|f| f.dest.clone())
            .chain([
                "game/dota_phoenix/pak01/textures.vpk".to_string(),
                "game/dota_phoenix/hud.vpk".to_string(),
                "game/bin/win64/winmm.dll".to_string(),
            ])
            .collect();
        let (extras, _) = scan_extras(&dir, &manifest, &claimed, None);
        let paths: Vec<&str> = extras.iter().map(|e| e.path.as_str()).collect();

        assert!(
            !paths.contains(&"game/dota_phoenix"),
            "the shim's own directory summarized as one foreign subtree: {paths:?}"
        );
        for p in &paths {
            assert!(!claimed.contains(*p), "{p} is claimed and must not be reported");
        }
        // the genuinely unclaimed file beside them is still found — the fix must not blind the
        // scan to real extras just because they share a directory with the shim
        assert!(paths.contains(&"game/dota_phoenix/leftover.txt"), "{paths:?}");

        // and deletion cannot reach the shim even if the UI asked for it
        let n = delete_extras(
            &dir,
            &manifest,
            &claimed,
            &["game/dota_phoenix".into(), "game/dota_phoenix/hud.vpk".into()],
        )
        .unwrap();
        assert_eq!(n, 0, "neither the shim's tree nor its files are legal targets");
        assert!(dir.join("game/dota_phoenix/hud.vpk").exists());
        assert!(dir.join("game/bin/win64/winmm.dll").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `ignore` is manifest-driven and total: exact path, `dir/` subtree, `*.ext` suffix.
    #[test]
    fn the_ignore_list_quiets_exactly_three_shapes() {
        let ig = vec![
            "game/dota/cfg/config.cfg".to_string(),
            "replays/".to_string(),
            "*.log".to_string(),
        ];
        assert!(ignores_extra(&ig, "game/dota/cfg/config.cfg"));
        assert!(!ignores_extra(&ig, "game/dota/cfg/config.cfg.bak"));
        assert!(ignores_extra(&ig, "replays/1.dem"));
        assert!(!ignores_extra(&ig, "myreplays/1.dem"));
        assert!(ignores_extra(&ig, "game/dota/console.LOG"), "extensions are case-insensitive");
        assert!(!ignores_extra(&ig, "game/dota/log"));
    }

    /// Uninstall reverts what it installed and REFUSES to delete what it no longer recognises.
    #[test]
    fn uninstall_keeps_a_file_somebody_changed() {
        let dir = tempdir("uninstall-modified");
        let (m, assets) = basic_release();
        let dl = arc(Fake::new("v1.0.0", &m, assets));
        install(&settings(&dir), &mod_wire(dl.clone()), None, None, None).unwrap();

        let state = InstalledState::load(&dir).unwrap();
        let victim = state.files[0].dest.clone();
        let untouched = state.files[1].dest.clone();
        std::fs::write(dir.join(&victim), b"I edited this").unwrap();

        let r = uninstall(&settings(&dir)).unwrap();
        assert_eq!(r.kept, vec![victim.clone()]);
        assert_eq!(
            std::fs::read(dir.join(&victim)).unwrap(),
            b"I edited this",
            "revert must not mean deleting work the launcher did not do"
        );
        assert!(r.deleted.contains(&untouched), "the untouched file still reverts");
        assert!(!dir.join(&untouched).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What the manifest currently ships at `dest` — the other half of a pin (see keep.rs).
    fn fe_sha(m: &Manifest, dest: &str) -> String {
        m.files.iter().find(|f| f.dest == dest).unwrap().sha256.clone()
    }

    /// A base-game release double. It publishes the GAME payload, because `manifest_of` refuses a
    /// signed manifest naming a different one — the same gate the real game repo clears.
    fn base_fake(tag: &str, manifest_json: &str, assets: Vec<(&str, &[u8])>) -> Fake {
        Fake::new(tag, manifest_json, assets).payload("game")
    }

    fn base_manifest(dl: &Fake) -> Manifest {
        let release = dl.fetch_release("r", None).unwrap();
        engine::manifest_of(&Settings::default(), dl, &release, crate::trust::Payload::Game)
            .unwrap()
    }

    #[test]
    fn base_install_into_empty_dir_writes_everything_and_no_state() {
        let dir = tempdir("base-fresh");
        let (m, assets) = base_release();
        let dl = arc(base_fake("v1805", &m, assets));
        let manifest = base_manifest(&dl);

        let r = install_base(&dir, &game_wire(dl.clone()), &manifest, None, None, None).unwrap();
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
        let dl = arc(base_fake("v1805", &m, vec![("real", b"DATA")]));
        let manifest = base_manifest(&dl);

        let r = install_base(&dir, &game_wire(dl.clone()), &manifest, None, None, None).unwrap();
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
        let dl = arc(base_fake("v1805", &m, vec![]));
        let manifest = base_manifest(&dl);
        let err = install_base(&dir, &game_wire(dl.clone()), &manifest, None, None, None).unwrap_err();
        assert!(format!("{err:#}").contains("not the empty hash"), "got: {err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A fresh download composes its destination INSIDE the folder the user picked, so the folder
    /// it installs into does not exist yet — every earlier caller handed it one the picker had just
    /// returned. Nothing may assume it is there.
    #[test]
    fn base_install_creates_a_destination_that_does_not_exist_yet() {
        let base = tempdir("base-nested");
        let (prefix, target) = target_of(&base.to_string_lossy(), Some(GAME_SUBDIR));
        assert_eq!(format!("{prefix}{GAME_SUBDIR}"), target);
        let dir = PathBuf::from(&target);
        assert!(!dir.exists(), "the point of the test");

        let (m, assets) = base_release();
        let dl = arc(base_fake("v1805", &m, assets));
        let manifest = base_manifest(&dl);
        let r = install_base(&dir, &game_wire(dl.clone()), &manifest, None, None, None).unwrap();
        assert_eq!(r.written, 4);
        assert_eq!(std::fs::read(dir.join("game/dota/pak01_dir.vpk")).unwrap(), b"PAK");
        assert!(game_present(&dir), "and it is a game folder afterwards");
        // the folder the user picked is untouched apart from the one it now contains
        assert_eq!(foreign_entry_count(&base), 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn base_repair_touches_only_damaged_files() {
        let dir = tempdir("base-repair");
        let (m, assets) = base_release();
        let dl = arc(base_fake("v1805", &m, assets));
        let manifest = base_manifest(&dl);
        install_base(&dir, &game_wire(dl.clone()), &manifest, None, None, None).unwrap();

        // corrupt one file, delete another, leave the rest alone
        std::fs::write(dir.join("game/dota/pak01_dir.vpk"), b"CORRUPT").unwrap();
        std::fs::remove_file(dir.join("game/dota/cfg/a.cfg")).unwrap();

        let r = install_base(&dir, &game_wire(dl.clone()), &manifest, None, None, None).unwrap();
        assert_eq!(r.written, 2, "only the corrupt + missing files");
        assert_eq!(r.up_to_date, 2);
        assert_eq!(std::fs::read(dir.join("game/dota/pak01_dir.vpk")).unwrap(), b"PAK");
        assert_eq!(std::fs::read(dir.join("game/dota/cfg/a.cfg")).unwrap(), b"CFG");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file we could not READ is not a file we know to be damaged. Both are `Write` (rewriting
    /// is the cure when the cause is the disk, and probe_writable names a lock or an ACL before a
    /// byte downloads) — but the report has to keep them apart, or a user whose antivirus is
    /// holding a VPK is sent to re-download 15 GB for a problem no download can fix.
    #[test]
    fn a_file_that_cannot_be_read_is_reported_apart_from_a_damaged_one() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempdir("base-unreadable");
        let (m, assets) = base_release();
        let dl = arc(base_fake("v1805", &m, assets));
        let manifest = base_manifest(&dl);
        install_base(&dir, &game_wire(dl.clone()), &manifest, None, None, None).unwrap();

        // held open with no sharing at all — what an antivirus or a second process produces
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(dir.join("game/dota/pak01_dir.vpk"))
            .unwrap();
        // and genuine damage for contrast, at the SAME length so only the hash can catch it
        std::fs::write(dir.join("game/core/cfg/b.cfg"), b"XXX").unwrap();

        let statuses = base_plan(&dir, &manifest, None, "verify", None).unwrap();
        let st = |dest: &str| statuses.iter().find(|s| s.dest() == dest).unwrap();
        // the cause travels WITH the verdict now: both would be rewritten by a repair, and only
        // the action word says which problem the user actually has
        assert_eq!(st("game/dota/pak01_dir.vpk").action, BaseAction::Unreadable);
        assert_eq!(st("game/core/cfg/b.cfg").action, BaseAction::Differs);
        assert!(st("game/dota/pak01_dir.vpk").action.writes(), "a repair still covers it");

        drop(lock);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The LENGTH settles a mismatch before a byte is read — a content hash implies a content
    /// length, so a truncated multi-GB VPK never has to be hashed to prove what its size already
    /// proved. Observable through the read itself: a file that is BOTH the wrong size and
    /// unreadable comes back as plain damage, which can only happen if nothing tried to read it.
    #[test]
    fn a_wrong_size_file_is_damaged_without_being_read() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempdir("base-size-gate");
        let (m, assets) = base_release();
        let dl = arc(base_fake("v1805", &m, assets));
        let manifest = base_manifest(&dl);
        install_base(&dir, &game_wire(dl.clone()), &manifest, None, None, None).unwrap();

        let victim = dir.join("game/dota/pak01_dir.vpk");
        std::fs::write(&victim, b"PAK plus a great deal more").unwrap(); // manifest says 3 bytes
        let lock =
            std::fs::OpenOptions::new().read(true).share_mode(0).open(&victim).unwrap();

        let statuses = base_plan(&dir, &manifest, None, "verify", None).unwrap();
        let st = statuses.iter().find(|s| s.dest() == "game/dota/pak01_dir.vpk").unwrap();
        assert_eq!(st.action, BaseAction::Differs, "the size settled it — nothing opened the file");

        drop(lock);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// steam.inf answers "which build is this", and the answer is reached by READING a file. A
    /// read that fails says nothing about the build — and a bare bool had to spell that as
    /// "foreign", which is the one verdict that routes the user to an irreversible overwrite.
    #[test]
    fn an_unreadable_steam_inf_is_neither_ours_nor_foreign() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempdir("build-identity");
        let m = serde_json::json!({
            "schema": 2, "version": "1805",
            "files": [ file_json("steam.inf", "game/dota/steam.inf", b"ClientVersion=1805") ]
        })
        .to_string();
        let manifest = crate::manifest::Manifest::parse(m.as_bytes()).unwrap();
        let inf = dir.join("game/dota/steam.inf");

        // an empty folder is a fresh target, not a foreign build — that is where installs start
        assert_eq!(build_identity(&dir, &manifest), BuildIdentity::Same);

        std::fs::create_dir_all(inf.parent().unwrap()).unwrap();
        std::fs::write(&inf, b"ClientVersion=1805").unwrap();
        assert_eq!(build_identity(&dir, &manifest), BuildIdentity::Same);

        // a different LENGTH, deliberately: two same-size writes microseconds apart share an
        // mtime on Windows, and the (size,mtime) hash memo would keep reporting the old content
        std::fs::write(&inf, b"ClientVersion=99999").unwrap();
        assert_eq!(build_identity(&dir, &manifest), BuildIdentity::Foreign);

        std::fs::write(&inf, b"ClientVersion=1805").unwrap();
        let lock = std::fs::OpenOptions::new().read(true).share_mode(0).open(&inf).unwrap();
        assert_eq!(build_identity(&dir, &manifest), BuildIdentity::Unknown);

        drop(lock);
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
        let dl = arc(base_fake("v1805", &mm, assets));
        let manifest = base_manifest(&dl);
        let r = install_base(&dir, &game_wire(dl.clone()), &manifest, None, None, None).unwrap();
        assert_eq!(r.written, 1);
        assert_eq!(r.skipped, 1);
        assert_eq!(std::fs::read(dir.join(".phoenix-vanilla/game/dota/cfg/a.cfg")).unwrap(), b"CFG");
        assert!(!dir.join("game/dota/cfg/a.cfg").exists(), "the removal must stick");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn game_presence_and_pending_download_bytes() {
        let dir = tempdir("presence");
        // empty folder: no game, nothing pending
        assert!(!game_present(&dir));
        assert_eq!(pending_base_bytes(&dir), 0);

        // an interrupted download's cache: still not a game, but bytes are pending
        let cache = dir.join(CACHE_DIR).join(BASE_CACHE_SUBDIR);
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join(sha(b"a")), b"12345").unwrap();
        std::fs::write(cache.join(format!("{}.part", sha(b"b"))), b"123").unwrap();
        assert!(!game_present(&dir));
        assert_eq!(pending_base_bytes(&dir), 8);

        // game/dota appearing makes it a game folder
        std::fs::create_dir_all(dir.join("game/dota")).unwrap();
        assert!(game_present(&dir));
        let _ = std::fs::remove_dir_all(&dir);

        // ...and so does a shim install record alone (game/ deleted but state intact)
        let dir2 = tempdir("presence-state");
        InstalledState {
            version: "1.0.0".into(),
            files: vec![],
            winmm_orig_created: false,
            restored: vec![],
        }
        .save(&dir2)
        .unwrap();
        assert!(game_present(&dir2));
        let _ = std::fs::remove_dir_all(&dir2);
    }

    /// The two facts the download dialog reads off a folder before it offers to fill it: is a game
    /// (or an interrupted download of one) already here, and how much of somebody else's is.
    #[test]
    fn a_destination_reports_what_is_already_in_it() {
        let dir = tempdir("dest-probe");
        assert!(!game_started(&dir));
        assert_eq!(foreign_entry_count(&dir), 0);

        // an interrupted download counts as started — the dialog offers to continue, not to fill
        let cache = dir.join(CACHE_DIR).join(BASE_CACHE_SUBDIR);
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join(sha(b"a")), b"12345").unwrap();
        assert!(game_started(&dir));
        // ...and our own bookkeeping is not "somebody else's files", exactly as the extras scan
        // does not report it
        assert_eq!(foreign_entry_count(&dir), 0);

        std::fs::write(dir.join("taxes.xlsx"), b"x").unwrap();
        std::fs::create_dir_all(dir.join("photos")).unwrap();
        assert_eq!(foreign_entry_count(&dir), 2, "top-level entries, files and folders alike");
        // a folder that does not exist is the ordinary fresh case, not an error
        assert_eq!(foreign_entry_count(&dir.join("nope")), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Everything refused here either cannot become a folder, or becomes one with a different name
    /// than the dialog showed — Win32 strips edge spaces and trailing dots before it resolves a
    /// path, and resolves device names to devices.
    #[test]
    fn a_destination_name_that_would_not_be_that_folder_is_refused() {
        assert_eq!(subdir_issue(GAME_SUBDIR), None);
        assert_eq!(subdir_issue("Dota 2 6.88f"), None, "spaces and dots inside are fine");
        assert_eq!(subdir_issue("Дота"), None, "and so is anything not ASCII");

        assert_eq!(subdir_issue(""), Some(SubdirIssue::Empty));
        assert_eq!(subdir_issue("a/b"), Some(SubdirIssue::Separator));
        assert_eq!(subdir_issue("a\\b"), Some(SubdirIssue::Separator));
        assert_eq!(subdir_issue("D:"), Some(SubdirIssue::Chars), "a drive means another place");
        assert_eq!(subdir_issue("what?"), Some(SubdirIssue::Chars));
        assert_eq!(subdir_issue("a\tb"), Some(SubdirIssue::Chars));
        assert_eq!(subdir_issue("dota "), Some(SubdirIssue::Edge));
        assert_eq!(subdir_issue(" dota"), Some(SubdirIssue::Edge));
        assert_eq!(subdir_issue("dota."), Some(SubdirIssue::Edge));
        assert_eq!(subdir_issue(".."), Some(SubdirIssue::Edge));
        assert_eq!(subdir_issue("."), Some(SubdirIssue::Edge));
        assert_eq!(subdir_issue(".hidden"), None, "a LEADING dot is a legal name");
        assert_eq!(subdir_issue("nul"), Some(SubdirIssue::Reserved));
        assert_eq!(subdir_issue("COM1.txt"), Some(SubdirIssue::Reserved));
        assert_eq!(subdir_issue(&"x".repeat(GAME_SUBDIR_MAX + 1)), Some(SubdirIssue::TooLong));
        // the length cap counts CHARACTERS, not bytes: a Cyrillic name is two bytes a letter and
        // must not be refused at half the length an ASCII one is allowed
        assert_eq!(subdir_issue(&"я".repeat(GAME_SUBDIR_MAX)), None);
    }

    /// The head shown and the path sent come from one rule, and the case that proves it is a drive
    /// root: it already ends in a separator and must not be given a second one.
    #[test]
    fn a_composed_destination_shows_the_path_it_sends() {
        let (prefix, target) = target_of("D:\\Games", Some("dota2_688f"));
        assert_eq!(prefix, "D:\\Games\\");
        assert_eq!(target, "D:\\Games\\dota2_688f");
        assert_eq!(format!("{prefix}dota2_688f"), target, "prefix + name IS the target");

        let (prefix, target) = target_of("D:\\", Some("dota2_688f"));
        assert_eq!(prefix, "D:\\");
        assert_eq!(target, "D:\\dota2_688f");

        // no subfolder: the picked folder is the destination, unchanged and unsuffixed
        let (prefix, target) = target_of("D:\\Games", None);
        assert_eq!(prefix, "D:\\Games");
        assert_eq!(target, "D:\\Games");
    }

    #[test]
    fn base_cached_counts_bytes_by_hash_and_files_by_dest() {
        let dir = tempdir("cached-bytes");
        let (m, _) = base_release();
        let manifest = crate::manifest::Manifest::parse(m.as_bytes()).unwrap();
        let statuses = base_plan(&dir, &manifest, None, "plan", None).unwrap();

        let cache = dir.join(CACHE_DIR).join(BASE_CACHE_SUBDIR);
        std::fs::create_dir_all(&cache).unwrap();
        // EXE (3 bytes) fully cached; PAK has a 2-byte .part; the shared CFG asset absent
        std::fs::write(cache.join(sha(b"EXE")), b"EXE").unwrap();
        std::fs::write(cache.join(format!("{}.part", sha(b"PAK"))), b"PA").unwrap();
        // bytes: 3 (full) + 2 (part). files: only EXE — a .part is byte progress, not a file
        assert_eq!(base_cached(&dir, &manifest, &statuses), (5, 1));

        // the CFG asset landing makes BOTH its dests fetched files, but its bytes count once
        std::fs::write(cache.join(sha(b"CFG")), b"CFG").unwrap();
        assert_eq!(base_cached(&dir, &manifest, &statuses), (8, 3));

        // an over-long leftover must not report more than the plan asked for
        std::fs::write(cache.join(sha(b"EXE")), b"EXE-OVERLONG").unwrap();
        assert_eq!(base_cached(&dir, &manifest, &statuses), (8, 3));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- bundles (manifest schema 3) ----

    /// Concatenate members and zstd-pack them: (packed bytes, psha256, decoded size).
    fn pack(members: &[&[u8]]) -> (Vec<u8>, String, u64) {
        let mut stream = Vec::new();
        for m in members {
            stream.extend_from_slice(m);
        }
        let packed = zstd::stream::encode_all(&stream[..], 3).unwrap();
        let psha = sha(&packed);
        (packed, psha, stream.len() as u64)
    }

    /// A schema-3 entry: no `name` — its bytes come from a bundle (or nowhere, when empty).
    fn bundled_json(dest: &str, bytes: &[u8]) -> serde_json::Value {
        serde_json::json!({ "dest": dest, "sha256": sha(bytes), "size": bytes.len() })
    }

    /// The full schema-3 feature set through the ordinary install pipeline: a raw named asset,
    /// a multi-member bundle whose content lands at several dests (including two dests sharing
    /// ONE member), a bundled choice variant, and a zero-byte entry that never touches the wire.
    #[test]
    fn bundled_files_install_and_the_packed_asset_is_reclaimed() {
        let dir = tempdir("bundle-install");
        let (x, y, shared, variant) = (b"X1X1" as &[u8], b"Y2" as &[u8], b"SHARED", b"VARIANT");
        let (packed, psha, dsize) = pack(&[x, y, shared, variant]);
        let m = serde_json::json!({
            "schema": 3,
            "version": "1.0.0",
            "bundles": [{
                "name": "b0.phxb", "codec": "zstd",
                "psize": packed.len(), "psha256": psha, "size": dsize,
                "members": [sha(x), sha(y), sha(shared), sha(variant)],
            }],
            "files": [
                file_json("a.vpk", "game/dota/a.vpk", b"raw"),
                bundled_json("game/dota/x.txt", x),
                bundled_json("game/dota/y.txt", y),
                bundled_json("game/dota/s1.txt", shared),
                bundled_json("game/dota/s2.txt", shared),
                bundled_json("game/dota/empty.marker", b""),
            ],
            "options": [{
                "id": "look", "kind": "choice", "label": "Look", "default": "mod",
                "dest": "game/dota/look.vpk",
                "variants": [
                    { "id": "mod", "label": "Mod",
                      "sha256": sha(variant), "size": variant.len() },
                ],
            }],
        })
        .to_string();
        let dl = arc(Fake::new("v1.0.0", &m, vec![("a.vpk", b"raw"), ("b0.phxb", &packed)]));

        let r = install(&settings(&dir), &mod_wire(dl.clone()), None, None, None).unwrap();
        assert_eq!(r.written.len(), 7);
        assert_eq!(std::fs::read(dir.join("game/dota/a.vpk")).unwrap(), b"raw");
        assert_eq!(std::fs::read(dir.join("game/dota/x.txt")).unwrap(), x);
        assert_eq!(std::fs::read(dir.join("game/dota/y.txt")).unwrap(), y);
        assert_eq!(std::fs::read(dir.join("game/dota/s1.txt")).unwrap(), shared);
        assert_eq!(std::fs::read(dir.join("game/dota/s2.txt")).unwrap(), shared);
        assert_eq!(std::fs::read(dir.join("game/dota/empty.marker")).unwrap(), b"");
        assert_eq!(std::fs::read(dir.join("game/dota/look.vpk")).unwrap(), variant);
        // the packed asset held wire-sized bytes with no further use — reclaimed after decode
        assert!(!dir.join(CACHE_DIR).join(&psha).exists(), "packed bundle must be deleted");
        // …while its members stay as ordinary content-addressed entries
        assert_eq!(std::fs::read(dir.join(CACHE_DIR).join(sha(x))).unwrap(), x);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Counts download_to calls per asset name, delegating to the Fake.
    struct Counting {
        inner: Fake,
        calls: Mutex<Vec<String>>,
    }

    impl crate::downloader::Downloader for Counting {
        fn fetch_release(&self, r: &str, t: Option<&str>) -> Result<Release> {
            self.inner.fetch_release(r, t)
        }
        fn fetch_releases(&self, r: &str) -> Result<Vec<Release>> {
            self.inner.fetch_releases(r)
        }
        fn download(&self, a: &crate::downloader::Asset) -> Result<Vec<u8>> {
            self.inner.download(a)
        }
        fn download_to(
            &self,
            a: &crate::downloader::Asset,
            d: &Path,
            r: u64,
            p: crate::downloader::ChunkProgress,
        ) -> Result<(u64, String)> {
            self.calls.lock().unwrap().push(a.name.clone());
            self.inner.download_to(a, d, r, p)
        }
    }

    /// R5: a member hash mismatch AFTER a clean psha256 is a producer defect — the wire carried
    /// exactly what the manifest asked for, so refetching reproduces it. One download, a loud
    /// failure naming a broken release, no retry loop burning bandwidth toward the same wall.
    #[test]
    fn a_member_hash_mismatch_fails_loudly_and_is_never_retried() {
        let dir = tempdir("bundle-defect");
        let good = b"GOOD" as &[u8];
        let evil = b"EVIL" as &[u8]; // same size, different bytes — B2 passes, R4 must not
        let (packed, psha, dsize) = pack(&[evil]);
        let m = serde_json::json!({
            "schema": 3, "version": "1.0.0",
            "bundles": [{ "name": "b0.phxb", "codec": "zstd",
                          "psize": packed.len(), "psha256": psha, "size": dsize,
                          "members": [sha(good)] }],
            "files": [ bundled_json("game/dota/g.txt", good) ],
        })
        .to_string();
        let dl = arc(Counting {
            inner: Fake::new("v1.0.0", &m, vec![("b0.phxb", &packed)]),
            calls: Mutex::new(Vec::new()),
        });

        let e = install(&settings(&dir), &mod_wire(dl.clone()), None, None, None).unwrap_err();
        assert!(format!("{e:#}").contains("broken release"), "got: {e:#}");
        assert_eq!(
            dl.calls.lock().unwrap().len(),
            1,
            "a producer defect must not be retried as a transfer problem"
        );
        assert!(!dir.join("game/dota/g.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// B4 at decode time: the stream must end exactly at the last member. Trailing bytes mean
    /// the byte-counting split was built on a lie — refuse, do not install what happened to
    /// align before the error.
    #[test]
    fn trailing_bytes_after_the_last_member_are_refused() {
        let dir = tempdir("bundle-trailing");
        let a = b"AAAA" as &[u8];
        let (packed, psha, _) = pack(&[a, b"TRAILING GARBAGE"]);
        let m = serde_json::json!({
            "schema": 3, "version": "1.0.0",
            "bundles": [{ "name": "b0.phxb", "codec": "zstd",
                          "psize": packed.len(), "psha256": psha, "size": a.len(),
                          "members": [sha(a)] }],
            "files": [ bundled_json("game/dota/a.txt", a) ],
        })
        .to_string();
        let dl = arc(Fake::new("v1.0.0", &m, vec![("b0.phxb", &packed)]));
        let e = install(&settings(&dir), &mod_wire(dl.clone()), None, None, None).unwrap_err();
        assert!(format!("{e:#}").contains("past its declared members"), "got: {e:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The decompression-bomb shape, sized up: a compressed frame that would inflate to tens of
    /// megabytes behind a member the manifest says is 4 bytes long. Those 4 bytes check out —
    /// what follows never does, and `extract_members` never asks the decoder for it (see its doc
    /// comment: it only ever requests `sum(declared sizes) + 1` probe byte). An extreme ratio
    /// here, rather than a few bytes of trailing garbage, is what makes that property read as
    /// "however much a bomb offers, we never ask for it" instead of "one byte over".
    #[test]
    fn a_bundle_whose_real_content_dwarfs_its_declared_size_is_refused_not_exhausted() {
        let dir = tempdir("bundle-bomb");
        let a = b"AAAA" as &[u8]; // the declared member — matches exactly, so ITS check passes
        let bomb: Vec<u8> = a.iter().copied().chain(std::iter::repeat(0u8).take(50_000_000)).collect();
        let (packed, psha, _) = pack(&[&bomb]); // the real stream: 4 honest bytes, then ~50 MB more
        assert!(packed.len() < 200_000, "the packed frame must stay tiny for this to be a bomb");
        let m = serde_json::json!({
            "schema": 3, "version": "1.0.0",
            "bundles": [{ "name": "b0.phxb", "codec": "zstd",
                          "psize": packed.len(), "psha256": psha, "size": a.len() as u64,
                          "members": [sha(a)] }],
            "files": [ bundled_json("game/dota/a.txt", a) ],
        })
        .to_string();
        let dl = arc(Fake::new("v1.0.0", &m, vec![("b0.phxb", &packed)]));
        let e = install(&settings(&dir), &mod_wire(dl.clone()), None, None, None).unwrap_err();
        assert!(format!("{e:#}").contains("past its declared members"), "got: {e:#}");
        assert!(!dir.join("game/dota/a.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An interrupted PACKED download resumes from its `.part` (R6: progress lives per asset).
    #[test]
    fn an_interrupted_bundle_download_resumes_instead_of_restarting() {
        let dir = tempdir("bundle-resume");
        // period-4099 pseudo-noise so the packed stream is big enough to cut meaningfully
        let big: Vec<u8> = (0..200_000u32).map(|i| (i.wrapping_mul(2654435761) % 251) as u8).collect();
        let (packed, psha, dsize) = pack(&[&big]);
        assert!(packed.len() > 2, "the member must not compress to nothing");
        let m = serde_json::json!({
            "schema": 3, "version": "1.0.0",
            "bundles": [{ "name": "b0.phxb", "codec": "zstd",
                          "psize": packed.len(), "psha256": psha, "size": dsize,
                          "members": [sha(&big)] }],
            "files": [ bundled_json("game/dota/big.vpk", &big) ],
        })
        .to_string();
        let dl = arc(CutOnce {
            inner: Fake::new("v1.0.0", &m, vec![("b0.phxb", &packed)]),
            cut: packed.len() / 2,
            failed: false.into(),
        });

        // first run: dies mid-packed-download, leaving the resumable .part under the psha256
        assert!(install(&settings(&dir), &mod_wire(dl.clone()), None, None, None).is_err());
        assert!(!dir.join("game/dota/big.vpk").exists());
        assert!(dir.join(CACHE_DIR).join(format!("{psha}.part")).exists());

        // second run: resumes the packed .part (asserted inside CutOnce) and completes
        let r = install(&settings(&dir), &mod_wire(dl.clone()), None, None, None).unwrap();
        assert_eq!(r.written, vec!["game/dota/big.vpk".to_string()]);
        assert_eq!(std::fs::read(dir.join("game/dota/big.vpk")).unwrap(), big);
        assert!(!dir.join(CACHE_DIR).join(&psha).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Gotcha 5 (repair granularity): needing members from ONE bundle fetches that bundle once
    /// — and leaves every other bundle alone.
    #[test]
    fn repair_fetches_only_the_bundle_holding_the_damage_and_only_once() {
        let dir = tempdir("bundle-repair");
        let (a1, a2) = (b"ALPHA-1" as &[u8], b"ALPHA-2" as &[u8]);
        let b1 = b"BETA-1" as &[u8];
        let (packed_a, psha_a, dsize_a) = pack(&[a1, a2]);
        let (packed_b, psha_b, dsize_b) = pack(&[b1]);
        let m = serde_json::json!({
            "schema": 3, "version": "1805",
            "bundles": [
                { "name": "a.phxb", "codec": "zstd",
                  "psize": packed_a.len(), "psha256": psha_a, "size": dsize_a,
                  "members": [sha(a1), sha(a2)] },
                { "name": "b.phxb", "codec": "zstd",
                  "psize": packed_b.len(), "psha256": psha_b, "size": dsize_b,
                  "members": [sha(b1)] },
            ],
            "files": [
                bundled_json("game/dota/a1.txt", a1),
                bundled_json("game/dota/a2.txt", a2),
                bundled_json("game/dota/b1.txt", b1),
            ],
        })
        .to_string();
        let manifest = Manifest::parse(m.as_bytes()).unwrap();
        let dl = arc(Counting {
            inner: base_fake("v1805", &m, vec![("a.phxb", &packed_a), ("b.phxb", &packed_b)]),
            calls: Mutex::new(Vec::new()),
        });

        // a full install, then damage BOTH files of bundle A; bundle B's file stays intact
        install_base(&dir, &game_wire(dl.clone()), &manifest, None, None, None).unwrap();
        std::fs::write(dir.join("game/dota/a1.txt"), b"corrupt").unwrap();
        std::fs::write(dir.join("game/dota/a2.txt"), b"corrupt").unwrap();
        dl.calls.lock().unwrap().clear();

        let r = install_base(&dir, &game_wire(dl.clone()), &manifest, None, None, None).unwrap();
        assert_eq!(r.written, 2);
        assert_eq!(std::fs::read(dir.join("game/dota/a1.txt")).unwrap(), a1);
        assert_eq!(std::fs::read(dir.join("game/dota/a2.txt")).unwrap(), a2);
        assert_eq!(
            *dl.calls.lock().unwrap(),
            vec!["a.phxb".to_string()],
            "one fetch of the damaged bundle; the intact bundle costs nothing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R6 in `base_cached`'s terms: a packed bundle on disk is wire progress toward all of its
    /// members; extracted members alone are fetched FILES but discount no wire bytes while the
    /// bundle still has missing members.
    #[test]
    fn base_cached_accounts_bundles_per_asset() {
        let dir = tempdir("bundle-cached");
        let (x, y) = (b"XX-CONTENT" as &[u8], b"YY" as &[u8]);
        let (packed, psha, dsize) = pack(&[x, y]);
        let m = serde_json::json!({
            "schema": 3, "version": "1805",
            "bundles": [{ "name": "b0.phxb", "codec": "zstd",
                          "psize": packed.len(), "psha256": psha, "size": dsize,
                          "members": [sha(x), sha(y)] }],
            "files": [ bundled_json("game/dota/x.txt", x), bundled_json("game/dota/y.txt", y) ],
        })
        .to_string();
        let manifest = Manifest::parse(m.as_bytes()).unwrap();
        let statuses = base_plan(&dir, &manifest, None, "plan", None).unwrap();
        let cache = dir.join(CACHE_DIR).join(BASE_CACHE_SUBDIR);
        std::fs::create_dir_all(&cache).unwrap();

        // nothing cached
        assert_eq!(base_cached(&dir, &manifest, &statuses), (0, 0));
        // one extracted member, packed gone: its dest needs no network, but the packed asset
        // still crosses whole for the other member — zero wire bytes discounted
        std::fs::write(cache.join(sha(x)), x).unwrap();
        assert_eq!(base_cached(&dir, &manifest, &statuses), (0, 1));
        // a packed .part is byte progress
        std::fs::write(cache.join(format!("{psha}.part")), &packed[..2]).unwrap();
        assert_eq!(base_cached(&dir, &manifest, &statuses), (2, 1));
        // the full packed asset present: the whole bundle is offline-obtainable
        std::fs::remove_file(cache.join(format!("{psha}.part"))).unwrap();
        std::fs::write(cache.join(&psha), &packed).unwrap();
        assert_eq!(base_cached(&dir, &manifest, &statuses), (packed.len() as u64, 2));
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
        let dl = arc(base_fake("v1805", &m, assets));
        let manifest = base_manifest(&dl);
        install_base(&dir, &game_wire(dl.clone()), &manifest, None, None, None).unwrap();
        let leftover = std::fs::read_dir(&cache).map(|rd| rd.count()).unwrap_or(0);
        assert_eq!(leftover, 0, "stale cache entries must be reclaimed on success");

        // and the nothing-to-do path (everything intact) reclaims too
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join(sha(b"junk2")), b"junk2").unwrap();
        install_base(&dir, &game_wire(dl.clone()), &manifest, None, None, None).unwrap();
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
        let dl = arc(base_fake("v1805", &m, assets));
        let manifest = base_manifest(&dl);

        let cancel = AtomicBool::new(true); // cancelled before the first chunk lands
        let err = install_base(&dir, &game_wire(dl.clone()), &manifest, None, Some(&cancel), None).unwrap_err();
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

    // ---- download-source failover ----

    /// The one asset every failover test is about. ONE, deliberately: these tests assert which
    /// source a given asset came from, and a single job keeps the 8-worker pool from turning that
    /// into a race.
    const SOLO: &[u8] = b"one base-game asset, long enough to have a middle to break in";

    fn solo_release() -> String {
        serde_json::json!({
            "schema": 2,
            "version": "1805",
            "files": [ file_json("solo.vpk", "game/dota/solo.vpk", SOLO) ]
        })
        .to_string()
    }

    /// How a `Peer` source answers a download. One double rather than four, because what these
    /// tests differ in is only the WAY an asset comes back wrong.
    enum Answer {
        /// Serves it, exactly as `Fake` does.
        Serves,
        /// Nothing answers — a `NetKind::Transport` failure, which is the retryable kind, so the
        /// source is only given up on after `DL_RETRIES`.
        Unreachable,
        /// Writes `n` REAL bytes and then drops the connection: the shape that leaves a `.part`
        /// worth resuming.
        Truncates(usize),
        /// Answers at the right LENGTH with the wrong bytes — a source contradicting the signed
        /// manifest, which is the case whose `.part` must never reach another source.
        Corrupt,
        /// Writes a little, trips the cancel flag, and reports the abort the chunk callback then
        /// asks for: a user pressing Stop mid-transfer.
        Cancels(std::sync::Arc<AtomicBool>),
    }

    /// A source that can be told how to fail, wrapping a real `Fake` for everything else.
    struct Peer {
        inner: Fake,
        answer: Answer,
        /// How many times the WIRE opened this source. One failover is one open, however many
        /// workers reported the failure that caused it.
        opens: AtomicU64,
        calls: AtomicU64,
        /// The `resume_from` of the LAST `download_to`, which is how a test proves what prefix a
        /// source inherited from the one before it. `u64::MAX` = never asked for anything.
        resumed: AtomicU64,
    }

    impl Peer {
        fn new(answer: Answer) -> Arc<Self> {
            Self::at_tag("v1805", answer)
        }

        /// A peer serving a NAMED release — for proving that a swap re-opens the SAME one.
        fn at_tag(tag: &str, answer: Answer) -> Arc<Self> {
            Arc::new(Self {
                inner: base_fake(tag, &solo_release(), vec![("solo.vpk", SOLO)]),
                answer,
                opens: AtomicU64::new(0),
                calls: AtomicU64::new(0),
                resumed: AtomicU64::new(u64::MAX),
            })
        }
        fn opens(&self) -> u64 {
            self.opens.load(Ordering::SeqCst)
        }
        fn calls(&self) -> u64 {
            self.calls.load(Ordering::SeqCst)
        }
        fn resumed(&self) -> u64 {
            self.resumed.load(Ordering::SeqCst)
        }
    }

    // The peer is held through an `Arc` everywhere below: a `Wire` OWNS the backend it is dialled,
    // and the test still has to read the peer's counters afterwards.
    impl crate::downloader::Downloader for Peer {
        fn fetch_release(&self, r: &str, t: Option<&str>) -> Result<Release> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            let release = self.inner.fetch_release(r, t)?;
            // A source serving ANOTHER release refuses itself, exactly as `Mirror::fetch_release`
            // does — rooted at a status, so the walk falls through to one that does serve the tag
            // this run is pinned to instead of quietly installing something else.
            if t.is_some_and(|want| want != release.tag_name) {
                return Err(anyhow::Error::new(NetKind::Status(404))
                    .context("this source serves another release"));
            }
            Ok(release)
        }
        fn fetch_releases(&self, r: &str) -> Result<Vec<Release>> {
            self.inner.fetch_releases(r)
        }
        fn download(&self, a: &Asset) -> Result<Vec<u8>> {
            self.inner.download(a)
        }
        fn download_to(
            &self,
            asset: &Asset,
            dest: &Path,
            resume_from: u64,
            progress: crate::downloader::ChunkProgress,
        ) -> Result<(u64, String)> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.resumed.store(resume_from, Ordering::SeqCst);
            // The typed root is what `transient_net_failure` and the source walk both read; a
            // bare string here would quietly change the behaviour under test.
            let dead = || {
                Err(anyhow::Error::new(NetKind::Transport).context("simulated unreachable source"))
            };
            match &self.answer {
                Answer::Serves => self.inner.download_to(asset, dest, resume_from, progress),
                Answer::Unreachable => dead(),
                Answer::Truncates(n) => {
                    std::fs::write(dest, &SOLO[..*n])?;
                    progress(*n as u64, Some(SOLO.len() as u64));
                    dead()
                }
                Answer::Corrupt => {
                    let wrong = vec![b'X'; SOLO.len()];
                    std::fs::write(dest, &wrong)?;
                    progress(wrong.len() as u64, Some(wrong.len() as u64));
                    Ok((wrong.len() as u64, sha(&wrong)))
                }
                Answer::Cancels(flag) => {
                    std::fs::write(dest, &SOLO[..4])?;
                    flag.store(true, Ordering::SeqCst);
                    // exactly what a real backend does when the callback says stop
                    if !progress(4, Some(SOLO.len() as u64)) {
                        bail!("download aborted");
                    }
                    unreachable!("the chunk callback must refuse once the cancel flag is set")
                }
            }
        }
    }

    /// The solo release's manifest, as the wire's first source publishes it.
    fn solo_manifest(a: &Arc<Peer>) -> Manifest {
        engine::manifest_of(
            &Settings::default(),
            &a.inner,
            &a.inner.fetch_release("r", None).unwrap(),
            crate::trust::Payload::Game,
        )
        .unwrap()
    }

    /// Two sources, and an install that is served by the second. Without this an asset the first
    /// source will not give up ends the whole run — irrelevant for a one-bundle shim, fatal for a
    /// 7.9 GiB base game where any one of ~136 bundles can be the unlucky one.
    #[test]
    fn an_asset_the_first_source_cannot_serve_comes_from_the_next() {
        let dir = tempdir("failover-next");
        let (a, b) = (Peer::new(Answer::Unreachable), Peer::new(Answer::Serves));
        let manifest = solo_manifest(&a);
        install_base(
            &dir,
            &game_wire2(a.clone(), b.clone()),
            &manifest,
            None,
            None,
            None,
        )
        .expect("the second source must finish the run");
        assert_eq!(std::fs::read(dir.join("game/dota/solo.vpk")).unwrap(), SOLO);
        assert!(a.calls() > 1, "the first source is retried before it is given up on");
        assert_eq!(b.calls(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The wire swaps UNDER the pool, and the next source picks the transfer up where the last
    /// one stopped.
    ///
    /// A TRANSPORT failure says nothing about the bytes already written, so they are kept: on the
    /// links this feature exists for that prefix is gigabytes, and discarding it would make every
    /// failover start from zero. And the swap happens ONCE however many workers report the
    /// failure — a worker hands back the generation it read the source with, and a report against
    /// a generation somebody has already moved past is ignored rather than causing a second swap.
    #[test]
    fn the_pool_switches_source_mid_run_and_resumes_from_the_prefix() {
        let dir = tempdir("failover-resume");
        let (a, b) = (Peer::new(Answer::Truncates(20)), Peer::new(Answer::Serves));
        let manifest = solo_manifest(&a);
        let wire = game_wire2(a.clone(), b.clone());
        install_base(&dir, &wire, &manifest, None, None, None)
            .expect("the second source must finish what the first started");

        assert_eq!(b.resumed(), 20, "the .part the first source left is the second's prefix");
        assert_eq!(std::fs::read(dir.join("game/dota/solo.vpk")).unwrap(), SOLO);
        assert_eq!(a.opens(), 1, "the first source is opened once and never returned to");
        assert_eq!(b.opens(), 1, "and the failover opened the second exactly once");
        assert!(
            !wire.fail(0).expect("the wire still has somewhere to be"),
            "a report against a generation already moved past must not swap again"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A swap re-opens the SAME RELEASE. The wire pins the tag it opened with, so a source serving
    /// something else refuses itself and the walk moves past it — identity keeps coming from the
    /// manifest already verified, and only the bytes move.
    #[test]
    fn a_mid_run_switch_opens_the_same_release() {
        let dir = tempdir("failover-tag");
        let a = Peer::new(Answer::Unreachable);
        let other = Peer::at_tag("v9.9.9", Answer::Serves);
        let b = Peer::new(Answer::Serves);
        let manifest = solo_manifest(&a);
        let wire = wire_over(
            vec![a.clone(), other.clone(), b.clone()],
            crate::trust::Payload::Game,
        );
        install_base(&dir, &wire, &manifest, None, None, None)
            .expect("the source serving the pinned release must finish the run");

        assert_eq!(other.opens(), 1, "the wrong release is asked once…");
        assert_eq!(other.calls(), 0, "…and never downloaded from");
        assert_eq!(b.calls(), 1);
        assert_eq!(std::fs::read(dir.join("game/dota/solo.vpk")).unwrap(), SOLO);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A VERIFICATION failure is the opposite case, and the distinction is the whole reason the two
    /// are separate outcomes: those bytes came from a source that has just been shown to contradict
    /// the signed manifest, so handing them to the next source as a resume prefix would let one
    /// source's corruption survive the failover meant to escape it.
    #[test]
    fn wrong_bytes_do_not_poison_the_next_source_after_a_switch() {
        let dir = tempdir("failover-corrupt");
        let (a, b) = (Peer::new(Answer::Corrupt), Peer::new(Answer::Serves));
        let manifest = solo_manifest(&a);
        install_base(
            &dir,
            &game_wire2(a.clone(), b.clone()),
            &manifest,
            None,
            None,
            None,
        )
        .expect("the second source must be able to serve it cleanly");
        assert_eq!(a.calls(), 1, "wrong bytes are a settled answer — never retried at the source");
        assert_eq!(b.resumed(), 0, "the poisoned .part must be gone before the next source starts");
        assert_eq!(std::fs::read(dir.join("game/dota/solo.vpk")).unwrap(), SOLO);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Only an EXHAUSTED chain fails the run, and it says so.
    #[test]
    fn every_source_exhausted_fails_the_run_cleanly() {
        let dir = tempdir("failover-exhausted");
        let (a, b) = (Peer::new(Answer::Unreachable), Peer::new(Answer::Corrupt));
        let manifest = solo_manifest(&a);
        let err = install_base(
            &dir,
            &game_wire2(a.clone(), b.clone()),
            &manifest,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(a.calls() > 0 && b.calls() > 0, "every source must actually be tried");
        assert!(
            format!("{err:#}").contains("every download source failed"),
            "the failure should say the ranking was spent, got: {err:#}"
        );
        assert!(!dir.join("game/dota/solo.vpk").exists(), "nothing may be placed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A cancel is an INSTRUCTION, not a source failure. Cancelling a 7.9 GiB install must not
    /// silently restart it against the next mirror — so the abort line is asked directly rather
    /// than inferred from an error message, and the second source is never reached at all.
    #[test]
    fn a_cancel_mid_transfer_is_never_a_failover() {
        let dir = tempdir("failover-cancel");
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let (a, b) = (Peer::new(Answer::Cancels(cancel.clone())), Peer::new(Answer::Serves));
        let manifest = solo_manifest(&a);
        let err = install_base(
            &dir,
            &game_wire2(a.clone(), b.clone()),
            &manifest,
            None,
            Some(&cancel),
            None,
        )
        .unwrap_err();
        assert!(
            err.chain().any(|c| c.downcast_ref::<engine::Cancelled>().is_some()),
            "a Stop must still report as Cancelled, got: {err:#}"
        );
        assert_eq!(b.opens(), 0, "a cancel must never advance to the next source");
        assert_eq!(b.calls(), 0);
        assert_eq!(a.calls(), 1, "and must not be retried against the source it stopped, either");
        assert!(!dir.join("game/dota/solo.vpk").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
