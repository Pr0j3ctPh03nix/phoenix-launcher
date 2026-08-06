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
    SettingsView {
        source_repo: s.source_repo,
        game_dir: s.game_dir.map(|p| p.display().to_string()).unwrap_or_default(),
        has_token: s.token.is_some(),
        language: s.language,
        launch_extra: s.launch_extra,
        renderer: s.renderer,
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
        // blank token field => keep whatever was saved (we never send the token to the UI);
        // the explicit clear flag is the only way to remove a saved token
        if clear_token {
            s.token = None;
        } else if !token.is_empty() {
            s.token = Some(token);
        }
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
    let s = Settings::load();
    let configured = s.game_dir.is_some();
    let dir = s.resolve_game_dir().map_err(CmdError::from)?;
    Ok(GameDirStatus {
        configured,
        client_version: steaminf::client_version(&dir),
        dir: dir.display().to_string(),
    })
}
