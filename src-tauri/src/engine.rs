//! Shared update logic: fetch the release + manifest, resolve the effective file set from the
//! user's option selections, and diff it against what is installed. `check` is the read-only
//! surface over this; `install` (in install.rs) reuses `fetch`, `resolve` and `plan`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::config::Settings;
use crate::downloader::{Downloader, Release};
use crate::manifest::{FileEntry, Manifest, OptionEntry, OptionKind, UnsupportedSchema};
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
///
/// Used by `selfupdate` to compare this build against the launcher repo's release tags. Note it
/// has nothing to do with manifest compatibility — that is `manifest::schema` alone.
pub(crate) fn version_lt(a: &str, b: &str) -> bool {
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
/// asset list (for private downloads) without a second round trip. Fails with `UnsupportedSchema`
/// when the manifest declares a format this build cannot read — a stale launcher must refuse a
/// manifest it doesn't understand rather than misinstall it.
pub fn fetch(settings: &Settings, dl: &dyn Downloader, tag: Option<&str>) -> Result<(Release, Manifest)> {
    let release = dl
        .fetch_release(&settings.source_repo, tag)
        .context("fetching the release")?;
    let manifest = manifest_of(dl, &release)?;
    Ok((release, manifest))
}

/// The manifest.json of an already-fetched release — for callers that resolved the release
/// themselves (the base-game commands probe repo credentials first and hold a `Release` by the
/// time they need the manifest). Same schema gate as `fetch`.
pub fn manifest_of(dl: &dyn Downloader, release: &Release) -> Result<Manifest> {
    let manifest_asset = release
        .asset("manifest.json")
        .context("the release has no manifest.json asset")?;
    let bytes = dl.download(manifest_asset).context("downloading manifest.json")?;
    // Manifest::parse owns the compatibility gate: it reads `schema` before deserializing, so a
    // manifest from the future fails as "update the launcher", never as a syntax error
    Manifest::parse(&bytes)
}

/// Merge every release of the game repo's assets into the manifest release.
///
/// GitHub caps a release at 1,000 assets and the base-game tree is ~4.6k files, so game-dist
/// SHARDS them: the versioned release (always the repo's latest — shards are prereleases, which
/// `/releases/latest` never resolves to) carries manifest.json, and `<tag>-assets-N` prereleases
/// carry the files. Folding every shard's assets into the main `Release` keeps the entire
/// download machinery on its single-release worldview — nothing downstream knows shards exist.
/// First name wins on a clash (the manifest release outranks shards); an unsharded repo merges
/// to itself.
pub fn merged_game_release(dl: &dyn Downloader, repo: &str, mut main: Release) -> Result<Release> {
    let all = dl.fetch_releases(repo).context("listing the game repo's asset shards")?;
    let mut have: HashSet<String> = main.assets.iter().map(|a| a.name.clone()).collect();
    for r in all {
        if r.tag_name == main.tag_name {
            continue;
        }
        for a in r.assets {
            if have.insert(a.name.clone()) {
                main.assets.push(a);
            }
        }
    }
    Ok(main)
}

/// The user cancelled a long operation (the base-game download). Rooted in the error chain so the
/// shell can put a `cancelled` kind on the wire — the UI closes quietly instead of painting an
/// error for something the user asked for.
#[derive(Debug, Clone, Copy)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cancelled")
    }
}

impl std::error::Error for Cancelled {}

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

/// How many release manifests the notes-history rebuild downloads at once. A first-ever open
/// walks the whole release list — serial round trips would put N×RTT behind one spinner.
const NOTES_WORKERS: usize = 4;

/// The full "What's new" history: every release's manifest notes, newest first (GitHub's release
/// order). Incremental: a release whose tag appears in `known` keeps its cached entry with no
/// manifest download — only unseen tags cost a round trip, and those download in parallel
/// (NOTES_WORKERS). (Releases whose manifest carried no notes are not in `known` and re-download
/// on each rebuild; rebuilds only happen on a new release, so that stays cheap.) Releases
/// without a manifest.json, with an unparsable manifest, or with empty notes are skipped — a
/// single bad release must not sink the whole history.
pub fn fetch_notes_history(
    settings: &Settings,
    dl: &dyn Downloader,
    known: &[NotesEntry],
) -> Result<NotesCache> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    let releases = dl.fetch_releases(&settings.source_repo).context("listing releases")?;
    let by_tag: BTreeMap<&str, &NotesEntry> =
        known.iter().map(|e| (e.tag.as_str(), e)).collect();
    // one slot per release keeps GitHub's newest-first order regardless of download timing
    let mut slots: Vec<Option<NotesEntry>> = releases
        .iter()
        .map(|r| by_tag.get(r.tag_name.as_str()).map(|e| (*e).clone()))
        .collect();
    let jobs: Vec<usize> =
        slots.iter().enumerate().filter_map(|(i, s)| s.is_none().then_some(i)).collect();
    let next = AtomicUsize::new(0);
    let fetched: Mutex<Vec<(usize, NotesEntry)>> = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..NOTES_WORKERS.min(jobs.len()) {
            s.spawn(|| loop {
                let j = next.fetch_add(1, Ordering::Relaxed);
                if j >= jobs.len() {
                    return;
                }
                let (i, rel) = (jobs[j], &releases[jobs[j]]);
                let Some(asset) = rel.asset("manifest.json") else { continue };
                let Ok(bytes) = dl.download(asset) else { continue };
                // A garbage manifest is skipped (not fatal — one bad release must not sink the
                // whole history). A FUTURE-schema one is different: the full parse is off the
                // table, but `version`/`notes` are additive-stable strings, and its notes are
                // the ones most worth showing — they're where "update the launcher" gets
                // explained. Read just those two permissively instead of leaving a hole in the
                // history exactly there.
                let (version, notes) = match Manifest::parse(&bytes) {
                    Ok(m) => (m.version, m.notes),
                    Err(e) if e.chain().any(|c| c.downcast_ref::<UnsupportedSchema>().is_some()) => {
                        let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                            continue;
                        };
                        (
                            doc.get("version")
                                .and_then(|v| v.as_str())
                                .unwrap_or(rel.tag_name.trim_start_matches('v'))
                                .to_string(),
                            doc.get("notes").and_then(|v| v.as_str()).map(str::to_string),
                        )
                    }
                    Err(_) => continue,
                };
                if let Some(notes) = notes.filter(|n| !n.trim().is_empty()) {
                    fetched.lock().unwrap().push((
                        i,
                        NotesEntry { tag: rel.tag_name.clone(), version, notes },
                    ));
                }
            });
        }
    });
    for (i, e) in fetched.into_inner().unwrap() {
        slots[i] = Some(e);
    }
    let entries: Vec<NotesEntry> = slots.into_iter().flatten().collect();
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
    // Dests where an earlier removal restored a preserved vanilla original: the file there is
    // STOCK, not ours. Without this skip it re-flags as Remove on every plan, and the next apply
    // undoes the restore (displaces the original back into the vanilla store) — the removal and
    // the restore chasing each other forever.
    let restored: HashSet<&str> = prev
        .map(|p| p.restored.iter().map(String::as_str).collect())
        .unwrap_or_default();
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
            && !restored.contains(r.dest.as_str())
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
    fn fetch_refuses_a_manifest_schema_it_cannot_read() {
        use crate::manifest::{UnsupportedSchema, MAX_SCHEMA};
        let settings = Settings::default();
        let future = MAX_SCHEMA + 1;
        let too_new = Fake::new(
            "v9.9.9",
            &format!(r#"{{ "schema": {future}, "version": "9.9.9", "files": [] }}"#),
            vec![],
        );
        let err = fetch(&settings, &too_new, None).unwrap_err();
        let refused = err
            .chain()
            .find_map(|c| c.downcast_ref::<UnsupportedSchema>())
            .expect("refused for the schema, not as a parse error");
        assert_eq!(refused.found, future);

        // a supported schema, and a legacy manifest with no `schema` key at all, both pass
        let ok = Fake::new("v1.0.0", r#"{ "schema": 2, "version": "1.0.0", "files": [] }"#, vec![]);
        assert!(fetch(&settings, &ok, None).is_ok());
        let legacy = Fake::new("v1.0.0", r#"{ "version": "1.0.0", "files": [] }"#, vec![]);
        assert!(fetch(&settings, &legacy, None).is_ok());
    }

    #[test]
    fn merged_game_release_folds_shards_into_the_main_release() {
        use crate::downloader::{Asset, ChunkProgress};
        // a two-release repo: the Fake serves one release, so hand-roll a tiny double here
        struct Sharded;
        fn rel(tag: &str, names: &[&str]) -> Release {
            Release {
                tag_name: tag.into(),
                body: None,
                assets: names
                    .iter()
                    .map(|n| Asset {
                        name: (*n).into(),
                        url: String::new(),
                        browser_download_url: String::new(),
                    })
                    .collect(),
            }
        }
        impl Downloader for Sharded {
            fn fetch_release(&self, _r: &str, _t: Option<&str>) -> Result<Release> {
                Ok(rel("v1805", &["manifest.json"]))
            }
            fn fetch_releases(&self, _r: &str) -> Result<Vec<Release>> {
                Ok(vec![
                    rel("v1805", &["manifest.json"]),
                    rel("v1805-assets-1", &["a.vpk", "b.vpk"]),
                    // a stale shard repeating a name must not shadow the first-seen asset
                    rel("v1805-assets-2", &["c.vpk", "a.vpk"]),
                ])
            }
            fn download(&self, _a: &Asset) -> Result<Vec<u8>> {
                unreachable!()
            }
            fn download_to(&self, _a: &Asset, _d: &Path, _r: u64, _p: ChunkProgress) -> Result<(u64, String)> {
                unreachable!()
            }
        }

        let dl = Sharded;
        let main = dl.fetch_release("r", None).unwrap();
        let merged = merged_game_release(&dl, "r", main).unwrap();
        let mut names: Vec<&str> = merged.assets.iter().map(|a| a.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["a.vpk", "b.vpk", "c.vpk", "manifest.json"]);
        assert_eq!(merged.tag_name, "v1805");
    }

    #[test]
    fn notes_history_keeps_a_future_schema_release() {
        use crate::manifest::MAX_SCHEMA;
        let settings = Settings::default();
        // installable? no. But its notes are exactly where "update the launcher" is explained,
        // so the What's new history must not develop a hole at it
        // json!, not a string literal: the notes' markdown heading (`"###`) embeds every raw
        // string delimiter an r#-string could use
        let future = Fake::new(
            "v9.9.9",
            &serde_json::json!({
                "schema": MAX_SCHEMA + 1,
                "version": "9.9.9",
                "notes": "### Requires a newer launcher",
                "files": []
            })
            .to_string(),
            vec![],
        );
        let cache = fetch_notes_history(&settings, &future, &[]).unwrap();
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.entries[0].version, "9.9.9");
        assert!(cache.entries[0].notes.contains("newer launcher"));

        // truly malformed stays skipped, not fatal
        let garbage = Fake::new("v1.0.0", r#"{ "version": 42 }"#, vec![]);
        assert!(fetch_notes_history(&settings, &garbage, &[]).unwrap().entries.is_empty());
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
            restored: Vec::new(),
        };
        let statuses = plan(&dir, &resolved, Some(&prev), &[]);
        let orphan = statuses.iter().find(|s| s.dest == "game/dota/fx.vpk").unwrap();
        assert_eq!(orphan.action, Action::Remove);
        // and the others are Install (nothing on disk)
        assert!(statuses.iter().filter(|s| s.dest != "game/dota/fx.vpk").all(|s| s.action == Action::Install));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
