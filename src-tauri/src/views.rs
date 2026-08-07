//! The wire contract between the shell and the webview: every struct serialized to the frontend
//! (camelCase, matching the JS), the check-view derivation from the engine result, and the
//! unified command error envelope.

use serde::Serialize;

use crate::downloader::NetKind;
use crate::engine::{self, Action, Cancelled, GameRunning};
use crate::manifest::{Label, OptionKind, UnsupportedSchema};
use crate::state;

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
    /// The optional launch flags, in table order — the UI renders one switch per entry.
    pub launch_flags: Vec<LaunchFlagView>,
    pub selections: serde_json::Value,
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
    pub status: String, // "ok" | "update" | "install" | "remove"
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
    pub changes: u32,
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
    pub winmm_orig: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UninstallView {
    pub version: String,
    pub restored: Vec<String>,
    pub deleted: Vec<String>,
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
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GamePlanView {
    pub version: String,
    /// Files that would download.
    pub files: u32,
    pub total_files: u32,
    /// Bytes that would download (unique content).
    pub bytes: u64,
    /// Free bytes on the target volume; absent when undeterminable.
    pub free_bytes: Option<u64>,
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameVerifyView {
    pub version: String,
    pub total: u32,
    pub ok: u32,
    /// Dests under shim management with no preserved original — not checkable, not damaged.
    pub skipped: u32,
    pub damaged: Vec<String>,
    /// Unique bytes a repair would download — the repair progress bar's full extent.
    pub damaged_bytes: u64,
    /// The folder holds a DIFFERENT game build (its steam.inf exists but doesn't match). Every
    /// file then reads as "damaged" while nothing is actually broken, and a repair would
    /// overwrite an unrelated installation — so the UI must say that, not offer a casual fix.
    pub foreign_build: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoexecView {
    pub content: String,
    /// The file on disk is not valid UTF-8 (e.g. a cp1251-commented cfg) and `content` is a
    /// lossy decode. The UI must show it read-only: saving the lossy text back would corrupt
    /// the original bytes.
    pub lossy: bool,
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
/// (`UnsupportedSchema`, `GameRunning`), then any io error; anything else is internal.
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
        // out-of-date launcher, and the frontend already words it that way
        if c.downcast_ref::<UnsupportedSchema>().is_some() {
            return "tooOld";
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

pub fn build_check_view(r: engine::CheckResult) -> CheckView {
    let installed = state::InstalledState::load(&r.game_dir).is_some();
    let changes = r.changes() as u32;
    let files = r
        .files
        .iter()
        .map(|f| FileView {
            dest: f.dest.clone(),
            status: match f.action {
                Action::UpToDate => "ok",
                Action::Update => "update",
                Action::Install => "install",
                Action::Remove => "remove",
            }
            .to_string(),
        })
        .collect();

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
        files,
        notes: r.notes,
        options,
        // no changes but no install state either: still offer Apply — the no-op install
        // rewrites the state and heals winmm_orig (a state-less but hash-perfect folder would
        // otherwise be stuck at "up to date" with Play locked and no way forward)
        primary_action: if changes > 0 || !installed { "apply" } else { "check" }.to_string(),
        can_play: installed && changes == 0,
        can_uninstall: installed,
        local: false,
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
    let files: Vec<FileView> = st
        .files
        .iter()
        .map(|f| {
            let ok = crate::verify::sha256_file_cached(&game_dir.join(&f.dest))
                .map(|h| h.eq_ignore_ascii_case(&f.sha256))
                .unwrap_or(false);
            FileView {
                dest: f.dest.clone(),
                status: if ok { "ok" } else { "update" }.to_string(),
            }
        })
        .collect();
    let changes = files.iter().filter(|f| f.status != "ok").count() as u32;
    CheckView {
        tag: String::new(), // no release was fetched — there is no tag to name
        version: st.version.clone(),
        game_dir: game_dir.display().to_string(),
        installed: true,
        changes,
        files,
        notes: None,
        options: Vec::new(), // options live in the manifest we could not fetch
        primary_action: "check".to_string(),
        can_play: changes == 0,
        can_uninstall: true,
        local: true,
    }
}
