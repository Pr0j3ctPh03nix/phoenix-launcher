//! Settings commands: read, save, and the single-field setters (setup flow, language toggle,
//! customization selections). All writes go through `Settings::update`, which serializes them.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::config::{self, Settings};
use crate::launch;
use crate::steaminf;
use crate::views::{CmdError, GameDirStatus, LaunchFlagView, SettingsView};

#[tauri::command]
pub fn get_settings() -> SettingsView {
    let s = Settings::load();
    // Read before the fields below are moved out of `s`.
    let has_token = s.token().is_some();
    SettingsView {
        source_repo: s.source_repo,
        game_dir: s.game_dir.map(|p| p.display().to_string()).unwrap_or_default(),
        // Whether the launcher has ANY credential at all — now solely the baked one. Read by the
        // (unrendered) Advanced pane for its placeholder text; nothing is stored per-user.
        has_token,
        language: s.language,
        launch_extra: s.launch_extra,
        renderer: s.renderer,
        animations: s.animations,
        launch_flags: launch::LAUNCH_FLAGS
            .iter()
            .map(|f| LaunchFlagView {
                id: f.id.to_string(),
                args: f.args.join(" "),
                enabled: launch::flag_enabled(&s.launch_flags, f.id),
            })
            .collect(),
        selections: serde_json::to_value(&s.selections).unwrap_or_default(),
    }
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn save_settings(
    source_repo: String,
    game_dir: String,
    token: String,
    clear_token: bool,
    language: Option<String>,
    launch_extra: String,
    renderer: String,
    launch_flags: BTreeMap<String, bool>,
) -> Result<(), CmdError> {
    Settings::update(move |s| {
        s.source_repo = if source_repo.trim().is_empty() {
            config::DEFAULT_REPO.to_string()
        } else {
            source_repo
        };
        s.game_dir = if game_dir.trim().is_empty() { None } else { Some(PathBuf::from(game_dir)) };
        // `token` / `clear_token` are accepted and IGNORED. The parameters stay so the existing
        // frontend call keeps type-checking, but no token is ever stored: authentication is the
        // baked credential alone (see Settings::token). Drop these two once the Advanced pane's
        // token input goes with them.
        let _ = (&token, clear_token);
        s.language = language;
        s.launch_extra = launch_extra;
        s.renderer = if renderer == "dx9" { renderer } else { "dx11".to_string() };
        // only ids the table knows are stored: a key from another build never accumulates on
        // disk, and a flag the UI didn't send keeps whatever was saved
        for f in launch::LAUNCH_FLAGS {
            if let Some(&on) = launch_flags.get(f.id) {
                s.launch_flags.insert(f.id.to_string(), on);
            }
        }
        // selections untouched — they persist independently
    })
    .map_err(CmdError::from)
}

/// Save just the game folder (setup flow / autofind pick).
#[tauri::command]
pub fn set_game_dir(path: String) -> Result<(), CmdError> {
    Settings::update(move |s| {
        s.game_dir = if path.trim().is_empty() { None } else { Some(PathBuf::from(path)) };
    })
    .map_err(CmdError::from)
}

/// Save just the language (settings toggle applies instantly).
#[tauri::command]
pub fn set_language(language: Option<String>) -> Result<(), CmdError> {
    Settings::update(move |s| s.language = language).map_err(CmdError::from)
}

/// Save just the animations switch (applies instantly in the settings view, like language).
#[tauri::command]
pub fn set_animations(on: bool) -> Result<(), CmdError> {
    Settings::update(move |s| s.animations = on).map_err(CmdError::from)
}

/// Save one option selection (customization view control).
#[tauri::command]
pub fn set_selection(id: String, value: serde_json::Value) -> Result<(), CmdError> {
    Settings::update(move |s| {
        s.selections.insert(id, value);
    })
    .map_err(CmdError::from)
}

/// Where does the game folder currently resolve to, and is it one? Drives the setup view.
#[tauri::command]
pub fn game_dir_status() -> Result<GameDirStatus, CmdError> {
    // Dev knob: PHOENIX_FORCE_SETUP=1 reports "never configured, no game beside the exe" — the
    // exact condition boot() shows the first-run setup view on — WITHOUT touching the saved
    // settings. This is the only way to SEE that view once a folder was ever chosen; it is
    // otherwise unreachable on a configured machine. Debug-only for the same reason the CLI is:
    // a shipped build must have no hidden switches that change what the user sees. Note the
    // override is read-side only — going THROUGH setup (picking a folder) still saves for real.
    #[cfg(debug_assertions)]
    if std::env::var_os("PHOENIX_FORCE_SETUP").is_some_and(|v| v != "0") {
        return Ok(GameDirStatus { dir: String::new(), configured: false, client_version: None });
    }
    let s = Settings::load();
    let configured = s.game_dir.is_some();
    let dir = s.resolve_game_dir().map_err(CmdError::from)?;
    Ok(GameDirStatus {
        configured,
        client_version: steaminf::client_version(&dir),
        dir: dir.display().to_string(),
    })
}
