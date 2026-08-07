//! Persisted updater settings. Both the GUI and the CLI load/override these.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// On-disk schema version of settings.json. v1 = initial. Bump and extend `migrate` when the
/// shape changes (e.g. a future `installs[]` for multi-install support).
const SETTINGS_VERSION: u32 = 1;

/// Baked default source repo. Settings override it (Advanced is hidden behind SHOW_ADVANCED in
/// the frontend, so for now this is effectively fixed).
pub const DEFAULT_REPO: &str = "Pr0j3ctPh03nix/client-dist-staging";

/// Where the launcher updates ITSELF from — this repo's own Releases, which publish the portable
/// `phoenix-launcher.exe`. Meant to be public; see `Settings::launcher_repo` for how it is
/// authenticated while it is not.
pub const DEFAULT_LAUNCHER_REPO: &str = "Pr0j3ctPh03nix/phoenix-launcher";

/// The base-game distribution: a release whose assets are the vanilla Dota 2 (build 1805) files
/// themselves, described by a manifest in the SAME format as the shim's. Fresh installs, "Verify
/// game files" and repair all run against it. Public by design — game downloads are gigabytes and
/// must ride the tokenless `browser_download_url` path (free CDN bandwidth, no API rate budget).
pub const DEFAULT_GAME_REPO: &str = "Pr0j3ctPh03nix/game-dist";

/// Read-only token for the private staging repo, injected at BUILD time:
///     PHOENIX_BAKED_TOKEN=github_pat_... bun run tauri build
/// Deliberately not a source literal — a committed github_pat_ gets blocked/revoked by GitHub
/// secret scanning on push. A user-saved token still wins over this. Merged at the point of
/// use (`Settings::token()`), never into the persisted struct — a settings save must not be
/// able to write the baked token to disk.
const BAKED_TOKEN: Option<&str> = option_env!("PHOENIX_BAKED_TOKEN");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Settings schema version (see SETTINGS_VERSION).
    #[serde(default = "default_version")]
    pub version: u32,
    /// `owner/name` of the dist repo whose Releases we install from.
    #[serde(default = "default_repo")]
    pub source_repo: String,
    /// `owner/name` of the repo the launcher self-updates from. None = `DEFAULT_LAUNCHER_REPO`.
    /// An Option (not a defaulted String) so an absent key keeps tracking the baked default
    /// instead of pinning whatever repo was current when the file was first written.
    #[serde(default)]
    pub launcher_repo: Option<String>,
    /// `owner/name` of the base-game distribution repo. None = `DEFAULT_GAME_REPO` (same Option
    /// rationale as `launcher_repo`).
    #[serde(default)]
    pub game_repo: Option<String>,
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
    /// Optional launch flags: `launch::LAUNCH_FLAGS` id -> on. A missing id means the flag's
    /// own default, so a new flag needs no migration.
    #[serde(default)]
    pub launch_flags: BTreeMap<String, bool>,
    /// Manifest option selections: option id -> variant id (choice) or bool (toggle).
    #[serde(default)]
    pub selections: BTreeMap<String, serde_json::Value>,
}

fn default_version() -> u32 {
    SETTINGS_VERSION
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
            version: SETTINGS_VERSION,
            source_repo: default_repo(),
            launcher_repo: None,
            game_repo: None,
            game_dir: None,
            token: None,
            language: None,
            launch_extra: String::new(),
            renderer: default_renderer(),
            launch_flags: BTreeMap::new(),
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
        let mut s: Self = match serde_json::from_str(&text) {
            Ok(s) => s,
            Err(_) => {
                // corrupt file: preserve it before defaults get saved over it
                let _ = std::fs::copy(&p, p.with_extension("json.bak"));
                Self::default()
            }
        };
        s.migrate();
        s
    }

    /// Bring an older on-disk schema up to SETTINGS_VERSION. No transformations yet — v1 is the
    /// first schema; future bumps migrate fields here.
    fn migrate(&mut self) {
        if self.version < SETTINGS_VERSION {
            self.version = SETTINGS_VERSION;
        }
    }

    /// `load` behind an mtime memo, for POLLING callers only (today: the 3-second game_running
    /// poll — a bare `load` there was 1,200 disk reads + JSON parses an hour, forever, for a
    /// value that changes only on a settings save). One stat per call; the file is re-read only
    /// when its mtime moved, which every save does (temp + rename writes a new file). One-shot
    /// commands keep calling `load` — strict reads are the default, the memo is the exception.
    pub fn load_cached() -> Self {
        static CACHE: Mutex<Option<(std::time::SystemTime, Settings)>> = Mutex::new(None);
        let Some(p) = Self::config_path() else { return Self::default() };
        // no file (or unreadable): nothing worth memoizing — load() is one failed read anyway
        let Ok(mtime) = std::fs::metadata(&p).and_then(|m| m.modified()) else {
            return Self::load();
        };
        let mut guard = CACHE.lock().unwrap();
        if let Some((t, s)) = guard.as_ref() {
            if *t == mtime {
                return s.clone();
            }
        }
        let s = Self::load();
        *guard = Some((mtime, s.clone()));
        s
    }

    /// Load → mutate → save, serialized process-wide so concurrent writers (today: commands;
    /// later: background tasks) can't lose each other's changes.
    pub fn update(mutate: impl FnOnce(&mut Self)) -> Result<()> {
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap();
        let mut s = Self::load();
        mutate(&mut s);
        s.save()
    }

    /// The token to authenticate with: a user-saved token wins, else the build-time baked one.
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref().or(BAKED_TOKEN)
    }

    /// The repo the launcher self-updates from. No token FIELD of its own: this repo is meant to
    /// be public and anonymous GitHub allows 60 requests/hour per IP, which is plenty for one
    /// check per launch.
    ///
    /// It is not, however, "never authenticated" — `open_repo` tries anonymously and retries with
    /// `Settings::token()` (the dist PAT) if and only if the anonymous attempt was REFUSED by the
    /// server. That is what keeps self-update working while this repo is still private. The
    /// header only ever reaches api.github.com and is stripped on redirect, so the retry costs
    /// nothing but a possible 403 where anonymous would have worked — which is why it is a retry
    /// and not the first attempt.
    pub fn launcher_repo(&self) -> &str {
        self.launcher_repo.as_deref().unwrap_or(DEFAULT_LAUNCHER_REPO)
    }

    /// The base-game distribution repo (fresh install / verify / repair source).
    pub fn game_repo(&self) -> &str {
        self.game_repo.as_deref().unwrap_or(DEFAULT_GAME_REPO)
    }

    pub fn save(&self) -> Result<()> {
        let p = Self::config_path().context("no config directory available")?;
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // temp + rename: a crash mid-write can't torch the settings (the corrupt-file .bak
        // path in `load` stays a last resort)
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, &p)?;
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
