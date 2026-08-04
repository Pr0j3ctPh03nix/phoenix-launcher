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

#[tauri::command]
pub async fn apply(app: tauri::AppHandle) -> Result<InstallView, CmdError> {
    tauri::async_runtime::spawn_blocking(move || {
        let settings = Settings::load();
        let dl = downloader(&settings);
        // the engine's progress ticks go straight to the webview
        let emit = |p: engine::OpProgress| {
            let _ = app.emit("op-progress", p);
        };
        let report = install::install(&settings, &dl, None, Some(&emit));
        if report.is_ok() {
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
pub async fn uninstall() -> Result<UninstallView, CmdError> {
    tauri::async_runtime::spawn_blocking(|| {
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
