//! Headless CLI (debug builds keep a console) — exercises the engine without the webview.
//! Reuses saved settings; flags override them. Authentication is the build-time baked credential
//! and nothing else (see `Settings::token`) — there is no `--token` flag and no environment
//! variable, because a second source of one is what let a stale value outrank the baked one.

use std::path::PathBuf;

use anyhow::Result;

use crate::config::{Settings, Source};
use crate::engine::{self, Action};
use crate::source::{self, Wire};
use crate::trust::Payload;
use crate::{install, manifest};

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
            "--game-repo" => s.game_repo = it.next().cloned(),
            "--tag" => tag = it.next().cloned(),
            _ => {}
        }
    }
    // No `--token` and no PHOENIX_GITHUB_TOKEN: the launcher authenticates with the credential
    // baked in at build time and nothing else (see Settings::token). A second source of one is
    // what let a stale value outrank the baked credential and 401 forever.
    (s, tag)
}

/// `sources [--save] [--mirror <url>]` — run the boot sequence headless and print the ranking.
///
/// The only way to exercise the whole loop without the GUI: bootstrap, refresh the published list
/// from the ranking, measure whatever is due, sort. `--save` persists the result exactly as the
/// GUI does; without it nothing is written, which is what makes this safe to run for a look.
///
/// There is no `--no-measure` any more, because there is no longer a decision to override: the
/// sequence measures when something has no settled answer and not otherwise (`source::launch_set`),
/// and a flag that forced it either way would be exercising a path the launcher never takes.
pub fn run_sources(flags: &[String]) -> Result<()> {
    let (mut settings, _) = settings_from_flags(flags);
    // `--mirror <url>` seeds one for this run only. Mirrors are normally discovered, and discovery
    // bootstraps from GitHub — so without this there is no way to exercise the mirror side of the
    // loop on a box that cannot reach GitHub, which is the very situation it is for.
    let mut it = flags.iter();
    while let Some(k) = it.next() {
        if k == "--mirror" {
            if let Some(url) = it.next().and_then(|u| crate::config::normalize_mirror_url(u)) {
                settings.sources.push(Source::at(url));
            }
        }
    }
    // The registry is what the walk reads, and nothing has booted here.
    source::adopt(&settings.sources);
    let outcome = source::refresh_and_measure(&settings);

    if let Some(e) = source::snapshot().refresh_error {
        println!("mirror list not refreshed: {e}");
    }
    // The head of the ranking is what the next operation uses — there is no second resolution to
    // guess at, which is the whole point of the model.
    for (i, s) in outcome.sources().iter().enumerate() {
        let in_use = if i == 0 { "  <- IN USE" } else { "" };
        // Address AND published name: this is the one place addresses are still printed (the GUI is
        // named-only, deliberately), so it is also the only place the two can be lined up.
        let named = s.name.as_deref().map(|n| format!(" [{n}]")).unwrap_or_default();
        println!("{}{named}{in_use}", s.key().unwrap_or("<github>"));
        let Some(m) = &s.measured else {
            println!("  NOT MEASURED");
            continue;
        };
        let speed = match m.bytes_per_sec {
            Some(b) => format!("{:.2} MiB/s", b as f64 / (1024.0 * 1024.0)),
            None => "—".to_string(),
        };
        println!(
            "  {:<8} latency {:>7}  speed {:>12}  range {:<3}  tag {}",
            if m.healthy() { "HEALTHY" } else { "UNUSABLE" },
            m.latency_ms.map(|v| format!("{v}ms")).unwrap_or_else(|| "—".into()),
            speed,
            if m.range_ok { "ok" } else { "NO" },
            m.tag.as_deref().unwrap_or("—"),
        );
        if let Some(e) = &m.error {
            println!("  error: {e}");
        }
    }

    if flags.iter().any(|f| f == "--save") {
        outcome.persist()?;
        println!("\nsaved {} source(s) to settings", outcome.sources().len());
    }
    Ok(())
}

pub fn run_check(flags: &[String]) -> Result<()> {
    let (settings, tag) = settings_from_flags(flags);
    // THROUGH THE MODEL, like every other read. `--repo` needs no special handling here any more:
    // an overridden repo collapses the ranking to GitHub inside `source::dial_for`, which is the
    // one place that rule now lives — this command used to apply it, `run_install` used to apply
    // it unconditionally, and the GUI never applied it at all.
    let r = source::with_active(
        &settings,
        &settings.source_repo,
        Payload::Mod,
        tag.as_deref(),
        |dl, release| engine::check(&settings, dl, release),
    )?;
    println!("Release {} (version {}) | changes {}", r.tag, r.version, r.changes());
    for f in &r.files {
        let s = match f.action {
            Action::UpToDate => "ok",
            Action::Update => "update",
            Action::Install => "install",
            Action::Remove => "remove",
            Action::Modified => "modified",
            Action::Kept => "kept",
        };
        println!("  [{s:>7}] {}", f.dest);
    }
    Ok(())
}

pub fn run_install(flags: &[String]) -> Result<()> {
    let (settings, tag) = settings_from_flags(flags);
    let wire = Wire::open(&settings, &settings.source_repo, Payload::Mod, tag.as_deref())?;
    let r = install::install(&settings, &wire, None, None, None)?;
    println!("Installed {}: wrote {}, removed {}", r.version, r.written.len(), r.removed.len());
    // headless: warm the customization cache synchronously (the GUI runs this detached), over the
    // wire that just installed and the manifest it verified — no second resolution of "latest"
    install::warm_cache(&settings, &wire, &r.manifest);
    Ok(())
}

pub fn run_uninstall(flags: &[String]) -> Result<()> {
    let (settings, _tag) = settings_from_flags(flags);
    let r = install::uninstall(&settings)?;
    println!("Uninstalled {}: restored {}, deleted {}", r.version, r.restored.len(), r.deleted.len());
    Ok(())
}

// ---- base game (game-install / game-verify against Settings::game_repo) ----

/// The base game's wire and manifest, headless.
///
/// The SOURCE MODEL, always — the game repo is public and a mirror serves it like any other
/// payload, so there is no reason for the headless path to see a different set of sources from the
/// GUI. `--game-repo` needs no branch here: an overridden repo is a GitHub-only run wherever it is
/// opened from, and `source::dial_for` is where that is decided.
fn game_wire(settings: &Settings) -> Result<(Wire, manifest::Manifest)> {
    let wire = Wire::open(settings, settings.game_repo(), Payload::Game, None)?;
    let manifest = wire.manifest()?;
    Ok((wire, manifest))
}

pub fn run_game_install(flags: &[String]) -> Result<()> {
    let (settings, _tag) = settings_from_flags(flags);
    let game_dir = settings.resolve_game_dir()?;
    let (wire, manifest) = game_wire(&settings)?;
    let r = install::install_base(&game_dir, &wire, &manifest, None, None, None)?;
    println!(
        "Base game {} ({}): wrote {} ({} MB), up-to-date {}, skipped {}",
        r.version,
        r.tag,
        r.written,
        r.bytes / (1024 * 1024),
        r.up_to_date,
        r.skipped
    );
    Ok(())
}

pub fn run_game_verify(flags: &[String]) -> Result<()> {
    let (settings, _tag) = settings_from_flags(flags);
    let game_dir = settings.resolve_game_dir()?;
    let (_wire, manifest) = game_wire(&settings)?;
    let statuses = install::base_plan(&game_dir, &manifest, None, "verify", None)?;
    let mut differing = 0;
    for s in &statuses {
        // one word per verdict, straight from the enum — the CLI and the GUI cannot drift into
        // describing the same state differently
        match s.action {
            install::BaseAction::UpToDate => {}
            a => {
                if a.writes() {
                    differing += 1;
                }
                println!("  [{:>10}] {}", a.word(), s.dest());
            }
        }
    }
    let claimed = std::collections::HashSet::new();
    let (extras, end) = install::scan_extras(&game_dir, &manifest, &claimed, None);
    let truncated = end == install::ExtrasEnd::Capped;
    for e in &extras {
        if e.files > 0 {
            println!("  [ extraDir] {} ({} files)", e.path, e.files);
        } else {
            println!("  [    extra] {}", e.path);
        }
    }
    println!(
        "Verified {}: {} files, {} to restore, {} extra{}",
        manifest.version,
        statuses.len(),
        differing,
        extras.len(),
        if truncated { " (scan truncated)" } else { "" }
    );
    Ok(())
}
