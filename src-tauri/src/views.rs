//! The wire contract between the shell and the webview: every struct serialized to the frontend
//! (camelCase, matching the JS), the check-view derivation from the engine result, and the
//! unified command error envelope.

use serde::Serialize;

use crate::downloader::NetKind;
use crate::engine::{self, Action, Cancelled, GameRunning};
use crate::manifest::{Label, OptionKind, UnsupportedCodec, UnsupportedSchema};
use crate::state;
use crate::trust::TrustError;

// ---------------- view types serialized to the webview ----------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsView {
    pub source_repo: String,
    pub game_dir: String,
    pub has_token: bool,
    pub language: Option<String>,
    pub launch_extra: String,
    pub renderer: String,
    /// UI animations master switch (frontend-only effect; persisted here).
    pub animations: bool,
    /// The optional launch flags, in table order — the UI renders one switch per entry.
    pub launch_flags: Vec<LaunchFlagView>,
    pub selections: serde_json::Value,
    /// Download sources in priority order. Unlike the fields above these are INSTANT-APPLY: the
    /// mirrors pane writes through on every edit, so they are never unsaved form state and never
    /// appear in the discard-changes snapshot.
    pub sources: Vec<SourceView>,
    pub auto_pick_best: bool,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SourceView {
    /// "primary" | "mirror". The primary has no `url` and no switch — it is the baked-in source,
    /// and no published mirror list can remove it.
    pub kind: &'static str,
    pub url: Option<String>,
    pub enabled: bool,
    /// The one downloads will be attempted from. Resolved by `config::active_index` and sent
    /// here rather than recomputed in JS, so the pane cannot come to disagree with the download
    /// path about which source is in use.
    pub active: bool,
    /// Has it ever been timed? False is the visible half of "a new mirror was acknowledged but
    /// not tested", which is the whole state the auto-pick setting leaves behind when it is off.
    pub measured: bool,
}

/// The list as the pane should paint it, with the active entry already resolved.
pub fn source_views(
    sources: &[crate::config::Source],
    pinned: Option<&crate::config::SourceRef>,
) -> Vec<SourceView> {
    let active = crate::config::active_index(sources, pinned);
    sources
        .iter()
        .enumerate()
        .map(|(i, s)| SourceView {
            kind: if s.is_primary() { "primary" } else { "mirror" },
            url: s.url().map(str::to_string),
            enabled: s.enabled(),
            active: active == Some(i),
            // the primary is never "untested" in the sense that matters here — it is not
            // something that newly appeared and might be better than what you are using
            measured: !matches!(s, crate::config::Source::Mirror { measured: false, .. }),
        })
        .collect()
}

/// One source's probe result. Kept apart from `SourceView` because it is a measurement, not a
/// setting: it is never persisted, and a source nobody has swept simply has none.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorProbeView {
    /// None for the primary; the UI keys on `primary` instead of inventing a sentinel URL.
    pub url: Option<String>,
    pub primary: bool,
    /// Index round-trip. All a plain reachability check would have told you.
    pub latency_ms: Option<u64>,
    /// Measured over a real asset chunk — the number worth sorting on.
    pub bytes_per_sec: Option<u64>,
    pub tag: Option<String>,
    pub range_ok: bool,
    pub error: Option<String>,
    /// Delivered the whole chunk in budget. Derived here so the UI and any later source-picking
    /// logic cannot disagree about what "usable" means.
    pub healthy: bool,
}

impl From<crate::mirror::Probe> for MirrorProbeView {
    fn from(p: crate::mirror::Probe) -> Self {
        Self {
            healthy: p.healthy(),
            url: p.url,
            primary: p.primary,
            latency_ms: p.latency_ms,
            bytes_per_sec: p.bytes_per_sec,
            tag: p.tag,
            range_ok: p.range_ok,
            error: p.error,
        }
    }
}

/// One sweep: the list as it now stands, plus what each entry measured.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorSweepView {
    pub sources: Vec<SourceView>,
    pub probes: Vec<MirrorProbeView>,
    /// Why the published list could not be read, if it could not. Never fatal.
    pub refresh_error: Option<String>,
}

impl MirrorSweepView {
    /// `pinned` is the selection AFTER the sweep applied its own policy, so the `active` flags
    /// describe the state that was just persisted.
    pub fn build(s: crate::mirror::Sweep, pinned: Option<&crate::config::SourceRef>) -> Self {
        Self {
            sources: source_views(&s.sources, pinned),
            probes: s.probes.into_iter().map(MirrorProbeView::from).collect(),
            refresh_error: s.refresh_error,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchFlagView {
    pub id: String,
    /// The options this flag adds, shown verbatim under its label.
    pub args: String,
    pub enabled: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileView {
    pub dest: String,
    /// "ok" | "update" | "install" | "remove" | "modified" | "kept". The last two are files apply
    /// will NOT touch — see `engine::Action::Modified`.
    pub status: String,
    /// The manifest option (group) owning this dest — a choice's shared dest or a toggle's file.
    /// The UI collapses same-group rows into ONE line carrying the group's label instead of
    /// listing every member file. Absent for plain files[] entries and for the local (offline)
    /// verdict, which has no manifest to know groups from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    /// The owning option's label (plain string or `{lang: text}`), resolved frontend-side.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<serde_json::Value>,
    /// For a choice's dest: the SELECTED variant's label — the row then reads "Lighting · Mod"
    /// instead of the shared dest path. Toggles have no variants.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<serde_json::Value>,
    /// Evidence, carried ONLY for the contested rows (`modified`/`kept`). Those are the ones the
    /// update menu asks the user to rule on, and "when did this change" is what makes that
    /// answerable — a file touched last week was touched by them. Absent everywhere else so the
    /// check payload does not grow a stat per file for rows nobody has to judge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtime: Option<u64>,
    /// The release has a newer version of this file than the one this row's state was decided
    /// against. Shown ALONGSIDE the state, not instead of it — "modified / update" is two facts,
    /// and a row that could only say one of them made the user guess the other.
    pub update_available: bool,
    /// This row is not a shim dest at all: it exists only because the user PINNED that path — a
    /// vanilla file they modded, say. The managed-files list files these under "Your files"
    /// rather than among the shim's own, and nothing in the update pipeline plans over them.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub yours: bool,
    /// This row's `group` comes from the manifest's presentational display TREE — a heading over
    /// always-installed files ("Hero Demo Plus") — not from an option. The UI renders it as a
    /// plain category with no checkbox semantics, and files the tree does not claim fall back to
    /// the generic core bucket. Absent for option rows and the local (offline) verdict.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub tree_group: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VariantView {
    pub id: String,
    pub label: serde_json::Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OptionView {
    pub id: String,
    pub kind: String, // "choice" | "toggle"
    pub label: serde_json::Value,
    pub description: Option<serde_json::Value>,
    pub variants: Vec<VariantView>,
    /// Effective current value: variant id (choice) or bool (toggle).
    pub value: serde_json::Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckView {
    pub tag: String,
    pub version: String,
    pub game_dir: String,
    pub installed: bool,
    /// Files an apply would act on. Excludes the ones the user changed — pressing the button
    /// would never clear those, and a pending count that cannot be cleared is a dead end.
    pub changes: u32,
    /// Phoenix files somebody has changed (or pinned). Apply leaves them alone; the main view
    /// says so beside the status rather than letting them read as "up to date".
    pub user_changed: u32,
    pub files: Vec<FileView>,
    pub notes: Option<String>, // markdown "What's new" for this release
    pub options: Vec<OptionView>,
    // derived UI hints (labels/status words are built frontend-side, where the language lives)
    pub primary_action: String, // "check" | "apply"
    pub can_play: bool,
    pub can_uninstall: bool,
    /// This verdict came from the install record alone — no manifest was fetched, so it describes
    /// what WE installed, not what the latest release is. The UI must word it as "couldn't check"
    /// rather than "up to date" (see `local_check`).
    pub local: bool,
    /// The folder holds a game (`game/dota`) or a prior install record. False = there is nothing
    /// to update INTO: the UI says "no game here" and offers the download instead of Install
    /// (which would otherwise read "Update available" over an empty folder), and the apply
    /// command refuses as the backend line behind that.
    pub game_present: bool,
    /// Bytes an interrupted base-game download left in `.phoenix-cache/base/` — turns the offer
    /// into "Resume download (~N GB fetched)".
    pub pending_base_bytes: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotesEntryView {
    pub tag: String,
    pub version: String,
    pub notes: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallView {
    pub version: String,
    pub written: Vec<String>,
    pub removed: Vec<String>,
    pub up_to_date: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UninstallView {
    pub version: String,
    pub restored: Vec<String>,
    pub deleted: Vec<String>,
    /// Left in place because they are no longer the bytes we installed — somebody edited them.
    /// The UI names this: "reverted" would otherwise describe a folder that still holds Phoenix
    /// files, and the user would have no idea why.
    pub kept: Vec<String>,
    /// Preserved originals survive under `.phoenix-vanilla/` because their dests are occupied by
    /// files in `kept`.
    pub vanilla_kept: bool,
    pub winmm_orig_removed: bool,
}

/// A launcher release newer than this build. The command returns `Option<Self>`; `None` means
/// current, and a command FAILURE means unknown — the frontend must not collapse the two.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LauncherUpdateView {
    pub tag: String,
    pub version: String,
    pub current: String,
    pub notes: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LauncherInfoView {
    pub version: String,
    /// This process was started by a self-update that just completed.
    pub just_updated: bool,
}

/// One `launcher-progress` tick while the new launcher downloads. `bytesTotal` is absent when the
/// server sends no Content-Length.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LauncherProgress {
    pub bytes_done: u64,
    pub bytes_total: Option<u64>,
}

/// What a fresh base-game download would do — the confirm dialog's numbers, before any bytes.
///
/// Since manifest schema 3 the byte totals come in TWO currencies (bundles compress): `bytes`
/// is the wire, `disk_bytes` is what lands. Every progress surface (bar, ETA, "downloaded so
/// far", `cached_bytes`) speaks wire; the free-space line speaks `need_bytes`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GamePlanView {
    pub version: String,
    /// Files that would download.
    pub files: u32,
    pub total_files: u32,
    /// Bytes that would cross the NETWORK (unique assets; a needed bundle counts its packed
    /// size once) — the download bar's full extent.
    pub bytes: u64,
    /// Decoded bytes that would LAND on disk (unique content) — the installed footprint.
    pub disk_bytes: u64,
    /// What the backend's disk preflight will demand (footprint + packed-bundle transient,
    /// before its safety margin) — the frontend's early space warning mirrors this exactly, so
    /// the confirm can never green-light a run the backend then refuses.
    pub need_bytes: u64,
    /// Of `bytes`, how much already sits in the base cache from an interrupted attempt (full
    /// entries, packed bundles, `.part` prefixes) — the confirm's "X of Y GB already
    /// downloaded" line.
    pub cached_bytes: u64,
    /// Of `files`, how many are already COMPLETELY fetched (a `.part` counts toward bytes, not
    /// here) — without this the resume confirm counted the finished files as still-to-do.
    pub cached_files: u32,
    /// Free bytes on the target volume; absent when undeterminable.
    pub free_bytes: Option<u64>,
}

/// Where a fresh download would go, answered for the dialog that asks — before any of it is real.
///
/// The path is composed HERE, not in the frontend: the dialog shows `prefix` and sends `path`, and
/// those two have to be the same act of joining or the folder on screen stops being the folder on
/// disk (see `install::target_of`). The rest is what the destination already contains, which is the
/// difference between filling an empty folder, continuing an install that exists, and moving in on
/// top of somebody's files.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameTargetView {
    /// The fixed head of the destination — the folder the user picked, plus the separator the
    /// subfolder is joined with. This is the part the dialog renders un-editable.
    pub prefix: String,
    /// The destination itself, exactly as `game_plan`/`game_install` want it. `None` when the name
    /// is unusable: there is no such folder to name, and nothing should be able to send one.
    pub path: Option<String>,
    /// Why the name is unusable, as a reason code the webview words in its own language (the shell
    /// owns no strings — see main.rs). `None` = usable.
    pub name_error: Option<&'static str>,
    /// The name to offer when the dialog opens. Shipped rather than hardcoded frontend-side so the
    /// prefill and what the backend composes cannot drift apart.
    pub default_name: String,
    /// The DESTINATION already holds a game, or an interrupted download of one — this run would
    /// continue it rather than fill an empty folder.
    pub occupied: bool,
    /// The folder the user PICKED does. Distinct from `occupied`, and the reason the dialog opens
    /// with the subfolder switched off: nesting inside a game folder installs a second copy of the
    /// game one level down instead of touching the one that is already there.
    pub base_occupied: bool,
    /// Top-level entries in the destination that are not the launcher's own — what the extras scan
    /// will report as files nothing claims, counted before the download instead of after it.
    pub foreign_entries: u32,
}

/// The code a `SubdirIssue` crosses the wire as. The frontend appends it to `gd.name.` and looks
/// the sentence up in its own table.
pub fn subdir_issue_key(i: crate::install::SubdirIssue) -> &'static str {
    use crate::install::SubdirIssue as S;
    match i {
        S::Empty => "empty",
        S::Separator => "sep",
        S::Chars => "chars",
        S::Edge => "edge",
        S::Reserved => "reserved",
        S::TooLong => "long",
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameInstallView {
    pub game_version: String,
    pub written: u32,
    pub up_to_date: u32,
    pub bytes: u64,
    /// The chained shim install's version — a fresh download ends playable, not merely present.
    pub shim_version: Option<String>,
}

/// One file the files view lists: something in the game folder that is not what its authority
/// expects, or that no authority claims at all.
///
/// Only differences travel. An intact install is 4,635 rows of "fine", which is a number
/// (`GameVerifyView::ok`), not a list.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileStateView {
    /// Relative to the game folder, `/`-separated.
    pub path: String,
    /// Which authority has an opinion about this file, and therefore what restoring it means:
    ///   `game`    the vanilla manifest — restore re-downloads stock bytes from the game repo;
    ///   `phoenix` the shim manifest — restore re-installs the Phoenix file from the dist repo;
    ///   `extra`   nobody. Nothing to restore it to; the only act available is deletion.
    pub owner: &'static str,
    /// `missing` | `modified` | `unreadable` | `kept` — or, for `extra` rows, `extra` /
    /// `extraDir`. Never `intact`: those are counted, not listed.
    pub state: &'static str,
    /// What the authority says this file should weigh. 0 for extras (nobody says).
    pub size: u64,
    /// What it actually weighs. `None` = not there, or not stattable. Together with `size` this is
    /// the hard evidence: a fraction of the expected length is a truncated download, and no mod
    /// looks like that.
    pub local_size: Option<u64>,
    /// Last modified, unix seconds. The other half: stock files carry the install date, so a file
    /// touched months later was changed on purpose.
    pub mtime: Option<u64>,
    /// Which download restoring this rides in, and that download's WIRE cost. Two rows sharing a
    /// key are ONE fetch (a bundle carries thousands of members), so the view's live
    /// "N selected · X GB" is a sum over distinct keys — the same rule `costs_of` applies
    /// backend-side, shipped as data so a checkbox never costs a round trip. `None` = free.
    pub wire_key: Option<String>,
    pub wire: u64,
    /// The release has a newer version than the one this row's state was decided against.
    pub update_available: bool,
    /// For an `extraDir` row: how many files the summarized subtree holds. 0 otherwise.
    pub files: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameVerifyView {
    pub version: String,
    pub total: u32,
    /// Files that match their authority exactly.
    pub ok: u32,
    /// Dests under shim management with no preserved original — not checkable, not damaged.
    pub skipped: u32,
    /// Files the user has pinned as intentionally different (see keep.rs). Counted separately
    /// from `ok` because they are NOT what the manifest says — they are what the user said.
    pub kept: u32,
    /// Everything that is not intact, both authorities plus unclaimed files. The view groups and
    /// filters this itself; the backend ships facts, not a presentation.
    pub files: Vec<FileStateView>,
    /// WIRE bytes the DEFAULT selection would download (everything unapproved). The view
    /// recomputes this live from `wireKey` as the user selects; this is the opening number.
    pub damaged_bytes: u64,
    /// The extras scan hit its entry ceiling — the `extra` rows are a prefix of the truth, and
    /// the UI must say so rather than presenting a short list as complete.
    pub extras_truncated: bool,
    /// The folder holds a DIFFERENT game build (its steam.inf exists but doesn't match). Every
    /// file then reads as modified while nothing is actually broken, and a repair would overwrite
    /// an unrelated installation — so the UI must say that, not offer a casual fix.
    pub foreign_build: bool,
    /// The shim half could not be computed (no network, no manifest). Its files are then absent
    /// from `files` entirely, and the view must not imply they were checked and found fine.
    pub phoenix_unknown: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoexecView {
    pub content: String,
    /// The file on disk is not valid UTF-8 (e.g. a cp1251-commented cfg) and `content` is a
    /// lossy decode. The UI must show it read-only: saving the lossy text back would corrupt
    /// the original bytes.
    pub lossy: bool,
    /// `launch::PINNED_CONVARS` — the editor flags lines that set these, because launch strips
    /// them from the file. Shipped with the view so the list has one source of truth.
    pub pinned: Vec<&'static str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateView {
    pub path: String,
    pub client_version: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameDirStatus {
    pub dir: String,
    /// The user explicitly picked a folder — any folder is accepted, setup never re-triggers.
    pub configured: bool,
    /// From `game/dota/steam.inf`, informational only; the auto-resolved (exe-dir) case uses its
    /// presence as the first-run heuristic.
    pub client_version: Option<String>,
}

// ---------------- command error envelope ----------------

/// Every command failure crosses to the webview as `{kind, message}`: `message` for display,
/// `kind` so the UI can react (prompt for a token on "auth", tell the user to update on
/// "tooOld", close-the-game on "gameRunning", …). Kinds: network | auth | notFound | io |
/// tooOld | gameRunning | cancelled | restartFailed | internal.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CmdError {
    pub kind: String,
    pub message: String,
}

/// Classify an anyhow chain: a `NetKind` from the transport edge wins, then the typed markers
/// (`UnsupportedSchema`, `TrustError`, `GameRunning`), then any io error; anything else is
/// internal.
fn wire_kind(e: &anyhow::Error) -> &'static str {
    for c in e.chain() {
        if let Some(n) = c.downcast_ref::<NetKind>() {
            return match n {
                NetKind::Transport => "network",
                NetKind::Status(401 | 403) => "auth",
                NetKind::Status(404) => "notFound",
                NetKind::Status(_) => "network",
            };
        }
        // wire kind stays "tooOld": from the user's side an unreadable manifest schema IS an
        // out-of-date launcher, and the frontend already words it that way. An undecodable
        // bundle codec is the same answer with a different detection point (manifest R2) —
        // never "your download is corrupt".
        if c.downcast_ref::<UnsupportedSchema>().is_some()
            || c.downcast_ref::<UnsupportedCodec>().is_some()
        {
            return "tooOld";
        }
        // An unverifiable release is a release we do not have — the same answer as one that was
        // pulled or renamed, and `notFound` is what says that. Not `tooOld` (nothing about a bad
        // signature is fixed by updating), and emphatically not an error kind of its own: the
        // frontend's SOFT_ERR set is `network | auth | notFound`, and those are the failures that
        // let Play proceed on an install that was already clean. A signing scheme whose failure
        // mode is "your game stops working" would be worse than the exposure it removes.
        if c.downcast_ref::<TrustError>().is_some() {
            return "notFound";
        }
        if c.downcast_ref::<GameRunning>().is_some() {
            return "gameRunning";
        }
        // the user asked for this stop — the UI closes quietly rather than painting an error
        if c.downcast_ref::<Cancelled>().is_some() {
            return "cancelled";
        }
        if c.downcast_ref::<std::io::Error>().is_some() {
            return "io";
        }
    }
    "internal"
}

impl CmdError {
    /// The self-update swapped the exe but could not start it. Its own kind because every other
    /// failure on that path means "nothing was replaced, you are still on the old build" — here
    /// the new build IS installed and the only thing missing is a relaunch. Telling the user the
    /// update failed would be false, and would send them round the same loop again.
    pub fn restart_failed(message: String) -> Self {
        Self { kind: "restartFailed".to_string(), message }
    }
}

impl From<anyhow::Error> for CmdError {
    fn from(e: anyhow::Error) -> Self {
        Self { kind: wire_kind(&e).to_string(), message: format!("{e:#}") }
    }
}

impl From<String> for CmdError {
    fn from(message: String) -> Self {
        Self { kind: "internal".to_string(), message }
    }
}

impl From<&str> for CmdError {
    fn from(message: &str) -> Self {
        Self::from(message.to_string())
    }
}

impl CmdError {
    /// A spawned blocking task failed to join (panic/cancel) — always internal.
    pub fn task(e: impl std::fmt::Display) -> Self {
        Self::from(format!("background task failed: {e}"))
    }
}

// ---------------- check view derivation ----------------

fn label_value(l: &Label) -> serde_json::Value {
    match l {
        Label::Plain(s) => serde_json::Value::String(s.clone()),
        Label::Localized(m) => serde_json::to_value(m).unwrap_or(serde_json::Value::Null),
    }
}

/// Append a `kept` row for every pin the shim does not already account for.
///
/// A pin on a dest outside the shim manifest — a vanilla file somebody modded, most often — is a
/// standing instruction the launcher honours on every plan and used to mention NOWHERE: the only
/// screen that listed it was a full game verification, which costs minutes of hashing. A decision
/// the user cannot see is one they cannot revisit, so the managed-files list carries all of them
/// (under "Your files"), and the Your-files view is what acts on them.
///
/// DECLARED, not verified. The keep list is one small read; confirming a pin still holds means
/// hashing whatever it points at, and on a modded VPK that is hundreds of MB — on a path that runs
/// at every launch. The row states what was decided. Whether the bytes still match is exactly the
/// question `your_files` answers, and it hashes these dests (and only these) when it is opened.
fn with_foreign_pins(game_dir: &std::path::Path, mut files: Vec<FileView>) -> Vec<FileView> {
    let planned: std::collections::HashSet<&str> =
        files.iter().map(|f| f.dest.as_str()).collect();
    let extra: Vec<FileView> = crate::keep::KeepList::load(game_dir)
        .files
        .keys()
        .filter(|d| !planned.contains(d.as_str()))
        // A pin whose file is GONE describes nothing. It can outlive its file two ways — the user
        // deleted the mod by hand, or a release stopped shipping that dest — and the row it used
        // to produce was unclearable: the managed-files list showed "kept" forever for a path with
        // nothing at it, while the Your-files view (which plans over the manifests) had no row to
        // offer. The pin itself is inert in that state, since nothing plans that dest any more.
        .filter(|d| game_dir.join(d).exists())
        .map(|dest| {
            let md = std::fs::metadata(game_dir.join(dest)).ok();
            FileView {
                dest: dest.clone(),
                status: "kept".to_string(),
                group_id: None,
                group: None,
                variant: None,
                local_size: md.as_ref().map(|m| m.len()),
                mtime: md
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs()),
                update_available: false,
                yours: true,
                tree_group: false,
            }
        })
        .collect();
    files.extend(extra);
    files
}

pub fn build_check_view(r: engine::CheckResult) -> CheckView {
    let installed = state::InstalledState::load(&r.game_dir).is_some();
    let changes = r.changes() as u32;
    // `Modified` ONLY, not `users()`. This number drives a confirm that says applying will
    // overwrite these files, and apply never touches a `Kept` one — counting pins here made the
    // dialog overstate what it was about to do, which is the exact failure the pin exists to
    // prevent. (`install::Ctx::user_changed` still wants both: an explicitly selected `Kept` dest
    // IS displaced, and must be preserved when it is.)
    let user_changed =
        r.files.iter().filter(|f| f.action == Action::Modified).count() as u32;
    // dest -> owning option, for the UI's group collapsing. Covers every dest an option manages:
    // a choice's shared dest (with the SELECTED variant's label riding along) and each of a
    // toggle's files (a deselected toggle's Remove rows still match — the dests are the same).
    let owner: std::collections::HashMap<&str, (&str, &Label, Option<&Label>)> = r
        .options
        .iter()
        .flat_map(|o| {
            let sel = r.selections.get(&o.id).and_then(|v| v.as_str());
            let var = o.variants.iter().find(|v| Some(v.id.as_str()) == sel).map(|v| &v.label);
            let choice = o.dest.iter().map(move |d| (d.as_str(), (o.id.as_str(), &o.label, var)));
            let toggle =
                o.files.iter().map(move |f| (f.dest.as_str(), (o.id.as_str(), &o.label, None)));
            choice.chain(toggle)
        })
        .collect();
    // dest -> the INNERMOST labeled node of the manifest's display tree. Refines the plain-files
    // bucket the same way option ownership refines option files: a labeled node becomes a
    // collapsible category, an unlabeled node splices its content into its parent (spec), and a
    // dest the tree names but files[] does not carry is simply never looked up — skipped, as the
    // spec requires, never refused. Ids are the node's position path ("tree:/0/1"): stable for a
    // given manifest, which is all the frontend's open/closed memory needs.
    fn walk_tree<'a>(
        nodes: &'a [crate::manifest::TreeNode],
        inherited: Option<(String, &'a Label)>,
        path: &str,
        out: &mut std::collections::HashMap<&'a str, (String, &'a Label)>,
    ) {
        for (i, n) in nodes.iter().enumerate() {
            let path = format!("{path}/{i}");
            let cur = n
                .label
                .as_ref()
                .map(|l| (format!("tree:{path}"), l))
                .or_else(|| inherited.clone());
            if let Some((id, l)) = &cur {
                for d in &n.files {
                    out.insert(d.as_str(), (id.clone(), l));
                }
            }
            walk_tree(&n.groups, cur, &path, out);
        }
    }
    let mut tree_owner = std::collections::HashMap::new();
    walk_tree(&r.tree, None, "", &mut tree_owner);
    let files = r
        .files
        .iter()
        .map(|f| {
            let own = owner.get(f.dest.as_str());
            // an option's ownership wins — a dest can't be both, but if a broken tree claimed
            // one anyway, the row that carries install semantics must not lose them to a label
            let tre = if own.is_none() { tree_owner.get(f.dest.as_str()) } else { None };
            let (local_size, mtime) = if f.action.is_users() {
                std::fs::metadata(r.game_dir.join(&f.dest)).ok().map_or((None, None), |m| {
                    (
                        Some(m.len()),
                        m.modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_secs()),
                    )
                })
            } else {
                (None, None)
            };
            FileView {
                dest: f.dest.clone(),
                local_size,
                mtime,
                update_available: f.update_available,
                status: match f.action {
                    Action::UpToDate => "ok",
                    Action::Update => "update",
                    Action::Install => "install",
                    Action::Remove => "remove",
                    // A pin the RELEASE outran still reads as KEPT: the user's decision stands
                    // until they change it, and what is new about the row is carried by
                    // `update_available` instead ("kept / update"). Reporting it as a plain
                    // difference would silently discard the fact that they already answered.
                    Action::Modified if f.superseded => "kept",
                    Action::Modified => "modified",
                    Action::Kept => "kept",
                }
                .to_string(),
                group_id: own
                    .map(|(id, _, _)| id.to_string())
                    .or_else(|| tre.map(|(id, _)| id.clone())),
                group: own
                    .map(|(_, l, _)| label_value(l))
                    .or_else(|| tre.map(|(_, l)| label_value(l))),
                variant: own.and_then(|(_, _, v)| v.map(label_value)),
                yours: false,
                tree_group: tre.is_some(),
            }
        })
        .collect();
    let files = with_foreign_pins(&r.game_dir, files);

    let options = r
        .options
        .iter()
        .map(|o| OptionView {
            id: o.id.clone(),
            kind: match o.kind {
                OptionKind::Choice => "choice",
                OptionKind::Toggle => "toggle",
            }
            .to_string(),
            label: label_value(&o.label),
            description: o.description.as_ref().map(label_value),
            variants: o
                .variants
                .iter()
                .map(|v| VariantView { id: v.id.clone(), label: label_value(&v.label) })
                .collect(),
            value: r.selections.get(&o.id).cloned().unwrap_or(serde_json::Value::Null),
        })
        .collect();

    CheckView {
        tag: r.tag,
        version: r.version,
        game_dir: r.game_dir.display().to_string(),
        installed,
        changes,
        user_changed,
        files,
        notes: r.notes,
        options,
        // no changes but no install state either: still offer Apply — the no-op install
        // rewrites the state and heals winmm_orig (a state-less but hash-perfect folder would
        // otherwise be stuck at "up to date" with Play locked and no way forward).
        //
        // "manage" is the third case: the release has nothing to install, but files at our dests
        // are no longer ours. There is no update to offer, so the button must not say Update —
        // what it opens is a menu for deciding about those files.
        primary_action: if changes > 0 || !installed {
            "apply"
        } else if user_changed > 0 {
            "manage"
        } else {
            "check"
        }
        .to_string(),
        can_play: installed && changes == 0,
        can_uninstall: installed,
        local: false,
        game_present: crate::install::game_present(&r.game_dir),
        pending_base_bytes: crate::install::pending_base_bytes(&r.game_dir),
    }
}

/// A verdict built from the install record alone, for when the network check failed.
///
/// The launcher must not become useless because GitHub is unreachable: Play and Uninstall are both
/// purely local operations, and refusing them turns "we could not ask about updates" into "your
/// game is unusable". What this CAN honestly say is whether the files we installed are still the
/// files we installed — every dest is re-hashed against the sha256 recorded at install time.
///
/// It deliberately never offers `apply`: repairing needs the assets, and those need the network
/// that just failed. A mismatch leaves the primary on Check so the user retries the real thing.
pub fn build_local_check_view(game_dir: &std::path::Path, st: &state::InstalledState) -> CheckView {
    // Pins are honoured offline too. Without this a file the user deliberately kept read as a
    // pending change, and since this view never offers `apply`, `can_play` went false with NOTHING
    // the user could do to clear it — the unclearable-pending-state trap, reached by using the
    // feature exactly as intended. `theirs_now` is None here: there is no manifest to compare
    // against, and unknown is not "changed".
    let keep = crate::keep::KeepList::load(game_dir);
    let files: Vec<FileView> = st
        .files
        .iter()
        .map(|f| {
            // `==`, like every other hash comparison in the engine. It used to be
            // case-insensitive here and exact everywhere else, which meant the offline verdict
            // and the online one could disagree about the same bytes; `Manifest::validate_hashes`
            // now refuses a non-lowercase digest outright, so the leniency protected nothing and
            // only hid that divergence.
            let local = crate::verify::sha256_file_cached(&game_dir.join(&f.dest)).ok();
            let ok = local.as_deref() == Some(f.sha256.as_str());
            let kept =
                !ok && local.as_deref().is_some_and(|h| keep.is_kept(&f.dest, h, None));
            FileView {
                dest: f.dest.clone(),
                status: if ok {
                    "ok"
                } else if kept {
                    "kept"
                } else {
                    "update"
                }
                .to_string(),
                // options live in the manifest we could not fetch — no groups to collapse into
                group_id: None,
                group: None,
                variant: None,
                local_size: None,
                mtime: None,
                // no manifest was fetched, so nothing here can claim a newer version exists
                update_available: false,
                yours: false,
                tree_group: false,
            }
        })
        .collect();
    let files = with_foreign_pins(game_dir, files);
    // `kept` is not a pending change: apply would not touch it, and offline there is no apply at
    // all. Counting it would block Play over a decision the user already made. (The pins appended
    // above are all `kept`, so they cannot move this number either — which is the point: they
    // describe files no update was ever going to touch.)
    let changes = files.iter().filter(|f| f.status != "ok" && f.status != "kept").count() as u32;
    CheckView {
        tag: String::new(), // no release was fetched — there is no tag to name
        version: st.version.clone(),
        game_dir: game_dir.display().to_string(),
        installed: true,
        changes,
        // Offline, "not the bytes we installed" is ALL this can see — there is no manifest to say
        // whether a newer release would have wanted different bytes anyway. Reporting that as
        // "changed by you" would be a guess, and the view already words itself as "couldn't
        // check"; the mismatches ride `changes` exactly as they always have.
        user_changed: 0,
        files,
        notes: None,
        options: Vec::new(), // options live in the manifest we could not fetch
        primary_action: "check".to_string(),
        can_play: changes == 0,
        can_uninstall: true,
        local: true,
        // an install record exists by construction here — that IS the game-present evidence
        game_present: true,
        pending_base_bytes: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pin the shim does not manage has to REACH the managed-files list — a "Your files"
    /// category with nothing in it is exactly the invisibility this was added to fix. And a pin it
    /// DOES manage must not be duplicated there: that row already exists, with a real verdict
    /// behind it, and two rows for one dest would double every count printed above them.
    #[test]
    fn foreign_pins_reach_the_managed_list_exactly_once() {
        let dir = std::env::temp_dir().join("phoenix-views-foreign-pins");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut k = crate::keep::KeepList::default();
        k.pin("game/dota_phoenix/hud.vpk", "aa", None); // a shim dest — already planned
        k.pin("game/dota/resource/flash3/x.txt", "bb", None); // nobody plans this one
        k.pin("game/dota/gone.txt", "cc", None); // a pin that outlived its file
        k.save(&dir).unwrap();
        for f in ["game/dota_phoenix/hud.vpk", "game/dota/resource/flash3/x.txt"] {
            let p = dir.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"x").unwrap();
        }

        let planned = vec![FileView {
            dest: "game/dota_phoenix/hud.vpk".into(),
            status: "kept".into(),
            group_id: None,
            group: None,
            variant: None,
            local_size: None,
            mtime: None,
            update_available: false,
            yours: false,
            tree_group: false,
        }];
        let out = with_foreign_pins(&dir, planned);
        assert_eq!(out.len(), 2, "the shim's own row is not duplicated");
        let mine: Vec<&FileView> = out.iter().filter(|f| f.yours).collect();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].dest, "game/dota/resource/flash3/x.txt");
        // `kept`, so neither `changes` nor `user_changed` can move because of it — these rows
        // describe files no update was ever going to touch
        assert_eq!(mine[0].status, "kept");
        assert!(!mine[0].update_available);
        // and the pin whose file is GONE claims nothing. It used to produce a "kept" row forever
        // for a path with nothing at it — a row no screen could act on and no gesture could clear.
        assert!(!out.iter().any(|f| f.dest == "game/dota/gone.txt"));

        // no keep file at all is the common case and must cost nothing but an empty read
        let empty = std::env::temp_dir().join("phoenix-views-no-pins");
        let _ = std::fs::remove_dir_all(&empty);
        std::fs::create_dir_all(&empty).unwrap();
        assert!(with_foreign_pins(&empty, Vec::new()).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&empty);
    }
}
