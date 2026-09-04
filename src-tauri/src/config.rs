//! Persisted updater settings. Both the GUI and the CLI load/override these.

use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

/// On-disk schema version of settings.json. v1 = initial. v2 = one kind of download source, with
/// its measurement persisted alongside it (see `Source`, `Measured` and `migrate`). Bump and extend
/// `migrate` when the shape changes (e.g. a future `installs[]` for multi-install support).
const SETTINGS_VERSION: u32 = 2;

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

/// Where the signed mirror list is published. A repo of its own, and PUBLIC: the list used to be
/// read from `source_repo`, which is wrong in both directions — the dist repo is private (so every
/// client would need the baked credential just to learn which mirrors exist, including the clients
/// that cannot reach GitHub, who are the entire audience for mirrors), and "who may register a
/// mirror" is not "who may cut a client release". A public registry can take a pull request from a
/// mirror operator without handing them the release channel.
pub const DEFAULT_MIRRORS_REPO: &str = "Pr0j3ctPh03nix/phoenix-mirror-registry";

/// Read-only GitHub access for the PRIVATE client-dist-staging repo, injected at BUILD time:
///     PHOENIX_CLIENT_DIST_STAGING_REPO_ACCESS=github_pat_... bun run tauri build
///
/// Named for the one repo it opens, because it is not a key and grants nothing else: it exists
/// solely so a client can list and download releases from a repo that is not public. It has no
/// part in trust — nothing it fetches is believed on its account, only on the signature over it.
///
/// Deliberately not a source literal — a committed github_pat_ gets blocked/revoked by GitHub
/// secret scanning on push. A user-saved token still wins over this. Merged at the point of
/// use (`Settings::token()`), never into the persisted struct — a settings save must not be
/// able to write it to disk.
const DIST_STAGING_REPO_ACCESS: Option<&str> =
    option_env!("PHOENIX_CLIENT_DIST_STAGING_REPO_ACCESS");

/// One place every payload can be fetched from.
///
/// There is no second KIND of entry: GitHub is a source with no URL, and that ABSENCE is its whole
/// identity — a published list carries URLs, so no value in it can name, replace or remove the
/// built-in source. That asymmetry used to be an enum with two variants and a pile of methods to
/// paper over them (`carries`, `payloads`, `is_primary`, `enabled`); making it the absence of a
/// field is the same guarantee with nothing to keep in step. `migrate` guarantees exactly one
/// urlless entry survives any file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Source {
    /// Base URL, `normalize_mirror_url`'d (no trailing slash). `None` = GitHub. Also the key the
    /// published list is merged and deduplicated on, and the key a measurement survives a refresh
    /// by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// What the published list CALLS this mirror (`phx-ca-1`), and the only thing the UI is allowed
    /// to name it by: a mirror is registered by address, and an address is frequently a bare IP —
    /// which is nobody's business but the launcher's.
    ///
    /// Never identity: two rows are one source when their URLs match, whatever they are called, so
    /// nothing here ranks, dedupes or routes on it. `None` is GitHub — the built-in source is named
    /// by the UI, in the user's language — or a mirror read out of a settings file written before
    /// names were kept, which the next refresh fills in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The last measurement. `None` = never measured, which is what triggers a measuring pass — on
    /// a fresh install GitHub itself is `None`, which is why the first launch measures at all.
    ///
    /// A v1 file spells this as a BOOL; `measured_compat` reads that as `None`. See `migrate`.
    #[serde(
        default,
        deserialize_with = "measured_compat",
        skip_serializing_if = "Option::is_none"
    )]
    pub measured: Option<Measured>,
}

impl Source {
    /// A mirror at `url`, unnamed and never measured — the debug CLI's `--mirror`, which is handed
    /// an address and nothing else.
    pub fn at(url: impl Into<String>) -> Self {
        Self { url: Some(url.into()), name: None, measured: None }
    }

    /// A mirror as the published list registers it: address and name together, never measured.
    pub fn named(url: impl Into<String>, name: impl Into<String>) -> Self {
        Self { url: Some(url.into()), name: Some(name.into()), measured: None }
    }

    /// The built-in GitHub entry.
    pub fn is_github(&self) -> bool {
        self.url.is_none()
    }

    /// Identity, for the runtime sets. `None` is GitHub and no mirror can collide with it — which
    /// is why this is an `Option<&str>` and not a `""` sentinel.
    pub fn key(&self) -> Option<&str> {
        self.url.as_deref()
    }

    /// How a MESSAGE names this source. English, like every other diagnosis the engine builds — the
    /// frontend prefixes it with a localized verdict, exactly as it does for a measurement's reason.
    ///
    /// Never the URL, on any branch. The UI's own labels come from `SourceRowView`, so this is only
    /// for the reasons that ride along inside an error string.
    pub fn label(&self) -> &str {
        match (self.name.as_deref(), self.is_github()) {
            (Some(name), _) => name,
            (None, true) => "the main source",
            (None, false) => "a mirror",
        }
    }
}

/// What one measurement concluded. PERSISTED — a restart must not re-time the world, and the
/// sources report has to have something to show before the first pass finishes.
///
/// This is the old `mirror::Probe` and the old `Source::measured: bool` collapsed into one value.
/// Two representations of one fact is what let the settings pane say "not tested" beside a live
/// measurement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Measured {
    /// Unix seconds. Absolute, not an age: the process that reads it is not the one that wrote it.
    pub at: u64,
    /// Bytes per second over a real 512 KiB transfer. The number worth sorting on. `None` = it
    /// failed the measurement.
    #[serde(default)]
    pub bytes_per_sec: Option<u64>,
    /// Milliseconds to fetch the document that names the release. A tiebreak, never a key — a
    /// latency figure is exactly what a throttled path still passes.
    #[serde(default)]
    pub latency_ms: Option<u64>,
    /// The release the source advertised. A source can be fast, healthy and three releases behind.
    #[serde(default)]
    pub tag: Option<String>,
    /// Answered 206. Without resume a dropped connection restarts a multi-GiB file.
    #[serde(default)]
    pub range_ok: bool,
    /// Why it failed, if it did. English; the UI localizes the verdict and shows this as detail.
    #[serde(default)]
    pub error: Option<String>,
}

impl Measured {
    /// A measurement taken at `at` that failed, and why.
    pub fn failed(at: u64, why: impl Into<String>) -> Self {
        Self { at, error: Some(why.into()), ..Self::blank(at) }
    }

    /// A measurement in progress: nothing concluded yet.
    pub fn blank(at: u64) -> Self {
        Self {
            at,
            bytes_per_sec: None,
            latency_ms: None,
            tag: None,
            range_ok: false,
            error: None,
        }
    }

    /// Delivered the whole chunk, in budget, without faulting. Deliberately strict: "answered" is
    /// not health.
    pub fn healthy(&self) -> bool {
        self.error.is_none() && self.bytes_per_sec.is_some()
    }
}

/// Sort key: usable sources first, then fastest, latency only breaking ties between two that both
/// deliver. Never latency-first — a latency figure is exactly what a throttled path still passes.
///
/// GitHub is ranked by this and nothing else: it is a peer, not a floor. An UNMEASURED source
/// sorts with the unhealthy ones, because it is not a settled answer either.
pub fn rank(m: Option<&Measured>) -> (bool, std::cmp::Reverse<u64>, u64) {
    let healthy = m.is_some_and(Measured::healthy);
    (
        !healthy,
        std::cmp::Reverse(m.and_then(|m| m.bytes_per_sec).unwrap_or(0)),
        m.and_then(|m| m.latency_ms).unwrap_or(u64::MAX),
    )
}

/// v1 persisted `measured` as a BOOL ("has this ever been timed"). Read as "never measured", both
/// for `true` and for `false`, and that is the safe direction — the next launch measures.
///
/// Without this the WHOLE settings file fails to parse, because a type mismatch is not an unknown
/// field: `Settings::load` would copy it to `.bak` and return defaults, and the file it just gave
/// up on is the one carrying `max_serial_seen["mirrors"]` — the anti-rollback high-water mark,
/// gone with no signal anywhere.
#[derive(Deserialize)]
#[serde(untagged)]
enum MeasuredCompat {
    New(Measured),
    /// The v1 spelling. Its VALUE is deliberately never read — `true` meant "has been timed at
    /// some point", which says nothing this model can rank on — but the variant still has to exist
    /// so a bool PARSES instead of failing the whole document.
    Legacy(#[allow(dead_code)] bool),
}

fn measured_compat<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Measured>, D::Error> {
    Ok(match Option::<MeasuredCompat>::deserialize(d)? {
        Some(MeasuredCompat::New(m)) => Some(m),
        _ => None,
    })
}

/// Canonical form of a published mirror base URL, or None if it is not one. Everything downstream
/// appends a path to this string, so a trailing slash and a missing scheme are the two ways an
/// entry that looks fine silently never resolves.
///
/// The two schemes accepted here are the same two `transport::Schemes::HttpOrHttps` allows, and
/// that is not a coincidence to be tidied away on either side: a base URL this admits but the
/// transport refuses is a mirror that is published, ranked, and unreachable — with the refusal
/// arriving per request, as a source failure, rather than where the list was read.
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
    /// `owner/name` of the repo publishing the signed mirror list. None = `DEFAULT_MIRRORS_REPO`
    /// (same Option rationale as `launcher_repo`).
    #[serde(default)]
    pub mirrors_repo: Option<String>,
    /// Folder that CONTAINS `game/`. None = resolve to the updater exe's own directory.
    #[serde(default)]
    pub game_dir: Option<PathBuf>,
    // NO `token` field, deliberately. The launcher authenticates with the credential baked in at
    // build time and nothing else — see `token()`. A key left in an existing settings.json is
    // ignored by serde and disappears on the next save.
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
    /// Download sources, RANKED — fastest working first. Always holds exactly one urlless
    /// (GitHub) entry, enforced by `migrate`. Mirrors are DISCOVERED, never user-authored: a
    /// refresh replaces them wholesale from the published `mirrors.json`, so nothing about a
    /// mirror is a preference.
    ///
    /// The order is a measurement result, not a setting. There is no knob for it, and there is no
    /// pin either: a slower source ahead of a faster one is not a preference anyone holds, and a
    /// pin is a choice a user has no information to make and would then be stuck with when the
    /// host it names goes dark. `source::Registry` walks this list in this order.
    #[serde(default = "default_sources")]
    pub sources: Vec<Source>,
    /// The highest signed `serial` accepted for each payload id, ever. The rollback ratchet: a
    /// mirror can always serve an older release it once held a valid signature for, and nothing
    /// else in a signed document says it is not the current one.
    ///
    /// Plaintext in the user's profile, and deliberately the ONLY floor. A build-time backstop
    /// used to sit under it; it was removed because anything able to edit this file can replace
    /// the launcher outright, which is strictly more powerful — see the note in `trust.rs`.
    #[serde(default)]
    pub max_serial_seen: BTreeMap<String, u64>,
}

fn default_sources() -> Vec<Source> {
    vec![Source::default()]
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
            mirrors_repo: None,
            game_dir: None,
            language: None,
            launch_extra: String::new(),
            renderer: default_renderer(),
            animations: true,
            launch_flags: BTreeMap::new(),
            selections: BTreeMap::new(),
            sources: default_sources(),
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

    /// Bring an older on-disk schema up to SETTINGS_VERSION.
    ///
    /// v1 -> v2 needs no field transformation, and that is by construction rather than by luck:
    /// `{"kind":"primary"}` reads as `Source { url: None }`, which is correct; a v1 mirror's
    /// `kind`/`enabled`/`payloads` are unknown fields and serde drops them (a v1-DISABLED mirror
    /// comes back enabled, which is also correct — there is no such concept any more); `selected`
    /// and `auto_pick_best` retire themselves on the next save. The one field that is a serde TYPE
    /// error rather than an unknown one is `measured`, and `measured_compat` covers it — read its
    /// doc for what a failure there would silently cost.
    fn migrate(&mut self) {
        self.version = SETTINGS_VERSION;
        // Exactly one urlless entry, always, restored at the FRONT — a file written before
        // `sources` existed, or hand-edited to drop it, must not leave the launcher with no
        // built-in source. That is the state a published list is never allowed to produce, so it
        // must not be reachable by accident either. The front, because a list with no measurements
        // has no better order to claim.
        if self.sources.iter().filter(|s| s.is_github()).count() != 1 {
            self.sources.retain(|s| !s.is_github());
            self.sources.insert(0, Source::default());
        }
        // …and no duplicate mirrors by URL: identity IS the URL, and two rows for one host would
        // rank, fail over and be measured independently.
        let mut seen = HashSet::new();
        self.sources.retain(|s| seen.insert(s.url.clone()));
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

    /// The token to authenticate with: ALWAYS the build-time baked credential.
    ///
    /// There is deliberately no stored alternative. A saved token used to outrank this one, while
    /// the only UI that could set or clear it went unrendered (frontend/main.js, SHOW_ADVANCED) —
    /// so a value left behind by an older build won forever, unreachable and unfixable: every call
    /// 401'd, and neither rebuilding the launcher nor rotating the credential helped, because the
    /// baked one was never consulted. A credential the user cannot see, change or remove must not
    /// be able to outvote the one compiled in.
    pub fn token(&self) -> Option<&str> {
        DIST_STAGING_REPO_ACCESS
    }

    /// The repo the launcher self-updates from. No token FIELD of its own: this repo is meant to
    /// be public and anonymous GitHub allows 60 requests/hour per IP, which is plenty for one
    /// check per launch.
    ///
    /// It is not, however, "never authenticated" — `Github::for_repo` tries anonymously and
    /// retries with `Settings::token()` (the dist PAT) if and only if the anonymous attempt was
    /// REFUSED by the server. That is what keeps self-update working while this repo is still
    /// private. The header only ever reaches api.github.com and is stripped on redirect, so the
    /// retry costs nothing but a possible 403 where anonymous would have worked — which is why it
    /// is a retry and not the first attempt.
    pub fn launcher_repo(&self) -> &str {
        self.launcher_repo.as_deref().unwrap_or(DEFAULT_LAUNCHER_REPO)
    }

    /// The base-game distribution repo (fresh install / verify / repair source).
    pub fn game_repo(&self) -> &str {
        self.game_repo.as_deref().unwrap_or(DEFAULT_GAME_REPO)
    }

    /// The repo the signed mirror list is published from. Public, so `mirror::fetch_list_from`
    /// reads it through `Github::for_repo` — anonymous first, the baked credential only if the
    /// anonymous attempt was REFUSED. That credential is scoped to the dist repo and a fine-grained
    /// PAT can be turned away where anonymous access would have worked, which on this repo would
    /// cost every client its mirror list for nothing.
    ///
    /// It names no PAYLOAD DIRECTORY, and must not be given one: a mirror serves the list at its
    /// own root (`<base>/mirrors.json`), not under `<base>/mirrors/`, because the list describes
    /// the HOSTS rather than any payload's content.
    pub fn mirrors_repo(&self) -> &str {
        self.mirrors_repo.as_deref().unwrap_or(DEFAULT_MIRRORS_REPO)
    }

    /// The lowest `serial` a signed manifest for `payload` may carry: this machine's own
    /// high-water mark. Zero on a fresh install, which is correct — there is nothing yet to be
    /// rolled back FROM, and the first thing installed is whatever the source offers.
    pub fn serial_floor(&self, payload: crate::trust::Payload) -> u64 {
        self.max_serial_seen.get(payload.id()).copied().unwrap_or(0)
    }

    /// Is this serial past what we have recorded for `payload`? The read half of the ratchet, so a
    /// caller holding a settings snapshot can decide whether a WRITE is needed at all —
    /// `Settings::update` always saves, and the common case is the same release checked again.
    pub fn serial_is_newer(&self, payload: crate::trust::Payload, serial: u64) -> bool {
        serial > self.serial_floor(payload)
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
    use crate::trust::Payload;

    /// A mirror entry with a HEALTHY measurement of the given age.
    fn measured(url: &str, at: u64) -> Source {
        Source {
            url: Some(url.to_string()),
            name: None,
            measured: Some(Measured { bytes_per_sec: Some(1), ..Measured::blank(at) }),
        }
    }

    /// THE loss that would be silent and permanent.
    ///
    /// A v1 file spells `measured` as a bool, which is a serde TYPE error rather than an unknown
    /// field, so the whole document fails to parse — and `Settings::load` answers a parse failure
    /// by copying the file to `.bak` and returning defaults. The file it gives up on is the one
    /// carrying `max_serial_seen["mirrors"]`: the anti-rollback high-water mark, gone, with the
    /// only symptom being a rollback nobody notices.
    #[test]
    fn a_v1_settings_file_upgrades_without_losing_the_mirror_serial_floor() {
        // exactly what v1 wrote: kind-tagged sources, a bool `measured`, `payloads`, and the two
        // settings that no longer exist
        let v1 = r#"{
          "version": 1,
          "sources": [
            {"kind": "primary", "measured": true},
            {"kind": "mirror", "url": "https://fi1.example", "enabled": false,
             "measured": true, "payloads": ["mod", "game"]}
          ],
          "selected": {"kind": "mirror", "url": "https://fi1.example"},
          "auto_pick_best": false,
          "max_serial_seen": {"mirrors": 7, "mod": 3}
        }"#;
        let mut s: Settings = serde_json::from_str(v1).expect("a v1 file must still parse");
        s.migrate();

        assert_eq!(s.serial_floor(Payload::Mirrors), 7, "the floor is the whole point");
        assert_eq!(s.serial_floor(Payload::Mod), 3);
        assert_eq!(s.version, SETTINGS_VERSION);
        assert_eq!(
            s.sources,
            vec![Source::default(), Source::at("https://fi1.example")],
            "both survive, unmeasured — a v1 bool says nothing this model can use, and a v1 \
             DISABLED mirror comes back usable because there is no such concept any more"
        );

        // and the dead keys retire themselves rather than accumulating on disk
        let round: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert!(round.get("selected").is_none());
        assert!(round.get("auto_pick_best").is_none());
        assert!(round["sources"][0].get("kind").is_none());
        assert!(round["sources"][1].get("payloads").is_none());
        assert!(round["sources"][1].get("measured").is_none(), "None is absent, not null");
    }

    /// NO file and no published list can leave the launcher without the built-in source, and none
    /// can give it two of anything. GitHub's identity is the ABSENCE of a URL, so "exactly one
    /// urlless entry" is the whole invariant — and a duplicate mirror would rank, fail over and be
    /// measured twice under one name.
    #[test]
    fn exactly_one_github_entry_survives_any_file() {
        let github_first = |s: &Settings| {
            assert_eq!(s.sources.iter().filter(|x| x.is_github()).count(), 1);
            assert!(s.sources[0].is_github(), "restored at the front: {:?}", s.sources);
        };

        // missing entirely (hand-edited, or written before `sources` existed)
        let mut s = Settings { sources: vec![Source::at("https://a")], ..Settings::default() };
        s.migrate();
        github_first(&s);
        assert_eq!(s.sources.len(), 2);

        // doubled — and the measurement on either copy is discarded with it, since neither copy
        // can be said to be the one that was measured
        let mut s = Settings {
            sources: vec![Source::default(), measured("https://a", 100), Source::default()],
            ..Settings::default()
        };
        s.migrate();
        github_first(&s);
        assert_eq!(s.sources.len(), 2);

        // the same mirror twice: identity is the URL, so the first one wins and keeps its rank
        let mut s = Settings {
            sources: vec![Source::default(), measured("https://a", 100), Source::at("https://a")],
            ..Settings::default()
        };
        s.migrate();
        github_first(&s);
        assert_eq!(s.sources, vec![Source::default(), measured("https://a", 100)]);
    }

    /// GitHub is ranked by the measurement and nothing else — it is a peer, not a floor — and an
    /// UNMEASURED source sorts with the unhealthy ones, because it is not a settled answer either.
    #[test]
    fn rank_puts_the_fastest_working_source_first() {
        let fast = Measured { bytes_per_sec: Some(5_000_000), ..Measured::blank(0) };
        let slow = Measured { bytes_per_sec: Some(1_000_000), ..Measured::blank(0) };
        let dead = Measured::failed(0, "down");
        assert!(rank(Some(&fast)) < rank(Some(&slow)));
        assert!(rank(Some(&slow)) < rank(None), "unmeasured is not an answer");
        assert!(rank(Some(&slow)) < rank(Some(&dead)));
        assert_eq!(
            rank(None),
            rank(Some(&dead)),
            "never measured and measured-and-failed are the same non-answer to a SORT; what \
             tells them apart is whether a pass is due, which is `source`'s question, not \
             this one's"
        );

        // latency only breaks a tie between two that both deliver
        let (mut a, mut b) = (fast.clone(), fast.clone());
        a.latency_ms = Some(10);
        b.latency_ms = Some(400);
        assert!(rank(Some(&a)) < rank(Some(&b)));
    }

    /// The rollback ratchet: forward only, per payload, and durable. It is the WHOLE floor — there
    /// is no build-time backstop under it any more (see the note in `trust.rs`).
    #[test]
    fn the_serial_ratchet_only_ever_moves_forward() {
        let mut s = Settings::default();
        assert_eq!(s.serial_floor(Payload::Mod), 0, "no history: nothing to roll back from");

        assert!(s.serial_is_newer(Payload::Mod, 5));
        assert!(s.advance_serial(Payload::Mod, 5));
        assert_eq!(s.serial_floor(Payload::Mod), 5);
        assert!(!s.serial_is_newer(Payload::Mod, 5), "so no settings write is needed for it");
        assert!(!s.advance_serial(Payload::Mod, 5), "the same release again is not news");
        assert!(!s.advance_serial(Payload::Mod, 4), "and it never walks back");
        assert_eq!(s.max_serial_seen["mod"], 5);
        assert_eq!(
            s.serial_floor(Payload::Game),
            0,
            "one payload's history says nothing about another's"
        );

        // it has to survive the round trip — an in-memory ratchet protects nothing
        let saved: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(saved.max_serial_seen["mod"], 5);
        // and a file written before the field existed simply has no history
        let old: Settings = serde_json::from_str(r#"{"version":1}"#).unwrap();
        assert!(old.max_serial_seen.is_empty());
    }

    /// A token in an existing settings.json must not authenticate anything. It once outranked the
    /// baked credential while the UI that could clear it went unrendered, so a stale value 401'd
    /// forever and no rebuild or rotation could reach it. The field is gone: the key parses
    /// harmlessly (serde ignores what it does not know) and `token()` is the baked one, always.
    #[test]
    fn a_token_left_in_settings_json_is_ignored() {
        let s: Settings =
            serde_json::from_str(r#"{"version":1,"token":"ghp_stale_and_revoked"}"#).unwrap();
        assert_eq!(
            s.token(),
            DIST_STAGING_REPO_ACCESS,
            "a persisted token must never outvote the baked credential"
        );
        // and it does not survive a save, so the dead key retires itself
        let round: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert!(round.get("token").is_none(), "no token may be written back to disk");
    }
}
