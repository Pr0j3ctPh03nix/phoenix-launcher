//! Shared update logic: fetch the release + manifest, resolve the effective file set from the
//! user's option selections, and diff it against what is installed. `check` is the read-only
//! surface over this; `install` (in install.rs) reuses `fetch`, `resolve` and `plan`.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::config::Settings;
use crate::downloader::{Downloader, Release};
use crate::manifest::{FileEntry, Manifest, OptionEntry, OptionKind, UnsupportedSchema};
use crate::state::InstalledState;
use crate::trust::{self, Payload};
use crate::verify;

// ---- long-operation progress (the shell bridges these to UI events) ----

/// One progress tick of a long engine operation. Serializable: the shell forwards it to the UI
/// as-is (the `op-progress` event).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpProgress {
    /// Which operation, e.g. "install".
    pub op: &'static str,
    /// Which HALF of that operation: `plan` (reading what is already there) or `fetch`
    /// (downloading what is missing). A base run does both under one `op`, and the UI has one
    /// progress line for them, so it has to be able to tell them apart.
    ///
    /// It must be stated, never inferred. The UI used to infer it from "does this tick carry
    /// bytes", which is wrong in the one direction that matters: the PLAN narrates bytes too, for
    /// files big enough that hashing them would otherwise freeze the counter. One 300 MB VPK
    /// re-hashed on a retry was enough to make the line claim a download that had not started,
    /// and to seed the byte accumulator with hash progress — after which the real download's
    /// first tick drove the running total negative and pinned the bar at 0% for the whole run.
    pub phase: &'static str,
    /// Current item number, 1-based (item `current` of `total` is in progress).
    pub current: u64,
    /// Items total.
    pub total: u64,
    /// The item currently being worked (e.g. a dest path).
    pub item: Option<String>,
    /// Bytes of the current item done / total, when it's a download.
    pub bytes_done: Option<u64>,
    pub bytes_total: Option<u64>,
    /// True on the tick that finishes `item` (its bar is now complete). Downloads run in
    /// parallel, so ticks for different items interleave — the UI keys per-file state on `item`
    /// and uses this to settle a bar rather than inferring completion from `current`.
    pub done: bool,
}

/// Optional progress sink; `None` = headless (CLI, tests). Must be Send + Sync: phase-1
/// downloads run on a small worker pool and report from multiple threads at once (install
/// serializes the actual calls internally).
pub type Progress<'a> = Option<&'a (dyn Fn(OpProgress) + Send + Sync)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Installed file already matches the manifest hash.
    UpToDate,
    /// Installed but the hash differs — and the bytes there are OURS (what we last wrote at this
    /// dest, or a file that predates us and is preserved on displacement). Safe to overwrite.
    Update,
    /// Not present locally.
    Install,
    /// We placed it previously but it left the effective set (deselected option) — delete it.
    Remove,
    /// We wrote this dest, and what is there now is neither the manifest's bytes nor the bytes we
    /// wrote. Somebody else changed our file — or it rotted; from here the two are the same
    /// observation and nothing in this process can tell them apart.
    ///
    /// Apply still FIXES it. That is deliberate and was the hard call: refusing would mean a
    /// corrupted shim file could never be repaired by the button whose entire job is repairing
    /// shim files, and "Up to date" would sit above a broken install. What changes is that the
    /// user is told first — `CheckView::user_changed` drives a confirm that names the count and
    /// offers the files view — and that a pin makes it `Kept` instead, permanently.
    ///
    /// The asymmetry with `Remove` is on purpose: a removal of modified content is not reported at
    /// all (see `plan`). Overwriting a managed file with the content it is supposed to have is a
    /// repair; deleting a file we have stopped managing because somebody edited it is just
    /// deletion, and there is no version of that worth doing automatically.
    Modified,
    /// `Modified`, and the user pinned exactly these bytes as intentional (see keep.rs). Reported
    /// so it is never invisible, acted on only when named.
    Kept,
}

impl Action {
    /// Work the RELEASE brings: a file it adds, replaces, or retires. Distinct from the whole
    /// unattended set, because `Modified` is not the release's doing — it is the user's, and
    /// counting it as "an update is available" put an Update button over a folder where nothing
    /// new was on offer.
    pub fn is_release_change(self) -> bool {
        matches!(self, Action::Update | Action::Install | Action::Remove)
    }

    /// Does an ordinary apply act on this dest? Everything except `Kept` — a pin is the one
    /// instruction that survives without being restated.
    pub fn is_unattended(self) -> bool {
        matches!(self, Action::Update | Action::Install | Action::Remove | Action::Modified)
    }

    /// Is this somebody's own work sitting at one of our dests — the thing the files view exists
    /// to show, and the confirm exists to warn about?
    pub fn is_users(self) -> bool {
        matches!(self, Action::Modified | Action::Kept)
    }
}

#[derive(Debug)]
pub struct FileStatus {
    pub dest: String,
    pub action: Action,
    /// `Modified` reached by a pin EXPIRING because the release changed this file — as opposed to
    /// a difference nobody has ruled on. The two want opposite defaults: one is "take the new
    /// version", the other is "you already said keep mine, and only the other side moved".
    pub superseded: bool,
    /// The release ships something NEWER than the version this file's current state was
    /// established against — its pin's `theirs` if it has one, else the bytes we recorded
    /// installing. Orthogonal to `action`: "these are my bytes" and "there is a new version of
    /// this file" are two separate facts, and a row can carry both (it then reads "modified /
    /// update"). Using the pin as the baseline where one exists is what stops a re-pinned file
    /// from advertising the same update forever.
    pub update_available: bool,
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
    /// The manifest's display tree over files[] — presentational grouping for the files view.
    pub tree: Vec<crate::manifest::TreeNode>,
    /// Effective selection per option id (the user's valid choice, else the manifest default).
    pub selections: BTreeMap<String, serde_json::Value>,
}

impl CheckResult {
    /// Number of files that would change (written or removed).
    /// What the RELEASE would change here — the number behind "N file(s) to change", the Update
    /// button, and whether Play is held back. `Modified` is deliberately not counted: those files
    /// are the user's own, the install is complete without touching them, and blocking Play over
    /// somebody's mod (or counting it as an available update) claims something untrue.
    pub fn changes(&self) -> usize {
        self.files.iter().filter(|f| f.action.is_release_change()).count()
    }

    // No `users()` counter here on purpose. The two callers that once shared it want DIFFERENT
    // sets, and collapsing them into one number is what made the apply confirm overstate itself:
    // the view counts `Modified` (what apply would overwrite), while `install::Ctx::user_changed`
    // wants `Modified | Kept` (what must be preserved if displaced). Both read `Action::is_users`
    // or match the variant directly, where the distinction is visible at the call site.
}

/// A file the install must replace or delete is locked by a live process — the game keeps its
/// loaded DLLs and mmapped VPKs open. Rooted in the error chain so the shell puts a
/// "gameRunning" kind on the wire (the UI tells the user to close the game and retry).
#[derive(Debug, Clone)]
pub struct GameRunning(pub PathBuf);

impl std::fmt::Display for GameRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} is in use — close Dota 2 and try again", self.0.display())
    }
}

impl std::error::Error for GameRunning {}

/// Lenient dotted-numeric compare: is version `a` older than `b`? ("1.10.0" > "1.9.9"; a leading
/// "v" and missing segments are tolerated, unparsable pieces count as 0).
///
/// Used by `selfupdate` to compare this build against the launcher repo's release tags. Note it
/// has nothing to do with manifest compatibility — that is `manifest::schema` alone.
pub(crate) fn version_lt(a: &str, b: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        v.trim_start_matches('v').split('.').map(|s| s.parse().unwrap_or(0)).collect()
    }
    let (a, b) = (parts(a), parts(b));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x < y;
        }
    }
    false
}

/// The release asset every payload describes itself with.
pub const MANIFEST_ASSET: &str = "manifest.json";

/// The manifest.json of an already-fetched release, VERIFIED — for callers that resolved the
/// release themselves (the base-game commands probe repo credentials first and hold a `Release` by
/// the time they need the manifest, and self-update reads the launcher payload).
///
/// This is the trust boundary of the whole updater. Below it every sha256 the launcher acts on is
/// hearsay; above it they are what a pinned key said. Four gates, and the ORDER is load-bearing:
///
/// 1. **bounded read** — the document is buffered to be verified, so its size is a trust input
///    (`trust::MAX_DOC_BYTES`); an unbounded read hands a hostile host the process's memory before
///    a single check has run;
/// 2. **signature, BEFORE parsing** — the parser is the largest attack surface here, and running
///    it over unauthenticated bytes is the thing signing exists to stop;
/// 3. **format** — `Manifest::parse` then owns compatibility, exactly as before (a manifest from
///    the future fails as "update the launcher", never as a syntax error);
/// 4. **identity and freshness** — `trust::accept`: a valid signature says we produced this
///    document, not that it is the document that was asked for, nor that it is current.
///
/// Every refusal here reaches the user as "no release available" (`views::wire_kind` maps the
/// typed errors to `notFound`), and that is deliberate: an unverifiable release is one we do not
/// have. It must never turn into an error that stops somebody playing a game already installed
/// and clean — the failure mode of a signing scheme should be "no update today", not "no game".
pub fn manifest_of(
    settings: &Settings,
    dl: &dyn Downloader,
    release: &Release,
    payload: Payload,
) -> Result<Manifest> {
    let sig_name = format!("{MANIFEST_ASSET}{}", trust::SIG_SUFFIX);
    let manifest_asset = release
        .asset(MANIFEST_ASSET)
        .with_context(|| format!("the release has no {MANIFEST_ASSET} asset"))?;
    let sig_asset = release
        .asset(&sig_name)
        .ok_or_else(|| anyhow!(trust::TrustError::Unsigned(MANIFEST_ASSET.to_string())))?;

    let bytes = dl
        .download_limited(manifest_asset, trust::MAX_DOC_BYTES)
        .with_context(|| format!("downloading {MANIFEST_ASSET}"))?;
    let sig = dl
        .download_limited(sig_asset, trust::MAX_SIG_BYTES)
        .with_context(|| format!("downloading {sig_name}"))?;
    let sig = String::from_utf8(sig)
        .map_err(|_| anyhow!(crate::minisig::SigError::Malformed("not UTF-8")))?;
    // WHICH key signed is deliberately not acted on: a release signed by the cold spare installs
    // exactly like one signed by the active key (the spare exists for the day the other is gone,
    // and a client that treated it as suspicious would defeat its only purpose). The id is
    // returned so a future "signed by the recovery key" notice has something to read.
    let _key = trust::verify(&bytes, &sig)
        .map_err(anyhow::Error::new)
        .with_context(|| format!("verifying {MANIFEST_ASSET}"))?;

    let manifest = Manifest::parse(&bytes)?;
    // Checked against the floor, but NOT advanced past it — see `ratchet_installed`. Reading a
    // manifest is not consenting to it: this same call backs every poll, every "check for
    // updates", and every self-update offer the user goes on to decline.
    trust::accept(payload, &manifest, settings.serial_floor(payload)).map_err(anyhow::Error::new)?;
    Ok(manifest)
}

/// Advance the anti-rollback floor for a payload that has actually been INSTALLED.
///
/// Split from `manifest_of` deliberately. The floor is permanent and machine-wide, so what raises
/// it has to be an act the user committed to, not one they merely performed a lookup for. With it
/// on the read path, a release that was fetched and then rejected — by the user, or by a failure
/// later in the install — still floored the machine, and pulling that release left every client
/// that had polled once refusing the older good one with no in-band fix.
///
/// Takes the manifest rather than a bare serial so a caller cannot ratchet to a number that did
/// not come from a verified document.
pub fn ratchet_installed(settings: &Settings, payload: Payload, manifest: &Manifest) {
    if let Some(serial) = manifest.serial {
        ratchet(settings, payload, serial);
    }
}

/// Remember that this payload has been seen at `serial`, so nothing older is ever accepted again.
///
/// Only when it MOVES. `Settings::update` always saves, and most fetches are the same release
/// being checked again — writing settings.json every time would also bump its mtime, which is the
/// key `Settings::load_cached` memoizes on, so the three-second game-running poll would go back to
/// re-reading and re-parsing the file. `settings` is a snapshot and may be stale, but it only
/// decides whether to bother: `update` re-loads, and the ratchet is monotonic either way.
///
/// Best-effort: a failed write costs the ratchet one step, never the update. Skipped entirely in
/// test builds — `Settings::update` writes the REAL user config (there is one config path per
/// machine, not per test), and a suite that quietly edits the developer's settings.json is a worse
/// bug than an untested line. What it persists is `Settings::advance_serial`, which is tested
/// directly, and what it protects is `trust::accept`, which is tested against explicit floors.
fn ratchet(settings: &Settings, payload: Payload, serial: u64) {
    #[cfg(not(test))]
    if settings.serial_is_newer(payload, serial) {
        let _ = Settings::update(|s| {
            s.advance_serial(payload, serial);
        });
    }
    #[cfg(test)]
    let _ = (settings, payload, serial);
}

/// Merge every release of the game repo's assets into the manifest release.
///
/// Historical: GitHub caps a release at 1,000 assets, and before manifest schema 3 the ~4.6k
/// base-game files were SHARDED across `<tag>-assets-N` prereleases (the versioned release —
/// always the repo's latest, since prereleases never resolve as latest — carried manifest.json).
/// Bundles collapsed the tree to ~146 assets in ONE release, so today this folds a single
/// release into itself — a harmless no-op, kept in case a sharded release ever reappears.
/// First name wins on a clash (the manifest release outranks shards).
pub fn merged_game_release(dl: &dyn Downloader, repo: &str, mut main: Release) -> Result<Release> {
    let all = dl.fetch_releases(repo).context("listing the game repo's asset shards")?;
    let mut have: HashSet<String> = main.assets.iter().map(|a| a.name.clone()).collect();
    for r in all {
        if r.tag_name == main.tag_name {
            continue;
        }
        for a in r.assets {
            if have.insert(a.name.clone()) {
                main.assets.push(a);
            }
        }
    }
    Ok(main)
}

/// The user cancelled a long operation (the base-game download). Rooted in the error chain so the
/// shell can put a `cancelled` kind on the wire — the UI closes quietly instead of painting an
/// error for something the user asked for.
#[derive(Debug, Clone, Copy)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// One release's "What's new" entry, for the version-history view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotesEntry {
    pub tag: String,
    pub version: String,
    pub notes: String,
}

/// The notes history plus its freshness key. Persisted to disk (next to settings.json) so
/// "What's new" opens instantly across app restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotesCache {
    pub repo: String,
    /// The repo's latest release tag when this history was built. The freshness key — the first
    /// entry's tag can't serve, since the latest release may carry no notes.
    pub latest_tag: String,
    pub entries: Vec<NotesEntry>,
    /// Built from a source that has NO RELEASE INDEX, so this is not the archive — it is whatever
    /// one release could say. A mirror serves exactly one release per payload, so a history built
    /// through one is a single entry (or none at all, for the launcher, whose notes live in the
    /// listing). That answer is honest while it is on screen and worthless afterwards: nothing else
    /// in the cache records WHERE it came from, so a one-entry history written to disk under the
    /// current tag was then served on every later launch, GitHub perfectly reachable, until the tag
    /// moved. `cmd::notes` treats it as never fresh and never saves it.
    ///
    /// `#[serde(default)]`: a cache written before this field existed reads as complete, which is
    /// what every cache written by a GitHub-reachable launcher actually was.
    #[serde(default)]
    pub partial: bool,
}

/// The file each history persists to. TWO files, not one keyed by repo: the shim's history and the
/// launcher's are read from different pages, at different times, and a single slot would make
/// opening either one evict the other — turning a page switch into a network round trip.
/// The shim's name is historical and stays: an existing cache must not be orphaned by the split.
pub const NOTES_FILE_SHIM: &str = "notes_cache.json";
pub const NOTES_FILE_LAUNCHER: &str = "launcher_notes.json";

fn notes_cache_path(file: &str) -> Option<PathBuf> {
    Settings::config_path().map(|p| p.with_file_name(file))
}

impl NotesCache {
    /// Best-effort disk load; None on any miss or parse failure.
    pub fn load(file: &str) -> Option<Self> {
        let text = std::fs::read_to_string(notes_cache_path(file)?).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Does this cache ANSWER for `repo` at `current_tag`? The freshness rule, on the type it is
    /// about rather than in the command that happens to ask.
    ///
    /// `current_tag` is the newest tag the corresponding check saw; `None` means "accept any cached
    /// history for this repo" — the UI only opens these views after a check anyway, and a history
    /// is worth more than a round trip proving it is still the same one. A PARTIAL history never
    /// answers, whatever tag it names: see the field.
    pub fn serves(&self, repo: &str, current_tag: Option<&str>) -> bool {
        !self.partial && self.repo == repo && current_tag.is_none_or(|t| t == self.latest_tag)
    }

    /// Best-effort disk save; a failure only costs a refetch next launch.
    ///
    /// A PARTIAL history is not written at all. `serves` already refuses to answer with one, but
    /// only the two together are enough: without this it would outlive the process that built it
    /// and sit in the profile of a machine that can reach GitHub perfectly well, waiting to seed a
    /// rebuild with a single release's notes.
    pub fn save(&self, file: &str) {
        if self.partial {
            return;
        }
        let Some(p) = notes_cache_path(file) else { return };
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string(self) {
            let _ = std::fs::write(p, json);
        }
    }
}

/// How many release manifests the notes-history rebuild downloads at once. A first-ever open
/// walks the whole release list — serial round trips would put N×RTT behind one spinner.
const NOTES_WORKERS: usize = 4;

/// The full "What's new" history: every release's manifest notes, newest first (GitHub's release
/// order). Incremental: a release whose tag appears in `known` keeps its cached entry with no
/// manifest download — only unseen tags cost a round trip, and those download in parallel
/// (NOTES_WORKERS). (Releases whose manifest carried no notes are not in `known` and re-download
/// on each rebuild; rebuilds only happen on a new release, so that stays cheap.) Releases
/// without a manifest.json, with an unparsable manifest, or with empty notes are skipped — a
/// single bad release must not sink the whole history.
pub fn fetch_notes_history(
    settings: &Settings,
    dl: &dyn Downloader,
    known: &[NotesEntry],
) -> Result<NotesCache> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    let all = dl.fetch_releases(&settings.source_repo).context("listing releases")?;
    // Drafts and prereleases are dropped. The LISTING carries them; `/releases/latest`, which
    // every check follows, does not — so keeping them would advertise a version the updater can
    // never install, and would date the cache against a tag no check will ever report, making the
    // freshness key miss on every open. Filtering here also makes `latest_tag` below agree with
    // `/releases/latest` by construction.
    let releases: Vec<&Release> = all.iter().filter(|r| r.is_published()).collect();
    let by_tag: BTreeMap<&str, &NotesEntry> =
        known.iter().map(|e| (e.tag.as_str(), e)).collect();
    // one slot per release keeps GitHub's newest-first order regardless of download timing
    let mut slots: Vec<Option<NotesEntry>> = releases
        .iter()
        .map(|r| by_tag.get(r.tag_name.as_str()).map(|e| (*e).clone()))
        .collect();
    let jobs: Vec<usize> =
        slots.iter().enumerate().filter_map(|(i, s)| s.is_none().then_some(i)).collect();
    let next = AtomicUsize::new(0);
    let fetched: Mutex<Vec<(usize, NotesEntry)>> = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..NOTES_WORKERS.min(jobs.len()) {
            s.spawn(|| loop {
                let j = next.fetch_add(1, Ordering::Relaxed);
                if j >= jobs.len() {
                    return;
                }
                let (i, rel) = (jobs[j], &releases[jobs[j]]);
                // Deliberately NOT verified, unlike `manifest_of`. This is the archive page: it
                // reads two display strings and yields no hash anything is ever installed from,
                // while a rebuild walks EVERY release — signature-checking it would double that
                // round trip count to authenticate prose. The notes that matter, the ones on the
                // release being offered, ride the verified manifest through `evaluate`.
                let Some(asset) = rel.asset(MANIFEST_ASSET) else { continue };
                let Ok(bytes) = dl.download_limited(asset, trust::MAX_DOC_BYTES) else { continue };
                // A garbage manifest is skipped (not fatal — one bad release must not sink the
                // whole history). A FUTURE-schema one is different: the full parse is off the
                // table, but `version`/`notes` are additive-stable strings, and its notes are
                // the ones most worth showing — they're where "update the launcher" gets
                // explained. Read just those two permissively instead of leaving a hole in the
                // history exactly there.
                let (version, notes) = match Manifest::parse(&bytes) {
                    Ok(m) => (m.version, m.notes),
                    Err(e) if e.chain().any(|c| c.downcast_ref::<UnsupportedSchema>().is_some()) => {
                        let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                            continue;
                        };
                        (
                            doc.get("version")
                                .and_then(|v| v.as_str())
                                .unwrap_or(rel.tag_name.trim_start_matches('v'))
                                .to_string(),
                            doc.get("notes").and_then(|v| v.as_str()).map(str::to_string),
                        )
                    }
                    Err(_) => continue,
                };
                if let Some(notes) = notes.filter(|n| !n.trim().is_empty()) {
                    fetched.lock().unwrap().push((
                        i,
                        NotesEntry { tag: rel.tag_name.clone(), version, notes },
                    ));
                }
            });
        }
    });
    for (i, e) in fetched.into_inner().unwrap() {
        slots[i] = Some(e);
    }
    let entries: Vec<NotesEntry> = slots.into_iter().flatten().collect();
    Ok(NotesCache {
        repo: settings.source_repo.clone(),
        latest_tag: releases.first().map(|r| r.tag_name.clone()).unwrap_or_default(),
        entries,
        // A content-addressed backend has no release index: `fetch_releases` answers with the one
        // release it serves, so what came back is that release's notes and not a history.
        partial: dl.content_addressed(),
    })
}

/// The LAUNCHER's version history, from an already-fetched release list.
///
/// A pure transform, and deliberately not a sibling of `fetch_notes_history`: the launcher
/// publishes its notes as the GitHub release DESCRIPTION (release.yml puts the annotated tag's
/// body there), so the listing already carries every entry inline. That makes this history one
/// API call with nothing to download per release and nothing to rebuild incrementally — the
/// opposite cost shape from the shim's, whose notes live inside each release's manifest.json.
///
/// A release with a blank body is skipped, the same rule `selfupdate::available` applies to the
/// pending update: an empty description is "no notes", not an empty section in the UI. The version
/// is the tag without its leading "v", also as `available` reports it — these two views name the
/// same build and must not disagree about what to call it.
/// Drafts and prereleases are excluded here for the same reason as in `fetch_notes_history`: this
/// page must not offer a version `launcher_check` will never see.
///
/// A GITHUB VIEW, and only ever that. The listing is the only place these bodies exist — a mirror
/// publishes no release index — so on a mirror this history is empty, and what a user there reads
/// about the release being offered comes from the update banner instead, out of the manifest that
/// release SIGNED (`selfupdate::available`). An archive assembled from prose a third-party host
/// hands over was not worth what it cost: nobody signs it, and the launcher renders it as its own
/// changelog with its links live.
pub fn launcher_notes_history(repo: &str, releases: &[Release], partial: bool) -> NotesCache {
    let published = || releases.iter().filter(|r| r.is_published());
    let entries = published()
        .filter_map(|r| {
            let notes = r.body.as_deref()?.trim();
            if notes.is_empty() {
                return None;
            }
            Some(NotesEntry {
                tag: r.tag_name.clone(),
                version: r.tag_name.trim_start_matches('v').to_string(),
                notes: notes.to_string(),
            })
        })
        .collect();
    NotesCache {
        repo: repo.to_string(),
        latest_tag: published().next().map(|r| r.tag_name.clone()).unwrap_or_default(),
        entries,
        // The caller's, because this is a pure transform over a list somebody else fetched and the
        // fact in question is about the SOURCE it came from — a backend with no release index
        // yields no history here at all, and an empty answer must not be cached as the archive.
        partial,
    }
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
    // What WE last wrote at each dest, and which bytes the user has approved there. Together they
    // answer the question a bare manifest comparison cannot: a file that matches neither the
    // manifest nor our own record was changed by somebody else, and is not ours to overwrite.
    //
    // ONE map answers all three questions this pass asks — "did we place this dest?" is
    // `contains_key`, "are these exactly the bytes we wrote?" is `get(..) == Some(&h)`, and
    // `baseline` reads the recorded hash directly. They were three collections built from three
    // traversals of the same Vec, two of them holding identical (dest, sha256) pairs; a dest
    // cannot legitimately be in one and not another, so any divergence between them could only
    // ever have been a silent misclassification of somebody's modified file.
    let keep = crate::keep::KeepList::load(game_dir);
    let prev_sha: std::collections::HashMap<&str, &str> = prev
        .map(|p| p.files.iter().map(|f| (f.dest.as_str(), f.sha256.as_str())).collect())
        .unwrap_or_default();
    let placed = |dest: &str| prev_sha.contains_key(dest);
    let ours = |dest: &str, h: &str| prev_sha.get(dest) == Some(&h);
    // What this file's current state was decided against: the release version its pin was weighed
    // over, or — with no pin — the bytes we recorded installing.
    let baseline = |dest: &str| -> Option<&str> {
        keep.files
            .get(dest)
            .and_then(|p| p.theirs())
            .or_else(|| prev_sha.get(dest).copied())
    };

    // How a local hash that is NOT the manifest's reads. Split out because the removal pass below
    // has to ask the same question about the same file, and the two answers must never diverge.
    let classify = |dest: &str, h: &str, theirs: &str| {
        if !placed(dest) || ours(dest, h) {
            // Not a file we placed (a genuine pre-existing one — `back_up` preserves it into the
            // vanilla store on displacement, which is what makes installing over it reversible),
            // or exactly the bytes we last wrote. Either way, ours to replace.
            Action::Update
        } else if keep.is_kept(dest, h, Some(theirs)) {
            Action::Kept
        } else {
            Action::Modified
        }
    };

    let mut out: Vec<FileStatus> = resolved
        .iter()
        .map(|f| {
            let local = game_dir.join(&f.dest);
            let mut local_hash = String::new();
            let action = match std::fs::metadata(&local) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Action::Install,
                // Present but unstattable, or the wrong LENGTH: either way this is not the file
                // the manifest describes, and the length says so without reading it — a content
                // hash implies a content length. This runs on every check, over a payload that
                // includes multi-hundred-MB VPKs.
                //
                // The shortcut is suspended for a dest we placed: "not the manifest's bytes" and
                // "not anybody's bytes we know" are different verdicts, and only the hash can
                // tell them apart. That set is a handful of files, not the base game's 4,635.
                Err(_) => Action::Update,
                Ok(md) if md.len() != f.size && !placed(f.dest.as_str()) => Action::Update,
                Ok(_) => match verify::sha256_file_cached(&local) {
                    Ok(h) if h == f.sha256 => Action::UpToDate,
                    Ok(h) => {
                        let a = classify(&f.dest, &h, &f.sha256);
                        local_hash = h;
                        a
                    }
                    // A read failure lands on Update, unlike the base-game plan which reports it
                    // apart (`BaseAction::Unreadable`). The asymmetry is deliberate: this set is
                    // a handful of files whose next action is `apply`, and apply's
                    // `probe_writable` diagnoses a lock or an ACL by name before downloading
                    // anything. A base verify has no such follow-up — it IS the diagnosis, over
                    // thousands of files, so there the cause has to travel with the verdict.
                    Err(_) => Action::Update,
                },
            };
            FileStatus {
                superseded: action == Action::Modified
                    && keep.superseded(&f.dest, &local_hash, Some(&f.sha256)),
                update_available: baseline(&f.dest).is_some_and(|b| b != f.sha256),
                dest: f.dest.clone(),
                action,
            }
        })
        .collect();

    let managed: HashSet<&str> = resolved.iter().map(|f| f.dest.as_str()).collect();
    // Dests where an earlier removal restored a preserved vanilla original: the file there is
    // STOCK, not ours. Without this skip it re-flags as Remove on every plan, and the next apply
    // undoes the restore (displaces the original back into the vanilla store) — the removal and
    // the restore chasing each other forever.
    let restored: HashSet<&str> = prev
        .map(|p| p.restored.iter().map(String::as_str).collect())
        .unwrap_or_default();
    // A deletion is as destructive as an overwrite, and unlike an overwrite it has no upside: the
    // dest is one we have STOPPED managing, so there is no correct content to put back and
    // nothing is repaired by removing it. If the bytes are no longer the ones we wrote, the file
    // is somebody's work — we drop the row entirely, which both leaves the file alone and stops
    // claiming it, so the extras scan reports it as what it now is: a file in the folder that
    // belongs to whoever put it there.
    //
    // A dest we never placed keeps its `Remove` (commit preserves it into the vanilla store, so
    // that path is already reversible); only our own files can be modified out from under us.
    let removal_action = |dest: &str| match verify::sha256_file_cached(&game_dir.join(dest)) {
        Ok(h) if placed(dest) && !ours(dest, h.as_str()) => None,
        // unreadable: it was ours by record, and a read failure is not evidence of a change
        _ => Some(Action::Remove),
    };
    let mut removed: HashSet<&str> = HashSet::new();
    if let Some(prev) = prev {
        for f in &prev.files {
            if !managed.contains(f.dest.as_str())
                && removed.insert(f.dest.as_str())
                && game_dir.join(&f.dest).exists()
            {
                if let Some(action) = removal_action(&f.dest) {
                    out.push(FileStatus { dest: f.dest.clone(), action, superseded: false, update_available: false });
                }
            }
        }
    }
    for r in remove {
        if !managed.contains(r.dest.as_str())
            && !restored.contains(r.dest.as_str())
            && removed.insert(r.dest.as_str())
            && game_dir.join(&r.dest).exists()
        {
            if let Some(action) = removal_action(&r.dest) {
                out.push(FileStatus { dest: r.dest.clone(), action, superseded: false, update_available: false });
            }
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
        tree: manifest.tree.clone(),
        selections: effective_selections(manifest, &settings.selections),
    })
}

/// Read-only check of ONE already-opened release: verify its manifest, evaluate it.
///
/// It takes the release rather than resolving one, so it composes with a walk: both callers (the
/// debug-only CLI and the test suite) get it handed to them by whatever opened the source, and the
/// trust gate below stays INSIDE that walk — a source whose manifest is refused fails over instead
/// of ending the check, exactly as the GUI's `check` command does with the same two steps. Release
/// builds compile neither caller, so the release-only dead-code silence below is accurate, and a
/// debug build still warns if this ever becomes genuinely unused.
#[cfg_attr(not(debug_assertions), allow(dead_code))]
pub fn check(settings: &Settings, dl: &dyn Downloader, release: &Release) -> Result<CheckResult> {
    let manifest = manifest_of(settings, dl, release, Payload::Mod)?;
    evaluate(settings, &release.tag_name, &manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::downloader::fake::Fake;
    use crate::state::InstalledFile;

    #[test]
    fn version_lt_compares_numerically() {
        assert!(version_lt("1.0.1", "1.0.2"));
        assert!(version_lt("1.9.9", "1.10.0"));
        assert!(version_lt("0.9", "1.0"));
        assert!(!version_lt("1.0.2", "1.0.1"));
        assert!(!version_lt("1.0.1", "1.0.1"));
        assert!(!version_lt("v1.2", "1.2.0")); // equal, not less
    }

    #[test]
    fn a_manifest_schema_this_build_cannot_read_is_refused_as_such() {
        use crate::manifest::{UnsupportedSchema, MAX_SCHEMA};
        let settings = Settings::default();
        let future = MAX_SCHEMA + 1;
        let too_new = Fake::new(
            "v9.9.9",
            &format!(r#"{{ "schema": {future}, "version": "9.9.9", "files": [] }}"#),
            vec![],
        );
        let rel = |d: &Fake| d.fetch_release("r", None).unwrap();
        let err = check(&settings, &too_new, &rel(&too_new)).unwrap_err();
        let refused = err
            .chain()
            .find_map(|c| c.downcast_ref::<UnsupportedSchema>())
            .expect("refused for the schema, not as a parse error");
        assert_eq!(refused.found, future);

        // a supported schema, and a legacy manifest with no `schema` key at all, both pass
        let ok = Fake::new("v1.0.0", r#"{ "schema": 2, "version": "1.0.0", "files": [] }"#, vec![]);
        assert!(check(&settings, &ok, &rel(&ok)).is_ok());
        let legacy = Fake::new("v1.0.0", r#"{ "version": "1.0.0", "files": [] }"#, vec![]);
        assert!(check(&settings, &legacy, &rel(&legacy)).is_ok());
    }

    /// The gate itself: a release whose manifest is not signed by a key we pin is not a release
    /// we have. Every variant is checked at the boundary the shell sees, because that is where the
    /// consequence lives — `views::wire_kind` turns each of these into `notFound`, which is in the
    /// frontend's soft set and therefore never blocks Play on an install that is already clean.
    #[test]
    fn an_unverifiable_manifest_is_refused_as_notfound() {
        use crate::minisig::SigError;
        use crate::trust::TrustError;
        let settings = Settings::default();
        let doc = r#"{"schema":2,"version":"1.0.0","files":[]}"#;
        let refusal = |dl: &dyn Downloader| -> String {
            let release = dl.fetch_release("r", None).unwrap();
            let e = manifest_of(&settings, dl, &release, Payload::Mod).unwrap_err();
            // EITHER typed refusal — the signature file's own (`SigError`) or the document's
            // identity (`TrustError`). The variants below produce both, and `wire_kind` has to
            // classify each of them; an assertion naming only one type would let the other fall
            // through to "internal", which the frontend treats as fatal.
            assert!(
                e.chain().any(|c| c.downcast_ref::<TrustError>().is_some()
                    || c.downcast_ref::<SigError>().is_some()),
                "must carry a typed trust failure so the shell can classify it: {e:#}"
            );
            assert_eq!(crate::views::CmdError::from(e).kind, "notFound");
            String::new()
        };

        // no signature at all
        refusal(&Fake::new("v1.0.0", doc, vec![]).unsigned());
        // a signature over a DIFFERENT document (the manifest was edited after signing)
        let mut edited = Fake::new("v1.0.0", doc, vec![]);
        let tampered = String::from_utf8(edited.assets["manifest.json"].clone())
            .unwrap()
            .replace("1.0.0", "9.9.9");
        edited.assets.insert("manifest.json".into(), tampered.into_bytes());
        refusal(&edited);
        // a signature file that is not a signature file
        let mut junk = Fake::new("v1.0.0", doc, vec![]);
        junk.assets.insert("manifest.json.minisig".into(), b"nonsense".to_vec());
        refusal(&junk);
    }

    /// A document signed by our own key, for a different payload, is not this payload. Without
    /// this the game repo could be answered with a perfectly valid shim manifest.
    #[test]
    fn a_manifest_for_another_payload_is_refused() {
        let settings = Settings::default();
        let dl = Fake::new("v1.0.0", r#"{"schema":2,"version":"1.0.0","files":[]}"#, vec![]);
        let release = dl.fetch_release("r", None).unwrap();
        assert!(manifest_of(&settings, &dl, &release, Payload::Mod).is_ok());
        let e = manifest_of(&settings, &dl, &release, Payload::Game).unwrap_err();
        assert!(format!("{e:#}").contains("\"game\" was asked for"), "got: {e:#}");
    }

    /// The rollback ratchet, from both directions. A mirror keeps a valid signature over every
    /// release it ever served, so "correctly signed" is not the same as "current".
    #[test]
    fn a_serial_below_the_floor_is_refused() {
        let doc = r#"{"schema":2,"version":"1.0.0","files":[]}"#;
        let base: u64 = 100;
        let dl = Fake::new("v1.0.0", doc, vec![]).serial(base + 5);
        let release = dl.fetch_release("r", None).unwrap();
        let at = |seen: u64| {
            let mut s = Settings::default();
            s.max_serial_seen.insert("mod".into(), seen);
            manifest_of(&s, &dl, &release, Payload::Mod)
        };
        assert!(at(base + 4).is_ok(), "newer than what we have seen");
        assert!(at(base + 5).is_ok(), "the same release, checked again — the common case");
        let e = at(base + 6).unwrap_err();
        assert!(format!("{e:#}").contains(&format!("older than {}", base + 6)), "got: {e:#}");

        // and a manifest with no serial cannot be shown to be current at all
        let dl = Fake::new("v1.0.0", doc, vec![]).without("serial");
        let e = manifest_of(&Settings::default(), &dl, &dl.fetch_release("r", None).unwrap(), Payload::Mod)
            .unwrap_err();
        assert!(format!("{e:#}").contains("no serial"), "got: {e:#}");
    }

    /// The floor is per payload and comes from the machine's own history — nothing else. A fresh
    /// install has none, which is correct: there is nothing yet to be rolled back FROM.
    #[test]
    fn the_floor_is_this_machines_history_and_nothing_else() {
        let mut s = Settings::default();
        assert_eq!(s.serial_floor(Payload::Mod), 0);
        s.max_serial_seen.insert("mod".into(), 12);
        assert_eq!(s.serial_floor(Payload::Mod), 12);
        assert_eq!(
            s.serial_floor(Payload::Game),
            0,
            "one payload's history says nothing about another's"
        );
    }

    /// A document is buffered whole to be verified, so its SIZE is a trust input: a host that
    /// answers manifest.json with an endless stream must be refused before the bytes are believed,
    /// not after they are in memory.
    #[test]
    fn a_document_over_the_cap_is_refused() {
        let big = vec![b'x'; 1024];
        let dl = Fake::new("v1.0.0", "{}", vec![("big", &big)]);
        let release = dl.fetch_release("r", None).unwrap();
        let asset = release.asset("big").unwrap();
        assert_eq!(dl.download_limited(asset, 1024).unwrap().len(), 1024, "exactly at the cap");
        assert!(dl.download_limited(asset, 1023).is_err(), "one byte over");
    }

    /// The wiring, not just `download_limited` in isolation: `manifest_of` itself has to refuse a
    /// `manifest.json` past the REAL 16 MiB ceiling, cleanly — this is the call every check, every
    /// self-update offer, and every install goes through.
    #[test]
    fn manifest_of_refuses_a_manifest_over_the_real_cap() {
        // valid, parseable JSON so the Fake signs it exactly as it would any other document — what
        // is under test is the size gate, not the parser.
        let padding = "x".repeat((trust::MAX_DOC_BYTES + 1) as usize);
        let doc = format!(r#"{{"version":"1.0.0","files":[],"padding":"{padding}"}}"#);
        let dl = Fake::new("v1.0.0", &doc, vec![]);
        let release = dl.fetch_release("r", None).unwrap();
        let e = manifest_of(&Settings::default(), &dl, &release, Payload::Mod).unwrap_err();
        assert!(format!("{e:#}").contains("larger than"), "expected the size-cap refusal, got: {e:#}");
    }

    #[test]
    fn merged_game_release_folds_shards_into_the_main_release() {
        use crate::downloader::{Asset, ChunkProgress};
        // a two-release repo: the Fake serves one release, so hand-roll a tiny double here
        struct Sharded;
        fn rel(tag: &str, names: &[&str]) -> Release {
            Release {
                tag_name: tag.into(),
                body: None,
                draft: false,
                prerelease: false,
                assets: names
                    .iter()
                    .map(|n| Asset {
                        name: (*n).into(),
                        url: String::new(),
                        browser_download_url: String::new(),
                        size: 0,
                    })
                    .collect(),
            }
        }
        impl Downloader for Sharded {
            fn fetch_release(&self, _r: &str, _t: Option<&str>) -> Result<Release> {
                Ok(rel("v1805", &["manifest.json"]))
            }
            fn fetch_releases(&self, _r: &str) -> Result<Vec<Release>> {
                Ok(vec![
                    rel("v1805", &["manifest.json"]),
                    rel("v1805-assets-1", &["a.vpk", "b.vpk"]),
                    // a stale shard repeating a name must not shadow the first-seen asset
                    rel("v1805-assets-2", &["c.vpk", "a.vpk"]),
                ])
            }
            fn download(&self, _a: &Asset) -> Result<Vec<u8>> {
                unreachable!()
            }
            fn download_to(&self, _a: &Asset, _d: &Path, _r: u64, _p: ChunkProgress) -> Result<(u64, String)> {
                unreachable!()
            }
        }

        let dl = Sharded;
        let main = dl.fetch_release("r", None).unwrap();
        let merged = merged_game_release(&dl, "r", main).unwrap();
        let mut names: Vec<&str> = merged.assets.iter().map(|a| a.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["a.vpk", "b.vpk", "c.vpk", "manifest.json"]);
        assert_eq!(merged.tag_name, "v1805");
    }

    #[test]
    fn notes_history_keeps_a_future_schema_release() {
        use crate::manifest::MAX_SCHEMA;
        let settings = Settings::default();
        // installable? no. But its notes are exactly where "update the launcher" is explained,
        // so the What's new history must not develop a hole at it
        // json!, not a string literal: the notes' markdown heading (`"###`) embeds every raw
        // string delimiter an r#-string could use
        let future = Fake::new(
            "v9.9.9",
            &serde_json::json!({
                "schema": MAX_SCHEMA + 1,
                "version": "9.9.9",
                "notes": "### Requires a newer launcher",
                "files": []
            })
            .to_string(),
            vec![],
        );
        let cache = fetch_notes_history(&settings, &future, &[]).unwrap();
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.entries[0].version, "9.9.9");
        assert!(cache.entries[0].notes.contains("newer launcher"));

        // truly malformed stays skipped, not fatal
        let garbage = Fake::new("v1.0.0", r#"{ "version": 42 }"#, vec![]);
        assert!(fetch_notes_history(&settings, &garbage, &[]).unwrap().entries.is_empty());
    }

    /// A prerelease is in the `/releases` listing and NOT in `/releases/latest`. Showing it would
    /// offer a version no check can ever see — and leave `latest_tag` naming a tag the freshness
    /// key never matches, so every open would refetch the whole history.
    #[test]
    fn a_prerelease_is_not_history() {
        let settings = Settings::default();
        let json = serde_json::json!({ "version": "9.9.9", "notes": "### Nope", "files": [] })
            .to_string();
        assert_eq!(fetch_notes_history(&settings, &Fake::new("v9.9.9", &json, vec![]), &[])
            .unwrap()
            .entries
            .len(), 1);
        let cache =
            fetch_notes_history(&settings, &Fake::new("v9.9.9", &json, vec![]).prerelease(), &[])
                .unwrap();
        assert!(cache.entries.is_empty(), "a prerelease has no place in the history");
        assert_eq!(cache.latest_tag, "", "nor may it date the cache");
    }

    fn launcher_rel(tag: &str, body: Option<&str>) -> Release {
        Release {
            tag_name: tag.into(),
            body: body.map(str::to_string),
            draft: false,
            prerelease: false,
            assets: vec![],
        }
    }

    #[test]
    fn launcher_history_reads_release_bodies() {
        let rels = vec![
            launcher_rel("v1.3.0", Some("#### Added\n- two pages")),
            launcher_rel("v1.2.9", Some("   \n  ")), // blank body = no notes, not an empty section
            launcher_rel("v1.2.8", None),            // no body at all
            launcher_rel("1.2.7", Some("plain")),    // a tag without the "v" still reports a version
        ];
        let c = launcher_notes_history("o/r", &rels, false);

        assert_eq!(c.repo, "o/r");
        // newest first, the listing's own order, with the note-less releases dropped
        let got: Vec<(&str, &str)> =
            c.entries.iter().map(|e| (e.tag.as_str(), e.version.as_str())).collect();
        assert_eq!(got, [("v1.3.0", "1.3.0"), ("1.2.7", "1.2.7")]);
        assert_eq!(c.entries[0].notes, "#### Added\n- two pages");
        // the freshness key is the newest RELEASE, not the newest entry — the latest one carrying
        // no notes must not silently date the cache to the one below it
        assert_eq!(c.latest_tag, "v1.3.0");

        assert_eq!(launcher_notes_history("o/r", &[], false).latest_tag, "");
    }

    #[test]
    fn launcher_history_skips_drafts_and_prereleases() {
        let mut draft = launcher_rel("v2.0.0", Some("unpublished"));
        draft.draft = true;
        let mut pre = launcher_rel("v1.9.0-rc1", Some("not for everyone"));
        pre.prerelease = true;
        let rels = vec![draft, pre, launcher_rel("v1.8.0", Some("shipped"))];

        let c = launcher_notes_history("o/r", &rels, false);
        assert_eq!(c.entries.len(), 1);
        assert_eq!(c.entries[0].tag, "v1.8.0");
        // and the key dates to the newest PUBLISHED release, which is what /releases/latest —
        // and therefore launcher_check — reports
        assert_eq!(c.latest_tag, "v1.8.0");
    }

    /// A HISTORY BUILT THROUGH A SOURCE WITH NO RELEASE INDEX IS NOT THE ARCHIVE.
    ///
    /// A mirror serves one release per payload, so `fetch_releases` answers with one and the
    /// "history" is that release's notes. Nothing in the cache recorded where it came from, so that
    /// single entry was written to disk keyed to the current tag and then served on every later
    /// launch — GitHub perfectly reachable — until the tag moved. It is marked instead, and a
    /// marked one neither answers nor persists.
    #[test]
    fn a_history_from_a_source_with_no_release_index_is_never_the_archive() {
        /// A `Fake` that addresses by content, which is the whole of what makes a backend
        /// index-less — the same answer `Mirror` gives.
        struct NoIndex(Fake);
        impl Downloader for NoIndex {
            fn content_addressed(&self) -> bool {
                true
            }
            fn fetch_release(&self, r: &str, t: Option<&str>) -> Result<Release> {
                self.0.fetch_release(r, t)
            }
            fn fetch_releases(&self, r: &str) -> Result<Vec<Release>> {
                self.0.fetch_releases(r)
            }
            fn download(&self, a: &crate::downloader::Asset) -> Result<Vec<u8>> {
                self.0.download(a)
            }
            fn download_to(
                &self,
                a: &crate::downloader::Asset,
                d: &std::path::Path,
                r: u64,
                p: crate::downloader::ChunkProgress,
            ) -> Result<(u64, String)> {
                self.0.download_to(a, d, r, p)
            }
        }

        let settings = Settings::default();
        let json =
            serde_json::json!({ "version": "9.9.9", "notes": "### One", "files": [] }).to_string();
        let indexed = Fake::new("v9.9.9", &json, vec![]);
        let complete = fetch_notes_history(&settings, &indexed, &[]).unwrap();
        assert!(!complete.partial, "a release index yields the archive it lists");
        assert!(complete.serves(&settings.source_repo, Some("v9.9.9")));

        let partial = fetch_notes_history(&settings, &NoIndex(indexed), &[]).unwrap();
        assert_eq!(partial.entries.len(), 1, "one release is all there is to have");
        assert!(partial.partial);
        assert!(
            !partial.serves(&settings.source_repo, Some("v9.9.9")),
            "…and it must not answer for the archive, however current its tag is"
        );
        assert!(!partial.serves(&settings.source_repo, None), "not even unkeyed");

        // the launcher's history takes the same fact from its caller, which is the one holding the
        // backend — and there it is EMPTY, since those notes exist only in a release listing
        assert!(launcher_notes_history("o/r", &[], true).partial);
        assert!(!launcher_notes_history("o/r", &[], false).partial);
    }

    /// The two histories must not share a file: one slot keyed by repo would make opening either
    /// page evict the other's cache, turning a tab switch into a network round trip.
    #[test]
    fn the_two_histories_cache_to_different_files() {
        assert_ne!(NOTES_FILE_SHIM, NOTES_FILE_LAUNCHER);
        let (shim, launcher) =
            (notes_cache_path(NOTES_FILE_SHIM), notes_cache_path(NOTES_FILE_LAUNCHER));
        assert_ne!(shim, launcher);
        // both beside settings.json, or neither (no config dir on this machine)
        assert_eq!(shim.is_some(), launcher.is_some());
        if let (Some(s), Some(l)) = (shim, launcher) {
            assert_eq!(s.parent(), l.parent());
            assert_eq!(s.file_name().unwrap(), NOTES_FILE_SHIM);
            assert_eq!(l.file_name().unwrap(), NOTES_FILE_LAUNCHER);
        }
    }

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

    /// The scoring order: length first, hash second. Each case gets its OWN dest on purpose —
    /// rewriting one path at the same length is invisible to the (size, mtime) hash memo (two
    /// writes microseconds apart share an mtime on Windows), which would make this flaky rather
    /// than wrong.
    #[test]
    fn plan_scores_by_length_then_hash() {
        use sha2::Digest;
        let dir = std::env::temp_dir().join("phoenix-engine-test-size");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("game/dota")).unwrap();
        let good = hex::encode(sha2::Sha256::digest(b"GOOD"));
        let entry = |dest: &str| {
            serde_json::json!({ "name": "a.vpk", "dest": dest, "sha256": good, "size": 4 })
        };
        let m: Manifest = serde_json::from_value(serde_json::json!({
            "version": "1.0.0",
            "files": [entry("game/dota/ok.vpk"), entry("game/dota/bad.vpk"), entry("game/dota/long.vpk")]
        }))
        .unwrap();

        std::fs::write(dir.join("game/dota/ok.vpk"), b"GOOD").unwrap();
        std::fs::write(dir.join("game/dota/bad.vpk"), b"BAD!").unwrap(); // right length, wrong bytes
        std::fs::write(dir.join("game/dota/long.vpk"), b"MUCH LONGER").unwrap(); // wrong length

        let statuses = plan(&dir, &resolve(&m, &BTreeMap::new()), None, &[]);
        let action = |dest: &str| statuses.iter().find(|s| s.dest == dest).unwrap().action;
        assert_eq!(action("game/dota/ok.vpk"), Action::UpToDate);
        assert_eq!(action("game/dota/bad.vpk"), Action::Update, "only the hash can catch this");
        assert_eq!(action("game/dota/long.vpk"), Action::Update, "the length alone settles this");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A pin is a decision about a COMPARISON. When the release changes the file the user chose
    /// their version over, the thing they weighed is gone — the pin expires and the file comes
    /// back as `Modified`, which is what puts it in the update menu. Without this a kept file
    /// silently never received another update while the launcher reported "up to date".
    #[test]
    fn a_pin_expires_when_the_release_changes_that_file() {
        let dir = std::env::temp_dir().join("phoenix-engine-test-pin-expiry");
        let _ = std::fs::remove_dir_all(&dir);
        let m = manifest();
        let resolved = resolve(&m, &BTreeMap::new());
        let dest = resolved[0].dest.clone();
        std::fs::create_dir_all(dir.join(&dest).parent().unwrap()).unwrap();
        std::fs::write(dir.join(&dest), b"my own version").unwrap();
        let prev = InstalledState {
            version: "0.9".into(),
            files: vec![InstalledFile { dest: dest.clone(), sha256: "dd".into() }],
            winmm_orig_created: false,
            restored: Vec::new(),
        };

        // pinned against THIS release's version of the file
        let mine = crate::verify::sha256_file_cached(&dir.join(&dest)).unwrap();
        let mut k = crate::keep::KeepList::default();
        k.pin(&dest, &mine, Some(resolved[0].sha256.clone()));
        k.save(&dir).unwrap();
        let st = plan(&dir, &resolved, Some(&prev), &[]);
        assert_eq!(st.iter().find(|s| s.dest == dest).unwrap().action, Action::Kept);

        // a new release ships different bytes at the same dest: the comparison the user made no
        // longer exists, so we ask again rather than suppressing the update forever
        let mut next = resolved.clone();
        next[0].sha256 = "ff".repeat(32);
        let st = plan(&dir, &next, Some(&prev), &[]);
        let row = st.iter().find(|s| s.dest == dest).unwrap();
        assert_eq!(row.action, Action::Modified, "the release moved on — re-ask");
        assert!(row.action.is_unattended(), "and the update menu can act on it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An expired pin is not the same thing as a difference nobody ruled on, and the plan says
    /// which is which — that is what lets the update menu leave the user's earlier answer standing
    /// instead of silently reversing it.
    #[test]
    fn an_expired_pin_is_reported_as_superseded_not_merely_modified() {
        let dir = std::env::temp_dir().join("phoenix-engine-test-superseded");
        let _ = std::fs::remove_dir_all(&dir);
        let m = manifest();
        let mut resolved = resolve(&m, &BTreeMap::new());
        let dest = resolved[0].dest.clone();
        std::fs::create_dir_all(dir.join(&dest).parent().unwrap()).unwrap();
        std::fs::write(dir.join(&dest), b"my own version").unwrap();
        let prev = InstalledState {
            version: "0.9".into(),
            files: vec![InstalledFile { dest: dest.clone(), sha256: "dd".into() }],
            winmm_orig_created: false,
            restored: Vec::new(),
        };
        let mine = crate::verify::sha256_file_cached(&dir.join(&dest)).unwrap();
        let mut k = crate::keep::KeepList::default();
        k.pin(&dest, &mine, Some(resolved[0].sha256.clone()));
        k.save(&dir).unwrap();

        // same release: the decision stands, untouched and unmentioned
        let row = plan(&dir, &resolved, Some(&prev), &[]);
        let row = row.iter().find(|s| s.dest == dest).unwrap();
        assert_eq!(row.action, Action::Kept);
        assert!(!row.superseded);

        // the release changes that file: ask again, and say WHY it is being asked
        resolved[0].sha256 = "ff".repeat(32);
        let row = plan(&dir, &resolved, Some(&prev), &[]);
        let row = row.iter().find(|s| s.dest == dest).unwrap();
        assert_eq!(row.action, Action::Modified);
        assert!(row.superseded, "the user ruled on this once; only the other side moved");

        // and a file nobody ever pinned is plain Modified — the two must not collapse
        std::fs::remove_file(crate::keep::KeepList::path(&dir)).unwrap();
        let row = plan(&dir, &resolved, Some(&prev), &[]);
        let row = row.iter().find(|s| s.dest == dest).unwrap();
        assert_eq!(row.action, Action::Modified);
        assert!(!row.superseded);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A pin written before `theirs` existed carries no comparison, so it cannot have expired.
    /// Holding it is the right way to be wrong: re-asking about every file somebody already
    /// decided is worse than deferring one question.
    #[test]
    fn a_legacy_pin_without_a_comparison_still_holds() {
        let dir = std::env::temp_dir().join("phoenix-engine-test-pin-legacy");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // the old on-disk shape: dest -> bare content hash
        std::fs::write(
            crate::keep::KeepList::path(&dir),
            br#"{ "files": { "game/dota/x.vpk": "aa" } }"#,
        )
        .unwrap();
        let k = crate::keep::KeepList::load(&dir);
        assert!(k.is_kept("game/dota/x.vpk", "aa", Some("anything")), "no comparison to expire");
        assert!(!k.is_kept("game/dota/x.vpk", "bb", None), "content still has to match");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An orphan whose bytes are still the ones we wrote is ours to clean up.
    #[test]
    fn plan_flags_orphans() {
        use sha2::Digest;
        let dir = std::env::temp_dir().join("phoenix-engine-test-orphan");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("game/dota")).unwrap();
        std::fs::write(dir.join("game/dota/fx.vpk"), b"x").unwrap();

        let m = manifest();
        let resolved = resolve(&m, &BTreeMap::new()); // fx off
        let prev = InstalledState {
            version: "0.9".into(),
            files: vec![InstalledFile {
                dest: "game/dota/fx.vpk".into(),
                // the REAL hash of what is on disk — the record has to be true for the file to
                // count as ours, which is the whole distinction this plan now draws
                sha256: hex::encode(sha2::Sha256::digest(b"x")),
            }],
            winmm_orig_created: false,
            restored: Vec::new(),
        };
        let statuses = plan(&dir, &resolved, Some(&prev), &[]);
        let orphan = statuses.iter().find(|s| s.dest == "game/dota/fx.vpk").unwrap();
        assert_eq!(orphan.action, Action::Remove);
        // and the others are Install (nothing on disk)
        assert!(statuses.iter().filter(|s| s.dest != "game/dota/fx.vpk").all(|s| s.action == Action::Install));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same orphan after somebody edited it: NOT reported at all. Dropping the row is what
    /// leaves the file alone (apply only acts on rows) and simultaneously stops claiming the dest,
    /// so the files view's extras scan reports it as what it has become — somebody else's file.
    /// Deleting it to tidy up a deselected option would destroy work we did not do, and unlike an
    /// overwrite of a MANAGED file there is no repair on the other side of it.
    #[test]
    fn plan_leaves_an_orphan_somebody_edited() {
        let dir = std::env::temp_dir().join("phoenix-engine-test-orphan-edited");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("game/dota")).unwrap();
        std::fs::write(dir.join("game/dota/fx.vpk"), b"my own edit").unwrap();

        let m = manifest();
        let resolved = resolve(&m, &BTreeMap::new());
        let prev = InstalledState {
            version: "0.9".into(),
            // what we wrote, which is not what is there now
            files: vec![InstalledFile { dest: "game/dota/fx.vpk".into(), sha256: "dd".into() }],
            winmm_orig_created: false,
            restored: Vec::new(),
        };
        let statuses = plan(&dir, &resolved, Some(&prev), &[]);
        assert!(
            !statuses.iter().any(|s| s.dest == "game/dota/fx.vpk"),
            "an edited orphan is nobody's to delete, so it is not an action at all"
        );
        assert_eq!(std::fs::read(dir.join("game/dota/fx.vpk")).unwrap(), b"my own edit");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A MANAGED file somebody edited is still repaired — but reported as theirs, so the shell can
    /// warn before overwriting. Refusing outright would mean a corrupted shim file could never be
    /// fixed by the button whose whole job is fixing shim files.
    #[test]
    fn plan_marks_a_managed_file_somebody_edited() {
        let dir = std::env::temp_dir().join("phoenix-engine-test-modified");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("game/bin/win64")).unwrap();
        let m = manifest();
        let resolved = resolve(&m, &BTreeMap::new());
        let dest = &resolved[0].dest;
        std::fs::create_dir_all(dir.join(dest).parent().unwrap()).unwrap();
        std::fs::write(dir.join(dest), b"somebody else's bytes").unwrap();

        let prev = InstalledState {
            version: "0.9".into(),
            files: vec![InstalledFile { dest: dest.clone(), sha256: "dd".into() }],
            winmm_orig_created: false,
            restored: Vec::new(),
        };
        let statuses = plan(&dir, &resolved, Some(&prev), &[]);
        let st = statuses.iter().find(|s| &s.dest == dest).unwrap();
        assert_eq!(st.action, Action::Modified);
        assert!(st.action.is_unattended(), "apply still repairs it");
        assert!(st.action.is_users(), "and the shell is told whose bytes it would overwrite");

        // pin those exact bytes and it becomes Kept: reported, never written
        let h = crate::verify::sha256_file_cached(&dir.join(dest)).unwrap();
        let mut k = crate::keep::KeepList::default();
        k.pin(dest, &h, Some(resolved[0].sha256.clone()));
        k.save(&dir).unwrap();
        let statuses = plan(&dir, &resolved, Some(&prev), &[]);
        let st = statuses.iter().find(|s| &s.dest == dest).unwrap();
        assert_eq!(st.action, Action::Kept);
        assert!(!st.action.is_unattended(), "a pin is an instruction that outlasts one dialog");
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// Old releases predate signing, and their notes must stay readable: the history page is an
    /// archive of prose, not an install path. Regression guard for the signing cutover — the day a
    /// signed launcher ships, every already-published release is unsigned.
    #[test]
    fn the_notes_history_still_reads_unsigned_releases() {
        let doc = concat!(
            "{\"schema\":2,\"version\":\"1.2.0\",",
            "\"notes\":\"### Fixed\\n- an old bug\",\"files\":[]}"
        );
        // a release carrying manifest.json and NO manifest.json.minisig
        let dl = Fake::new("v1.2.0", doc, vec![]).unsigned();
        let settings = Settings::default();

        let hist =
            fetch_notes_history(&settings, &dl, &[]).expect("history must not require signatures");
        assert_eq!(hist.entries.len(), 1, "an unsigned release must still appear in the history");
        assert_eq!(hist.entries[0].version, "1.2.0");
        assert!(hist.entries[0].notes.contains("an old bug"));

        // ...while the install path refuses that very same release
        let rel = dl.fetch_release("r", None).unwrap();
        assert!(
            manifest_of(&settings, &dl, &rel, Payload::Mod).is_err(),
            "an unsigned release must not be installable"
        );
    }

}
