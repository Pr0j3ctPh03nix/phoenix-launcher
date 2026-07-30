//! Launches the game with the Phoenix launch options.
//!
//! The base options are hardcoded (the shim requires them); the renderer flag and the user's own
//! extras come from settings.

use anyhow::{bail, Context, Result};
use std::path::Path;

/// Always passed, before everything else.
pub const BASE_OPTIONS: [&str; 4] = ["-insecure", "-console", "+exec", "autoexec.cfg"];

/// The renderer flag for a settings `renderer` value ("dx11" is the default for anything unknown).
pub fn renderer_flag(renderer: &str) -> &'static str {
    match renderer {
        "dx9" => "-dx9",
        _ => "-dx11",
    }
}

/// Split user extras on whitespace, honoring double quotes ("path with spaces" stays one arg).
fn split_extra(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for ch in s.chars() {
        match ch {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Spawn `game/bin/win64/dota2.exe` with base options + renderer flag + the user's extras.
pub fn launch(game_dir: &Path, renderer: &str, extra: &str) -> Result<()> {
    let win64 = game_dir.join("game").join("bin").join("win64");
    let exe = win64.join("dota2.exe");
    if !exe.exists() {
        bail!("dota2.exe not found at {}", exe.display());
    }
    let mut cmd = std::process::Command::new(&exe);
    cmd.current_dir(&win64).args(BASE_OPTIONS).arg(renderer_flag(renderer));
    cmd.args(split_extra(extra));
    cmd.spawn()
        .map(|_| ())
        .with_context(|| format!("launching {}", exe.display()))
}
