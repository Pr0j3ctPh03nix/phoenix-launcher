//! Per-install record of what the updater placed, stored next to the game so it is portable and
//! survives a lost config dir. Drives uninstall and future diffs.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const STATE_FILE: &str = ".phoenix-state.json";

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct InstalledState {
    pub version: String,
    pub files: Vec<InstalledFile>,
    /// True only if the updater itself created winmm_orig.dll (copied from System32). A user's own
    /// pre-existing winmm_orig.dll leaves this false, so uninstall never deletes it.
    #[serde(default)]
    pub winmm_orig_created: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InstalledFile {
    pub dest: String,
    pub sha256: String,
}

impl InstalledState {
    pub fn path(game_dir: &Path) -> PathBuf {
        game_dir.join(STATE_FILE)
    }

    pub fn load(game_dir: &Path) -> Option<Self> {
        std::fs::read_to_string(Self::path(game_dir))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
    }

    pub fn save(&self, game_dir: &Path) -> Result<()> {
        std::fs::write(Self::path(game_dir), serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}
