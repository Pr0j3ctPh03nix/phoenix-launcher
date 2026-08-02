//! The Tauri command layer, one module per domain. Each command is a thin wrapper: load
//! settings, call the engine, shape the result into a view (views.rs). All commands fail with
//! `CmdError` so the webview always receives `{kind, message}`.

pub mod autofind;
pub mod launch;
pub mod misc;
pub mod notes;
pub mod settings;
pub mod update;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use crate::engine;
use crate::manifest::Manifest;

/// The last successfully fetched manifest, so selection changes re-plan without network I/O.
pub struct CachedManifest {
    pub repo: String,
    pub tag_name: String,
    pub manifest: Manifest,
}

#[derive(Default)]
pub struct AppState {
    pub manifest_cache: Mutex<Option<CachedManifest>>,
    /// The "What's new" history (also persisted to disk by the engine). `release_notes` validates
    /// freshness itself against the last checked tag, so nothing here needs invalidating.
    pub notes_cache: Mutex<Option<engine::NotesCache>>,
    pub autofind_cancel: Arc<AtomicBool>,
    pub autofind_running: AtomicBool,
}
