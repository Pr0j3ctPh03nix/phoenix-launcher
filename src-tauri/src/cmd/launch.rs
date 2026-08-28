//! Launch commands: spawn the game, and the autoexec.cfg editor's read/save.

use std::path::PathBuf;

use crate::config::Settings;
use crate::launch;
use crate::views::{AutoexecView, CmdError};

/// Spawn the game. Guarded on BOTH counts the UI relies on: the frontend disables Play while the
/// game runs and while an op is in flight, and this is the backend line behind those buttons —
/// without it, any path that reaches the command anyway (a keyboard handler, a stale webview
/// after a failed poll, a double fire) would start a second client, or start one out of a folder
/// that phase 2 is at that moment moving files into.
#[tauri::command]
pub fn play(state: tauri::State<'_, std::sync::Arc<crate::cmd::AppState>>) -> Result<(), CmdError> {
    let _op = state.begin_op("play")?;
    let s = Settings::load();
    let gd = s.resolve_game_dir().map_err(CmdError::from)?;
    if launch::game_running(&gd) {
        return Err(CmdError::from(
            anyhow::Error::new(crate::engine::GameRunning(gd.clone()))
                .context("Dota 2 is already running"),
        ));
    }
    launch::launch(&gd, &s.renderer, &s.launch_extra, &s.launch_flags).map_err(CmdError::from)
}

/// Is the game currently running? The frontend polls this every few seconds: it shows an
/// "in game" status while the game runs and re-plans once the game closes. Async so the
/// settings read + write-probe stay off the main thread. `load_cached` (mtime memo), not `load`:
/// this is the one command that runs forever on a timer, and a full settings read + JSON parse
/// every 3 s bought nothing — the game dir changes only on a save, which moves the mtime.
#[tauri::command]
pub async fn game_running() -> Result<bool, CmdError> {
    tauri::async_runtime::spawn_blocking(|| {
        let s = Settings::load_cached();
        let gd = s.resolve_game_dir().map_err(CmdError::from)?;
        Ok(launch::game_running(&gd))
    })
    .await
    .map_err(CmdError::task)?
}

// ---- autoexec.cfg ----

fn autoexec_path() -> Result<PathBuf, CmdError> {
    let s = Settings::load();
    let gd = s.resolve_game_dir().map_err(CmdError::from)?;
    Ok(launch::autoexec_cfg(&gd))
}

#[tauri::command]
pub fn read_autoexec() -> Result<AutoexecView, CmdError> {
    let p = autoexec_path()?;
    let pinned = launch::PINNED_CONVARS.to_vec();
    match std::fs::read(&p) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(content) => Ok(AutoexecView { content, lossy: false, pinned }),
            // not UTF-8 (e.g. cp1251 comments): still show it, but flagged — the UI blocks
            // saving so a lossy round-trip can never overwrite the user's real bytes
            Err(e) => Ok(AutoexecView {
                content: String::from_utf8_lossy(e.as_bytes()).into_owned(),
                lossy: true,
                pinned,
            }),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(AutoexecView { content: String::new(), lossy: false, pinned })
        }
        Err(e) => Err(CmdError::from(format!("reading {}: {e}", p.display()))),
    }
}

#[tauri::command]
pub fn save_autoexec(content: String) -> Result<(), CmdError> {
    let p = autoexec_path()?;
    // The backend line behind the editor's read-only mode (the same pattern as begin_op behind
    // the busy flag): a non-UTF-8 cfg reaches the UI as a LOSSY decode, and writing that decode
    // back would corrupt the user's real bytes. The frontend already refuses — but nothing else
    // may trust the frontend to be the only caller.
    if let Ok(bytes) = std::fs::read(&p) {
        if String::from_utf8(bytes).is_err() {
            return Err(CmdError::from(format!(
                "refusing to overwrite {}: the file is not UTF-8, so the editor was shown a \
                 lossy copy — saving it would corrupt the original",
                p.display()
            )));
        }
    }
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| CmdError::from(format!("creating {}: {e}", parent.display())))?;
    }
    // temp + rename, like settings and the install state: this is the USER'S file — a crash or
    // disk-full mid-write must never leave it truncated
    let tmp = p.with_extension("cfg.tmp");
    std::fs::write(&tmp, content)
        .map_err(|e| CmdError::from(format!("writing {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, &p)
        .map_err(|e| CmdError::from(format!("writing {}: {e}", p.display())))
}
