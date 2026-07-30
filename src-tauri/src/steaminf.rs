//! Reads the game-build marker. Purely informational: autofind uses steam.inf presence to spot
//! game folders and shows the found build; nothing gates on it.

use std::path::Path;

/// The `ClientVersion` from `<game_dir>/game/dota/steam.inf`, if the file exists and carries it.
pub fn client_version(game_dir: &Path) -> Option<String> {
    let path = game_dir.join("game").join("dota").join("steam.inf");
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("ClientVersion=") {
            return Some(v.trim().to_string());
        }
    }
    None
}
