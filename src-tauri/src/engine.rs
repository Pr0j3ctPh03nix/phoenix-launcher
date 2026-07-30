//! Shared update logic: fetch the release + manifest, resolve the effective file set from the
//! user's option selections, and diff it against what is installed. `check` is the read-only
//! surface over this; `install` (in install.rs) reuses `fetch`, `resolve` and `plan`.

use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::config::Settings;
use crate::manifest::{FileEntry, Manifest, OptionEntry, OptionKind};
use crate::state::InstalledState;
use crate::{github, verify};

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

/// Fetch the release and its manifest.json. Returned together so an install can reuse the release's
/// asset list (for private downloads) without a second round trip.
pub fn fetch(settings: &Settings, tag: Option<&str>) -> Result<(github::Release, Manifest)> {
    let release = github::fetch_release(&settings.source_repo, tag, settings.token.as_deref())
        .context("fetching the release")?;
    let manifest_asset = release
        .asset("manifest.json")
        .context("the release has no manifest.json asset")?;
    let bytes = github::download_asset(manifest_asset, settings.token.as_deref())
        .context("downloading manifest.json")?;
    let manifest = serde_json::from_slice(&bytes).context("parsing manifest.json")?;
    Ok((release, manifest))
}

/// One release's "What's new" entry, for the version-history view.
#[derive(Debug, Clone)]
pub struct NotesEntry {
    pub tag: String,
    pub version: String,
    pub notes: String,
}

/// The full "What's new" history: every release's manifest notes, newest first (GitHub's release
/// order). Releases without a manifest.json, with an unparsable manifest, or with empty notes are
/// skipped — a single bad release must not sink the whole history.
pub fn fetch_notes_history(settings: &Settings) -> Result<Vec<NotesEntry>> {
    let token = settings.token.as_deref();
    let releases =
        github::fetch_releases(&settings.source_repo, token).context("listing releases")?;
    let mut out = Vec::new();
    for rel in &releases {
        let Some(asset) = rel.asset("manifest.json") else { continue };
        let Ok(bytes) = github::download_asset(asset, token) else { continue };
        let Ok(m) = serde_json::from_slice::<Manifest>(&bytes) else { continue };
        if let Some(notes) = m.notes.filter(|n| !n.trim().is_empty()) {
            out.push(NotesEntry { tag: rel.tag_name.clone(), version: m.version, notes });
        }
    }
    Ok(out)
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
pub fn check(settings: &Settings, tag: Option<&str>) -> Result<CheckResult> {
    let (release, manifest) = fetch(settings, tag)?;
    evaluate(settings, &release.tag_name, &manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::InstalledFile;

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
