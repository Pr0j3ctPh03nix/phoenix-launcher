//! Launcher self-update commands. selfupdate.rs does the network work and the on-disk swap; this
//! layer owns what the engine deliberately does not — restarting the process into the new binary.

use anyhow::Result;
use tauri::Emitter;

use crate::cmd::AppState;
use crate::config::Settings;
use crate::source;
use crate::trust::Payload;
use crate::views::{CmdError, LauncherInfoView, LauncherProgress, LauncherUpdateView};
use crate::{install, selfupdate};

/// Is a newer launcher published? `Ok(None)` = this build is current.
///
/// A failure here means UNKNOWN, not up-to-date. The frontend keeps those apart: an unreachable
/// GitHub must neither pass a stale launcher off as current, nor block the user from playing.
#[tauri::command]
pub async fn launcher_check(
    state: tauri::State<'_, std::sync::Arc<AppState>>,
) -> Result<Option<LauncherUpdateView>, CmdError> {
    let st = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let s = Settings::load();
        let found = source::with_active(
            &s,
            s.launcher_repo(),
            Payload::Launcher,
            None,
            |dl, release| Ok((release.tag_name.clone(), selfupdate::available(&s, dl, release)?)),
        );
        // A release this launcher cannot BELIEVE is a release we do not have, and `available` now
        // says so with an error so the walk can fail the source over. Once the whole ranking has
        // answered that way it is not an error any more, it is "no update" — which is the answer
        // the user gets today and the one that must never block Play.
        let (tag, available) = match found {
            Ok(v) => v,
            Err(e) if selfupdate::is_untrustworthy(&e) => return Ok(None),
            Err(e) => return Err(CmdError::from(e)),
        };
        // Record the tag whether or not it is an update: this is the freshness key the "What's
        // new" launcher page checks its cached history against, and the common case — this build
        // IS the latest — is exactly the one where that history must open without a round trip.
        *st.launcher_tag.lock().unwrap() = Some(tag);
        Ok(available.map(|a| LauncherUpdateView {
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
        let emit = |bytes_done: u64, bytes_total: Option<u64>| {
            let _ = handle.emit("launcher-progress", LauncherProgress { bytes_done, bytes_total });
            true // nothing cancels a self-update mid-flight; the swap only happens after verify
        };
        // The WALK covers the download and nothing else. A source that serves a bad exe fails over
        // and the next one is asked from zero (a `.part` from a different version stitched onto new
        // bytes is a corrupt file of plausible length, so this path never resumes); the SWAP is
        // outside it, because renaming the running launcher is not an operation to retry against
        // another host.
        let staged = source::with_active(
            &s,
            s.launcher_repo(),
            Payload::Launcher,
            tag.as_deref(),
            |dl, release| {
                // Refuse to "update" to something that is not newer. Guards the pinned path too: a
                // tag can be re-pointed at other bytes, and this is the only check that the release
                // we are about to execute is an upgrade at all.
                let Some(offer) = selfupdate::available(&s, dl, release)? else {
                    anyhow::bail!(
                        "release {} is not newer than this build ({}) — check for updates again",
                        release.tag_name,
                        env!("CARGO_PKG_VERSION")
                    );
                };
                // The manifest that judgement was made on, already verified: the download resolves
                // the exe out of THAT document instead of fetching and Ed25519-checking a second
                // copy of it on the one path that is also pulling a multi-megabyte binary.
                selfupdate::fetch_verified(dl, release, offer.manifest.as_ref(), &mut |d, t| {
                    emit(d, t)
                })
            },
        )
        .map_err(CmdError::from)?;
        let exe = selfupdate::swap_in(&staged).map_err(CmdError::from)?;

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
