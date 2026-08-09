//! Base-game commands: fresh install, verify, repair. All run against the game-dist repo
//! (`Settings::game_repo`) whose release assets are the vanilla game files themselves, described
//! by a manifest in the standard format — install.rs's base pipeline does the actual work.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use tauri::Emitter;

use crate::cmd::{open_repo, AppState, CachedManifest};
use crate::config::Settings;
use crate::engine;
use crate::github::Github;
use crate::install::{self, BaseAction};
use crate::views::{CmdError, FileStateView, GameInstallView, GamePlanView, GameVerifyView};

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
        // R7's two currencies: `bytes` (wire) feeds the bar/ETA/confirm, `disk_bytes` the
        // footprint, `need_bytes` the space warning — same math as the backend preflight
        let (wire, disk, need) =
            install::base_costs(&manifest, &statuses).map_err(CmdError::from)?;
        let (cached_bytes, cached_files) = install::base_cached(&dir, &manifest, &statuses);
        Ok(GamePlanView {
            version: manifest.version,
            files: statuses.iter().filter(|s| s.action.writes()).count() as u32,
            total_files: statuses.len() as u32,
            bytes: wire,
            disk_bytes: disk,
            need_bytes: need,
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
            None, // a fresh download is the whole game, and honours any pin already in the folder
        )
        .map_err(CmdError::from)?;

        // the folder is a game now — adopt it BEFORE the shim chain, so even a chain failure
        // leaves the UI pointed at the right folder (showing Install as the next step)
        Settings::update(|s| s.game_dir = Some(dir.clone())).map_err(CmdError::from)?;

        // chain the shim: its own repo, its own credentials
        let settings = Settings::load();
        let shim_dl = Github::new(settings.token());
        let shim =
            install::install(&settings, &shim_dl, None, Some(&emit), Some(&st.game_cancel), None);
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

/// Restore selected base-game files. The same base pipeline as a fresh install — the plan diff
/// makes repair and install one operation — narrowed to what the user actually checked.
///
/// `restore` names the files to rewrite; `keep` names files they are deliberately leaving as they
/// are, which is recorded as a content pin so the next verify stops reporting them (see keep.rs).
/// Both are applied in the same call because they are two halves of one decision: the user looked
/// at a list and said "these, not those", and persisting only half of that answer would mean the
/// other half is asked again tomorrow.
///
/// An empty `restore` with a non-empty `keep` is a legitimate, common call — "nothing here is
/// broken, stop asking about my mods" — and must not be mistaken for a no-op.
#[tauri::command]
pub async fn game_repair(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
    restore: Vec<String>,
    keep: Vec<String>,
) -> Result<GameInstallView, CmdError> {
    let st = state.inner().clone();
    st.game_cancel.store(false, Ordering::Relaxed); // before the queue — see game_install
    tauri::async_runtime::spawn_blocking(move || {
        let _op = st.begin_op("game repair")?;
        let settings = Settings::load();
        let dir = settings.resolve_game_dir().map_err(CmdError::from)?;
        let (dl, release) = open_repo(settings.game_repo(), &settings).map_err(CmdError::from)?;
        let manifest = engine::manifest_of(dl.as_ref(), &release).map_err(CmdError::from)?;

        // Pins FIRST, and only then the writes. If the order were reversed a failure mid-download
        // would lose the "leave these alone" half of the answer, and the retry would open with the
        // user's mods checked for overwrite again — the one mistake this whole feature exists to
        // prevent. Pinning costs nothing and is independently correct, so it does not wait on the
        // network. Files that hash to the manifest by now are not pinned (nothing to approve).
        let by_dest: std::collections::HashMap<&str, &str> =
            manifest.files.iter().map(|f| (f.dest.as_str(), f.sha256.as_str())).collect();
        // The plan may have judged a base dest by its preserved original rather than the live
        // path, so the pin has to be recorded against the same file — see `install::base_target`.
        crate::keep::pin_all(&dir, &keep, install::base_paths(&dir), |dest| {
            by_dest.get(dest).map(|h| h.to_string())
        })
        .map_err(CmdError::from)?;

        let only: HashSet<String> = restore.into_iter().collect();
        if only.is_empty() {
            // Nothing to fetch. Reported as a real (empty) result rather than an error: the pins
            // above are the whole point of this call.
            return Ok(GameInstallView {
                game_version: manifest.version,
                written: 0,
                up_to_date: 0,
                bytes: 0,
                shim_version: None,
            });
        }

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
            Some(&only),
        )
        .map_err(CmdError::from)?;
        // Restoring a file the user had pinned is them taking the approval back.
        crate::keep::unpin_all(&dir, &only).map_err(CmdError::from)?;
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

/// Delete files nothing in the folder claims — the user's own additions, removed at their explicit
/// request. Irreversible, and treated as such: the engine re-derives the legal set from a fresh
/// scan, so a stale UI list can never reach a manifest file, the shim's files or `.phoenix*`.
///
/// Returns how many entries were removed.
#[tauri::command]
pub async fn game_delete_extras(
    state: tauri::State<'_, Arc<AppState>>,
    paths: Vec<String>,
) -> Result<u32, CmdError> {
    let st = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _op = st.begin_op("delete extras")?;
        let settings = Settings::load();
        let dir = settings.resolve_game_dir().map_err(CmdError::from)?;
        let (dl, release) = open_repo(settings.game_repo(), &settings).map_err(CmdError::from)?;
        let manifest = engine::manifest_of(dl.as_ref(), &release).map_err(CmdError::from)?;
        // The same claimed-set the verify used, rebuilt: whatever the shim accounts for is not an
        // extra and must stay unreachable from here even if the UI thought otherwise.
        let mut claimed: HashSet<String> = manifest.files.iter().map(|f| f.dest.clone()).collect();
        if let Some((_, dests)) = shim_plan(&settings, &dir) {
            claimed.extend(dests);
        } else if let Some(s) = crate::state::InstalledState::load(&dir) {
            claimed.extend(s.files.into_iter().map(|f| f.dest));
        }
        install::delete_extras(&dir, &manifest, &claimed, &paths).map_err(CmdError::from)
    })
    .await
    .map_err(CmdError::task)?
}

/// Pin Phoenix files the user is keeping as they are. The shim's counterpart to `game_repair`'s
/// `keep` half — restoring them goes through `apply`'s own selection, so this command only ever
/// records approvals.
#[tauri::command]
pub async fn phoenix_keep(
    state: tauri::State<'_, Arc<AppState>>,
    keep: Vec<String>,
) -> Result<u32, CmdError> {
    let st = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _op = st.begin_op("keep")?;
        let settings = Settings::load();
        let dir = settings.resolve_game_dir().map_err(CmdError::from)?;
        // Intact-ness is judged against the SHIM manifest here; unreachable means we cannot tell,
        // and a pin on a file that turns out to be intact is harmless (it matches, so it never
        // reports as a difference in the first place).
        let dl = Github::new(settings.token());
        let resolved: Option<std::collections::HashMap<String, String>> =
            engine::fetch(&settings, &dl, None).ok().map(|(_, m)| {
                engine::resolve(&m, &settings.selections)
                    .into_iter()
                    .map(|f| (f.dest, f.sha256))
                    .collect()
            });
        // Only dests this authority actually carries. A dest the shim does not manage is not the
        // shim's to rule on: it has no `theirs` here, so recording an approval would replace a
        // two-sided pin with a one-sided one that then holds against every future release. When
        // the manifest is unreachable we cannot tell which is which, so nothing is filtered out
        // and `pin_all` keeps whatever `theirs` each pin already had.
        let keep: Vec<String> = match &resolved {
            Some(map) => keep.into_iter().filter(|d| map.contains_key(d)).collect(),
            None => keep,
        };
        // The shim's own files are always judged at the live path — the vanilla redirect is a
        // BASE-plan rule, and applying it here would pin the wrong file entirely.
        crate::keep::pin_all(&dir, &keep, |dest| dir.join(dest), |dest| {
            resolved.as_ref().and_then(|m| m.get(dest).cloned())
        })
        .map_err(CmdError::from)
    })
    .await
    .map_err(CmdError::task)?
}

/// The user's OWN files: every pin, plus everything in the folder nothing claims — WITHOUT the
/// full integrity pass.
///
/// `game_verify` answers "is this install intact", and the price of that answer is hashing ~15 GB.
/// This screen asks a much narrower question — what have I told the launcher to leave alone, and
/// what have I added? — and every part of it is cheap: the keep list is one small read, the shim's
/// own plan covers a handful of files, the extras scan walks directories without hashing anything,
/// and the only files hashed are the pinned dests themselves. Someone who wants to un-keep one
/// file, or delete a mod they dropped in, should not have to sit through a multi-minute
/// verification to reach it.
///
/// Refuses an unreadable `steam.inf` exactly as `game_verify` does. This view offers restore too,
/// and restore is the one irreversible act here: it would write build-1805 files into a folder
/// whose build we could not confirm.
#[tauri::command]
pub async fn your_files(
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<GameVerifyView, CmdError> {
    let st = state.inner().clone();
    st.game_cancel.store(false, Ordering::Relaxed); // before the queue — see game_install
    tauri::async_runtime::spawn_blocking(move || {
        let _op = st.begin_op("your files")?;
        let settings = Settings::load();
        let dir = settings.resolve_game_dir().map_err(CmdError::from)?;
        if !install::game_present(&dir) {
            return Err(CmdError::from(format!(
                "{} does not look like a game folder (no game/dota inside)",
                dir.display()
            )));
        }
        let (dl, release) = open_repo(settings.game_repo(), &settings).map_err(CmdError::from)?;
        let manifest = engine::manifest_of(dl.as_ref(), &release).map_err(CmdError::from)?;
        let identity = install::build_identity(&dir, &manifest);
        if identity == install::BuildIdentity::Unknown {
            return Err(CmdError::from(format!(
                "could not read {} — this folder's Dota 2 build cannot be confirmed, and putting \
                 files back is not safe to offer without it. Check the file for an antivirus hold, \
                 a permissions problem or disk errors, then try again.",
                dir.join("game").join("dota").join("steam.inf").display()
            )));
        }

        // The shim half first, and for two reasons: it is the same plan `game_verify` shows (so
        // the two screens cannot disagree about a Phoenix file), and it is what tells us which
        // dests are Phoenix's — every pin on a dest it does NOT own is the game's.
        let mut claimed: HashSet<String> = manifest.files.iter().map(|f| f.dest.clone()).collect();
        let shim = shim_plan(&settings, &dir);
        let phoenix_unknown = shim.is_none();
        let mut files: Vec<FileStateView> = Vec::new();
        let shim_dests: HashSet<String> = match shim {
            Some((shim_files, dests)) => {
                files.extend(shim_files);
                claimed.extend(dests.iter().cloned());
                dests.into_iter().collect()
            }
            // No plan means no per-file verdict, but the record still names the dests — enough to
            // keep the shim's own files out of both the pin half and the extras list.
            None => match crate::state::InstalledState::load(&dir) {
                Some(s) => {
                    let dests: HashSet<String> = s.files.into_iter().map(|f| f.dest).collect();
                    claimed.extend(dests.iter().cloned());
                    dests
                }
                None => HashSet::new(),
            },
        };

        // The game half: ONLY the pinned dests are planned, so only they are read. Everything else
        // in the base manifest is `game_verify`'s subject, not this screen's.
        let pinned: HashSet<String> = crate::keep::KeepList::load(&dir)
            .files
            .into_keys()
            .filter(|d| !shim_dests.contains(d))
            .collect();
        let statuses =
            install::base_plan_of(&dir, &manifest, None, "yours", Some(&st.game_cancel), Some(&pinned))
                .map_err(CmdError::from)?;
        let wire = install::WireIndex::new(&manifest);
        files.extend(
            statuses
                .iter()
                .filter(|s| s.action != BaseAction::UpToDate && s.action != BaseAction::Skipped)
                .map(|s| {
                    let (wire_key, w) = wire.of(&s.entry);
                    FileStateView {
                        path: s.dest().to_string(),
                        owner: "game",
                        state: if s.superseded { "kept" } else { s.action.word() },
                        size: s.entry.size,
                        local_size: s.local_size,
                        mtime: s.mtime,
                        wire_key,
                        wire: w,
                        update_available: s.superseded,
                        files: 0,
                    }
                }),
        );

        // And what nobody claims — the files the user put there themselves, which is the other
        // half of "my files" and the only half with a delete button.
        // A STOP is a stop, not a short list: this walk used to report a cancel the same way it
        // reports its entry ceiling, so pressing Stop opened the view anyway under "there are more
        // extra files than could be listed" — a statement about a cap nobody hit. The plan phase
        // above already fails with `Cancelled` for the same gesture; this now matches it.
        let (extras, extras_end) =
            install::scan_extras(&dir, &manifest, &claimed, Some(&st.game_cancel));
        if extras_end == install::ExtrasEnd::Cancelled {
            return Err(CmdError::from(anyhow::anyhow!(engine::Cancelled)));
        }
        let extras_truncated = extras_end == install::ExtrasEnd::Capped;
        files.extend(extras.into_iter().map(|e| FileStateView {
            path: e.path,
            owner: "extra",
            state: if e.files > 0 { "extraDir" } else { "extra" },
            size: 0,
            local_size: Some(e.size),
            mtime: e.mtime,
            wire_key: None,
            wire: 0,
            update_available: false,
            files: e.files,
        }));

        let kept = files.iter().filter(|f| f.state == "kept").count() as u32;
        Ok(GameVerifyView {
            version: manifest.version,
            // `total`/`ok` describe an integrity pass, and no integrity pass happened here. The
            // view knows this payload by its own mode and words its summary from `kept` and the
            // rows themselves; these two carry the only honest numbers available.
            total: files.len() as u32,
            ok: 0,
            skipped: 0,
            kept,
            files,
            // the opening cost of the DEFAULT selection, and this screen opens with nothing
            // selected — restoring somebody's own files is never a default
            damaged_bytes: 0,
            extras_truncated,
            foreign_build: identity == install::BuildIdentity::Foreign,
            phoenix_unknown,
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

        // Which build is this? Decided BEFORE the plan, not after: hashing a full install is
        // minutes of work, and if the answer is "we cannot tell" none of that work can be acted
        // on anyway. `Unknown` is refused rather than reported — every verdict below rests on
        // this file, and both readings of an unreadable one are dangerous: calling it foreign
        // offers an irreversible overwrite of a folder that is probably fine, and calling it ours
        // offers a repair that would overwrite a working unrelated install.
        let identity = install::build_identity(&dir, &manifest);
        if identity == install::BuildIdentity::Unknown {
            return Err(CmdError::from(format!(
                "could not read {} — this folder's Dota 2 build cannot be confirmed, and without \
                 it no verdict about its files is safe to act on. Check the file for an antivirus \
                 hold, a permissions problem or disk errors, then verify again.",
                dir.join("game").join("dota").join("steam.inf").display()
            )));
        }

        let emit = |p: engine::OpProgress| {
            let _ = app.emit("op-progress", p);
        };
        let statuses =
            install::base_plan(&dir, &manifest, Some(&emit), "verify", Some(&st.game_cancel))
                .map_err(CmdError::from)?;
        let count = |a: BaseAction| statuses.iter().filter(|s| s.action == a).count() as u32;
        // wire cost: repairing one member of a bundle re-fetches the whole packed bundle, and
        // the repair bar/confirm must promise that number, not the differing files' own sizes.
        // This is the DEFAULT selection's cost — everything unapproved — and the view recomputes
        // it live from each row's `wireKey` as the user changes what is checked.
        let (damaged_bytes, _disk, _need) =
            install::base_costs(&manifest, &statuses).map_err(CmdError::from)?;

        let wire = install::WireIndex::new(&manifest);
        let mut files: Vec<FileStateView> = statuses
            .iter()
            .filter(|s| s.action != BaseAction::UpToDate && s.action != BaseAction::Skipped)
            .map(|s| {
                let (wire_key, w) = wire.of(&s.entry);
                FileStateView {
                    path: s.dest().to_string(),
                    owner: "game",
                    // an outrun pin still reads as the user's decision; what changed is carried
                    // by update_available (see views::FileView)
                    state: if s.superseded { "kept" } else { s.action.word() },
                    size: s.entry.size,
                    local_size: s.local_size,
                    mtime: s.mtime,
                    wire_key,
                    wire: w,
                    // no install record exists for base files, so the only baseline available is
                    // the pin's own — which is exactly what `superseded` compares
                    update_available: s.superseded,
                    files: 0,
                }
            })
            .collect();

        // --- the Phoenix half ---
        // Its own repo, its own credentials, its own manifest. Best-effort: the base verdict is
        // the reason the user pressed the button and must not be lost because the dist repo is
        // unreachable — but a failure is REPORTED (`phoenix_unknown`), never rendered as "we
        // looked and everything was fine".
        let mut claimed: HashSet<String> = manifest.files.iter().map(|f| f.dest.clone()).collect();
        let shim = shim_plan(&settings, &dir);
        let phoenix_unknown = shim.is_none();
        if let Some((shim_files, shim_dests)) = shim {
            claimed.extend(shim_dests);
            files.extend(shim_files);
        } else if let Some(s) = crate::state::InstalledState::load(&dir) {
            // No plan, so no per-file verdict — but the record still says these dests are spoken
            // for, which keeps them out of the extras list rather than reporting the whole shim
            // install as foreign files the moment GitHub is down.
            claimed.extend(s.files.into_iter().map(|f| f.dest));
        }

        // A STOP is a stop, not a short list: this walk used to report a cancel the same way it
        // reports its entry ceiling, so pressing Stop opened the view anyway under "there are more
        // extra files than could be listed" — a statement about a cap nobody hit. The plan phase
        // above already fails with `Cancelled` for the same gesture; this now matches it.
        let (extras, extras_end) =
            install::scan_extras(&dir, &manifest, &claimed, Some(&st.game_cancel));
        if extras_end == install::ExtrasEnd::Cancelled {
            return Err(CmdError::from(anyhow::anyhow!(engine::Cancelled)));
        }
        let extras_truncated = extras_end == install::ExtrasEnd::Capped;
        files.extend(extras.into_iter().map(|e| FileStateView {
            path: e.path,
            owner: "extra",
            state: if e.files > 0 { "extraDir" } else { "extra" },
            size: 0,
            local_size: Some(e.size),
            mtime: e.mtime,
            wire_key: None,
            wire: 0,
            update_available: false,
            files: e.files,
        }));

        // Counted off the assembled ROWS, not off the base plan: a pin is a pin whichever
        // authority owns the file, and the view's own "kept" chip counts rows. Taking this from
        // `count(BaseAction::Kept)` made it the game-side total alone, so a pinned Phoenix file
        // put the summary line ("1 kept as yours") next to a chip reading "kept 2" — one number
        // contradicting another about the same thing, on the screen whose entire job is to be
        // believed. Extras never carry this state, so appending them above is harmless.
        let kept = files.iter().filter(|f| f.state == "kept").count() as u32;

        Ok(GameVerifyView {
            version: manifest.version,
            total: statuses.len() as u32,
            ok: count(BaseAction::UpToDate),
            skipped: count(BaseAction::Skipped),
            kept,
            files,
            damaged_bytes,
            extras_truncated,
            foreign_build: identity == install::BuildIdentity::Foreign,
            phoenix_unknown,
        })
    })
    .await
    .map_err(CmdError::task)?
}

/// The shim's own differences, as files-view rows, plus every dest it accounts for.
///
/// `None` = could not be computed (no network, unreadable manifest). That is a THIRD answer, not
/// an empty list: an empty list means "checked, nothing wrong", and the two must never be
/// confused — the same rule `BuildIdentity::Unknown` exists for.
///
/// Only differences are returned. An intact shim file is the main view's subject, not this one's.
fn shim_plan(
    settings: &Settings,
    game_dir: &std::path::Path,
) -> Option<(Vec<FileStateView>, Vec<String>)> {
    let dl = Github::new(settings.token());
    let (_release, manifest) = engine::fetch(settings, &dl, None).ok()?;
    let resolved = engine::resolve(&manifest, &settings.selections);
    let prev = crate::state::InstalledState::load(game_dir);
    let statuses = engine::plan(game_dir, &resolved, prev.as_ref(), &manifest.remove);
    let by_dest: std::collections::HashMap<&str, &crate::manifest::FileEntry> =
        resolved.iter().map(|f| (f.dest.as_str(), f)).collect();
    let wire = install::WireIndex::new(&manifest);

    let dests: Vec<String> = statuses.iter().map(|s| s.dest.clone()).collect();
    let rows = statuses
        .iter()
        .filter(|s| s.action.is_users())
        .map(|s| {
            let fe = by_dest.get(s.dest.as_str());
            let (wire_key, w) = fe.map(|fe| wire.of(fe)).unwrap_or((None, 0));
            let md = std::fs::metadata(game_dir.join(&s.dest)).ok();
            FileStateView {
                path: s.dest.clone(),
                owner: "phoenix",
                state: match s.action {
                    engine::Action::Kept => "kept",
                    _ if s.superseded => "kept",
                    _ => "modified",
                },
                size: fe.map(|f| f.size).unwrap_or(0),
                local_size: md.as_ref().map(|m| m.len()),
                mtime: md
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs()),
                wire_key,
                wire: w,
                update_available: s.update_available,
                files: 0,
            }
        })
        .collect();
    Some((rows, dests))
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
