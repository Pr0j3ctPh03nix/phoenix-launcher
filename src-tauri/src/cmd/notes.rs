//! The "What's new" version-history command.

use std::sync::Arc;

use crate::cmd::AppState;
use crate::config::Settings;
use crate::github::Github;
use crate::views::{CmdError, NotesEntryView};
use crate::engine;

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
        // freshness key: the tag the last check saw for this repo (None = accept any cached
        // history — the UI only opens this view after a successful check anyway)
        let current_tag: Option<String> = st
            .manifest_cache
            .lock()
            .unwrap()
            .as_ref()
            .filter(|c| c.repo == settings.source_repo)
            .map(|c| c.tag_name.clone());
        let fresh = |c: &engine::NotesCache| {
            c.repo == settings.source_repo
                && current_tag.as_ref().map_or(true, |t| *t == c.latest_tag)
        };
        // memory first, then disk (survives restarts); a stale same-repo cache still seeds the
        // incremental rebuild
        let mut known: Vec<engine::NotesEntry> = Vec::new();
        {
            let guard = st.notes_cache.lock().unwrap();
            if let Some(c) = guard.as_ref() {
                if fresh(c) {
                    return Ok(to_views(&c.entries));
                }
                if c.repo == settings.source_repo {
                    known = c.entries.clone();
                }
            }
        }
        if known.is_empty() {
            if let Some(c) = engine::NotesCache::load() {
                if fresh(&c) {
                    let views = to_views(&c.entries);
                    *st.notes_cache.lock().unwrap() = Some(c);
                    return Ok(views);
                }
                if c.repo == settings.source_repo {
                    known = c.entries;
                }
            }
        }
        let dl = Github::new(settings.token());
        let mut cache =
            engine::fetch_notes_history(&settings, &dl, &known).map_err(CmdError::from)?;
        // key the cache to the checked tag, not the release list's first item — those differ when
        // the newest release is a prerelease (check follows /releases/latest), and a mismatch
        // would refetch on every open
        if let Some(t) = current_tag {
            cache.latest_tag = t;
        }
        cache.save();
        let views = to_views(&cache.entries);
        *st.notes_cache.lock().unwrap() = Some(cache);
        Ok(views)
    })
    .await
    .map_err(CmdError::task)?
}
