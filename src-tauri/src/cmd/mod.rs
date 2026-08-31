//! The Tauri command layer, one module per domain. Each command is a thin wrapper: load
//! settings, call the engine, shape the result into a view (views.rs). All commands fail with
//! `CmdError` so the webview always receives `{kind, message}`.

pub mod autofind;
pub mod game;
pub mod launch;
pub mod mirrors;
pub mod misc;
pub mod notes;
pub mod selfupdate;
pub mod settings;
pub mod update;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result as AnyResult};

use crate::config::{self, Settings, Source};
use crate::downloader::{Downloader, NetKind, Release};
use crate::engine;
use crate::github::Github;
use crate::install::Origin;
use crate::manifest::Manifest;
use crate::mirror::{self, Mirror};
use crate::trust::Payload;
use crate::views::CmdError;

/// Open a repo's latest release from the best source that can actually serve it, returning the
/// downloader that worked — later asset downloads must ride the same source and the same auth.
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
    open_repo_with(settings, repo, what, |dl| dl.fetch_release(repo, tag))
}

/// `open_repo` for a repo's WHOLE release list — the launcher's version history. Same rules,
/// because it is the same repo reached by a different endpoint: a private launcher repo refuses
/// the listing exactly as it refuses the latest release.
pub fn open_repo_releases(
    repo: &str,
    settings: &Settings,
) -> AnyResult<(Box<dyn Downloader>, Vec<Release>)> {
    open_repo_with(settings, repo, || format!("the {repo} releases"), |dl| dl.fetch_releases(repo))
}

/// A source that ANSWERED: its backend and the release it served. The owning form of
/// `install::Origin`, which borrows — the borrow has to come from somewhere, and this is it.
pub struct Opened {
    pub dl: Box<dyn Downloader>,
    pub release: Release,
}

impl Opened {
    pub fn origin(&self) -> Origin<'_> {
        Origin::new(self.dl.as_ref(), &self.release)
    }
}

/// EVERY source that can serve `repo`'s latest release, in priority order — the chain a payload
/// download falls through per asset (`install::Origin`).
///
/// `open_repo` is this stopped at the first success, and that is the right shape for a check: one
/// round trip, no cost paid for sources nothing will read. A multi-gigabyte install is the opposite
/// trade — one API call per source, against hours of transfer — so it opens them all up front and
/// hands the pool somewhere to go when an asset will not come.
///
/// The FIRST element is the one a manifest should be read from; the rest are fallbacks for BYTES,
/// never for identity. Fails only if nothing answered at all, reporting the same error `open_repo`
/// would have.
pub fn open_all(repo: &str, settings: &Settings) -> AnyResult<Vec<Opened>> {
    let mut opened = Vec::new();
    let mut err = None;
    for candidate in candidates(settings, repo) {
        match try_candidate(&candidate, |dl| dl.fetch_release(repo, None)) {
            Ok((dl, release)) => opened.push(Opened { dl, release }),
            Err(e) => {
                if err.is_none() || candidate.authoritative {
                    err = Some(e); // the authoritative source's answer outranks a mirror's
                }
            }
        }
    }
    match (opened.is_empty(), err) {
        (false, _) => Ok(opened),
        (true, Some(e)) => Err(e.context(format!("opening {repo}"))),
        (true, None) => Err(anyhow::anyhow!("no download source is configured for {repo}")),
    }
}

/// One place to try, and how much authority its answer carries.
///
/// Built lazily (`open` is a closure) because opening a source costs a round trip and the common
/// case stops at the first. Split out from the walk itself so the walk can be exercised over
/// injected backends: both real constructors here are https-only agents, which no loopback test
/// can reach.
struct Candidate<'a> {
    open: Box<dyn Fn() -> Box<dyn Downloader> + 'a>,
    /// The same source re-tried WITH credentials — GitHub only. A private repo answers 404, which
    /// is indistinguishable from missing, so an HTTP refusal earns this retry; nothing else does,
    /// because credentials can turn a 404 into a 200 but cannot fix DNS, and offline must not pay
    /// two connect timeouts.
    credentials: Option<Box<dyn Fn() -> Box<dyn Downloader> + 'a>>,
    /// Does a DEFINITE answer from this source end the walk?
    ///
    /// True for the primary and only the primary. A refusal it gives after the credential retry is
    /// a real answer about the release — "there is no such release", "you may not have it" — and
    /// falling past that to a mirror, which might serve some other release entirely, would be
    /// worse than reporting it. A mirror is never authoritative in either direction: its 404 says
    /// only that THIS host does not carry the payload, which is precisely what the next source is
    /// for. Being unREACHABLE is not a definite answer from anyone, so it always falls through.
    authoritative: bool,
}

/// The sources for `repo`, in priority order.
///
/// The user's PIN leads (`config::active_index` resolves which source is the one in use — the same
/// call the settings pane paints `active` from, so the pane cannot come to disagree with the
/// download path), then the rest in list order, which a sweep keeps fastest-first. Disabled mirrors
/// are skipped: a disabled mirror is still PROBED, which is how one that has recovered gets
/// noticed, but it is never downloaded from.
///
/// Mirrors appear only when `mirror::MIRROR_DOWNLOADS_ENABLED` — with the flag off this is the
/// primary alone and every rule below degenerates to what it always was — and only for a repo whose
/// PAYLOAD this build can name: a mirror is addressed `<base>/<payload>/…`, so a repo that maps to
/// no payload (the debug CLI's `--repo`) has no mirror path to build at all.
fn candidates<'a>(settings: &'a Settings, repo: &str) -> Vec<Candidate<'a>> {
    let primary = Candidate {
        open: Box::new(|| Box::new(Github::new(None))),
        credentials: settings.token().map(|t| {
            Box::new(move || Box::new(Github::new(Some(t))) as Box<dyn Downloader>)
                as Box<dyn Fn() -> Box<dyn Downloader> + 'a>
        }),
        authoritative: true,
    };
    let Some(payload) = payload_of(settings, repo).filter(|_| mirror::MIRROR_DOWNLOADS_ENABLED)
    else {
        return vec![primary];
    };
    let active = config::active_index(&settings.sources, settings.selected.as_ref());
    let order = active
        .into_iter()
        .chain((0..settings.sources.len()).filter(move |i| Some(*i) != active));
    let mut primary = Some(primary);
    let mut out = Vec::new();
    for source in order.map(|i| &settings.sources[i]).filter(|s| s.enabled()) {
        match source {
            // The primary keeps its own credential rule wherever the ranking puts it.
            Source::Primary => out.extend(primary.take()),
            Source::Mirror { url, .. } => out.push(Candidate {
                open: Box::new(move || Box::new(Mirror::new(url, payload))),
                credentials: None,
                authoritative: false,
            }),
        }
    }
    out.extend(primary); // a source list somehow without a Primary still gets one, last
    out
}

/// The payload a repo names, and therefore the directory a mirror serves it from. `None` for a repo
/// this build knows nothing about, which is reachable only through the debug CLI's `--repo`.
fn payload_of(settings: &Settings, repo: &str) -> Option<Payload> {
    if repo == settings.source_repo {
        Some(Payload::Mod)
    } else if repo == settings.game_repo() {
        Some(Payload::Game)
    } else if repo == settings.launcher_repo() {
        Some(Payload::Launcher)
    } else {
        None
    }
}

/// One candidate's whole attempt: anonymously, then with credentials if it was REFUSED.
///
/// ANONYMOUS FIRST, token second: the launcher and game repos are meant to be public, and the dist
/// token may be a fine-grained PAT scoped to the dist repo alone — sending it could be refused
/// where anonymous access succeeds.
fn try_candidate<T>(
    candidate: &Candidate,
    call: impl Fn(&dyn Downloader) -> AnyResult<T>,
) -> AnyResult<(Box<dyn Downloader>, T)> {
    let dl = (candidate.open)();
    match call(dl.as_ref()) {
        Ok(v) => Ok((dl, v)),
        Err(e) => {
            let refused =
                e.chain().any(|c| matches!(c.downcast_ref::<NetKind>(), Some(NetKind::Status(_))));
            let (true, Some(with_creds)) = (refused, candidate.credentials.as_ref()) else {
                return Err(e);
            };
            let auth = with_creds();
            let v = call(auth.as_ref()).context("anonymously and with a token")?;
            Ok((auth, v))
        }
    }
}

/// The source walk itself, over any single repo read. It lives in ONE place on purpose: the rules
/// are subtle (a 404 is indistinguishable from private so only an HTTP refusal earns the credential
/// retry; offline must not pay two connect timeouts; only the primary's definite answer is final),
/// and a second hand-rolled copy would drift from them.
fn open_repo_with<T>(
    settings: &Settings,
    repo: &str,
    what: impl Fn() -> String,
    call: impl Fn(&dyn Downloader) -> AnyResult<T>,
) -> AnyResult<(Box<dyn Downloader>, T)> {
    walk_sources(&candidates(settings, repo), what, call)
}

fn walk_sources<T>(
    chain: &[Candidate],
    what: impl Fn() -> String,
    call: impl Fn(&dyn Downloader) -> AnyResult<T>,
) -> AnyResult<(Box<dyn Downloader>, T)> {
    let mut first_err: Option<anyhow::Error> = None;
    let mut authoritative_err: Option<anyhow::Error> = None;
    for candidate in chain {
        let err = match try_candidate(candidate, &call) {
            Ok(v) => return Ok(v),
            Err(e) => e,
        };
        // Unreachable (or hostile) is never a source's ANSWER, so it always falls through to the
        // next one — which is the whole point of having a chain on a network where one host being
        // dark is the ordinary state. Anything else is an answer, and an answer from the
        // authoritative source ends the walk.
        let unreachable =
            err.chain().any(|c| matches!(c.downcast_ref::<NetKind>(), Some(NetKind::Transport)));
        if candidate.authoritative && !unreachable {
            return Err(err.context(format!("fetching {}", what())));
        }
        if candidate.authoritative {
            authoritative_err = Some(err);
        } else if first_err.is_none() {
            first_err = Some(err);
        }
    }
    // Nothing served it. Report the authoritative source's failure where there is one — it is the
    // one worth showing, the same precedent `mirror::fetch_published_mirrors` follows.
    Err(authoritative_err
        .or(first_err)
        .unwrap_or_else(|| anyhow::anyhow!("no download source is configured"))
        .context(format!("fetching {}", what())))
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
