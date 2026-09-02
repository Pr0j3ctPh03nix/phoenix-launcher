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

use anyhow::Result as AnyResult;

use crate::config::{self, Settings, Source};
use crate::downloader::{Downloader, NetKind, Release};
use crate::engine;
use crate::github::Github;
use crate::install::Origin;
use crate::manifest::Manifest;
use crate::mirror::{self, Mirror};
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
        let dl = (candidate.open)();
        match dl.fetch_release(repo, None) {
            Ok(release) => opened.push(Opened { dl, release }),
            Err(e) => {
                // The same rule `walk_sources` follows, and for the same reason: a definite answer
                // from the authoritative source ends everything, whatever a mirror would have
                // said. This chain's HEAD is where the manifest is read from, and reading one from
                // a mirror past "there is no such release" is exactly what that rule prevents.
                if candidate.authoritative && !unreachable(&e) {
                    return Err(e.context(format!("opening {repo}")));
                }
                if err.is_none() || candidate.authoritative {
                    err = Some(e); // the authoritative source's failure outranks a mirror's
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
    /// Does a DEFINITE answer from this source end the walk?
    ///
    /// True for the primary and only the primary. A refusal it gives is a real answer about the
    /// release — "there is no such release", "you may not have it" — and falling past that to a
    /// mirror, which might serve some other release entirely, would be worse than reporting it. A
    /// mirror is never authoritative in either direction: its 404 says only that THIS host does
    /// not carry the payload, which is precisely what the next source is for. Being unREACHABLE is
    /// not a definite answer from anyone, so it always falls through.
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
///
/// The GitHub candidate carries no credential logic of its own any more: `Github::for_repo` IS the
/// credential rule (anonymous first, the token only once the server has refused; the private
/// source repo leads with it), and it lives in the backend so that a caller reaching for one
/// directly cannot bypass it.
fn candidates<'a>(settings: &'a Settings, repo: &str) -> Vec<Candidate<'a>> {
    let owned = repo.to_string();
    let primary = Candidate {
        open: Box::new(move || Box::new(Github::for_repo(settings, &owned)) as Box<dyn Downloader>),
        authoritative: true,
    };
    let Some(payload) = settings.payload_of(repo).filter(|_| mirror::MIRROR_DOWNLOADS_ENABLED)
    else {
        return vec![primary];
    };
    let active = config::active_index(&settings.sources, settings.selected.as_ref());
    let order = active
        .into_iter()
        .chain((0..settings.sources.len()).filter(move |i| Some(*i) != active));
    let mut primary = Some(primary);
    let mut out = Vec::new();
    // `carries` is the second filter and it is not cosmetic: a mirror publishes which payload trees
    // it holds, and one that holds only `mod` has no `game/` directory at all. Letting it into a
    // game chain spends a connection and a 404 per asset before falling through, on the largest
    // download in the system — and ranks it as a usable source for a payload it has never had.
    // An entry that advertises nothing is trusted (see `Source::carries`), so this cannot silently
    // empty the chain for settings written before mirrors advertised anything.
    for source in order
        .map(|i| &settings.sources[i])
        .filter(|s| s.enabled() && s.carries(payload))
    {
        match source {
            Source::Primary => out.extend(primary.take()),
            Source::Mirror { url, .. } => out.push(Candidate {
                open: Box::new(move || Box::new(Mirror::new(url, payload))),
                authoritative: false,
            }),
        }
    }
    out.extend(primary); // a source list somehow without a Primary still gets one, last
    out
}

/// Is this failure a source being DARK rather than a source ANSWERING?
///
/// The one predicate both walks branch on. Unreachable (or hostile) is never an answer about the
/// release, so it always falls through to the next source — which is the point of having a chain on
/// a network where one host being dark is the ordinary state.
fn unreachable(e: &anyhow::Error) -> bool {
    e.chain().any(|c| matches!(c.downcast_ref::<NetKind>(), Some(NetKind::Transport)))
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
        let dl = (candidate.open)();
        let err = match call(dl.as_ref()) {
            Ok(v) => return Ok((dl, v)),
            Err(e) => e,
        };
        // Anything that is not the source being dark IS an answer, and an answer from the
        // authoritative source ends the walk.
        if candidate.authoritative && !unreachable(&err) {
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

#[cfg(test)]
mod tests {
    //! The source WALK, over injected backends. `candidates` builds https-only agents that no
    //! loopback test can reach, which is exactly why the walk is a separate function from the
    //! construction: the rules below are the part worth proving, and they are transport-free.
    use super::*;
    use crate::config::{Source, SourceRef};
    use crate::downloader::fake::Fake;
    use crate::trust::Payload;
    use std::sync::atomic::{AtomicU32, Ordering};

    const DOC: &str = r#"{"version":"1.0.0","files":[]}"#;

    /// A backend that answers the release lookup with a canned failure, counting the asks. `None`
    /// = it serves.
    struct Peer {
        inner: Fake,
        fails: Option<NetKind>,
        calls: AtomicU32,
    }

    impl Peer {
        fn serving() -> Arc<Self> {
            Arc::new(Self {
                inner: Fake::new("v1.0.0", DOC, vec![]),
                fails: None,
                calls: AtomicU32::new(0),
            })
        }
        fn failing(kind: NetKind) -> Arc<Self> {
            Arc::new(Self {
                inner: Fake::new("v1.0.0", DOC, vec![]),
                fails: Some(kind),
                calls: AtomicU32::new(0),
            })
        }
        fn calls(&self) -> u32 {
            self.calls.load(Ordering::SeqCst)
        }
    }

    // On the `Arc`, not on `Peer`: the walk takes OWNERSHIP of the downloader a candidate opens,
    // and the test still has to read the peer's call count afterwards. Sharing the peer is the
    // whole point, so the shared handle is what the trait is implemented for.
    impl Downloader for Arc<Peer> {
        fn fetch_release(&self, r: &str, t: Option<&str>) -> AnyResult<Release> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.fails {
                Some(k) => Err(anyhow::Error::new(k).context("scripted failure")),
                None => self.inner.fetch_release(r, t),
            }
        }
        fn fetch_releases(&self, r: &str) -> AnyResult<Vec<Release>> {
            self.inner.fetch_releases(r)
        }
        fn download(&self, a: &crate::downloader::Asset) -> AnyResult<Vec<u8>> {
            self.inner.download(a)
        }
        fn download_to(
            &self,
            a: &crate::downloader::Asset,
            d: &std::path::Path,
            r: u64,
            p: crate::downloader::ChunkProgress,
        ) -> AnyResult<(u64, String)> {
            self.inner.download_to(a, d, r, p)
        }
    }

    /// One link of a test chain. The `Arc` is what lets the test still read the peer's call count
    /// after the walk has taken ownership of the box it handed out.
    fn link(peer: &Arc<Peer>, authoritative: bool) -> Candidate<'static> {
        let open = peer.clone();
        Candidate { open: Box::new(move || Box::new(open.clone())), authoritative }
    }

    fn walk(chain: &[Candidate]) -> AnyResult<Release> {
        walk_sources(chain, || "the release".into(), |dl| dl.fetch_release("r", None)).map(|(_, v)| v)
    }

    /// An unreachable source is not an ANSWER — it is a host being dark, which on the networks
    /// this feature exists for is the ordinary state. The walk moves on and the run completes.
    #[test]
    fn a_source_that_cannot_be_reached_falls_through_to_the_next() {
        let (down, up) = (Peer::failing(NetKind::Transport), Peer::serving());
        let chain = [link(&down, true), link(&up, false)];
        assert_eq!(walk(&chain).expect("the second source serves it").tag_name, "v1.0.0");
        assert_eq!(down.calls(), 1);
        assert_eq!(up.calls(), 1);
    }

    /// A REFUSAL from the primary is a real answer about the release — "there is no such release",
    /// "you may not have it" — and falling past it to a mirror, which might serve some other
    /// release entirely, would be worse than reporting it. So the walk stops there.
    #[test]
    fn a_refusal_from_the_authoritative_source_ends_the_walk() {
        let (refused, mirror) = (Peer::failing(NetKind::Status(404)), Peer::serving());
        let chain = [link(&refused, true), link(&mirror, false)];
        assert!(walk(&chain).is_err());
        assert_eq!(mirror.calls(), 0, "a mirror must not be asked past a definite answer");
    }

    /// A mirror is never authoritative in either direction: its 404 says only that THIS host does
    /// not carry the payload, which is precisely what the next source is for.
    #[test]
    fn a_mirrors_refusal_is_only_about_that_mirror() {
        let (stale, primary) = (Peer::failing(NetKind::Status(404)), Peer::serving());
        let chain = [link(&stale, false), link(&primary, true)];
        assert!(walk(&chain).is_ok(), "a stale mirror ranked first must not brick the check");
        assert_eq!(primary.calls(), 1);
    }

    /// Nothing served it. The authoritative source's failure is the one worth showing — a mirror's
    /// is a fact about a host the user never chose.
    #[test]
    fn an_exhausted_chain_reports_the_authoritative_failure() {
        let (mirror, primary) =
            (Peer::failing(NetKind::Status(503)), Peer::failing(NetKind::Transport));
        let chain = [link(&mirror, false), link(&primary, true)];
        let err = walk(&chain).unwrap_err();
        assert!(
            err.chain().any(|c| matches!(c.downcast_ref::<NetKind>(), Some(NetKind::Transport))),
            "expected the primary's transport failure, got: {err:#}"
        );
        assert!(format!("{err:#}").contains("the release"));
    }

    /// The deployment gate. `MIRROR_DOWNLOADS_ENABLED` is false, so no configured mirror may enter
    /// a chain however the list is ranked or pinned — the primary alone, and every rule above
    /// degenerates to what it always was.
    #[test]
    fn no_mirror_enters_a_chain_while_downloads_are_disabled() {
        let url = "https://mirror.example".to_string();
        let settings = Settings {
            sources: vec![
                Source::Mirror { url: url.clone(), enabled: true, measured: true, payloads: Vec::new() },
                Source::Primary,
            ],
            selected: Some(SourceRef::Mirror { url }),
            ..Default::default()
        };
        let chain = candidates(&settings, &settings.source_repo);
        assert!(!mirror::MIRROR_DOWNLOADS_ENABLED, "this test describes the flag being OFF");
        assert_eq!(chain.len(), 1, "only the primary may be reachable while the flag is off");
        assert!(chain[0].authoritative);
    }

    /// A mirror is addressed `<base>/<payload>/…`, so which payload a repo names is what decides
    /// whether it can have a mirror at all. A repo this build knows nothing about — only the debug
    /// CLI's `--repo` can produce one — has no mirror path to build.
    #[test]
    fn a_repo_maps_to_the_payload_directory_a_mirror_serves_it_from() {
        let s = Settings::default();
        assert_eq!(s.payload_of(&s.source_repo).map(Payload::id), Some("mod"));
        assert_eq!(s.payload_of(s.game_repo()).map(Payload::id), Some("game"));
        assert_eq!(s.payload_of(s.launcher_repo()).map(Payload::id), Some("launcher"));
        assert!(s.payload_of("somebody/else").is_none());
    }
}
