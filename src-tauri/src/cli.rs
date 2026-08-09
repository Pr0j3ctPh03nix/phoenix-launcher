//! Headless CLI (debug builds keep a console) — exercises the engine without the webview.
//! Reuses saved settings; flags override them. A token may also come from PHOENIX_GITHUB_TOKEN.

use std::path::PathBuf;

use anyhow::Result;

use crate::config::Settings;
use crate::downloader::Downloader as _;
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
) -> Result<(Github, crate::downloader::Release, crate::manifest::Manifest)> {
    // headless keeps auth simple: the game repo is public by design, and the CLI's token flag /
    // env var is available for testing against a private one
    let dl = Github::new(settings.token());
    let release = dl.fetch_release(settings.game_repo(), None)?;
    let manifest = engine::manifest_of(&dl, &release)?;
    // file assets are sharded across prereleases (GitHub caps 1000 assets per release)
    let release = engine::merged_game_release(&dl, settings.game_repo(), release)?;
    Ok((dl, release, manifest))
}

pub fn run_game_install(flags: &[String]) -> Result<()> {
    let (settings, _tag) = settings_from_flags(flags);
    let game_dir = settings.resolve_game_dir()?;
    let (dl, release, manifest) = game_repo_manifest(&settings)?;
    let r = install::install_base(&game_dir, &dl, &release, &manifest, None, None, None)?;
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
