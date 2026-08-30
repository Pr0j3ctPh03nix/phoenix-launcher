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

/// One place releases can be downloaded from. Position in `Settings::sources` is priority order.
///
/// The two variants are deliberately asymmetric, and that asymmetry is the safety property:
/// `Primary` has NO `enabled` field and NO url. Mirrors are discovered from a published
/// `mirrors.json`, which is a list of mirror URLs — so there exists no value that document could
/// carry, however malformed or hostile, that names, disables or removes the main source. Mirrors
/// can only ever be a complement to it. `migrate` guarantees exactly one `Primary` survives.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Source {
    /// The baked-in GitHub source repo (`Settings::source_repo`). Always present, always used.
    Primary,
    Mirror {
        /// Base URL, normalized (no trailing slash) — the entry's identity, and what the list is
        /// deduplicated on.
        url: String,
        /// A disabled mirror stays in the list and is still PROBED, which is how one that has
        /// come back gets noticed; it is simply never downloaded from.
        #[serde(default = "default_true")]
        enabled: bool,
        /// Has this mirror ever been timed? A newly published one arrives `false`, and that — not
        /// a clock — is the only thing that triggers an automatic measurement. Speeds are never
        /// re-taken on a schedule: measuring costs a real download per source, and re-ordering the
        /// list unprompted is exactly what would move a user off the source they chose.
        #[serde(default)]
        measured: bool,
    },
}

impl Source {
    pub fn url(&self) -> Option<&str> {
        match self {
            Source::Primary => None,
            Source::Mirror { url, .. } => Some(url),
        }
    }

    pub fn is_primary(&self) -> bool {
        matches!(self, Source::Primary)
    }

    /// The primary is unconditionally enabled — see the type's note.
    pub fn enabled(&self) -> bool {
        match self {
            Source::Primary => true,
            Source::Mirror { enabled, .. } => *enabled,
        }
    }
}

/// A reference to a source, for naming one the user pinned. Separate from `Source` because a
/// pin stores an identity, not a state — carrying `enabled` here would be a second copy of a
/// fact the list already holds, free to disagree with it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SourceRef {
    Primary,
    Mirror { url: String },
}

impl Source {
    pub fn is(&self, r: &SourceRef) -> bool {
        match (self, r) {
            (Source::Primary, SourceRef::Primary) => true,
            (Source::Mirror { url, .. }, SourceRef::Mirror { url: pinned }) => url == pinned,
            _ => false,
        }
    }
}

/// Which source will actually be used.
///
/// THE one definition — the settings pane, the CLI and any future downloader all resolve through
/// here, so "in use" cannot come to mean different things in the UI and in the download path.
///
/// A pin wins while it is still in the list and still enabled; otherwise the head of the ranking
/// does, which after a sweep is the fastest working source. A pin that has gone stale (its mirror
/// unpublished, or switched off) is therefore ignored rather than obeyed into a dead end — and it
/// is kept, not cleared, so re-enabling the mirror restores the user's choice.
pub fn active_index(sources: &[Source], pinned: Option<&SourceRef>) -> Option<usize> {
    pinned
        .and_then(|r| sources.iter().position(|s| s.is(r) && s.enabled()))
        .or_else(|| sources.iter().position(Source::enabled))
}

/// Canonical form of a published mirror base URL, or None if it is not one. Everything downstream
/// appends a path to this string, so a trailing slash and a missing scheme are the two ways an
/// entry that looks fine silently never resolves.
pub fn normalize_mirror_url(url: &str) -> Option<String> {
    let u = url.trim().trim_end_matches('/');
    let rest = u.strip_prefix("https://").or_else(|| u.strip_prefix("http://"))?;
    (!rest.is_empty()).then(|| u.to_string())
}

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
    /// UI animations master switch. Purely a frontend concern (off = the `anim-off` kill class);
    /// persisted backend-side like every other setting. Default ON.
    #[serde(default = "default_true")]
    pub animations: bool,
    /// Optional launch flags: `launch::LAUNCH_FLAGS` id -> on. A missing id means the flag's
    /// own default, so a new flag needs no migration.
    #[serde(default)]
    pub launch_flags: BTreeMap<String, bool>,
    /// Manifest option selections: option id -> variant id (choice) or bool (toggle).
    #[serde(default)]
    pub selections: BTreeMap<String, serde_json::Value>,
    /// Download sources in priority order. Always holds exactly one `Source::Primary` (enforced
    /// by `migrate`). The `Mirror` entries are DISCOVERED, never user-authored: a sweep replaces
    /// them wholesale from the published `mirrors.json`, so the only thing a user decides about a
    /// mirror is whether to use it.
    /// Always ordered fastest-first by the last sweep that measured, so the head of the list is
    /// the source in use. There is no setting for this: a slower source ahead of a faster one is
    /// not a preference anyone holds.
    #[serde(default = "default_sources")]
    pub sources: Vec<Source>,
    /// The source the user pinned, if any. `None` means "follow the ranking" — use the fastest.
    ///
    /// A choice is never overridden quietly. Exactly two things clear it: the TEST BUTTON (asking
    /// to be re-tested is asking for the answer the test gives), and a measurement finding the
    /// pinned source unusable — which is the user's own rule, "unless their mirror goes offline".
    /// Nothing else touches it: not a launch, not a list refresh, not a newly published mirror.
    #[serde(default)]
    pub selected: Option<SourceRef>,
    /// When a newly published mirror turns up, test everything and switch to the best — pin and
    /// all. On by default: a mirror is published because it is worth using, and the people this
    /// exists for are the least likely to go looking for a settings pane.
    ///
    /// Off means a new mirror is only listed, marked untested, and left alone until the user runs
    /// the test themselves. It does not merely defer the switch — with it off, NOTHING measures
    /// automatically, so nothing reorders and nothing touches the pin.
    #[serde(default = "default_true")]
    pub auto_pick_best: bool,
    /// The highest signed `serial` accepted for each payload id, ever. The rollback ratchet: a
    /// mirror can always serve an older release it once held a valid signature for, and nothing
    /// else in a signed document says it is not the current one.
    ///
    /// Plaintext in the user's profile, which is exactly why it is not the only floor — anything
    /// that can edit this file can hand the ratchet back, so `trust::Payload::baked_min_serial`
    /// sits underneath it in the binary. `serial_floor` is where the two meet, and it is the ONE
    /// definition: nothing may compare a serial against half of it.
    #[serde(default)]
    pub max_serial_seen: BTreeMap<String, u64>,
}

fn default_sources() -> Vec<Source> {
    vec![Source::Primary]
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

fn default_true() -> bool {
    true
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
            animations: true,
            launch_flags: BTreeMap::new(),
            selections: BTreeMap::new(),
            sources: default_sources(),
            selected: None,
            auto_pick_best: true,
            max_serial_seen: BTreeMap::new(),
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

    /// Bring an older on-disk schema up to SETTINGS_VERSION. No field transformations yet — v1 is
    /// the first schema; future bumps migrate here.
    fn migrate(&mut self) {
        if self.version < SETTINGS_VERSION {
            self.version = SETTINGS_VERSION;
        }
        // Exactly one Primary, always. A file written before `sources` existed, or one hand-edited
        // to drop it, must not leave the launcher with no main source — that is the state mirrors
        // are never allowed to produce, so it cannot be reachable by accident either. Restored at
        // the FRONT, since a list with no measurements has no better order to claim.
        if self.sources.iter().filter(|s| s.is_primary()).count() != 1 {
            self.sources.retain(|s| !s.is_primary());
            self.sources.insert(0, Source::Primary);
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

    /// The lowest `serial` a signed manifest for `payload` may carry: whichever of the baked
    /// backstop and this machine's own high-water mark is greater. Neither alone is enough — the
    /// baked one cannot know what the user has since installed, and the persisted one lives in an
    /// editable file.
    pub fn serial_floor(&self, payload: crate::trust::Payload) -> u64 {
        let seen = self.max_serial_seen.get(payload.id()).copied().unwrap_or(0);
        seen.max(payload.baked_min_serial())
    }

    /// Is this serial past what we have recorded for `payload`? The read half of the ratchet, so a
    /// caller holding a settings snapshot can decide whether a WRITE is needed at all —
    /// `Settings::update` always saves, and the common case is the same release checked again.
    ///
    /// Deliberately NOT `serial_floor`: that folds in the baked backstop, and a build whose baked
    /// floor already exceeds the persisted one would then never record anything.
    pub fn serial_is_newer(&self, payload: crate::trust::Payload, serial: u64) -> bool {
        serial > self.max_serial_seen.get(payload.id()).copied().unwrap_or(0)
    }

    /// Move the ratchet forward. Returns whether anything changed.
    ///
    /// Never moves it BACK: the floor is a high-water mark, and a lower serial arriving here at
    /// all would mean the gate that rejects one had already been passed.
    pub fn advance_serial(&mut self, payload: crate::trust::Payload, serial: u64) -> bool {
        if !self.serial_is_newer(payload, serial) {
            return false;
        }
        self.max_serial_seen.insert(payload.id().to_string(), serial);
        true
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

#[cfg(test)]
mod tests {
    use super::*;

    fn mirror(url: &str, enabled: bool) -> Source {
        Source::Mirror { url: url.to_string(), enabled, measured: true }
    }
    fn pin(url: &str) -> SourceRef {
        SourceRef::Mirror { url: url.to_string() }
    }

    /// No pin: the head of the ranking wins, which after a sweep is the fastest working source.
    #[test]
    fn unpinned_uses_the_head() {
        let s = vec![mirror("https://a", true), Source::Primary];
        assert_eq!(active_index(&s, None), Some(0));
    }

    #[test]
    fn a_pin_wins_over_the_ranking() {
        let s = vec![mirror("https://a", true), Source::Primary, mirror("https://b", true)];
        assert_eq!(active_index(&s, Some(&pin("https://b"))), Some(2));
        assert_eq!(active_index(&s, Some(&SourceRef::Primary)), Some(1));
    }

    /// Switching off the pinned mirror must hand the job to the ranking, not strand the user on a
    /// source that is excluded — the pin itself is deliberately NOT cleared, so turning the mirror
    /// back on restores the choice.
    #[test]
    fn a_disabled_pin_falls_back() {
        let s = vec![mirror("https://a", true), mirror("https://b", false)];
        assert_eq!(active_index(&s, Some(&pin("https://b"))), Some(0));
    }

    /// A mirror the publisher has dropped is gone from the list; a pin naming it is stale and must
    /// not resolve to nothing.
    #[test]
    fn a_pin_to_a_vanished_mirror_falls_back() {
        let s = vec![Source::Primary, mirror("https://a", true)];
        assert_eq!(active_index(&s, Some(&pin("https://gone"))), Some(0));
    }

    /// Every mirror off is a reachable state; the primary has no switch, so there is always one
    /// enabled source left and this can never resolve to None.
    #[test]
    fn primary_survives_everything_being_switched_off() {
        let s = vec![mirror("https://a", false), Source::Primary, mirror("https://b", false)];
        assert_eq!(active_index(&s, Some(&pin("https://a"))), Some(1));
        assert_eq!(active_index(&s, None), Some(1));
    }

    /// `migrate` is the guarantee that no settings file — hand-edited, or written by a build that
    /// predates `sources` — can leave the launcher without a main source.
    #[test]
    fn migrate_restores_a_missing_primary() {
        let mut s = Settings { sources: vec![mirror("https://a", true)], ..Settings::default() };
        s.migrate();
        assert_eq!(s.sources.first(), Some(&Source::Primary));
        assert_eq!(s.sources.len(), 2);
    }

    /// The rollback ratchet: forward only, per payload, and durable. It is half of the floor —
    /// `trust::Payload::baked_min_serial` is the other half, and `serial_floor` is the only place
    /// they are allowed to meet.
    #[test]
    fn the_serial_ratchet_only_ever_moves_forward() {
        use crate::trust::Payload;
        let baked = Payload::Mod.baked_min_serial();
        let mut s = Settings::default();
        assert_eq!(s.serial_floor(Payload::Mod), baked, "no history: the baked backstop alone");

        assert!(s.serial_is_newer(Payload::Mod, 5));
        assert!(s.advance_serial(Payload::Mod, 5));
        assert_eq!(s.serial_floor(Payload::Mod), 5.max(baked));
        assert!(!s.serial_is_newer(Payload::Mod, 5), "so no settings write is needed for it");
        assert!(!s.advance_serial(Payload::Mod, 5), "the same release again is not news");
        assert!(!s.advance_serial(Payload::Mod, 4), "and it never walks back");
        assert_eq!(s.max_serial_seen["mod"], 5);
        assert_eq!(
            s.serial_floor(Payload::Game),
            Payload::Game.baked_min_serial(),
            "one payload's history says nothing about another's"
        );

        // it has to survive the round trip — an in-memory ratchet protects nothing
        let saved: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(saved.max_serial_seen["mod"], 5);
        // and a file written before the field existed simply has no history
        let old: Settings = serde_json::from_str(r#"{"version":1}"#).unwrap();
        assert!(old.max_serial_seen.is_empty());
    }

    #[test]
    fn migrate_dedupes_a_doubled_primary() {
        let mut s = Settings {
            sources: vec![Source::Primary, mirror("https://a", true), Source::Primary],
            ..Settings::default()
        };
        s.migrate();
        assert_eq!(s.sources.iter().filter(|x| x.is_primary()).count(), 1);
    }
}
