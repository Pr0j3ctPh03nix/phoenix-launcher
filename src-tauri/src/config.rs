//! Persisted updater settings. Both the GUI and the CLI load/override these.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Baked default source repo — the public distribution. Settings override it (e.g. to a private
/// staging repo, which also needs `token`).
pub const DEFAULT_REPO: &str = "Pr0j3ctPh03nix/client-dist";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// `owner/name` of the dist repo whose Releases we install from.
    #[serde(default = "default_repo")]
    pub source_repo: String,
    /// Folder that CONTAINS `game/`. None = resolve to the updater exe's own directory.
    #[serde(default)]
    pub game_dir: Option<PathBuf>,
    /// GitHub token, only needed when `source_repo` is private.
    #[serde(default)]
    pub token: Option<String>,
    /// UI language, "en" / "ru". None = auto-detect in the frontend.
    #[serde(default)]
    pub language: Option<String>,
    /// User's additional launch options, appended after the hardcoded base set.
    #[serde(default)]
    pub launch_extra: String,
    /// Renderer flag for launch: "dx11" (default) or "dx9".
    #[serde(default = "default_renderer")]
    pub renderer: String,
    /// Manifest option selections: option id -> variant id (choice) or bool (toggle).
    #[serde(default)]
    pub selections: BTreeMap<String, serde_json::Value>,
}

fn default_repo() -> String {
    DEFAULT_REPO.to_string()
}

fn default_renderer() -> String {
    "dx11".to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            source_repo: default_repo(),
            game_dir: None,
            token: None,
            language: None,
            launch_extra: String::new(),
            renderer: default_renderer(),
            selections: BTreeMap::new(),
        }
    }
}

impl Settings {
    pub fn config_path() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "ProjectPhoenix", "PhoenixLauncher")
            .map(|d| d.config_dir().join("settings.json"))
    }

    pub fn load() -> Self {
        let Some(p) = Self::config_path() else { return Self::default() };
        let Ok(text) = std::fs::read_to_string(&p) else { return Self::default() };
        match serde_json::from_str(&text) {
            Ok(s) => s,
            Err(_) => {
                // corrupt file: preserve it before defaults get saved over it
                let _ = std::fs::copy(&p, p.with_extension("json.bak"));
                Self::default()
            }
        }
    }

    pub fn save(&self) -> Result<()> {
        let p = Self::config_path().context("no config directory available")?;
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&p, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// The folder that contains `game/`. An explicit setting wins; otherwise the updater exe's own
    /// directory (the updater is meant to ship alongside the game bundle).
    pub fn resolve_game_dir(&self) -> Result<PathBuf> {
        if let Some(g) = &self.game_dir {
            return Ok(g.clone());
        }
        let exe = std::env::current_exe().context("locating the updater executable")?;
        Ok(exe.parent().context("updater executable has no parent dir")?.to_path_buf())
    }
}
