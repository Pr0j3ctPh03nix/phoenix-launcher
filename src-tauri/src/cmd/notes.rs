//! The "What's new" version-history commands — one per repo the user sees releases from.
//!
//! Two histories, because they are two products on two version lines: the Phoenix client (notes
//! carried inside each dist release's manifest.json) and the launcher itself (notes carried as
//! each GitHub release's description). They are fetched, cached and expired independently; the
//! frontend shows them as two pages.

use std::sync::{Arc, Mutex};

use crate::cmd::{open_repo_releases, AppState};
use crate::config::Settings;
use crate::engine;
use crate::github::Github;
use crate::views::{CmdError, NotesEntryView};

/// The full "What's new" history (every release's notes, newest first). Cached in memory AND on
/// disk, keyed by the last checked tag — reopens are instant across app restarts; a new release
/// triggers an incremental rebuild (only unseen tags download a manifest).
#[tauri::command]
pub async fn release_notes(
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<Vec<NotesEntryView>, CmdError> {
    let st = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let settings = Settings::load();
        // freshness key: the tag the last check saw for this repo
        let current_tag: Option<String> = st
            .manifest_cache
            .lock()
            .unwrap()
            .as_ref()
            .filter(|c| c.repo == settings.source_repo)
            .map(|c| c.tag_name.clone());
        history(
            &settings.source_repo,
            &st.notes_cache,
            engine::NOTES_FILE_SHIM,
            current_tag,
            |known| {
                let dl = Github::for_repo(&settings, &settings.source_repo);
                engine::fetch_notes_history(&settings, &dl, known)
            },
        )
    })
    .await
    .map_err(CmdError::task)?
}

/// The LAUNCHER's own history — every launcher release's description, newest first.
///
/// Costs one API call when it is cold and none at all when it is warm: `launcher_check` runs on
/// every launch and records the tag it saw, so a cached history that names the same tag is
/// provably current. The launcher repo is meant to be public, so the listing goes through the
/// same anonymous-first / token-on-refusal rule the self-update check uses.
#[tauri::command]
pub async fn launcher_notes(
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<Vec<NotesEntryView>, CmdError> {
    let st = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let settings = Settings::load();
        let repo = settings.launcher_repo().to_string();
        let current_tag = st.launcher_tag.lock().unwrap().clone();
        history(
            &repo,
            &st.launcher_notes_cache,
            engine::NOTES_FILE_LAUNCHER,
            current_tag,
            |_known| {
                // `known` is ignored on purpose: there is no per-release download to skip. One
                // listing carries every body, so a rebuild is always whole and always cheap.
                let (_, releases) = open_repo_releases(&repo, &settings)?;
                Ok(engine::launcher_notes_history(&repo, &releases))
            },
        )
    })
    .await
    .map_err(CmdError::task)?
}

/// The memory -> disk -> rebuild policy both histories share.
///
/// `current_tag` is the freshness key: the newest tag the corresponding check saw. `None` means
/// accept any cached history for this repo — the UI only opens these views after a check anyway,
/// and a history is worth more than a round trip proving it is still the same one. A cache for
/// the SAME repo that is merely stale is not thrown away either: it is handed to `rebuild` as the
/// known entries, so only what is genuinely new can cost anything.
fn history(
    repo: &str,
    slot: &Mutex<Option<engine::NotesCache>>,
    file: &str,
    current_tag: Option<String>,
    rebuild: impl FnOnce(&[engine::NotesEntry]) -> anyhow::Result<engine::NotesCache>,
) -> Result<Vec<NotesEntryView>, CmdError> {
    let to_views = |entries: &[engine::NotesEntry]| {
        entries
            .iter()
            .map(|e| NotesEntryView {
                tag: e.tag.clone(),
                version: e.version.clone(),
                notes: e.notes.clone(),
            })
            .collect::<Vec<_>>()
    };
    let fresh =
        |c: &engine::NotesCache| c.repo == repo && current_tag.as_ref().is_none_or(|t| *t == c.latest_tag);
    // memory first, then disk (survives restarts); a stale same-repo cache still seeds the rebuild
    let mut known: Vec<engine::NotesEntry> = Vec::new();
    {
        let guard = slot.lock().unwrap();
        if let Some(c) = guard.as_ref() {
            if fresh(c) {
                return Ok(to_views(&c.entries));
            }
            if c.repo == repo {
                known = c.entries.clone();
            }
        }
    }
    if known.is_empty() {
        if let Some(c) = engine::NotesCache::load(file) {
            if fresh(&c) {
                let views = to_views(&c.entries);
                *slot.lock().unwrap() = Some(c);
                return Ok(views);
            }
            if c.repo == repo {
                known = c.entries;
            }
        }
    }
    let mut cache = rebuild(&known).map_err(CmdError::from)?;
    // key the cache to the checked tag, not the release list's first item — those differ when the
    // newest release is a prerelease (both checks follow /releases/latest, the history follows
    // /releases), and a mismatch would refetch on every open
    if let Some(t) = current_tag {
        cache.latest_tag = t;
    }
    cache.save(file);
    let views = to_views(&cache.entries);
    *slot.lock().unwrap() = Some(cache);
    Ok(views)
}
