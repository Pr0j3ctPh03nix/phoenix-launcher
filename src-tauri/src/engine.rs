//! Shared update logic: fetch the release + manifest, resolve the effective file set from the
//! user's option selections, and diff it against what is installed. `check` is the read-only
//! surface over this; `install` (in install.rs) reuses `fetch`, `resolve` and `plan`.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::config::Settings;
use crate::downloader::{Downloader, Release};
use crate::manifest::{FileEntry, Manifest, OptionEntry, OptionKind};
use crate::state::InstalledState;
use crate::verify;

// ---- long-operation progress (the shell bridges these to UI events) ----

/// One progress tick of a long engine operation. Serializable: the shell forwards it to the UI
/// as-is (the `op-progress` event).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpProgress {
    /// Which operation, e.g. "install".
    pub op: &'static str,
    /// Current item number, 1-based (item `current` of `total` is in progress).
    pub current: u64,
    /// Items total.
    pub total: u64,
    /// The item currently being worked (e.g. a dest path).
    pub item: Option<String>,
    /// Bytes of the current item done / total, when it's a download.
    pub bytes_done: Option<u64>,
    pub bytes_total: Option<u64>,
    /// True on the tick that finishes `item` (its bar is now complete). Downloads run in
    /// parallel, so ticks for different items interleave — the UI keys per-file state on `item`
    /// and uses this to settle a bar rather than inferring completion from `current`.
    pub done: bool,
}

/// Optional progress sink; `None` = headless (CLI, tests). Must be Send + Sync: phase-1
/// downloads run on a small worker pool and report from multiple threads at once (install
/// serializes the actual calls internally).
pub type Progress<'a> = Option<&'a (dyn Fn(OpProgress) + Send + Sync)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Installed file already matches the manifest hash.
    UpToDate,
    /// Installed but the hash differs.
    Update,
    /// Not present locally.
    Install,
    /// We placed it previously but it left the effective set (deselected option) — delete it.
    Remove,
}

#[derive(Debug)]
pub struct FileStatus {
    pub dest: String,
    pub action: Action,
}

#[derive(Debug)]
pub struct CheckResult {
    pub tag: String,
    pub version: String,
    pub game_dir: PathBuf,
    pub files: Vec<FileStatus>,
    /// Markdown "What's new" for this release, if the manifest carries it.
    pub notes: Option<String>,
    /// The manifest's user-selectable options, for the customization UI.
    pub options: Vec<OptionEntry>,
    /// Effective selection per option id (the user's valid choice, else the manifest default).
    pub selections: BTreeMap<String, serde_json::Value>,
}

impl CheckResult {
    /// Number of files that would change (written or removed).
    pub fn changes(&self) -> usize {
        self.files.iter().filter(|f| f.action != Action::UpToDate).count()
    }

}

/// The manifest requires a newer launcher than this build. Rooted in the error chain so the
/// shell can put a "tooOld" kind on the wire (the UI then tells the user to update the launcher).
#[derive(Debug, Clone)]
pub struct TooOld {
    pub required: String,
    pub current: String,
}

impl std::fmt::Display for TooOld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "this release needs launcher {} or newer (you have {}) — update the launcher first",
            self.required, self.current
        )
    }
}

impl std::error::Error for TooOld {}

/// A file the install must replace or delete is locked by a live process — the game keeps its
/// loaded DLLs and mmapped VPKs open. Rooted in the error chain so the shell puts a
/// "gameRunning" kind on the wire (the UI tells the user to close the game and retry).
#[derive(Debug, Clone)]
pub struct GameRunning(pub PathBuf);

impl std::fmt::Display for GameRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} is in use — close Dota 2 and try again", self.0.display())
    }
}

impl std::error::Error for GameRunning {}

/// Lenient dotted-numeric compare: is version `a` older than `b`? ("1.10.0" > "1.9.9"; a leading
/// "v" and missing segments are tolerated, unparsable pieces count as 0).
fn version_lt(a: &str, b: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        v.trim_start_matches('v').split('.').map(|s| s.parse().unwrap_or(0)).collect()
    }
    let (a, b) = (parts(a), parts(b));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x < y;
        }
    }
    false
}

/// Fetch the release and its manifest.json. Returned together so an install can reuse the release's
/// asset list (for private downloads) without a second round trip. Fails with `TooOld` when the
/// manifest's `min_launcher` is newer than this build — a stale launcher must refuse a manifest
/// it doesn't understand rather than misinstall it.
pub fn fetch(settings: &Settings, dl: &dyn Downloader, tag: Option<&str>) -> Result<(Release, Manifest)> {
    let release = dl
        .fetch_release(&settings.source_repo, tag)
        .context("fetching the release")?;
    let manifest_asset = release
        .asset("manifest.json")
        .context("the release has no manifest.json asset")?;
    let bytes = dl.download(manifest_asset).context("downloading manifest.json")?;
    let manifest: Manifest = serde_json::from_slice(&bytes).context("parsing manifest.json")?;
    if let Some(min) = &manifest.min_launcher {
        let current = env!("CARGO_PKG_VERSION").to_string();
        if version_lt(&current, min) {
            return Err(anyhow!(TooOld { required: min.clone(), current }));
        }
    }
    Ok((release, manifest))
}

/// One release's "What's new" entry, for the version-history view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotesEntry {
    pub tag: String,
    pub version: String,
    pub notes: String,
}

/// The notes history plus its freshness key. Persisted to disk (next to settings.json) so
/// "What's new" opens instantly across app restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotesCache {
    pub repo: String,
    /// The repo's latest release tag when this history was built. The freshness key — the first
    /// entry's tag can't serve, since the latest release may carry no notes.
    pub latest_tag: String,
    pub entries: Vec<NotesEntry>,
}

fn notes_cache_path() -> Option<PathBuf> {
    Settings::config_path().map(|p| p.with_file_name("notes_cache.json"))
}

impl NotesCache {
    /// Best-effort disk load; None on any miss or parse failure.
    pub fn load() -> Option<Self> {
        let text = std::fs::read_to_string(notes_cache_path()?).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Best-effort disk save; a failure only costs a refetch next launch.
    pub fn save(&self) {
        let Some(p) = notes_cache_path() else { return };
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string(self) {
            let _ = std::fs::write(p, json);
        }
    }
}

/// The full "What's new" history: every release's manifest notes, newest first (GitHub's release
/// order). Incremental: a release whose tag appears in `known` keeps its cached entry with no
/// manifest download — only unseen tags cost a round trip. (Releases whose manifest carried no
/// notes are not in `known` and re-download on each rebuild; rebuilds only happen on a new
/// release, so that stays cheap.) Releases without a manifest.json, with an unparsable manifest,
/// or with empty notes are skipped — a single bad release must not sink the whole history.
pub fn fetch_notes_history(
    settings: &Settings,
    dl: &dyn Downloader,
    known: &[NotesEntry],
) -> Result<NotesCache> {
    let releases = dl.fetch_releases(&settings.source_repo).context("listing releases")?;
    let by_tag: BTreeMap<&str, &NotesEntry> =
        known.iter().map(|e| (e.tag.as_str(), e)).collect();
    let mut entries = Vec::new();
    for rel in &releases {
        if let Some(e) = by_tag.get(rel.tag_name.as_str()) {
            entries.push((*e).clone());
            continue;
        }
        let Some(asset) = rel.asset("manifest.json") else { continue };
        let Ok(bytes) = dl.download(asset) else { continue };
        let Ok(m) = serde_json::from_slice::<Manifest>(&bytes) else { continue };
        if let Some(notes) = m.notes.filter(|n| !n.trim().is_empty()) {
            entries.push(NotesEntry { tag: rel.tag_name.clone(), version: m.version, notes });
        }
    }
    Ok(NotesCache {
        repo: settings.source_repo.clone(),
        latest_tag: releases.first().map(|r| r.tag_name.clone()).unwrap_or_default(),
        entries,
    })
}

/// The effective selection for one option: the user's value if it is valid for this manifest,
/// else the manifest default.
fn effective_selection(
    opt: &OptionEntry,
    selections: &BTreeMap<String, serde_json::Value>,
) -> serde_json::Value {
    let user = selections.get(&opt.id);
    match opt.kind {
        OptionKind::Choice => user
            .and_then(|v| v.as_str())
            .filter(|id| opt.variants.iter().any(|v| v.id == *id))
            .map(|id| serde_json::Value::String(id.to_string()))
            .unwrap_or_else(|| opt.default.clone()),
        OptionKind::Toggle => user
            .and_then(|v| v.as_bool())
            .map(serde_json::Value::Bool)
            .unwrap_or_else(|| opt.default.clone()),
    }
}

/// Effective selections for every option in the manifest (unknown ids in `selections` ignored).
pub fn effective_selections(
    manifest: &Manifest,
    selections: &BTreeMap<String, serde_json::Value>,
) -> BTreeMap<String, serde_json::Value> {
    manifest
        .options
        .iter()
        .map(|o| (o.id.clone(), effective_selection(o, selections)))
        .collect()
}

/// Materialize the effective file set: core files + the selected variant of each choice + the
/// files of each enabled toggle.
pub fn resolve(
    manifest: &Manifest,
    selections: &BTreeMap<String, serde_json::Value>,
) -> Vec<FileEntry> {
    let mut out = manifest.files.clone();
    for opt in &manifest.options {
        let sel = effective_selection(opt, selections);
        match opt.kind {
            OptionKind::Choice => {
                let Some(dest) = &opt.dest else { continue };
                let Some(id) = sel.as_str() else { continue };
                if let Some(var) = opt.variants.iter().find(|v| v.id == id) {
                    out.push(FileEntry {
                        name: var.name.clone(),
                        dest: dest.clone(),
                        sha256: var.sha256.clone(),
                        size: var.size,
                    });
                }
            }
            OptionKind::Toggle => {
                if sel.as_bool().unwrap_or(false) {
                    out.extend(opt.files.iter().cloned());
                }
            }
        }
    }
    out
}

/// Diff the resolved file set against what is installed under `game_dir`. `Action::Remove` rows
/// cover both orphans (files the previous install placed that left the effective set) and the
/// manifest's `remove[]` entries still present on disk — so the check view and the install agree
/// on what changes.
pub fn plan(
    game_dir: &Path,
    resolved: &[FileEntry],
    prev: Option<&InstalledState>,
    remove: &[crate::manifest::RemoveEntry],
) -> Vec<FileStatus> {
    let mut out: Vec<FileStatus> = resolved
        .iter()
        .map(|f| {
            let local = game_dir.join(&f.dest);
            let action = if !local.exists() {
                Action::Install
            } else {
                match verify::sha256_file_cached(&local) {
                    Ok(h) if h == f.sha256 => Action::UpToDate,
                    _ => Action::Update,
                }
            };
            FileStatus { dest: f.dest.clone(), action }
        })
        .collect();

    let managed: HashSet<&str> = resolved.iter().map(|f| f.dest.as_str()).collect();
    let mut removed: HashSet<&str> = HashSet::new();
    if let Some(prev) = prev {
        for f in &prev.files {
            if !managed.contains(f.dest.as_str())
                && removed.insert(f.dest.as_str())
                && game_dir.join(&f.dest).exists()
            {
                out.push(FileStatus { dest: f.dest.clone(), action: Action::Remove });
            }
        }
    }
    for r in remove {
        if !managed.contains(r.dest.as_str())
            && removed.insert(r.dest.as_str())
            && game_dir.join(&r.dest).exists()
        {
            out.push(FileStatus { dest: r.dest.clone(), action: Action::Remove });
        }
    }
    out
}

/// Evaluate a manifest against the local install without any network I/O — the shared core of
/// `check` and the cached `replan`. Writes nothing.
pub fn evaluate(settings: &Settings, tag_name: &str, manifest: &Manifest) -> Result<CheckResult> {
    let game_dir = settings.resolve_game_dir()?;
    let resolved = resolve(manifest, &settings.selections);
    let prev = InstalledState::load(&game_dir);
    let files = plan(&game_dir, &resolved, prev.as_ref(), &manifest.remove);

    Ok(CheckResult {
        tag: tag_name.to_string(),
        version: manifest.version.clone(),
        game_dir,
        files,
        notes: manifest.notes.clone(),
        options: manifest.options.clone(),
        selections: effective_selections(manifest, &settings.selections),
    })
}

/// Read-only check: fetch the manifest and evaluate it.
pub fn check(settings: &Settings, dl: &dyn Downloader, tag: Option<&str>) -> Result<CheckResult> {
    let (release, manifest) = fetch(settings, dl, tag)?;
    evaluate(settings, &release.tag_name, &manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::downloader::fake::Fake;
    use crate::state::InstalledFile;

    #[test]
    fn version_lt_compares_numerically() {
        assert!(version_lt("1.0.1", "1.0.2"));
        assert!(version_lt("1.9.9", "1.10.0"));
        assert!(version_lt("0.9", "1.0"));
        assert!(!version_lt("1.0.2", "1.0.1"));
        assert!(!version_lt("1.0.1", "1.0.1"));
        assert!(!version_lt("v1.2", "1.2.0")); // equal, not less
    }

    #[test]
    fn fetch_refuses_a_too_new_manifest() {
        let settings = Settings::default();
        let too_new = Fake::new(
            "v9.9.9",
            r#"{ "version": "9.9.9", "min_launcher": "999.0.0", "files": [] }"#,
            vec![],
        );
        let err = fetch(&settings, &too_new, None).unwrap_err();
        let too_old = err.chain().find_map(|c| c.downcast_ref::<TooOld>());
        assert_eq!(too_old.unwrap().required, "999.0.0");

        // and the same manifest WITHOUT the gate installs fine
        let ok = Fake::new("v1.0.0", r#"{ "version": "1.0.0", "files": [] }"#, vec![]);
        assert!(fetch(&settings, &ok, None).is_ok());
    }

    fn manifest() -> Manifest {
        serde_json::from_value(serde_json::json!({
            "version": "1.0.0", "tag": "v1.0.0",
            "requires_install": { "steam_inf": { "ClientVersion": "1805" } },
            "files": [
                { "name": "winmm.dll", "dest": "game/bin/win64/winmm.dll",
                  "sha256": "aa", "size": 1, "url": "u" }
            ],
            "options": [
                { "id": "hud", "kind": "choice",
                  "label": { "en": "HUD", "ru": "Худ" }, "default": "classic",
                  "dest": "game/dota/hud.vpk",
                  "variants": [
                    { "id": "classic", "label": "Classic", "name": "hud_classic.vpk",
                      "sha256": "bb", "size": 2, "url": "u" },
                    { "id": "modern", "label": "Modern", "name": "hud_modern.vpk",
                      "sha256": "cc", "size": 3, "url": "u" }
                  ] },
                { "id": "fx", "kind": "toggle", "label": "FX", "default": false,
                  "files": [
                    { "name": "fx.vpk", "dest": "game/dota/fx.vpk",
                      "sha256": "dd", "size": 4, "url": "u" }
                  ] }
            ]
        }))
        .unwrap()
    }

    #[test]
    fn resolve_defaults() {
        let m = manifest();
        let r = resolve(&m, &BTreeMap::new());
        // core + default choice variant; toggle defaults off
        let dests: Vec<&str> = r.iter().map(|f| f.dest.as_str()).collect();
        assert_eq!(dests, ["game/bin/win64/winmm.dll", "game/dota/hud.vpk"]);
        assert_eq!(r[1].sha256, "bb");
    }

    #[test]
    fn resolve_selections_and_invalid_fallback() {
        let m = manifest();
        let mut sel = BTreeMap::new();
        sel.insert("hud".into(), serde_json::json!("modern"));
        sel.insert("fx".into(), serde_json::json!(true));
        let r = resolve(&m, &sel);
        assert_eq!(r.iter().find(|f| f.dest == "game/dota/hud.vpk").unwrap().sha256, "cc");
        assert!(r.iter().any(|f| f.dest == "game/dota/fx.vpk"));
        // invalid variant id falls back to the default
        sel.insert("hud".into(), serde_json::json!("nonsense"));
        let r = resolve(&m, &sel);
        assert_eq!(r.iter().find(|f| f.dest == "game/dota/hud.vpk").unwrap().sha256, "bb");
    }

    #[test]
    fn plan_flags_orphans() {
        let dir = std::env::temp_dir().join("phoenix-engine-test-orphan");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("game/dota")).unwrap();
        std::fs::write(dir.join("game/dota/fx.vpk"), b"x").unwrap();

        let m = manifest();
        let resolved = resolve(&m, &BTreeMap::new()); // fx off
        let prev = InstalledState {
            version: "0.9".into(),
            files: vec![InstalledFile { dest: "game/dota/fx.vpk".into(), sha256: "dd".into() }],
            winmm_orig_created: false,
        };
        let statuses = plan(&dir, &resolved, Some(&prev), &[]);
        let orphan = statuses.iter().find(|s| s.dest == "game/dota/fx.vpk").unwrap();
        assert_eq!(orphan.action, Action::Remove);
        // and the others are Install (nothing on disk)
        assert!(statuses.iter().filter(|s| s.dest != "game/dota/fx.vpk").all(|s| s.action == Action::Install));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
