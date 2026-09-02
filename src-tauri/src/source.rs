//! Where a payload is fetched from: the ranking, the walk, and the wire a long download rides.
//!
//! ONE model, for every read the launcher makes. A source is a place — GitHub, or a published
//! mirror — and they are ranked by a real measurement, fastest working first. Every operation
//! starts at the head of that ranking and moves to the next source when the one it is on fails,
//! retrying the whole operation there. Nothing is pinned, nothing is switched off, and nothing is
//! routed per payload: the only question ever asked is "which source, in order".
//!
//! Three shapes, and the difference between them is what a failure costs:
//!
//! * `with_active` — a single-shot read (a check, a manifest, a self-update lookup, a list
//!   refresh). There is no partial state worth keeping, so a failure re-runs the whole operation
//!   against the next source. Both documents a read needs are small and must come from ONE source
//!   anyway: a manifest from A and a signature from B is not a thing we want.
//! * `Wire` — a long download. The source is swappable UNDER the pool, mid-run, pinned to the tag
//!   it opened with: identity keeps coming from the manifest already verified, and only the bytes
//!   move. Eight workers failing at once cause ONE failover, not eight.
//! * the boot sequence (`start`) — adopt the last ranking, bootstrap and refresh the published
//!   list, decide what is worth measuring, measure it, rank, persist. Then a scheduler keeps the
//!   ranking honest for the rest of the process's life.
//!
//! The `Registry` is what the status block paints and what a walk starts from. It deliberately
//! holds no cursor: a walk owns its own "already tried" set, which is what makes "each source is
//! asked at most once per operation" structural rather than a rule somebody has to keep.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;

use crate::config::{self, Measured, Settings, Source};
use crate::downloader::{Asset, Downloader, Release};
use crate::engine;
use crate::github::Github;
use crate::manifest::{Manifest, UnsupportedCodec, UnsupportedSchema};
use crate::mirror::{self, Mirror};
use crate::trust::Payload;

/// How long a HEALTHY measurement is believed.
///
/// Staleness never STARTS a launch-time pass — it only widens one that a new (or newly failed)
/// source already triggered. Re-timing costs a real transfer per source, and re-ranking unprompted
/// is the thing that would move a user off a source that works.
const MEASUREMENT_TTL: Duration = Duration::from_secs(60 * 60);

/// When NOTHING is healthy, everything is re-measured this often until something is.
///
/// The one case where an hourly retry is too slow to be useful: a launcher that booted offline has
/// every row red and no ranking worth the name, and the moment the network comes back it should
/// notice within a couple of minutes rather than at the next launch. It costs a real transfer per
/// source, which is why it is gated on nothing working at all — the state where there is nothing to
/// lose and no ranking to disturb.
const ALL_DEAD_RETRY: Duration = Duration::from_secs(2 * 60);

/// How often the scheduler wakes to ask whether anything is due. Well under `ALL_DEAD_RETRY` so
/// that interval is honoured rather than rounded up to twice itself.
const SCHEDULER_TICK: Duration = Duration::from_secs(15);

/// How much of a real asset a probe pulls. Comfortably past the ~16 KiB that throttling middleboxes
/// have been observed to let through before choking a connection, so a path that only *looks* alive
/// is measured as slow rather than reported as healthy.
pub(crate) const PROBE_BYTES: u64 = 512 * 1024;

/// Wall-clock cap on the asset read.
///
/// A per-read timeout does not catch a trickle: a path that delivers a few bytes every second keeps
/// resetting the socket's read deadline and would hold the probe open for as long as it cares to.
/// The loop is therefore bounded by total elapsed time, and running out mid-chunk counts as a
/// failure — not a low score — because a source that cannot finish 512 KiB cannot finish a release.
pub(crate) const PROBE_BUDGET: Duration = Duration::from_secs(8);

/// The payload a source is TIMED through.
///
/// Fixed, because every mirror carries every payload: the launcher does not route per payload, and
/// `dial_for` below is the one place that assumption is acted on. `mod` is the smallest tree every
/// host has and the one an ordinary check pulls, so a probe measures the transfer the next download
/// will actually repeat.
const PROBE_PAYLOAD: Payload = Payload::Mod;

// ---------------------------------------------------------------- the registry

/// The download sources as this process is using them: one ranked list, plus what to SHOW.
///
/// `active`/`failed`/`measuring` are REPORTING state. `active` is the source the last operation
/// actually used — successful or not, because on a machine where nothing works "the source in use"
/// can only honestly mean "the last one tried". `failed` paints rows red and is runtime-only: a
/// source that was down while the user was on a train must not stay sidelined across restarts, and
/// finding out costs one request. Neither of them filters a walk; the RANKING is the whole routing
/// decision, and `tried` (per operation) is the whole "do not ask twice" decision.
struct Registry {
    /// Ranked, fastest-healthy first — the order settings are persisted in.
    sources: Vec<Source>,
    /// Index into `sources`. Never out of range, structurally: `adopt` refuses an empty ranking
    /// and `mark` leaves the index alone when it cannot find the key.
    active: usize,
    /// Failed an operation in THIS process. Cleared by a completed measuring pass and by start.
    failed: HashSet<Option<String>>,
    /// Being measured right now (the status block's third row state).
    measuring: HashSet<Option<String>>,
    /// Why the published list could not be refreshed this launch. Never fatal.
    refresh_error: Option<String>,
}

/// Seeded from `Settings::load()` on first touch — the same shape as `install::INFLIGHT`, and what
/// lets the CLI and the tests reach it without a boot call.
static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| {
    Mutex::new(Registry {
        sources: Settings::load().sources,
        active: 0,
        failed: HashSet::new(),
        measuring: HashSet::new(),
        refresh_error: None,
    })
});

/// "Something about the sources changed." A bare notification — no Tauri, no view type — installed
/// by `main.rs`, same shape as `engine::Progress`. The engine stays UI-agnostic and the shell
/// decides what an event is.
type Sink = Box<dyn Fn() + Send + Sync>;
static SINK: Mutex<Option<Sink>> = Mutex::new(None);

/// Install the change sink. Called once, from the Tauri setup.
pub fn on_change(sink: impl Fn() + Send + Sync + 'static) {
    *SINK.lock().unwrap() = Some(Box::new(sink));
}

/// Fire the sink. ALWAYS outside the registry lock: the sink's whole job is to read the registry
/// back out, and calling it under the lock is a deadlock with the shape of a hang at boot.
///
/// The sink is called while `SINK` is held, so a panic inside it poisons this mutex — and an
/// `unwrap` here would then turn one bad emit into every later notification panicking, for the rest
/// of the process. A poisoned `SINK` protects nothing that can be inconsistent (it holds one
/// `Option<Box<dyn Fn>>`, installed once at setup), so the guard is taken anyway and the launcher
/// goes on reporting.
fn notify() {
    let sink = SINK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(sink) = sink.as_ref() {
        sink();
    }
}

/// The registry as the status block paints it, owned — nothing outside this module ever holds the
/// lock, and nothing outside it can hold a view that disagrees with the walk.
pub struct Snapshot {
    pub sources: Vec<Source>,
    pub active: usize,
    pub failed: HashSet<Option<String>>,
    pub measuring: HashSet<Option<String>>,
    pub refresh_error: Option<String>,
}

pub fn snapshot() -> Snapshot {
    let reg = REGISTRY.lock().unwrap();
    Snapshot {
        sources: reg.sources.clone(),
        active: reg.active,
        failed: reg.failed.clone(),
        measuring: reg.measuring.clone(),
        refresh_error: reg.refresh_error.clone(),
    }
}

/// Take `sources` as the ranking this process uses, and start at its head.
///
/// An EMPTY slice is ignored, and that is what makes `active`'s "never out of range" an invariant
/// rather than a claim: `active = 0` indexes nothing in an empty list, and every producer of a
/// ranking already guarantees a non-empty one (`migrate` restores the built-in entry, `rebuild`
/// re-inserts it). So an empty one can only be a caller mistake, and adopting it would leave the
/// launcher with no source to walk at all — strictly worse than keeping the ranking it had.
pub fn adopt(sources: &[Source]) {
    if sources.is_empty() {
        return;
    }
    {
        let mut reg = REGISTRY.lock().unwrap();
        reg.sources = sources.to_vec();
        reg.active = 0;
    }
    notify();
}

/// The ranking as it stands, for an operation about to start walking it.
///
/// Taken ONCE per operation rather than re-read per attempt: a walk's `tried` set is meaningful
/// only against the list it was built from, and a `Wire` outlives a scheduler pass that may re-rank
/// underneath it. Both want a list that does not move while they are stepping through it, and the
/// next operation picks up whatever the ranking has become.
fn ranking() -> Vec<Source> {
    REGISTRY.lock().unwrap().sources.clone()
}

/// A source an operation may still try: the head of `ranking` not in `tried`.
///
/// `failed` is deliberately NOT consulted. A walk's own set is what stops it asking twice, and a
/// process-wide exclusion would mean a network that came back needed a restart — the second walk
/// after an outage has to be free to ask everything again.
fn next<'a>(ranking: &'a [Source], tried: &HashSet<Option<String>>) -> Option<&'a Source> {
    ranking.iter().find(|s| !tried.contains(&s.url))
}

/// The source an operation just used. Moves the `active` marker the status block paints.
pub fn report_active(key: Option<&str>) {
    mark(key, false);
}

/// The same, plus a red row: this source failed an operation in this process.
pub fn report_failed(key: Option<&str>) {
    mark(key, true);
}

fn mark(key: Option<&str>, failed: bool) {
    {
        let mut reg = REGISTRY.lock().unwrap();
        let owned = key.map(str::to_string);
        // Only when the key is one of ours. A walk captured its ranking before a refresh replaced
        // the list, so the source it is reporting may no longer be in it — and the honest answer
        // then is that the marker has not moved, never an index into a list that has changed
        // under it.
        if let Some(i) = reg.sources.iter().position(|s| s.url == owned) {
            reg.active = i;
        }
        if failed {
            reg.failed.insert(owned);
        }
    }
    notify();
}

fn set_refresh_error(why: Option<String>) {
    REGISTRY.lock().unwrap().refresh_error = why;
}

fn begin_measuring(want: &HashSet<Option<String>>) {
    REGISTRY.lock().unwrap().measuring = want.clone();
    notify();
}

/// A measuring pass has finished: nothing is in flight, and the sources it ASKED are a settled
/// question again.
///
/// Only those. `failed` is what a walk reported — "this source failed an operation in this
/// process" — and a pass answers that by going and asking. A pass that measured a subset (the
/// scheduler's hourly retry of failures, which deliberately leaves healthy sources alone) has said
/// nothing about the rest, so clearing their rows would drop a red mark for a source nobody
/// re-asked and nothing has heard from since.
fn end_measuring(asked: &HashSet<Option<String>>) {
    {
        let mut reg = REGISTRY.lock().unwrap();
        reg.measuring.clear();
        reg.failed.retain(|k| !asked.contains(k));
    }
    notify();
}

// ---------------------------------------------------------------- opening a source

/// How a source is turned into a backend.
///
/// A parameter rather than a call, for two callers that cannot use the real one: the tests (every
/// production backend is an https-only agent no loopback listener can satisfy, and what a walk is
/// about is the ORDER, not the transport) and the debug CLI's `--repo`/`--game-repo`, which pin a
/// run to GitHub because a repo override is meaningful to nothing else — a mirror is addressed by
/// payload directory, so there is nothing for a repo name to override and routing it at one would
/// serve some other repo's tree.
pub(crate) type Dial = Box<dyn Fn(&Source) -> Arc<dyn Downloader> + Send + Sync>;

/// The production dial: GitHub for the urlless entry, a `Mirror` for everything else.
///
/// THIS IS THE ONE PLACE the launcher acts on "every mirror carries every payload". A published
/// list says nothing about which trees a host holds and the client asks nothing about it, so a
/// per-payload routing rule — if one is ever wanted — comes back through here and nowhere else.
fn dial_for(settings: &Settings, repo: &str, payload: Payload) -> Dial {
    let settings = settings.clone();
    let repo = repo.to_string();
    Box::new(move |s: &Source| match s.key() {
        None => Arc::new(Github::for_repo(&settings, &repo)) as Arc<dyn Downloader>,
        Some(url) => Arc::new(Mirror::new(url, payload)),
    })
}

/// Is this a fact about the SOURCE, or about US? Only the second ends a walk early.
///
/// `UnsupportedSchema`/`UnsupportedCodec` say "this launcher cannot read what was published", which
/// every other source will say too — asking them all buys N round trips to reach the same sentence.
/// `Cancelled` is an instruction and is never a failover.
fn ours(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<UnsupportedSchema>().is_some()
            || c.downcast_ref::<UnsupportedCodec>().is_some()
            || c.downcast_ref::<engine::Cancelled>().is_some()
    })
}

/// Run `op` against the active source, failing over and RETRYING the whole operation. The only way
/// to reach a backend for a payload.
///
/// A 5xx here fails over on the first answer; there is no per-source retry loop. The list IS the
/// retry — a host that answered 503 once will answer it again in the second it would take to ask.
pub fn with_active<T>(
    settings: &Settings,
    repo: &str,
    payload: Payload,
    tag: Option<&str>,
    op: impl Fn(&dyn Downloader, &Release) -> Result<T>,
) -> Result<T> {
    walk(&dial_for(settings, repo, payload), &ranking(), repo, tag, op)
}

/// `with_active` with the dial and the ranking injected — see `Dial`. `pub(crate)` for the tests
/// in the modules that USE a walk (self-update's failover is a property of self-update, not of
/// this file), which is the same seam `Wire::with_dial` opens for the same reason.
pub(crate) fn walk<T>(
    dial: &Dial,
    ranking: &[Source],
    repo: &str,
    tag: Option<&str>,
    op: impl Fn(&dyn Downloader, &Release) -> Result<T>,
) -> Result<T> {
    each_source(ranking, &mut HashSet::new(), |source| {
        let dl = dial(source);
        let release = dl.fetch_release(repo, tag)?;
        op(dl.as_ref(), &release)
    })
}

/// The walk itself: ask each source of `ranking` that is not already in `tried`, in order, until
/// one answers.
///
/// The three rules live HERE and nowhere else, which is the point of the function: a single-shot
/// read and a `Wire` opening its next source differ only in what they do with the source, and two
/// copies of "mark it, advance, and stop early when the failure is ours" would be free to disagree
/// about the case that decides everything.
///
///   * an answer ends it, and the source that gave it becomes the active one;
///   * a failure that is OURS (`ours`) ends it too, and does NOT mark the source — it answered,
///     and we could not read the answer;
///   * anything else marks the source failed, adds it to `tried`, and moves on. An exhausted list
///     reports the LAST error, which is the one from the source closest to being usable.
///
/// `tried` is the caller's, so a `Wire` that swaps repeatedly never returns to a source it has
/// already given up on, while a fresh single-shot read is free to ask everything again — which is
/// what makes a network that came back need no restart.
fn each_source<T>(
    ranking: &[Source],
    tried: &mut HashSet<Option<String>>,
    attempt: impl Fn(&Source) -> Result<T>,
) -> Result<T> {
    let mut last: Option<anyhow::Error> = None;
    while let Some(source) = next(ranking, tried) {
        match attempt(source) {
            Ok(v) => {
                report_active(source.key());
                return Ok(v);
            }
            Err(e) if ours(&e) => {
                report_active(source.key());
                return Err(e);
            }
            Err(e) => {
                report_failed(source.key());
                tried.insert(source.url.clone());
                last = Some(e);
            }
        }
    }
    Err(match last {
        Some(e) => e.context("every download source failed"),
        None => anyhow::anyhow!("no download source is configured"),
    })
}

/// One source, opened: which entry it was, its backend, and the release it serves.
struct Opened {
    key: Option<String>,
    dl: Arc<dyn Downloader>,
    release: Release,
}

/// Open the first source not in `tried` that can serve `repo` at `tag`.
fn open_next(
    dial: &Dial,
    ranking: &[Source],
    repo: &str,
    payload: Payload,
    tag: Option<&str>,
    tried: &mut HashSet<Option<String>>,
) -> Result<Opened> {
    each_source(ranking, tried, |source| {
        let dl = dial(source);
        let release = dl.fetch_release(repo, tag)?;
        // The base game's file assets may live sharded across prereleases (GitHub caps 1000 assets
        // per release), so each source's list is folded into ITSELF — per source, because the
        // shards are that source's and a release index published by one host can never be used to
        // address another. A mirror answers this with the one release it serves, which folds to
        // itself.
        let release = match payload {
            Payload::Game => engine::merged_game_release(dl.as_ref(), repo, release)?,
            _ => release,
        };
        Ok(Opened { key: source.url.clone(), dl, release })
    })
}

// ---------------------------------------------------------------- addressing

/// A content-addressed backend's asset for a payload entry: the entry's HASH is its address, so the
/// asset is SYNTHESIZED. That is the contract `Downloader::content_addressed` documents, and the
/// backend reads `name` back as the hash.
fn by_content(sha256: &str, size: u64) -> Asset {
    Asset {
        name: sha256.to_string(),
        url: String::new(),
        browser_download_url: String::new(),
        size,
    }
}

/// THE rule for turning a manifest entry into the asset a given source can serve — hash on a
/// content-addressed backend, `name` looked up in the release index on a name-addressed one.
///
/// This is the single-entry form, for a caller that resolves ONE file and would pay to build an
/// index it consults once (self-update). `Resolved` below is the same rule with the lookup table
/// built up front, which is what a pool of eight workers against a release carrying thousands of
/// assets needs. Two call shapes, one rule: a second copy of "name on GitHub, hash on a mirror"
/// would be free to drift, and the symptom would be a launcher that installs from one source and
/// 404s on the other.
pub fn asset_for(
    dl: &dyn Downloader,
    release: &Release,
    name: &str,
    sha256: &str,
    size: u64,
) -> Option<Asset> {
    match dl.content_addressed() {
        true => Some(by_content(sha256, size)),
        false => release.asset(name).cloned(),
    }
}

/// A backend plus the asset table its entry names resolve against — the form the download pool
/// uses.
///
/// OWNED, not borrowed: a `Wire` swaps one of these under a read-write lock while eight workers
/// are reading it, and a borrow would have to come from somewhere that outlives the swap.
pub struct Resolved {
    dl: Arc<dyn Downloader>,
    /// Empty for a content-addressed backend, which has no release index to build one from.
    index: HashMap<String, Asset>,
    by_hash: bool,
}

impl Resolved {
    /// The index is built ONCE per source rather than per job: `Release::asset` is a linear scan,
    /// which is fine for the handful of lookups the shim does and quadratic for the base game —
    /// 4,635 jobs against a release carrying 4,636 assets is ~10 million string comparisons.
    pub fn new(dl: Arc<dyn Downloader>, release: &Release) -> Self {
        let by_hash = dl.content_addressed();
        let index = match by_hash {
            true => HashMap::new(),
            false => release.assets.iter().map(|a| (a.name.clone(), a.clone())).collect(),
        };
        Self { dl, index, by_hash }
    }

    pub fn dl(&self) -> &dyn Downloader {
        self.dl.as_ref()
    }

    /// The fetchable asset for a payload entry, or None when this source cannot address it at all.
    /// `asset_for` above is the same rule; this one answers from the prebuilt index.
    pub fn asset_for(&self, name: &str, sha256: &str, size: u64) -> Option<Asset> {
        match self.by_hash {
            true => Some(by_content(sha256, size)),
            false => self.index.get(name).cloned(),
        }
    }

    /// Could this source deliver `name` at all? The install preflight's question.
    pub fn carries(&self, name: &str) -> bool {
        self.by_hash || self.index.contains_key(name)
    }
}

// ---------------------------------------------------------------- the wire

/// What a `Wire` is currently pulling from. Swapped wholesale under the write lock.
struct Live {
    gen: u64,
    key: Option<String>,
    resolved: Arc<Resolved>,
    release: Arc<Release>,
}

/// The source a long operation is pulling from, swappable mid-run.
///
/// The download pool reads `current()` per attempt and hands the generation back with any failure,
/// so eight workers failing at once cause ONE failover, not eight. `tag` is what makes a swap safe:
/// a new source is opened for the SAME release, so identity keeps coming from the manifest already
/// verified and only the bytes move.
pub struct Wire {
    repo: String,
    payload: Payload,
    /// Pinned at open. A source serving another release is skipped rather than quietly installed.
    tag: String,
    /// The ranking this run walks, captured at open. A download can outlive a scheduler pass that
    /// re-ranks, and `tried` is only meaningful against the list it was built from — a run whose
    /// order changed under it could ask a source it had already given up on, or skip one it had
    /// not. The next operation opens against whatever the ranking has become.
    ranking: Vec<Source>,
    settings: Settings,
    dial: Dial,
    inner: RwLock<Live>,
    /// Sources this run has given up on. Owned by the wire, not the registry: it is what makes
    /// "each source is asked at most once per operation" structural.
    tried: Mutex<HashSet<Option<String>>>,
}

impl Wire {
    /// Open the best source that can serve `repo` at `tag`.
    pub fn open(settings: &Settings, repo: &str, payload: Payload, tag: Option<&str>) -> Result<Self> {
        Self::with_dial(dial_for(settings, repo, payload), ranking(), settings, repo, payload, tag)
    }

    /// `open` with the dial and the ranking injected — see `Dial`. The tests hand it both, which is
    /// what lets a swap be exercised over in-memory backends and a known order, with no process
    /// state to take turns over.
    pub(crate) fn with_dial(
        dial: Dial,
        ranking: Vec<Source>,
        settings: &Settings,
        repo: &str,
        payload: Payload,
        tag: Option<&str>,
    ) -> Result<Self> {
        let mut tried = HashSet::new();
        let opened = open_next(&dial, &ranking, repo, payload, tag, &mut tried)?;
        Ok(Self {
            repo: repo.to_string(),
            payload,
            // The tag the SOURCE named, not the one asked for: with `tag: None` there is no other
            // answer, and every later swap is pinned to this one so the run cannot drift releases.
            tag: opened.release.tag_name.clone(),
            ranking,
            settings: settings.clone(),
            dial,
            inner: RwLock::new(Live {
                gen: 0,
                key: opened.key,
                resolved: Arc::new(Resolved::new(opened.dl, &opened.release)),
                release: Arc::new(opened.release),
            }),
            tried: Mutex::new(tried),
        })
    }

    /// The source in use right now, and the generation it belongs to. A worker hands that
    /// generation back with any failure; that is how the pool learns, without polling.
    pub fn current(&self) -> (u64, Arc<Resolved>, Arc<Release>) {
        let live = self.inner.read().unwrap();
        (live.gen, live.resolved.clone(), live.release.clone())
    }

    pub fn release(&self) -> Arc<Release> {
        self.inner.read().unwrap().release.clone()
    }

    /// Fail the source `seen` was taken from and open the next.
    ///
    /// `Ok(false)` = somebody already switched; the caller just re-reads `current()` and retries
    /// the same work against the new source. `Err` = the list is exhausted.
    pub fn fail(&self, seen: u64) -> Result<bool> {
        // Under a READ lock first. Eight workers fail at once and seven of them are reporting a
        // generation somebody has already moved past — the case the pool is built around, and the
        // one that is supposed to cost nothing. Asking the question under the write lock made each
        // of those seven wait out the whole failover round trip (for the base game: a release
        // lookup plus `merged_game_release`, or a mirror's two documents and a 4,600-entry parse)
        // before being told to just retry, and every worker's next `current()` queued behind the
        // same lock.
        if self.inner.read().unwrap().gen != seen {
            return Ok(false);
        }
        let mut live = self.inner.write().unwrap();
        // Asked again, because the two locks are not one: the worker that WON the race may have
        // finished swapping in the gap between them.
        if live.gen != seen {
            return Ok(false);
        }
        report_failed(live.key.as_deref());
        let mut tried = self.tried.lock().unwrap();
        tried.insert(live.key.clone());
        // Pinned to the tag this run opened with: a source serving a different release refuses
        // itself (`Mirror::fetch_release`, GitHub's tag lookup) and the walk moves past it.
        let opened = open_next(
            &self.dial,
            &self.ranking,
            &self.repo,
            self.payload,
            Some(&self.tag),
            &mut tried,
        )?;
        *live = Live {
            gen: live.gen + 1,
            key: opened.key,
            resolved: Arc::new(Resolved::new(opened.dl, &opened.release)),
            release: Arc::new(opened.release),
        };
        Ok(true)
    }

    /// The payload manifest, fetched and VERIFIED through the current source, failing over.
    ///
    /// The trust gate is INSIDE the loop, so a refused manifest is a source failure rather than the
    /// end of the operation — which cannot turn a refusal into an acceptance, only into another
    /// attempt at another host.
    pub fn manifest(&self) -> Result<Manifest> {
        loop {
            let (gen, resolved, release) = self.current();
            let got = engine::manifest_of(&self.settings, resolved.dl(), &release, self.payload);
            match got {
                Ok(m) => return Ok(m),
                Err(e) if ours(&e) => return Err(e),
                Err(e) => {
                    if self.fail(gen).is_err() {
                        return Err(e.context("every download source failed"));
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------- measuring

/// Unix seconds. Absolute, because what is persisted has to be readable by a later process.
fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Is this measurement old enough to be worth taking again?
///
/// `saturating_sub`, so a clock moved BACKWARDS makes everything look fresh — the safe direction:
/// no measuring, and a genuinely new source still triggers a pass on its own.
fn aged(m: &Measured, now: u64) -> bool {
    Duration::from_secs(now.saturating_sub(m.at)) >= MEASUREMENT_TTL
}

/// No settled answer: never measured, or measured badly long enough ago to be worth asking again.
///
/// The hourly re-trigger on a FAILED measurement is what stops a single offline boot freezing the
/// ranking: without it "nothing is unmeasured" would be true forever with every row red, while
/// downloads actually work again. A failed measurement is not a settled answer.
fn due(s: &Source, now: u64) -> bool {
    match &s.measured {
        None => true,
        Some(m) => !m.healthy() && aged(m, now),
    }
}

/// Measured, but long enough ago that a pass already running may as well re-ask.
fn stale(s: &Source, now: u64) -> bool {
    s.measured.as_ref().is_some_and(|m| aged(m, now))
}

/// What a LAUNCH measures, or `None` for "nothing is due".
///
/// A source with no settled answer STARTS a pass; staleness only WIDENS one. Nothing else starts
/// one — measuring costs a real transfer per source, and re-ordering the list unprompted is exactly
/// what would move a user off a source that works.
fn launch_set(sources: &[Source], now: u64) -> Option<HashSet<Option<String>>> {
    sources
        .iter()
        .any(|s| due(s, now))
        .then(|| sources.iter().filter(|s| due(s, now) || stale(s, now)).map(key_of).collect())
}

/// What a scheduler tick measures, or `None` for "nothing is due".
///
/// Two rules, and the second is the reason this is not launch-only: while EVERY source is dead
/// there is no ranking to disturb and nothing to lose, so everything is re-asked every
/// `ALL_DEAD_RETRY` until one answers. Otherwise only a source whose last measurement FAILED is
/// re-asked, and only once it is past the TTL. A healthy source is never re-measured on a timer.
fn timer_set(
    sources: &[Source],
    now: u64,
    all_dead_due: bool,
) -> Option<HashSet<Option<String>>> {
    if !sources.iter().any(|s| s.measured.as_ref().is_some_and(Measured::healthy)) {
        return all_dead_due.then(|| sources.iter().map(key_of).collect());
    }
    let retry: HashSet<Option<String>> =
        sources.iter().filter(|s| due(s, now)).map(key_of).collect();
    (!retry.is_empty()).then_some(retry)
}

fn key_of(s: &Source) -> Option<String> {
    s.url.clone()
}

/// Time the sources named in `want`, one thread each, leaving every other source's stored value
/// untouched.
///
/// Parallel because a probe is almost entirely network wait: measuring N sources serially costs
/// N × `PROBE_BUDGET`, which on a slow link is a multi-minute stretch with a stale ranking in
/// force. The count is published and small, so a thread each is simpler than a pool and costs
/// nothing worth managing.
fn measure(settings: &Settings, mut sources: Vec<Source>, want: &HashSet<Option<String>>) -> Vec<Source> {
    let jobs: Vec<usize> = sources
        .iter()
        .enumerate()
        .filter(|(_, s)| want.contains(&s.url))
        .map(|(i, _)| i)
        .collect();
    if jobs.is_empty() {
        return sources;
    }
    begin_measuring(want);
    let now = unix_now();
    let taken: Vec<Measured> = std::thread::scope(|scope| {
        let handles: Vec<_> = jobs
            .iter()
            .map(|&i| {
                let s = &sources[i];
                scope.spawn(move || probe(settings, s, now))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|_| Measured::failed(now, "the probe crashed")))
            .collect()
    });
    for (&i, m) in jobs.iter().zip(taken) {
        sources[i].measured = Some(m);
    }
    end_measuring(want);
    sources
}

/// One source, timed through the backend it really is: GitHub's release index and a ranged asset
/// read, or a mirror's payload manifest and a ranged blob read. Deliberately not unified — GitHub
/// IS a release index and a mirror has none at all.
fn probe(settings: &Settings, source: &Source, now: u64) -> Measured {
    match source.key() {
        None => crate::github::probe(settings, &settings.source_repo, now),
        Some(url) => mirror::probe(url, PROBE_PAYLOAD, now),
    }
}

/// The longest a measurement's failure reason may be.
///
/// `Measured.error` is not a log line: it is PERSISTED into settings.json, re-read and re-parsed on
/// every launch, and pushed to the webview on every `sources-changed`. On a mirror it can also
/// quote bytes that host chose — `serde_json`'s type errors reproduce the offending value in full,
/// so a manifest carrying a multi-megabyte string where a number belongs (bounded only by
/// `MAX_DOC_BYTES`) would otherwise land that string in the user's profile permanently. 200 is what
/// `github::net_err` has capped its own API snippet at since long before this; one rule, applied to
/// whichever backend is being distrusted rather than to one of them.
pub(crate) const REASON_MAX: usize = 200;

/// A probe's failure reason, capped at `REASON_MAX`.
///
/// CHARS, not bytes: the input is a foreign host's text, and cutting a `String` inside a UTF-8
/// sequence panics. Applied where each reason is BUILT rather than where a `Measured` is written —
/// there is one construction site per reason and several ways to store one, so the cap belongs on
/// the side that has exactly one of them.
pub(crate) fn short_reason(why: impl AsRef<str>) -> String {
    why.as_ref().chars().take(REASON_MAX).collect()
}

/// Read up to `PROBE_BYTES` under a wall-clock budget and record the throughput. Shared by both
/// probes, because throughput is the one number the ranking sorts on and two ways of measuring it
/// would be two rankings.
pub(crate) fn time_read(m: &mut Measured, mut reader: impl Read, what: &str) {
    let started = Instant::now();
    let mut buf = vec![0u8; 32 * 1024];
    let mut got: u64 = 0;
    let mut out_of_budget = false;
    loop {
        if started.elapsed() >= PROBE_BUDGET {
            out_of_budget = true;
            break;
        }
        match reader.read(&mut buf) {
            Ok(0) => break, // asset smaller than the chunk we asked for
            Ok(n) => {
                got += n as u64;
                if got >= PROBE_BYTES {
                    break;
                }
            }
            Err(e) => {
                if got == 0 {
                    m.error = Some(short_reason(format!("{what}: the transfer failed: {e}")));
                    return;
                }
                // A transfer that dies partway is broken, not slow — but the bytes that did arrive
                // are a real measurement, so keep both and let the ranking weigh them.
                m.error = Some(short_reason(format!("the transfer stalled after {} KiB", got / 1024)));
                break;
            }
        }
    }
    if got == 0 {
        m.error = Some("answered, but delivered no data".to_string());
        return;
    }
    m.bytes_per_sec = Some((got as f64 / started.elapsed().as_secs_f64().max(0.001)) as u64);
    // Running out of budget mid-chunk is a FAILURE, not merely a low score. This is the exact shape
    // of the throttled path the probe exists for — index instant, first few KiB instant, then a
    // drip — and reporting it as healthy-but-slow would paint a green row on a source that cannot
    // finish a download this decade. The measured rate rides along in the message.
    if out_of_budget && got < PROBE_BYTES {
        m.error = Some(format!("too slow — {} KiB in {}s", got / 1024, PROBE_BUDGET.as_secs()));
    }
}

// ---------------------------------------------------------------- the boot sequence

/// What a pass concluded: the sources as they should now stand, and the serial of any list accepted
/// on the way there — inseparable, which is why this wraps `mirror::Refresh` and `persist` is the
/// only way out. A caller that stored the sources alone would leave the anti-rollback floor at zero
/// forever, and the only symptom of that is a rollback nobody notices.
pub struct Outcome(mirror::Refresh);

impl Outcome {
    pub fn sources(&self) -> &[Source] {
        &self.0.sources
    }

    pub fn persist(&self) -> Result<()> {
        self.0.persist()
    }
}

/// Steps 2–6 of the boot sequence, as a PURE computation: bootstrap a baked list, refresh from the
/// ranking, decide what is due, measure it, rank. Writes no settings — the caller decides (the GUI
/// always does; the headless `sources` command only on `--save`).
pub fn refresh_and_measure(settings: &Settings) -> Outcome {
    let mut refresh = refresh_list(settings);
    let now = unix_now();
    if let Some(want) = launch_set(&refresh.sources, now) {
        refresh.sources = measure(settings, std::mem::take(&mut refresh.sources), &want);
        // Only when a pass actually ran. With nothing measured the ranking is the one the last pass
        // settled on, and re-sorting it would be a no-op at best.
        sort(&mut refresh.sources);
    }
    Outcome(refresh)
}

/// STABLE, so a list where nothing is measurable keeps GitHub at the front, where `migrate` put it.
fn sort(sources: &mut [Source]) {
    sources.sort_by_key(|s| config::rank(s.measured.as_ref()));
}

/// The published list as this launch should have it.
///
/// The BAKED bootstrap first, and only on a machine that has never accepted a list at all — the
/// serial floor already records that, so there is no new field and no new state. Then a refresh
/// from the ACTIVE source, failing over: a source that cannot serve the list is a source failure
/// like any other, and when every one of them is spent the existing list simply stands.
fn refresh_list(settings: &Settings) -> mirror::Refresh {
    let base = match mirror::bootstrap(settings) {
        // Adopted but NOT persisted: step 7 writes once, with whatever the refresh below concludes
        // and the higher of the two accepted serials. What adopting buys is that the freshly baked
        // hosts are reachable by the walk that follows, on the very launch that learned about them.
        Some(r) => {
            adopt(&r.sources);
            r
        }
        None => mirror::unchanged(&settings.sources),
    };
    refresh_list_with(settings, base, &|existing, source, floor| {
        mirror::refresh_from(settings, existing, source, floor)
    })
}

/// `refresh_list` over an already-decided `base`, with the fetch injected — the same seam as
/// `Dial`, for the same reason: the real one is an https-only agent no loopback listener can
/// satisfy, and what this function is about is which source is ASKED, what floor it is asked
/// against, and what happens when it will not answer.
fn refresh_list_with(
    settings: &Settings,
    base: mirror::Refresh,
    fetch: &dyn Fn(&[Source], &Source, u64) -> mirror::Refresh,
) -> mirror::Refresh {
    // `base.floor`, NOT the settings': a bootstrap has accepted a list at serial N and deliberately
    // not persisted it yet, so the settings still say 0 — and a fetch checked against 0 would take
    // a validly-signed OLDER list on top of the baked one, keep ITS hosts (`then` takes the later
    // sources) and persist them under a floor of N. A rollback on the one document that decides
    // where every future download comes from, on exactly the first run the bootstrap exists for.
    let floor = base.floor(settings);
    // The SAME walk every other read takes (`each_source`): mark, advance, retry. A source that
    // cannot serve the list is a source failure like any other, which cannot turn a refusal into an
    // application — `mirror::apply` still leaves the list exactly as it was — only into another
    // attempt at another host. `Refresh` carries its failure in a field rather than a `Result`, so
    // the two are bridged here and nowhere else.
    let asked = each_source(&ranking(), &mut HashSet::new(), |source| {
        let attempt = fetch(&base.sources, source, floor);
        match &attempt.error {
            None => Ok(attempt),
            Some(why) => Err(anyhow::anyhow!("{why}")),
        }
    });
    match asked {
        Ok(applied) => {
            set_refresh_error(None);
            base.then(applied)
        }
        // Nothing could be asked, or every copy was refused. Silence: the existing list stands, and
        // the reason is reported rather than acted on. A refused list is never an applied list and
        // never an empty one.
        Err(e) => {
            let why = format!("{e:#}");
            set_refresh_error(Some(why.clone()));
            let mut out = base;
            out.error = Some(why);
            out
        }
    }
}

/// Bring the source model up, once, from `main.rs`'s Tauri setup — never from the frontend. A
/// webview that fails to load must not be what decides whether sources get resolved, and the CLI
/// needs the same entry point.
///
/// Step 1 is synchronous, so every command is answerable from this instant and the active source
/// for the whole rest of the sequence is the head of the LAST ranking. Everything else runs on a
/// thread: boot must not wait on the network, and nothing on screen depends on it.
pub fn start() {
    let settings = Settings::load();
    // No `failed.clear()` here: `failed` is runtime-only and this runs once per process, so the
    // set is already empty — the line only ever looked like it was doing something. And if it ever
    // weren't empty, clearing it without `notify()` would leave the pushed view claiming red rows
    // the registry no longer holds. What answers a red row is a measuring pass (`end_measuring`),
    // which is the one thing that has been and asked.
    adopt(&settings.sources);
    std::thread::spawn(move || {
        let outcome = refresh_and_measure(&settings);
        let _ = outcome.persist();
        adopt(outcome.sources());
        scheduler();
    });
}

/// Keep the ranking honest for the rest of the process's life — see `MEASUREMENT_TTL` and
/// `ALL_DEAD_RETRY` for the two rules. A three-hour game download spans both of them, which is why
/// this is a running task and not a launch-time decision.
fn scheduler() {
    let mut last_all_dead = Instant::now();
    loop {
        std::thread::sleep(SCHEDULER_TICK);
        // memoized on mtime: one stat per tick for a value that changes twice a session
        let settings = Settings::load_cached();
        let all_dead_due = last_all_dead.elapsed() >= ALL_DEAD_RETRY;
        let Some(want) = timer_set(&settings.sources, unix_now(), all_dead_due) else { continue };
        if all_dead_due {
            last_all_dead = Instant::now();
        }
        let mut refresh = mirror::unchanged(&measure(&settings, settings.sources.clone(), &want));
        sort(&mut refresh.sources);
        // no list was accepted here, so nothing may raise the serial floor — `unchanged` is what
        // says so, and `persist` is still the only writer
        let _ = refresh.persist();
        adopt(&refresh.sources);
    }
}

#[cfg(test)]
mod tests {
    //! The walk and the schedule, over injected backends. Every production backend is an
    //! https-only agent no loopback listener can reach, which is exactly why the dial is a
    //! parameter: the rules below are about ORDER and RETRY, and they are transport-free.
    use super::*;
    use crate::downloader::fake::Fake;
    use crate::downloader::NetKind;
    use std::sync::atomic::{AtomicU32, Ordering};

    const DOC: &str = r#"{"version":"1.0.0","files":[]}"#;

    /// The registry is process-wide, and so is the settings file these would otherwise read — so
    /// the tests that adopt a ranking take turns. Poisoning is ignored: a panicking test has
    /// already failed, and letting it wedge every other one only hides which.
    static TURN: Mutex<()> = Mutex::new(());

    fn turn() -> std::sync::MutexGuard<'static, ()> {
        TURN.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A backend that answers the release lookup with a canned failure, counting the asks.
    struct Peer {
        inner: Fake,
        fails: Option<NetKind>,
        calls: AtomicU32,
    }

    impl Peer {
        fn serving() -> Arc<Self> {
            Arc::new(Self { inner: Fake::new("v1.0.0", DOC, vec![]), fails: None, calls: AtomicU32::new(0) })
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

    impl Downloader for Peer {
        fn fetch_release(&self, r: &str, t: Option<&str>) -> Result<Release> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.fails {
                Some(k) => Err(anyhow::Error::new(k).context("scripted failure")),
                None => self.inner.fetch_release(r, t),
            }
        }
        fn fetch_releases(&self, r: &str) -> Result<Vec<Release>> {
            self.inner.fetch_releases(r)
        }
        fn download(&self, a: &Asset) -> Result<Vec<u8>> {
            self.inner.download(a)
        }
        fn download_to(
            &self,
            a: &Asset,
            d: &std::path::Path,
            r: u64,
            p: crate::downloader::ChunkProgress,
        ) -> Result<(u64, String)> {
            self.inner.download_to(a, d, r, p)
        }
    }

    /// A ranking of `n` mirrors, ADOPTED — these tests are about what the registry ends up
    /// reporting as well as about the order — plus a dial that hands each entry its peer.
    fn ranked(peers: &[Arc<Peer>]) -> (Vec<Source>, Dial) {
        let sources: Vec<Source> =
            (0..peers.len()).map(|i| Source::at(format!("https://s{i}.example"))).collect();
        adopt(&sources);
        REGISTRY.lock().unwrap().failed.clear();
        let by_key: HashMap<Option<String>, Arc<Peer>> =
            sources.iter().map(|s| s.url.clone()).zip(peers.iter().cloned()).collect();
        let dial: Dial =
            Box::new(move |s: &Source| by_key[&s.url].clone() as Arc<dyn Downloader>);
        (sources, dial)
    }

    fn read(sources: &[Source], dial: &Dial) -> Result<String> {
        walk(dial, sources, "r", None, |_dl, rel| Ok(rel.tag_name.clone()))
    }

    /// RULE 3, both halves: a source that fails hands the operation to the next one, and the
    /// operation is RETRIED there rather than reported. The failure and the success both land in
    /// the registry, so the status block and the download path cannot come to disagree about which
    /// source is in use.
    #[test]
    fn a_failed_source_hands_the_operation_to_the_next_and_the_operation_retries() {
        let _t = turn();
        let (a, b) = (Peer::failing(NetKind::Transport), Peer::serving());
        let (sources, dial) = ranked(&[a.clone(), b.clone()]);

        assert_eq!(read(&sources, &dial).expect("the second source serves it"), "v1.0.0");
        assert_eq!(a.calls(), 1);
        assert_eq!(b.calls(), 1);

        let snap = snapshot();
        assert!(snap.failed.contains(&sources[0].url), "A is reported failed");
        assert!(!snap.failed.contains(&sources[1].url));
        assert_eq!(snap.active, 1, "B is what the block paints as in use");
    }

    /// Report; keep the last active; loop over nothing. And a network that came back needs no
    /// restart — the second walk asks everything again, because a walk's "already tried" set is
    /// LOCAL and the registry keeps no cursor.
    #[test]
    fn every_source_failing_reports_and_does_not_loop() {
        let _t = turn();
        let peers: Vec<Arc<Peer>> =
            (0..3).map(|_| Peer::failing(NetKind::Transport)).collect();
        let (sources, dial) = ranked(&peers);

        let err = read(&sources, &dial).unwrap_err();
        assert!(
            format!("{err:#}").contains("every download source failed"),
            "the exhausted walk has to say so: {err:#}"
        );
        for p in &peers {
            assert_eq!(p.calls(), 1, "each source is asked exactly once per operation");
        }
        let snap = snapshot();
        assert_eq!(snap.active, 2, "the last one tried stays active — there is no better answer");
        // containment, not a count: the registry is process-wide and other suites report into it
        assert!(sources.iter().all(|s| snap.failed.contains(&s.url)), "every one is reported");

        assert!(read(&sources, &dial).is_err());
        for p in &peers {
            assert_eq!(p.calls(), 2, "a second walk starts from a fresh set and asks them all");
        }
    }

    /// The `ours` predicate. "This launcher cannot read what was published" is a fact about US, and
    /// every other source will say it too — so asking them buys N round trips to reach the same
    /// sentence, and the source that answered is not marked as having failed.
    #[test]
    fn a_failure_that_is_ours_does_not_walk_the_list() {
        let _t = turn();
        let (a, b) = (Peer::serving(), Peer::serving());
        let (sources, dial) = ranked(&[a.clone(), b.clone()]);

        let err = walk(&dial, &sources, "r", None, |_dl, _rel| {
            Err::<(), _>(anyhow::Error::new(UnsupportedSchema { found: 9, supported: 3 }))
        })
        .unwrap_err();
        assert!(err.chain().any(|c| c.downcast_ref::<UnsupportedSchema>().is_some()));
        assert_eq!(a.calls(), 1);
        assert_eq!(b.calls(), 0, "the rest of the list has the same answer");
        let snap = snapshot();
        assert!(!snap.failed.contains(&sources[0].url), "the source answered — we could not read it");

        // …and a cancel is an instruction, never a failover
        let (a, b) = (Peer::serving(), Peer::serving());
        let (sources, dial) = ranked(&[a.clone(), b.clone()]);
        let err = walk(&dial, &sources, "r", None, |_dl, _rel| {
            Err::<(), _>(anyhow::Error::new(engine::Cancelled))
        })
        .unwrap_err();
        assert!(err.chain().any(|c| c.downcast_ref::<engine::Cancelled>().is_some()));
        assert_eq!(b.calls(), 0, "a cancel must never advance to the next source");
    }

    /// RULE 5. GitHub is a peer in the ranking, not a floor under it: a mirror that measures faster
    /// sorts ahead of it and is what the next operation uses. The stable sort is what keeps it at
    /// the front on a machine where nothing is measurable, which is where `migrate` put it.
    #[test]
    fn github_is_ranked_as_a_peer_and_nothing_prefers_it() {
        let at = |bps: u64| Some(Measured { bytes_per_sec: Some(bps), ..Measured::blank(1000) });
        let mut sources = vec![
            Source { url: None, measured: at(1_000_000) },
            Source { url: Some("https://fast.example".into()), measured: at(5_000_000) },
        ];
        sort(&mut sources);
        assert_eq!(sources[0].url.as_deref(), Some("https://fast.example"));
        assert!(sources[1].is_github());

        // nothing measurable at all: the order stands, GitHub included
        let mut none = vec![Source::default(), Source::at("https://a.example")];
        sort(&mut none);
        assert!(none[0].is_github(), "a stable sort cannot invent a preference");
    }

    /// `active` IS IN RANGE BY CONSTRUCTION, and this is the only way it could stop being.
    ///
    /// `adopt` sets the marker to the head of what it was handed, so an empty ranking would leave
    /// it pointing at nothing — and every producer of a ranking already refuses to make an empty
    /// one (`migrate` restores the built-in entry, `rebuild` re-inserts it), which makes an empty
    /// slice a caller's mistake and not an answer. Ignoring it keeps the last ranking, which is
    /// strictly better than a launcher with nowhere to download from. `mark` is the other half:
    /// a key that is not in the list moves nothing.
    #[test]
    fn adopt_never_leaves_the_registry_without_a_source() {
        let _t = turn();
        let sources = vec![Source::default(), Source::at("https://a.example")];
        adopt(&sources);
        report_active(Some("https://a.example"));
        assert_eq!(snapshot().active, 1);

        adopt(&[]);
        let snap = snapshot();
        assert_eq!(snap.sources, sources, "an empty ranking is not a ranking");
        assert_eq!(snap.active, 1, "…so nothing about the marker changed either");

        // and a source that is not in the list leaves the marker where it was
        report_active(Some("https://gone.example"));
        assert_eq!(snapshot().active, 1);
        assert!(snapshot().active < snapshot().sources.len(), "never out of range");
    }

    /// REQUIREMENT 4's trigger, all three parts: a source with no settled answer starts a pass,
    /// staleness only widens one, and a FAILED measurement becomes due again after the TTL.
    #[test]
    fn a_new_source_is_the_only_thing_that_starts_a_measuring_pass() {
        let hour = MEASUREMENT_TTL.as_secs();
        let healthy = |at: u64| Some(Measured { bytes_per_sec: Some(1), ..Measured::blank(at) });
        let now = 10 * hour;

        // everything healthy and fresh — and everything healthy but THREE HOURS old. Neither is a
        // reason to re-time the world: a stale healthy answer is still an answer.
        for age in [0, 3 * hour] {
            let all = vec![
                Source { url: None, measured: healthy(now - age) },
                Source { url: Some("https://a".into()), measured: healthy(now - age) },
            ];
            assert_eq!(launch_set(&all, now), None, "age {age}");
        }

        // one unmeasured entry starts a pass — and drags every STALE source into it, because a
        // ranking is a comparison and comparing a fresh number against a three-hour-old one is not
        // one. The fresh healthy source is left alone.
        let mixed = vec![
            Source { url: None, measured: healthy(now) },
            Source { url: Some("https://stale".into()), measured: healthy(now - 3 * hour) },
            Source::at("https://new"),
        ];
        let want = launch_set(&mixed, now).expect("an unmeasured source is due");
        assert_eq!(
            want,
            HashSet::from([Some("https://stale".to_string()), Some("https://new".to_string())])
        );

        // a FAILED measurement is not a settled answer: it becomes due again once it is old
        // enough, which is what stops one offline boot freezing the ranking with every row red.
        let failed = |at: u64| Source { url: None, measured: Some(Measured::failed(at, "down")) };
        assert_eq!(launch_set(&[failed(now)], now), None, "…but not before the TTL");
        let want = launch_set(&[failed(now - hour)], now).expect("an hour later it is");
        assert_eq!(want, HashSet::from([None]));
    }

    /// The running schedule (amendment 1): a healthy source is never re-measured on a timer, a
    /// failed one is re-measured hourly, and while NOTHING is healthy everything is re-measured on
    /// the short interval until something answers.
    #[test]
    fn the_scheduler_retries_failures_hourly_and_a_dead_world_every_two_minutes() {
        let hour = MEASUREMENT_TTL.as_secs();
        let now = 10 * hour;
        let healthy = |at: u64| Some(Measured { bytes_per_sec: Some(1), ..Measured::blank(at) });
        let dead = |at: u64| Some(Measured::failed(at, "down"));

        let one_works = vec![
            Source { url: None, measured: healthy(now) },
            Source { url: Some("https://a".into()), measured: dead(now) },
        ];
        assert_eq!(timer_set(&one_works, now, true), None, "a fresh failure waits out the TTL");
        let older = vec![
            Source { url: None, measured: healthy(now) },
            Source { url: Some("https://a".into()), measured: dead(now - hour) },
        ];
        assert_eq!(
            timer_set(&older, now, true),
            Some(HashSet::from([Some("https://a".to_string())])),
            "and only the failure is re-asked — the healthy source is left alone"
        );

        // nothing healthy anywhere: everything, on the short interval, and nothing in between
        let all_dead = vec![
            Source { url: None, measured: dead(now) },
            Source { url: Some("https://a".into()), measured: dead(now) },
        ];
        assert_eq!(timer_set(&all_dead, now, false), None, "…only when the interval is up");
        assert_eq!(
            timer_set(&all_dead, now, true),
            Some(HashSet::from([None, Some("https://a".to_string())]))
        );
    }

    /// REQUIREMENT 7. A source that cannot serve the published list is a source FAILURE like any
    /// other: mark it, move on, try the whole refresh again there. And when every copy is refused,
    /// that is silence — the existing list stands, untouched, with the reason reported rather than
    /// acted on. Collapsing "refused" into "there are none" is how a tampered answer would wipe a
    /// user's mirrors with nothing anywhere looking wrong.
    #[test]
    fn a_refused_list_fails_the_source_over_and_a_refusal_everywhere_is_silence() {
        let _t = turn();
        let existing = vec![Source::default(), Source::at("https://a"), Source::at("https://b")];
        adopt(&existing);
        REGISTRY.lock().unwrap().failed.clear();
        // A machine that has already accepted a list, so the BOOTSTRAP is out of the picture and
        // what is left is the walk. Without this the assertions below would describe one thing in a
        // build with a baked list and another in a build without one — and both would be right,
        // which is the same as neither being tested.
        let settings = Settings {
            sources: existing.clone(),
            max_serial_seen: [("mirrors".to_string(), 1u64)].into_iter().collect(),
            ..Settings::default()
        };
        let asked = Mutex::new(Vec::<Option<String>>::new());

        // the first two refuse it — a tampered document and a stale serial land in exactly the
        // same place as a host that could not be reached, which is the point
        let published = vec![Source::default(), Source::at("https://published")];
        let base = mirror::unchanged(&existing);
        let out = refresh_list_with(&settings, base, &|current, source, _floor| {
            asked.lock().unwrap().push(source.url.clone());
            match source.key() {
                Some("https://b") => mirror::unchanged(&published),
                _ => {
                    let mut r = mirror::unchanged(current);
                    r.error = Some(format!("{:?} refused it", source.key()));
                    r
                }
            }
        });
        assert_eq!(
            *asked.lock().unwrap(),
            [None, Some("https://a".to_string()), Some("https://b".to_string())],
            "each source is asked once, in ranking order, until one answers"
        );
        assert_eq!(out.sources, published, "and the answer that came is the one applied");
        assert!(out.error.is_none());
        assert!(snapshot().failed.contains(&None), "the sources that refused it are reported");

        // every copy refused: silence. The list stands and the reason is REPORTED, not acted on.
        let base = mirror::unchanged(&existing);
        let out = refresh_list_with(&settings, base, &|current, source, _floor| {
            let mut r = mirror::unchanged(current);
            r.error = Some(format!("{:?} refused it", source.key()));
            r
        });
        assert_eq!(out.sources, existing, "a refusal everywhere leaves the list exactly as it was");
        assert!(out.error.is_some());
        assert!(snapshot().refresh_error.is_some(), "…and the status block says why");
    }

    /// A pass settles only the sources it ASKED.
    ///
    /// `failed` is what a walk reported — "this source failed an operation in this process" — and
    /// the only thing that answers it is going and asking again. The scheduler's hourly retry
    /// measures a SUBSET on purpose (a healthy source is never re-timed on a timer), so clearing
    /// every row would drop a red mark for a source nobody re-asked and nothing has heard from
    /// since: a reporting claim the launcher has no evidence for.
    #[test]
    fn a_measuring_pass_settles_only_the_sources_it_asked() {
        let _t = turn();
        let sources = vec![Source::default(), Source::at("https://a"), Source::at("https://b")];
        adopt(&sources);
        {
            let mut reg = REGISTRY.lock().unwrap();
            reg.failed = sources.iter().map(key_of).collect();
        }

        end_measuring(&HashSet::from([Some("https://a".to_string())]));

        let snap = snapshot();
        assert!(
            !snap.failed.contains(&Some("https://a".to_string())),
            "the source the pass asked is a settled question again"
        );
        assert!(
            snap.failed.contains(&None) && snap.failed.contains(&Some("https://b".to_string())),
            "…and the ones it did not ask keep the row they earned: {:?}",
            snap.failed
        );
        assert!(snap.measuring.is_empty(), "and nothing is left in flight");
    }

    /// THE FIRST-RUN ROLLBACK. A baked bootstrap ACCEPTS a list at serial N and is deliberately not
    /// persisted until the one write at the end of the sequence — so `settings.serial_floor` is
    /// still 0 while the launch goes on to fetch the published list.
    ///
    /// A fetch checked against that 0 accepts a validly-signed OLDER list, and `Refresh::then`
    /// keeps the LATER sources with the HIGHER serial: the machine ends up believing the older
    /// list's hosts under a floor claiming N, which is precisely the rollback the ratchet exists to
    /// refuse — on the one document that decides where every future download comes from, on exactly
    /// the run the bootstrap exists for.
    #[test]
    fn a_fetched_list_older_than_the_baked_one_is_refused_on_first_run() {
        let _t = turn();
        let existing = vec![Source::default()];
        adopt(&existing);
        // a fresh machine: nothing persisted, so the settings floor is 0
        let settings = Settings { sources: existing.clone(), ..Settings::default() };
        assert_eq!(settings.serial_floor(Payload::Mirrors), 0);

        // …on top of a bootstrap that has accepted a list at 5
        let seen = Mutex::new(Vec::<u64>::new());
        let refuse_below_floor = |current: &[Source], _source: &Source, floor: u64| {
            seen.lock().unwrap().push(floor);
            let mut r = mirror::unchanged(current);
            if 4 < floor {
                r.error = Some("serial 4 is below the floor".into());
            }
            r
        };
        let out =
            refresh_list_with(&settings, mirror::accepted_at(&existing, 5), &refuse_below_floor);

        assert_eq!(
            *seen.lock().unwrap(),
            [5],
            "the fetch is checked against the floor the BOOTSTRAP established, not the settings' 0"
        );
        // the older list is refused, so it is silence: the baked hosts stand
        assert!(out.error.is_some());
        assert!(
            out.sources.iter().any(|s| s.key() == Some("https://baked.example")),
            "a refusal leaves the accepted list exactly as it was: {:?}",
            out.sources
        );

        // and a list that DOES clear the floor is applied on top, as before
        let published = vec![Source::default(), Source::at("https://newer.example")];
        let out = refresh_list_with(&settings, mirror::accepted_at(&existing, 5), &|_c, _s, _f| {
            mirror::unchanged(&published)
        });
        assert_eq!(out.sources, published);
        assert!(out.error.is_none());
    }

    /// A clock moved BACKWARDS must not make everything look due — that would re-time the world on
    /// a machine whose only problem is its clock. `saturating_sub` picks the safe direction: it
    /// looks fresh, and a genuinely new source still triggers a pass on its own.
    #[test]
    fn a_clock_that_moved_backwards_measures_nothing() {
        let future = Some(Measured { bytes_per_sec: Some(1), ..Measured::blank(9_000_000) });
        let sources = vec![Source { url: None, measured: future }];
        assert_eq!(launch_set(&sources, 1_000), None);
        assert_eq!(timer_set(&sources, 1_000, true), None);
    }
}
