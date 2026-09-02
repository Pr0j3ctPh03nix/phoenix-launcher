//! The Tauri command layer, one module per domain. Each command is a thin wrapper: load
//! settings, call the engine, shape the result into a view (views.rs). All commands fail with
//! `CmdError` so the webview always receives `{kind, message}`.

pub mod autofind;
pub mod game;
pub mod launch;
pub mod misc;
pub mod notes;
pub mod selfupdate;
pub mod settings;
pub mod sources;
pub mod update;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::engine;
use crate::manifest::Manifest;
use crate::views::CmdError;

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
    /// The LAUNCHER's own version history — a separate repo with a separate version line, shown
    /// on its own page. Same freshness contract, keyed by `launcher_tag` instead.
    pub launcher_notes_cache: Mutex<Option<engine::NotesCache>>,
    /// The launcher repo's latest tag as of the last successful `launcher_check`. Plays the part
    /// `manifest_cache.tag_name` plays for the shim: it is what lets a cached launcher history be
    /// served without a round trip, and what expires it the moment a new launcher is published.
    pub launcher_tag: Mutex<Option<String>>,
    pub autofind_cancel: Arc<AtomicBool>,
    pub autofind_running: AtomicBool,
    /// Cancels the base-game op in flight (`game_cancel` sets it; install_base's chunk callbacks
    /// and base_plan's hash workers poll it). Reset by each new game_plan/game_verify/
    /// game_install/game_repair — see `game_cancel` for why one flag covers all four.
    pub game_cancel: AtomicBool,
    /// One heavyweight op at a time (apply / uninstall / launcher self-update / play /
    /// game download / repair / plan / verify). The frontend's busy flag already serializes what
    /// the UI can trigger; this is the backend line behind it, same as the game-running interlock
    /// backs the disabled buttons. Plan/verify write nothing but still take the slot: they own
    /// the shared `game_cancel` flag while running. Truly read-only commands (check, replan,
    /// game_running) stay unguarded.
    op_running: AtomicBool,
}

/// Held for the duration of a mutating op; releases on drop (including unwind).
pub struct OpGuard<'a>(&'a AtomicBool);

impl Drop for OpGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl AppState {
    /// Claim the mutating-op slot, or fail with a typed error the UI shows verbatim.
    pub fn begin_op(&self, what: &str) -> Result<OpGuard<'_>, CmdError> {
        self.op_running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map(|_| OpGuard(&self.op_running))
            .map_err(|_| CmdError::from(format!("another operation is already running — {what} refused")))
    }
}
