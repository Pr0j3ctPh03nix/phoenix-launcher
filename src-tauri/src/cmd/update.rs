//! Update lifecycle commands: check / replan / apply / uninstall. `apply` forwards the engine's
//! progress ticks to the webview as the `op-progress` event.

use std::sync::Arc;

use tauri::Emitter;

use crate::cmd::{AppState, CachedManifest};
use crate::config::Settings;
use crate::downloader::Downloader;
use crate::github::Github;
use crate::views::{build_check_view, CheckView, CmdError, InstallView, UninstallView};
use crate::{engine, install};

fn downloader(s: &Settings) -> impl Downloader + '_ {
    Github::new(s.token())
}

#[tauri::command]
pub async fn check(state: tauri::State<'_, Arc<AppState>>) -> Result<CheckView, CmdError> {
    let st = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let settings = Settings::load();
        let dl = downloader(&settings);
        let (release, manifest) = engine::fetch(&settings, &dl, None).map_err(CmdError::from)?;
        // cache before evaluating: even if the local diff fails, the fetched manifest is kept
        *st.manifest_cache.lock().unwrap() = Some(CachedManifest {
            repo: settings.source_repo.clone(),
            tag_name: release.tag_name.clone(),
            manifest: manifest.clone(),
        });
        let r = engine::evaluate(&settings, &release.tag_name, &manifest).map_err(CmdError::from)?;
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
) -> Result<InstallView, CmdError> {
    let st = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _op = st.begin_op("install")?;
        let settings = Settings::load();
        let dl = downloader(&settings);
        // the engine's progress ticks go straight to the webview
        let emit = |p: engine::OpProgress| {
            let _ = app.emit("op-progress", p);
        };
        let report = install::install(&settings, &dl, tag.as_deref(), Some(&emit), None);
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
            // Best-effort by design; uninstall cancels it via install::cancel_warm.
            tauri::async_runtime::spawn_blocking(|| {
                let settings = Settings::load();
                let dl = downloader(&settings);
                install::warm_cache(&settings, &dl);
            });
        }
        report
            .map(|r| InstallView {
                version: r.version,
                written: r.written,
                removed: r.removed,
                up_to_date: r.up_to_date as u32,
                winmm_orig: match r.winmm_orig {
                    install::WinmmOrig::Created => "created",
                    install::WinmmOrig::Existed => "existed",
                    install::WinmmOrig::NotNeeded => "not_needed",
                }
                .to_string(),
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
                winmm_orig_removed: r.winmm_orig_removed,
            })
            .map_err(CmdError::from)
    })
    .await
    .map_err(CmdError::task)?
}
