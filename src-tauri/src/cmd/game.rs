//! Base-game commands: fresh install, verify, repair. All run against the game-dist repo
//! (`Settings::game_repo`) whose release assets are the vanilla game files themselves, described
//! by a manifest in the standard format — install.rs's base pipeline does the actual work.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use tauri::Emitter;

use crate::cmd::{open_repo, AppState, CachedManifest};
use crate::config::Settings;
use crate::github::Github;
use crate::install::{self, BaseAction};
use crate::views::{CmdError, GameInstallView, GamePlanView, GameVerifyView};
use crate::engine;

/// What a download into `target` would do — the numbers behind the confirm dialog. Read-only and
/// fast for the fresh-install case (an empty folder plans without hashing anything).
///
/// Holds the op slot despite writing nothing: it OWNS the shared `game_cancel` flag while it
/// runs (reset at entry, polled by its hash workers), and an overlapping mutating op would share
/// that flag — one op's Stop silently cancelling (or un-cancelling) the other. The UI's busy
/// token already prevents the overlap; this is the backend line behind it, like everywhere else.
#[tauri::command]
pub async fn game_plan(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
    target: String,
) -> Result<GamePlanView, CmdError> {
    let st = state.inner().clone();
    st.game_cancel.store(false, Ordering::Relaxed); // before the queue — see game_install
    tauri::async_runtime::spawn_blocking(move || {
        let _op = st.begin_op("game plan")?;
        let settings = Settings::load();
        let dir = PathBuf::from(target);
        let (dl, release) = open_repo(settings.game_repo(), &settings).map_err(CmdError::from)?;
        let manifest = engine::manifest_of(dl.as_ref(), &release).map_err(CmdError::from)?;
        // Planning an EMPTY folder is instant, but the user may well pick one that already holds
        // a game — then this hashes gigabytes. Emit ticks so the dialog reports progress instead
        // of showing a motionless spinner for minutes, and honour a cancel: closing the dialog
        // used to leave those minutes of hashing running with nobody left to read the result.
        let emit = |p: engine::OpProgress| {
            let _ = app.emit("op-progress", p);
        };
        let statuses =
            install::base_plan(&dir, &manifest, Some(&emit), "plan", Some(&st.game_cancel))
                .map_err(CmdError::from)?;
        // unique content only — dests sharing a hash download once
        let mut seen = std::collections::HashSet::new();
        let bytes = statuses
            .iter()
            .filter(|s| s.action == BaseAction::Write)
            .filter(|s| seen.insert(s.entry.sha256.as_str()))
            .map(|s| s.entry.size)
            .sum();
        let (cached_bytes, cached_files) = install::base_cached(&dir, &statuses);
        Ok(GamePlanView {
            version: manifest.version,
            files: statuses.iter().filter(|s| s.action == BaseAction::Write).count() as u32,
            total_files: statuses.len() as u32,
            bytes,
            cached_bytes,
            cached_files: cached_files as u32,
            free_bytes: install::free_space(&dir),
        })
    })
    .await
    .map_err(CmdError::task)?
}

/// Download the base game into `target`, then chain the normal shim install and adopt the folder
/// as the game dir — a fresh download ends PLAYABLE, not merely present. Progress rides the
/// `op-progress` event: op "game" for the base phase (hash ticks carry no bytes, download ticks
/// do), then the chained shim phase reports as the usual op "install".
#[tauri::command]
pub async fn game_install(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
    target: String,
) -> Result<GameInstallView, CmdError> {
    let st = state.inner().clone();
    // Reset BEFORE the blocking task is queued. Done inside the closure, a Cancel clicked while
    // the blocking pool was busy landed first and was then wiped by this store — the click
    // vanished and a multi-gigabyte download carried on.
    st.game_cancel.store(false, Ordering::Relaxed);
    tauri::async_runtime::spawn_blocking(move || {
        let _op = st.begin_op("game download")?;
        let settings = Settings::load();
        let dir = PathBuf::from(&target);
        let (dl, release) = open_repo(settings.game_repo(), &settings).map_err(CmdError::from)?;
        let manifest = engine::manifest_of(dl.as_ref(), &release).map_err(CmdError::from)?;
        // the file assets live sharded across prereleases (GitHub caps 1000 assets/release)
        let release = engine::merged_game_release(dl.as_ref(), settings.game_repo(), release)
            .map_err(CmdError::from)?;

        let emit = |p: engine::OpProgress| {
            let _ = app.emit("op-progress", p);
        };
        let report = install::install_base(
            &dir,
            dl.as_ref(),
            &release,
            &manifest,
            Some(&emit),
            Some(&st.game_cancel),
        )
        .map_err(CmdError::from)?;

        // the folder is a game now — adopt it BEFORE the shim chain, so even a chain failure
        // leaves the UI pointed at the right folder (showing Install as the next step)
        Settings::update(|s| s.game_dir = Some(dir.clone())).map_err(CmdError::from)?;

        // chain the shim: its own repo, its own credentials
        let settings = Settings::load();
        let shim_dl = Github::new(settings.token());
        let shim = install::install(&settings, &shim_dl, None, Some(&emit), Some(&st.game_cancel));
        let shim_version = match shim {
            Ok(r) => {
                *st.manifest_cache.lock().unwrap() = Some(CachedManifest {
                    repo: settings.source_repo.clone(),
                    tag_name: r.tag.clone(),
                    manifest: r.manifest.clone(),
                });
                // warm optional content detached, exactly like a normal apply
                tauri::async_runtime::spawn_blocking(|| {
                    let settings = Settings::load();
                    let dl = Github::new(settings.token());
                    install::warm_cache(&settings, &dl);
                });
                Some(r.version)
            }
            // the base game landed; a shim failure here is recoverable from the main view's
            // Install button — do not fail the whole download over it
            Err(_) => None,
        };

        Ok(GameInstallView {
            game_version: report.version,
            written: report.written as u32,
            up_to_date: report.up_to_date as u32,
            bytes: report.bytes,
            shim_version,
        })
    })
    .await
    .map_err(CmdError::task)?
}

/// Repair the CURRENT game folder: re-download whatever `game_verify` found damaged. The same
/// base pipeline as a fresh install — the plan diff makes repair and install one operation.
#[tauri::command]
pub async fn game_repair(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<GameInstallView, CmdError> {
    let st = state.inner().clone();
    st.game_cancel.store(false, Ordering::Relaxed); // before the queue — see game_install
    tauri::async_runtime::spawn_blocking(move || {
        let _op = st.begin_op("game repair")?;
        let settings = Settings::load();
        let dir = settings.resolve_game_dir().map_err(CmdError::from)?;
        let (dl, release) = open_repo(settings.game_repo(), &settings).map_err(CmdError::from)?;
        let manifest = engine::manifest_of(dl.as_ref(), &release).map_err(CmdError::from)?;
        let release = engine::merged_game_release(dl.as_ref(), settings.game_repo(), release)
            .map_err(CmdError::from)?;
        let emit = |p: engine::OpProgress| {
            let _ = app.emit("op-progress", p);
        };
        let report = install::install_base(
            &dir,
            dl.as_ref(),
            &release,
            &manifest,
            Some(&emit),
            Some(&st.game_cancel),
        )
        .map_err(CmdError::from)?;
        Ok(GameInstallView {
            game_version: report.version,
            written: report.written as u32,
            up_to_date: report.up_to_date as u32,
            bytes: report.bytes,
            shim_version: None,
        })
    })
    .await
    .map_err(CmdError::task)?
}

/// Steam-style integrity check of the current game folder against the game manifest. Read-only;
/// first run hashes the install (per-file `op-progress` ticks, op "verify"), repeats cost stats
/// thanks to the (size,mtime) hash memo.
///
/// Stoppable via `game_cancel` (the main view's Stop button): a cold run reads the whole install
/// and holds the UI for minutes. It writes nothing, so a stopped run is simply abandoned — no
/// state to unwind, and the hash memo keeps whatever it learned for the next attempt.
#[tauri::command]
pub async fn game_verify(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<GameVerifyView, CmdError> {
    let st = state.inner().clone();
    st.game_cancel.store(false, Ordering::Relaxed); // before the queue — see game_install
    tauri::async_runtime::spawn_blocking(move || {
        // reads only, but owns the shared cancel flag while it runs — see game_plan
        let _op = st.begin_op("game verify")?;
        let settings = Settings::load();
        let dir = settings.resolve_game_dir().map_err(CmdError::from)?;
        // refuse folders that are not a game at all — "everything is damaged" would be a lie
        // pointing at a download, when the truth is a wrong folder in settings... or an
        // interrupted download, which deserves to be named as exactly that
        if !install::game_present(&dir) {
            let pending = install::pending_base_bytes(&dir);
            return Err(CmdError::from(if pending > 0 {
                format!(
                    "{} holds an interrupted game download (~{} MB fetched), not a game yet — \
                     resume the download instead of verifying",
                    dir.display(),
                    pending / (1024 * 1024)
                )
            } else {
                format!("{} does not look like a game folder (no game/dota inside)", dir.display())
            }));
        }
        let (dl, release) = open_repo(settings.game_repo(), &settings).map_err(CmdError::from)?;
        let manifest = engine::manifest_of(dl.as_ref(), &release).map_err(CmdError::from)?;
        let emit = |p: engine::OpProgress| {
            let _ = app.emit("op-progress", p);
        };
        let statuses =
            install::base_plan(&dir, &manifest, Some(&emit), "verify", Some(&st.game_cancel))
                .map_err(CmdError::from)?;
        let count = |a: BaseAction| statuses.iter().filter(|s| s.action == a).count() as u32;
        let mut seen = std::collections::HashSet::new();
        let damaged_bytes = statuses
            .iter()
            .filter(|s| s.action == BaseAction::Write)
            .filter(|s| seen.insert(s.entry.sha256.as_str()))
            .map(|s| s.entry.size)
            .sum();
        // a populated folder whose steam.inf does not match is a DIFFERENT build, not a damaged
        // one — computed before `manifest.version` is moved into the view below
        let foreign_build = install::foreign_build(&dir, &manifest);
        Ok(GameVerifyView {
            version: manifest.version,
            total: statuses.len() as u32,
            ok: count(BaseAction::UpToDate),
            skipped: count(BaseAction::Skipped),
            damaged: statuses
                .iter()
                .filter(|s| s.action == BaseAction::Write)
                .map(|s| s.dest().to_string())
                .collect(),
            damaged_bytes,
            foreign_build,
        })
    })
    .await
    .map_err(CmdError::task)?
}

/// Stop the base-game operation in flight — download, verify, or the hashing phase either starts
/// with. In-progress `.part`s are kept, so the next attempt resumes; a stopped verify/plan simply
/// discards its partial verdicts.
///
/// ONE flag serves all of them because only one can be running: every entry point resets it before
/// queueing its work, the UI's busy token blocks a second flow, and the ops that reach here from a
/// dialog run behind an inert stage. Each op reports the stop as a typed `Cancelled`.
#[tauri::command]
pub fn game_cancel(state: tauri::State<'_, Arc<AppState>>) {
    state.game_cancel.store(true, Ordering::Relaxed);
}
