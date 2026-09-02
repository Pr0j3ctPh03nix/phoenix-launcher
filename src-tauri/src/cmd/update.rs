//! Update lifecycle commands: check / replan / apply / uninstall. `apply` forwards the engine's
//! progress ticks to the webview as the `op-progress` event.

use std::collections::HashSet;
use std::sync::Arc;

use tauri::Emitter;

use crate::cmd::{AppState, CachedManifest};
use crate::config::Settings;
use crate::source::{self, Wire};
use crate::trust::Payload;
use crate::views::{build_check_view, CheckView, CmdError, InstallView, UninstallView};
use crate::{engine, install};

#[tauri::command]
pub async fn check(state: tauri::State<'_, Arc<AppState>>) -> Result<CheckView, CmdError> {
    let st = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let settings = Settings::load();
        // The TRUST GATE is inside the closure, which is the whole point: a manifest a source
        // refuses fails that source over instead of ending the check.
        let (tag, manifest) = source::with_active(
            &settings,
            &settings.source_repo,
            Payload::Mod,
            None,
            |dl, release| {
                let manifest = engine::manifest_of(&settings, dl, release, Payload::Mod)?;
                Ok((release.tag_name.clone(), manifest))
            },
        )
        .map_err(CmdError::from)?;
        // cache before evaluating: even if the local diff fails, the fetched manifest is kept
        *st.manifest_cache.lock().unwrap() = Some(CachedManifest {
            repo: settings.source_repo.clone(),
            tag_name: tag.clone(),
            manifest: manifest.clone(),
        });
        let r = engine::evaluate(&settings, &tag, &manifest).map_err(CmdError::from)?;
        Ok(build_check_view(r))
    })
    .await
    .map_err(CmdError::task)?
}

/// The fallback when `check` fails: a verdict from the install record alone, no network at all.
///
/// Without it an offline cold start is a dead end — `lastCheck` is what unlocks Play and
/// Uninstall in the UI, and it is only written by a successful check, so an unreachable GitHub
/// left the user with nothing but a Check button that fails again. Both of those operations are
/// local; refusing them because a remote lookup failed is the wrong trade. Errors here mean
/// genuinely nothing is installed, so the caller keeps showing the original network error.
#[tauri::command]
pub async fn local_check() -> Result<CheckView, CmdError> {
    tauri::async_runtime::spawn_blocking(move || {
        let settings = Settings::load();
        let game_dir = settings.resolve_game_dir().map_err(CmdError::from)?;
        let st = crate::state::InstalledState::load(&game_dir)
            .ok_or_else(|| CmdError::from("nothing is installed in this folder"))?;
        Ok(crate::views::build_local_check_view(&game_dir, &st))
    })
    .await
    .map_err(CmdError::task)?
}

/// Re-diff with current settings/selections against the cached manifest — no network.
#[tauri::command]
pub async fn replan(state: tauri::State<'_, Arc<AppState>>) -> Result<CheckView, CmdError> {
    let st = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let settings = Settings::load();
        // clone out and drop the lock — evaluate re-hashes files and must not hold it
        let (tag_name, manifest) = {
            let guard = st.manifest_cache.lock().unwrap();
            let cached = guard
                .as_ref()
                .filter(|c| c.repo == settings.source_repo)
                .ok_or_else(|| CmdError::from("no cached manifest — run a check first"))?;
            (cached.tag_name.clone(), cached.manifest.clone())
        };
        let r = engine::evaluate(&settings, &tag_name, &manifest).map_err(CmdError::from)?;
        Ok(build_check_view(r))
    })
    .await
    .map_err(CmdError::task)?
}

/// `tag` pins the install to the release the UI checked and showed — the same rule the
/// self-update path already follows (`launcher_update(tag)`): what the button OFFERS is what the
/// button installs. Left to re-resolve "latest" here, a release flipped to prerelease (or a new
/// one published) between check and click silently installed something the user never saw — and
/// `install` has no newer-than gate, so that could even be a downgrade. Omitted (no prior check
/// view, an older frontend) falls back to latest.
#[tauri::command]
pub async fn apply(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
    tag: Option<String>,
    restore: Option<Vec<String>>,
) -> Result<InstallView, CmdError> {
    let st = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _op = st.begin_op("install")?;
        // A warm from a PREVIOUS install may still be running, and it prunes the cache against the
        // manifest it was given. Left alone it would finish after this install had seeded the
        // cache and delete the entries this release needs — the epoch exists so a later install
        // can say so, and this is where it says it.
        install::cancel_warm();
        let settings = Settings::load();
        // The backend line behind the frontend not OFFERING Install here: the shim into a folder
        // with no game in it yields a folder that still has no game, reported as "up to date".
        // A presence gate, NOT a build gate (that stays removed by decision — any folder with a
        // game/dota in it is still accepted, whatever build it holds). The CLI bypasses this on
        // purpose (it talks to the engine directly; decoys are its whole point).
        let game_dir = settings.resolve_game_dir().map_err(CmdError::from)?;
        if !install::game_present(&game_dir) {
            return Err(CmdError::from(format!(
                "{} has no game in it — download the game first (Settings → Game files)",
                game_dir.display()
            )));
        }
        // Pinned to the tag the UI showed. The wire holds that pin for the whole run: a source it
        // swaps to mid-download is opened for the SAME release, so what the button offered is what
        // the button installs however many hosts it takes to finish.
        let wire = Wire::open(&settings, &settings.source_repo, Payload::Mod, tag.as_deref())
            .map_err(CmdError::from)?;
        // the engine's progress ticks go straight to the webview
        let emit = |p: engine::OpProgress| {
            let _ = app.emit("op-progress", p);
        };
        // A selection restricts the run to named dests — the files view restoring Phoenix files
        // the user changed. Without one this is the ordinary apply, which never touches a pinned
        // dest; with one, naming a pinned dest IS the user taking the pin back, so it is dropped
        // after the run rather than left to re-hide the file on the next check.
        let only: Option<HashSet<String>> = restore.map(|v| v.into_iter().collect());
        let report = install::install(&settings, &wire, Some(&emit), None, only.as_ref());
        if let (Ok(_), Some(sel)) = (&report, &only) {
            let _ = crate::keep::unpin_all(&game_dir, sel);
        }
        if let Ok(r) = &report {
            // the installed manifest is the freshest there is (install fetches its own) — keep
            // the replan cache in step, so the UI's post-apply refresh is a no-network replan
            // that can't diff against a manifest older than what was just installed
            *st.manifest_cache.lock().unwrap() = Some(CachedManifest {
                repo: settings.source_repo.clone(),
                tag_name: r.tag.clone(),
                manifest: r.manifest.clone(),
            });
            // warm the asset cache (unselected variants, disabled toggles) DETACHED — optional
            // content can be hundreds of MB and must not delay the install result / Play unlock.
            // Best-effort by design; uninstall and the next install cancel it via cancel_warm.
            //
            // Pinned to the release that was just installed, in both halves: the TAG the wire opens
            // (so the bytes come from that release) and the MANIFEST the install verified (so the
            // prune that follows is about the same release). An untagged wire made the warm resolve
            // "latest" for itself, which is a different release the moment one is published between
            // the check and this apply.
            let (tag, manifest) = (r.tag.clone(), r.manifest.clone());
            tauri::async_runtime::spawn_blocking(move || {
                let settings = Settings::load();
                // best-effort like the warm itself: a source that cannot be opened just means the
                // optional content downloads on demand later. Its own wire, so a background warm
                // that outlives the install gains failover without sharing the install's swaps.
                let opened =
                    Wire::open(&settings, &settings.source_repo, Payload::Mod, Some(&tag));
                if let Ok(wire) = opened {
                    install::warm_cache(&settings, &wire, &manifest);
                }
            });
        }
        report
            .map(|r| InstallView {
                version: r.version,
                written: r.written,
                removed: r.removed,
                up_to_date: r.up_to_date as u32,
            })
            .map_err(CmdError::from)
    })
    .await
    .map_err(CmdError::task)?
}

#[tauri::command]
pub async fn uninstall(state: tauri::State<'_, Arc<AppState>>) -> Result<UninstallView, CmdError> {
    let st = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _op = st.begin_op("uninstall")?;
        // a background cache warm may be running — stop it so it can't recreate .phoenix-cache
        install::cancel_warm();
        let settings = Settings::load();
        install::uninstall(&settings)
            .map(|r| UninstallView {
                version: r.version,
                restored: r.restored,
                deleted: r.deleted,
                kept: r.kept,
                vanilla_kept: r.vanilla_kept,
                winmm_orig_removed: r.winmm_orig_removed,
            })
            .map_err(CmdError::from)
    })
    .await
    .map_err(CmdError::task)?
}
