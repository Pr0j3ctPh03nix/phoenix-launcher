//! Launch commands: spawn the game, and the autoexec.cfg editor's read/save.

use std::path::PathBuf;

use crate::config::Settings;
use crate::launch;
use crate::views::CmdError;

#[tauri::command]
pub fn play() -> Result<(), CmdError> {
    let s = Settings::load();
    let gd = s.resolve_game_dir().map_err(CmdError::from)?;
    launch::launch(&gd, &s.renderer, &s.launch_extra).map_err(CmdError::from)
}

// ---- autoexec.cfg ----

fn autoexec_path() -> Result<PathBuf, CmdError> {
    let s = Settings::load();
    let gd = s.resolve_game_dir().map_err(CmdError::from)?;
    Ok(gd.join("game").join("dota").join("cfg").join("autoexec.cfg"))
}

#[tauri::command]
pub fn read_autoexec() -> Result<String, CmdError> {
    let p = autoexec_path()?;
    match std::fs::read_to_string(&p) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
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
