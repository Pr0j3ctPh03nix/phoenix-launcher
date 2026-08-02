//! Headless CLI (debug builds keep a console) — exercises the engine without the webview.
//! Reuses saved settings; flags override them. A token may also come from PHOENIX_GITHUB_TOKEN.

use std::path::PathBuf;

use anyhow::Result;

use crate::config::Settings;
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
        };
        println!("  [{s:>7}] {}", f.dest);
    }
    Ok(())
}

pub fn run_install(flags: &[String]) -> Result<()> {
    let (settings, tag) = settings_from_flags(flags);
    let dl = Github::new(settings.token());
    let r = install::install(&settings, &dl, tag.as_deref(), None)?;
    println!("Installed {}: wrote {}, removed {}", r.version, r.written.len(), r.removed.len());
    Ok(())
}

pub fn run_uninstall(flags: &[String]) -> Result<()> {
    let (settings, _tag) = settings_from_flags(flags);
    let r = install::uninstall(&settings)?;
    println!("Uninstalled {}: restored {}, deleted {}", r.version, r.restored.len(), r.deleted.len());
    Ok(())
}
