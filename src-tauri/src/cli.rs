//! Headless CLI (debug builds keep a console) — exercises the engine without the webview.
//! Reuses saved settings; flags override them. Authentication is the build-time baked credential
//! and nothing else (see `Settings::token`) — there is no `--token` flag and no environment
//! variable, because a second source of one is what let a stale value outrank the baked one.

use std::path::PathBuf;

use anyhow::Result;

use crate::config::Settings;
use crate::downloader::Downloader;
use crate::engine::{self, Action};
use crate::github::Github;
use crate::install;

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

/// `sweep [--save]` — refresh the published mirror list and measure every source, headless.
///
/// The only way to exercise the whole loop without the GUI, and the only way to seed a list at all
/// now that mirrors are discovered rather than typed. `--save` persists the result, exactly as the
/// settings pane does; without it nothing is written.
pub fn run_sweep(flags: &[String]) -> Result<()> {
    let (mut settings, _) = settings_from_flags(flags);
    // `--mirror <url>` seeds one for this run only. Mirrors are normally discovered, and discovery
    // bootstraps from the primary — so without this there is no way to exercise the mirror side of
    // the loop on a box that cannot reach the primary, which is the very situation it is for.
    let mut it = flags.iter();
    while let Some(k) = it.next() {
        if k == "--mirror" {
            if let Some(url) = it.next().and_then(|u| crate::config::normalize_mirror_url(u)) {
                settings.sources.push(crate::config::Source::Mirror { url, enabled: true, measured: false, payloads: Vec::new() });
            }
        }
    }
    // `--no-measure` is the launch-time path: refresh the list only. Worth being able to run on
    // its own, because its whole contract is that it must NOT disturb the speed ranking.
    let measure = !flags.iter().any(|f| f == "--no-measure");
    let sweep = crate::mirror::sweep(&settings, measure);

    if let Some(e) = &sweep.refresh_error {
        println!("mirror list not refreshed: {e}");
    }
    // Resolved through config::active_index — the same call the download path will make, so this
    // marker is the real answer to "which source gets used", not a second guess at it.
    let active = crate::config::active_index(&sweep.sources, settings.selected.as_ref());
    let mark = |i: usize, s: &crate::config::Source| {
        format!(
            "{}{}{}",
            s.url().unwrap_or("<primary>"),
            if s.enabled() { "" } else { "  (off)" },
            if active == Some(i) { "  <- IN USE" } else { "" }
        )
    };

    if !measure {
        for (i, s) in sweep.sources.iter().enumerate() {
            println!("{}", mark(i, s));
        }
        return Ok(());
    }
    for (i, (s, p)) in sweep.sources.iter().zip(sweep.probes.iter()).enumerate() {
        let speed = match p.bytes_per_sec {
            Some(b) => format!("{:.2} MiB/s", b as f64 / (1024.0 * 1024.0)),
            None => "—".to_string(),
        };
        println!(
            "{}\n  {:<8} latency {:>7}  speed {:>12}  range {:<3}  tag {}",
            mark(i, s),
            if p.healthy() { "HEALTHY" } else { "UNUSABLE" },
            p.latency_ms.map(|m| format!("{m}ms")).unwrap_or_else(|| "—".into()),
            speed,
            if p.range_ok { "ok" } else { "NO" },
            p.tag.as_deref().unwrap_or("—"),
        );
        if let Some(e) = &p.error {
            println!("  error: {e}");
        }
    }

    if flags.iter().any(|f| f == "--save") {
        // `false`: the headless sweep does not touch the user's pin — only the settings pane's own
        // test button does, and only because pressing it asks for the ranking's answer.
        sweep.persist(false)?;
        println!("\nsaved {} source(s) to settings", sweep.sources.len());
    }
    Ok(())
}

pub fn run_check(flags: &[String]) -> Result<()> {
    let (settings, tag) = settings_from_flags(flags);
    let dl = Github::new(settings.token());
    let r = engine::check(&settings, &dl, tag.as_deref())?;
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
    let dl = Github::new(settings.token());
    let r = install::install(&settings, &dl, tag.as_deref(), None, None, None)?;
    println!("Installed {}: wrote {}, removed {}", r.version, r.written.len(), r.removed.len());
    // headless: warm the customization cache synchronously (the GUI runs this detached)
    install::warm_cache(&settings, &dl);
    Ok(())
}

pub fn run_uninstall(flags: &[String]) -> Result<()> {
    let (settings, _tag) = settings_from_flags(flags);
    let r = install::uninstall(&settings)?;
    println!("Uninstalled {}: restored {}, deleted {}", r.version, r.restored.len(), r.deleted.len());
    Ok(())
}

// ---- base game (game-install / game-verify against Settings::game_repo) ----

fn game_repo_manifest(
    settings: &Settings,
) -> Result<(Box<dyn Downloader>, crate::downloader::Release, crate::manifest::Manifest)> {
    // Through `open_repo`, because the game repo is PUBLIC and the credential rule is not the GUI's
    // private business: anonymous first, the baked token only once the server has actually refused.
    // This used to build `Github::new(settings.token())` and so sent the token on every request —
    // and the baked credential is scoped to the DIST repo, which a fine-grained PAT may be refused
    // for here, failing `game-install` against a repo that would have served it anonymously.
    // "Headless keeps auth simple" was the old justification; simple and wrong is not simpler.
    let (dl, release) = crate::cmd::open_repo(settings.game_repo(), settings)?;
    let manifest =
        engine::manifest_of(settings, dl.as_ref(), &release, crate::trust::Payload::Game)?;
    // file assets are sharded across prereleases (GitHub caps 1000 assets per release)
    let release = engine::merged_game_release(dl.as_ref(), settings.game_repo(), release)?;
    Ok((dl, release, manifest))
}

pub fn run_game_install(flags: &[String]) -> Result<()> {
    let (settings, _tag) = settings_from_flags(flags);
    let game_dir = settings.resolve_game_dir()?;
    let (dl, release, manifest) = game_repo_manifest(&settings)?;
    // ONE origin for the file downloads: `game_repo_manifest` walks the source chain to RESOLVE the
    // release, and whichever source answered then serves every asset. Per-asset failover ACROSS
    // sources mid-download is the GUI's business.
    let origins = [install::Origin::new(dl.as_ref(), &release)];
    let r = install::install_base(&game_dir, &origins, &manifest, None, None, None)?;
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
    let (_dl, _release, manifest) = game_repo_manifest(&settings)?;
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
