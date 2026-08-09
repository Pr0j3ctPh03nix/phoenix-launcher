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
    open_repo_with(settings, what, |dl| dl.fetch_release(repo, tag))
}

/// `open_repo` for a repo's WHOLE release list — the launcher's version history. Same credential
/// rule, because it is the same repo reached by a different endpoint: a private launcher repo
/// refuses the listing exactly as it refuses the latest release.
pub fn open_repo_releases(
    repo: &str,
    settings: &Settings,
) -> AnyResult<(Box<dyn Downloader>, Vec<Release>)> {
    open_repo_with(settings, || format!("the {repo} releases"), |dl| dl.fetch_releases(repo))
}

/// The anonymous-first / token-on-refusal rule itself, over any single repo read. It lives in ONE
/// place on purpose: the rationale above is subtle (a 404 is indistinguishable from private, only
/// an HTTP refusal earns the retry, offline must not pay two connect timeouts), and a second
/// hand-rolled copy would drift from it.
fn open_repo_with<T>(
    settings: &Settings,
    what: impl Fn() -> String,
    call: impl Fn(&dyn Downloader) -> AnyResult<T>,
) -> AnyResult<(Box<dyn Downloader>, T)> {
    let anon = Github::new(None);
    match call(&anon) {
        Ok(v) => Ok((Box::new(anon), v)),
        Err(anon_err) => {
            let refused = anon_err
                .chain()
                .any(|c| matches!(c.downcast_ref::<NetKind>(), Some(NetKind::Status(_))));
            let (true, Some(t)) = (refused, settings.token()) else { return Err(anon_err) };
            let auth = Github::new(Some(t));
            let v = call(&auth)
                .with_context(|| format!("fetching {} (anonymously and with a token)", what()))?;
            Ok((Box::new(auth), v))
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
