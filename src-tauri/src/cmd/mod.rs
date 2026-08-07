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
pub mod update;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result as AnyResult};

use crate::config::Settings;
use crate::downloader::{Downloader, NetKind, Release};
use crate::engine;
use crate::github::Github;
use crate::manifest::Manifest;
use crate::views::CmdError;

/// Open a repo's latest release with whatever credentials can actually see it, returning the
/// downloader that worked — later asset downloads must ride the same auth.
///
/// ANONYMOUS FIRST, token second: the launcher and game repos are meant to be public, and the
/// dist token may be a fine-grained PAT scoped to the dist repo alone — sending it could be
/// refused where anonymous access succeeds. The token retry fires ONLY on an HTTP refusal
/// (a private repo answers 404, indistinguishable from missing): credentials can turn a 404
/// into a 200, but not fix DNS — offline must not pay two connect timeouts.
pub fn open_repo(repo: &str, settings: &Settings) -> AnyResult<(Box<dyn Downloader>, Release)> {
    open_repo_tagged(repo, settings, None)
}

/// `open_repo` for a SPECIFIC release. Self-update needs it: re-resolving "latest" between showing
/// an update and installing it can hand back a different (even older) release than the one the
/// user agreed to.
pub fn open_repo_tagged(
    repo: &str,
    settings: &Settings,
    tag: Option<&str>,
) -> AnyResult<(Box<dyn Downloader>, Release)> {
    let what = || match tag {
        Some(t) => format!("the {repo} release {t}"),
        None => format!("the latest {repo} release"),
    };
    let anon = Github::new(None);
    match anon.fetch_release(repo, tag) {
        Ok(r) => Ok((Box::new(anon), r)),
        Err(anon_err) => {
            let refused = anon_err
                .chain()
                .any(|c| matches!(c.downcast_ref::<NetKind>(), Some(NetKind::Status(_))));
            let (true, Some(t)) = (refused, settings.token()) else { return Err(anon_err) };
            let auth = Github::new(Some(t));
            let r = auth
                .fetch_release(repo, tag)
                .with_context(|| format!("fetching {} (anonymously and with a token)", what()))?;
            Ok((Box::new(auth), r))
        }
    }
}

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
