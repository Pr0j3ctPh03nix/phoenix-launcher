//! Launch commands: spawn the game, and the autoexec.cfg editor's read/save.

use std::path::PathBuf;

use crate::config::Settings;
use crate::launch;
use crate::views::{AutoexecView, CmdError};

#[tauri::command]
pub fn play() -> Result<(), CmdError> {
    let s = Settings::load();
    let gd = s.resolve_game_dir().map_err(CmdError::from)?;
    launch::launch(&gd, &s.renderer, &s.launch_extra).map_err(CmdError::from)
}

/// Is the game currently running? The frontend polls this every few seconds: it shows an
/// "in game" status while the game runs and re-plans once the game closes. Async so the
/// settings read + write-probe stay off the main thread.
#[tauri::command]
pub async fn game_running() -> Result<bool, CmdError> {
    tauri::async_runtime::spawn_blocking(|| {
        let s = Settings::load();
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
    Ok(gd.join("game").join("dota").join("cfg").join("autoexec.cfg"))
}

#[tauri::command]
pub fn read_autoexec() -> Result<AutoexecView, CmdError> {
    let p = autoexec_path()?;
    match std::fs::read(&p) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(content) => Ok(AutoexecView { content, lossy: false }),
            // not UTF-8 (e.g. cp1251 comments): still show it, but flagged — the UI blocks
            // saving so a lossy round-trip can never overwrite the user's real bytes
            Err(e) => Ok(AutoexecView {
                content: String::from_utf8_lossy(e.as_bytes()).into_owned(),
                lossy: true,
            }),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(AutoexecView { content: String::new(), lossy: false })
        }
        Err(e) => Err(CmdError::from(format!("reading {}: {e}", p.display()))),
    }
}

#[tauri::command]
pub fn save_autoexec(content: String) -> Result<(), CmdError> {
    let p = autoexec_path()?;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| CmdError::from(format!("creating {}: {e}", parent.display())))?;
    }
    std::fs::write(&p, content).map_err(|e| CmdError::from(format!("writing {}: {e}", p.display())))
}
