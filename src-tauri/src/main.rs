#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Tauri desktop app. The window/UI is HTML/CSS/JS under ../frontend; this file exposes the engine
//! (config/github/manifest/install/state/verify/launch/autofind) to the webview as commands, and
//! keeps a headless CLI (check/install/uninstall) for testing in debug builds.
//!
//! i18n note: user-facing labels are derived in the frontend (it owns the language); this layer
//! ships raw data + minimal hints (`primary_action`, `can_play`, …). Manifest labels pass through
//! as plain strings or `{lang: text}` objects for the frontend to resolve.

mod autofind;
mod config;
mod engine;
mod github;
mod install;
mod launch;
mod manifest;
mod state;
mod steaminf;
mod verify;

use config::Settings;
use engine::Action;
use manifest::{Label, Manifest, OptionKind};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::Emitter;

// ---------------- shared app state ----------------

/// The last successfully fetched manifest, so selection changes re-plan without network I/O.
struct CachedManifest {
    repo: String,
    tag_name: String,
    manifest: Manifest,
}

#[derive(Default)]
struct AppState {
    manifest_cache: Mutex<Option<CachedManifest>>,
    /// The "What's new" history (also persisted to disk by the engine). `release_notes` validates
    /// freshness itself against the last checked tag, so nothing here needs invalidating.
    notes_cache: Mutex<Option<engine::NotesCache>>,
    autofind_cancel: Arc<AtomicBool>,
    autofind_running: AtomicBool,
}

// ---------------- view types serialized to the webview ----------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsView {
    source_repo: String,
    game_dir: String,
    has_token: bool,
    language: Option<String>,
    launch_extra: String,
    renderer: String,
    selections: serde_json::Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FileView {
    dest: String,
    status: String, // "ok" | "update" | "install" | "remove"
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VariantView {
    id: String,
    label: serde_json::Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OptionView {
    id: String,
    kind: String, // "choice" | "toggle"
    label: serde_json::Value,
    description: Option<serde_json::Value>,
    variants: Vec<VariantView>,
    /// Effective current value: variant id (choice) or bool (toggle).
    value: serde_json::Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CheckView {
    tag: String,
    version: String,
    game_dir: String,
    installed: bool,
    changes: u32,
    files: Vec<FileView>,
    notes: Option<String>, // markdown "What's new" for this release
    options: Vec<OptionView>,
    // derived UI hints (labels/status words are built frontend-side, where the language lives)
    primary_action: String, // "check" | "apply"
    can_play: bool,
    can_uninstall: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NotesEntryView {
    tag: String,
    version: String,
    notes: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InstallView {
    version: String,
    written: Vec<String>,
    removed: Vec<String>,
    up_to_date: u32,
    winmm_orig: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UninstallView {
    version: String,
    restored: Vec<String>,
    deleted: Vec<String>,
    winmm_orig_removed: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CandidateView {
    path: String,
    client_version: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GameDirStatus {
    dir: String,
    /// The user explicitly picked a folder — any folder is accepted, setup never re-triggers.
    configured: bool,
    /// From `game/dota/steam.inf`, informational only; the auto-resolved (exe-dir) case uses its
    /// presence as the first-run heuristic.
    client_version: Option<String>,
}

fn label_value(l: &Label) -> serde_json::Value {
    match l {
        Label::Plain(s) => serde_json::Value::String(s.clone()),
        Label::Localized(m) => serde_json::to_value(m).unwrap_or(serde_json::Value::Null),
    }
}

fn build_check_view(r: engine::CheckResult) -> CheckView {
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
        primary_action: if changes > 0 { "apply" } else { "check" }.to_string(),
        can_play: installed && changes == 0,
        can_uninstall: installed,
    }
}

// ---------------- commands ----------------

#[tauri::command]
fn get_settings() -> SettingsView {
    let s = Settings::load();
    SettingsView {
        source_repo: s.source_repo,
        game_dir: s.game_dir.map(|p| p.display().to_string()).unwrap_or_default(),
        has_token: s.token.is_some(),
        language: s.language,
        launch_extra: s.launch_extra,
        renderer: s.renderer,
        selections: serde_json::to_value(&s.selections).unwrap_or_default(),
    }
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn save_settings(
    source_repo: String,
    game_dir: String,
    token: String,
    language: Option<String>,
    launch_extra: String,
    renderer: String,
) -> Result<(), String> {
    let prev = Settings::load();
    let s = Settings {
        source_repo: if source_repo.trim().is_empty() {
            config::DEFAULT_REPO.to_string()
        } else {
            source_repo
        },
        game_dir: if game_dir.trim().is_empty() { None } else { Some(PathBuf::from(game_dir)) },
        // blank token field => keep whatever was saved (we never send the token to the UI)
        token: if token.is_empty() { prev.token } else { Some(token) },
        language,
        launch_extra,
        renderer: if renderer == "dx9" { renderer } else { "dx11".to_string() },
        selections: prev.selections,
    };
    s.save().map_err(|e| format!("{e:#}"))
}

/// Save just the game folder (setup flow / autofind pick).
#[tauri::command]
fn set_game_dir(path: String) -> Result<(), String> {
    let mut s = Settings::load();
    s.game_dir = if path.trim().is_empty() { None } else { Some(PathBuf::from(path)) };
    s.save().map_err(|e| format!("{e:#}"))
}

/// Save just the language (settings toggle applies instantly).
#[tauri::command]
fn set_language(language: Option<String>) -> Result<(), String> {
    let mut s = Settings::load();
    s.language = language;
    s.save().map_err(|e| format!("{e:#}"))
}

/// Save one option selection (customization view control).
#[tauri::command]
fn set_selection(id: String, value: serde_json::Value) -> Result<(), String> {
    let mut s = Settings::load();
    s.selections.insert(id, value);
    s.save().map_err(|e| format!("{e:#}"))
}

/// Where does the game folder currently resolve to, and is it one? Drives the setup view.
#[tauri::command]
fn game_dir_status() -> Result<GameDirStatus, String> {
    let s = Settings::load();
    let configured = s.game_dir.is_some();
    let dir = s.resolve_game_dir().map_err(|e| format!("{e:#}"))?;
    Ok(GameDirStatus {
        configured,
        client_version: steaminf::client_version(&dir),
        dir: dir.display().to_string(),
    })
}

#[tauri::command]
async fn check(state: tauri::State<'_, Arc<AppState>>) -> Result<CheckView, String> {
    let st = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let settings = Settings::load();
        let (release, manifest) = engine::fetch(&settings, None).map_err(|e| format!("{e:#}"))?;
        // cache before evaluating: even if the local diff fails, the fetched manifest is kept
        *st.manifest_cache.lock().unwrap() = Some(CachedManifest {
            repo: settings.source_repo.clone(),
            tag_name: release.tag_name.clone(),
            manifest: manifest.clone(),
        });
        let r = engine::evaluate(&settings, &release.tag_name, &manifest)
            .map_err(|e| format!("{e:#}"))?;
        Ok(build_check_view(r))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Re-diff with current settings/selections against the cached manifest — no network.
#[tauri::command]
async fn replan(state: tauri::State<'_, Arc<AppState>>) -> Result<CheckView, String> {
    let st = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let settings = Settings::load();
        // clone out and drop the lock — evaluate re-hashes files and must not hold it
        let (tag_name, manifest) = {
            let guard = st.manifest_cache.lock().unwrap();
            let cached = guard
                .as_ref()
                .filter(|c| c.repo == settings.source_repo)
                .ok_or_else(|| "no cached manifest — run a check first".to_string())?;
            (cached.tag_name.clone(), cached.manifest.clone())
        };
        let r = engine::evaluate(&settings, &tag_name, &manifest).map_err(|e| format!("{e:#}"))?;
        Ok(build_check_view(r))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The full "What's new" history (every release's notes, newest first). Cached in memory AND on
/// disk, keyed by the last checked tag — reopens are instant across app restarts; a new release
/// triggers an incremental rebuild (only unseen tags download a manifest).
#[tauri::command]
async fn release_notes(
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<Vec<NotesEntryView>, String> {
    let st = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let settings = Settings::load();
        let to_views = |entries: &[engine::NotesEntry]| {
            entries
                .iter()
                .map(|e| NotesEntryView {
                    tag: e.tag.clone(),
                    version: e.version.clone(),
                    notes: e.notes.clone(),
                })
                .collect::<Vec<_>>()
        };
        // freshness key: the tag the last check saw for this repo (None = accept any cached
        // history — the UI only opens this view after a successful check anyway)
        let current_tag: Option<String> = st
            .manifest_cache
            .lock()
            .unwrap()
            .as_ref()
            .filter(|c| c.repo == settings.source_repo)
            .map(|c| c.tag_name.clone());
        let fresh = |c: &engine::NotesCache| {
            c.repo == settings.source_repo
                && current_tag.as_ref().map_or(true, |t| *t == c.latest_tag)
        };
        // memory first, then disk (survives restarts); a stale same-repo cache still seeds the
        // incremental rebuild
        let mut known: Vec<engine::NotesEntry> = Vec::new();
        {
            let guard = st.notes_cache.lock().unwrap();
            if let Some(c) = guard.as_ref() {
                if fresh(c) {
                    return Ok(to_views(&c.entries));
                }
                if c.repo == settings.source_repo {
                    known = c.entries.clone();
                }
            }
        }
        if known.is_empty() {
            if let Some(c) = engine::NotesCache::load() {
                if fresh(&c) {
                    let views = to_views(&c.entries);
                    *st.notes_cache.lock().unwrap() = Some(c);
                    return Ok(views);
                }
                if c.repo == settings.source_repo {
                    known = c.entries;
                }
            }
        }
        let mut cache =
            engine::fetch_notes_history(&settings, &known).map_err(|e| format!("{e:#}"))?;
        // key the cache to the checked tag, not the release list's first item — those differ when
        // the newest release is a prerelease (check follows /releases/latest), and a mismatch
        // would refetch on every open
        if let Some(t) = current_tag {
            cache.latest_tag = t;
        }
        cache.save();
        let views = to_views(&cache.entries);
        *st.notes_cache.lock().unwrap() = Some(cache);
        Ok(views)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn apply() -> Result<InstallView, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let settings = Settings::load();
        install::install(&settings, None)
            .map(|r| InstallView {
                version: r.version,
                written: r.written,
                removed: r.removed,
                up_to_date: r.up_to_date as u32,
                winmm_orig: match r.winmm_orig {
                    install::WinmmOrig::Created => "created",
                    install::WinmmOrig::Existed => "existed",
                    install::WinmmOrig::NotNeeded => "not_needed",
                }
                .to_string(),
            })
            .map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn uninstall() -> Result<UninstallView, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let settings = Settings::load();
        install::uninstall(&settings)
            .map(|r| UninstallView {
                version: r.version,
                restored: r.restored,
                deleted: r.deleted,
                winmm_orig_removed: r.winmm_orig_removed,
            })
            .map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
fn play() -> Result<(), String> {
    let s = Settings::load();
    let gd = s.resolve_game_dir().map_err(|e| format!("{e:#}"))?;
    launch::launch(&gd, &s.renderer, &s.launch_extra).map_err(|e| format!("{e:#}"))
}

// ---- autoexec.cfg ----

fn autoexec_path() -> Result<PathBuf, String> {
    let s = Settings::load();
    let gd = s.resolve_game_dir().map_err(|e| format!("{e:#}"))?;
    Ok(gd.join("game").join("dota").join("cfg").join("autoexec.cfg"))
}

#[tauri::command]
fn read_autoexec() -> Result<String, String> {
    let p = autoexec_path()?;
    match std::fs::read_to_string(&p) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("reading {}: {e}", p.display())),
    }
}

#[tauri::command]
fn save_autoexec(content: String) -> Result<(), String> {
    let p = autoexec_path()?;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    std::fs::write(&p, content).map_err(|e| format!("writing {}: {e}", p.display()))
}

// ---- autofind ----

#[tauri::command]
async fn autofind_start(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<Vec<CandidateView>, String> {
    let st = state.inner().clone();
    // one scan at a time — a double-fired Continue must not spawn a second disk walk
    if st.autofind_running.swap(true, Ordering::SeqCst) {
        return Err("a scan is already running".into());
    }
    st.autofind_cancel.store(false, Ordering::Relaxed);
    tauri::async_runtime::spawn_blocking(move || {
        let found = autofind::autofind(
            |p| {
                let _ = app.emit(
                    "autofind-progress",
                    serde_json::json!({ "scanned": p.scanned, "current": p.current, "found": p.found }),
                );
            },
            &st.autofind_cancel,
        );
        st.autofind_running.store(false, Ordering::SeqCst);
        Ok(found
            .into_iter()
            .map(|c| CandidateView {
                path: c.path.display().to_string(),
                client_version: c.client_version,
            })
            .collect())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
fn autofind_cancel(state: tauri::State<'_, Arc<AppState>>) {
    state.autofind_cancel.store(true, Ordering::Relaxed);
}

/// Open an http(s) link in the default browser (changelog links must not navigate the webview).
#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("only http(s) links can be opened".into());
    }
    // `explorer <url>` hands it to the default browser without flashing a console window
    std::process::Command::new("explorer")
        .arg(&url)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("opening {url}: {e}"))
}

#[tauri::command]
fn browse_folder() -> Option<String> {
    rfd::FileDialog::new()
        .set_title("Select the game folder (the one that contains game\\)")
        .pick_folder()
        .map(|p| p.display().to_string())
}

fn run_gui() {
    tauri::Builder::default()
        .manage(Arc::new(AppState::default()))
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            set_game_dir,
            set_language,
            set_selection,
            game_dir_status,
            check,
            replan,
            release_notes,
            apply,
            uninstall,
            play,
            read_autoexec,
            save_autoexec,
            autofind_start,
            autofind_cancel,
            open_url,
            browse_folder
        ])
        .run(tauri::generate_context!())
        .expect("error while running the updater");
}

// ---------------- headless CLI (debug builds keep a console) ----------------

fn settings_from_flags(flags: &[String]) -> (Settings, Option<String>) {
    let mut s = Settings::load();
    let mut tag = None;
    let mut it = flags.iter();
    while let Some(k) = it.next() {
        match k.as_str() {
            "--game" => s.game_dir = it.next().map(PathBuf::from),
            "--repo" => {
                if let Some(v) = it.next() {
                    s.source_repo = v.clone();
                }
            }
            "--token" => s.token = it.next().cloned(),
            "--tag" => tag = it.next().cloned(),
            _ => {}
        }
    }
    if s.token.is_none() {
        s.token = std::env::var("PHOENIX_GITHUB_TOKEN").ok().filter(|v| !v.is_empty());
    }
    (s, tag)
}

fn cli_check(flags: &[String]) -> anyhow::Result<()> {
    let (settings, tag) = settings_from_flags(flags);
    let r = engine::check(&settings, tag.as_deref())?;
    println!("Release {} (version {}) | changes {}", r.tag, r.version, r.changes());
    for f in &r.files {
        let s = match f.action {
            Action::UpToDate => "ok",
            Action::Update => "update",
            Action::Install => "install",
            Action::Remove => "remove",
        };
        println!("  [{s:>7}] {}", f.dest);
    }
    Ok(())
}

fn cli_install(flags: &[String]) -> anyhow::Result<()> {
    let (settings, tag) = settings_from_flags(flags);
    let r = install::install(&settings, tag.as_deref())?;
    println!("Installed {}: wrote {}, removed {}", r.version, r.written.len(), r.removed.len());
    Ok(())
}

fn cli_uninstall(flags: &[String]) -> anyhow::Result<()> {
    let (settings, _tag) = settings_from_flags(flags);
    let r = install::uninstall(&settings)?;
    println!("Uninstalled {}: restored {}, deleted {}", r.version, r.restored.len(), r.deleted.len());
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.first().map(String::as_str) {
        Some("check") => cli_check(&args[1..]),
        Some("install") => cli_install(&args[1..]),
        Some("uninstall") => cli_uninstall(&args[1..]),
        _ => {
            run_gui();
            return;
        }
    };
    if let Err(e) = r {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
