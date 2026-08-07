//! Launcher self-update commands. selfupdate.rs does the network work and the on-disk swap; this
//! layer owns what the engine deliberately does not — restarting the process into the new binary.

use anyhow::Result;
use tauri::Emitter;

use crate::cmd::{open_repo, open_repo_tagged, AppState};
use crate::config::Settings;
use crate::downloader::{Downloader, Release};
use crate::views::{CmdError, LauncherInfoView, LauncherProgress, LauncherUpdateView};
use crate::{install, selfupdate};

/// The launcher repo's latest release + the downloader that could see it. Anonymous first with a
/// token retry only on an HTTP refusal — the shared `open_repo` rationale, which matters most
/// here: this runs on the Play path, where offline must not pay two connect timeouts.
fn fetch(settings: &Settings) -> Result<(Box<dyn Downloader>, Release)> {
    open_repo(settings.launcher_repo(), settings)
}

/// The specific release the UI offered. `launcher_update` pins to it instead of re-resolving
/// "latest", which is what the engine's `apply` documents and expects.
fn fetch_tag(settings: &Settings, tag: &str) -> Result<(Box<dyn Downloader>, Release)> {
    open_repo_tagged(settings.launcher_repo(), settings, Some(tag))
}

/// Is a newer launcher published? `Ok(None)` = this build is current.
///
/// A failure here means UNKNOWN, not up-to-date. The frontend keeps those apart: an unreachable
/// GitHub must neither pass a stale launcher off as current, nor block the user from playing.
#[tauri::command]
pub async fn launcher_check() -> Result<Option<LauncherUpdateView>, CmdError> {
    tauri::async_runtime::spawn_blocking(|| {
        let s = Settings::load();
        let (_, release) = fetch(&s).map_err(CmdError::from)?;
        Ok(selfupdate::available(&release).map(|a| LauncherUpdateView {
            tag: a.tag,
            version: a.version,
            current: a.current,
            notes: a.notes,
        }))
    })
    .await
    .map_err(CmdError::task)?
}

/// Download + verify + swap in the new launcher, then restart into it. On success this process is
/// on its way out, so the webview will not observe the return value.
/// `tag` is the release the UI is showing. It is pinned deliberately: re-resolving "latest" here
/// would install whatever the repo says NOW, which is not necessarily what the user agreed to —
/// flipping a bad release to prerelease between check and click would silently DOWNGRADE them,
/// and `available()` is never consulted on that path to notice. Omitted (older frontend) falls
/// back to latest, still gated by `available`.
#[tauri::command]
pub async fn launcher_update(
    app: tauri::AppHandle,
    state: tauri::State<'_, std::sync::Arc<AppState>>,
    tag: Option<String>,
) -> Result<(), CmdError> {
    let handle = app.clone();
    let st = state.inner().clone();
    // Everything — including the restart — happens INSIDE the blocking closure so the op guard
    // covers it. Ending the closure at the swap left a window where the exe was already replaced
    // but the event loop still lived, so an apply could claim the freed slot and then be killed
    // mid-commit by the exit: exactly what the guard exists to prevent.
    tauri::async_runtime::spawn_blocking(move || {
        let _op = st.begin_op("launcher update")?;
        let s = Settings::load();
        let (dl, release) = match tag.as_deref() {
            Some(t) => fetch_tag(&s, t),
            None => fetch(&s),
        }
        .map_err(CmdError::from)?;
        // Refuse to "update" to something that is not newer. Guards the pinned path too: a tag
        // can be re-pointed at other bytes, and this is the only check that the release we are
        // about to execute is an upgrade at all.
        if selfupdate::available(&release).is_none() {
            return Err(CmdError::from(format!(
                "release {} is not newer than this build ({}) — check for updates again",
                release.tag_name,
                env!("CARGO_PKG_VERSION")
            )));
        }
        let mut emit = |bytes_done: u64, bytes_total: Option<u64>| {
            let _ = handle.emit("launcher-progress", LauncherProgress { bytes_done, bytes_total });
            true // nothing cancels a self-update mid-flight; the swap only happens after verify
        };
        let exe = selfupdate::apply(dl.as_ref(), &release, &mut emit).map_err(CmdError::from)?;

        // A detached cache warm may still be streaming optional content. Exiting would abort it
        // anyway (it is best-effort and resumable), but stopping it deliberately keeps the handoff
        // quiet instead of killing writes mid-chunk.
        install::cancel_warm();

        // The swap is done, so `exe` now names the NEW binary — Windows never let the old image's
        // path follow the rename, which is exactly what makes restarting this simple.
        std::process::Command::new(&exe)
            .arg(selfupdate::UPDATED_FLAG)
            .spawn()
            // PAST THE POINT OF NO RETURN: the new build is already on disk under the launcher's
            // name. A distinct kind keeps the UI from claiming "nothing was replaced" — the only
            // true and useful thing to say here is "it is installed, start it again".
            .map_err(|e| CmdError::restart_failed(format!("starting the updated launcher: {e}")))?;
        app.exit(0);
        Ok::<_, CmdError>(())
    })
    .await
    .map_err(CmdError::task)?
}

/// This build's version, and whether a self-update just restarted us into it.
#[tauri::command]
pub fn launcher_info() -> LauncherInfoView {
    LauncherInfoView {
        version: env!("CARGO_PKG_VERSION").to_string(),
        just_updated: std::env::args().any(|a| a == selfupdate::UPDATED_FLAG),
    }
}
