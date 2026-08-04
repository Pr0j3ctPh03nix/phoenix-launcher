//! Launches the game with the Phoenix launch options.
//!
//! The base options are hardcoded (the shim requires them); the renderer flag and the user's own
//! extras come from settings.

use anyhow::{bail, Context, Result};
use std::path::Path;

use crate::fslock;

/// Always passed, before everything else.
pub const BASE_OPTIONS: [&str; 4] = ["-insecure", "-console", "+exec", "autoexec.cfg"];

/// Is the game currently running? Detected by write-probing the executable: a running process
/// image sharing-violates a write open. Only sharing/lock violations count — a read-only or
/// ACL-denied exe is unwritable but not running (see fslock). False when the exe is absent.
/// Cheap (one syscall) — the frontend polls it to track the game.
pub fn game_running(game_dir: &Path) -> bool {
    let exe = game_dir.join("game").join("bin").join("win64").join("dota2.exe");
    fslock::held_by_process(&exe)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // set_readonly(false) is fine here: Windows-only test file, deleted right after
    #[allow(clippy::permissions_set_readonly_false)]
    fn game_running_tracks_the_exe_lock() {
        let dir = std::env::temp_dir().join("phoenix-launch-test-running");
        let _ = std::fs::remove_dir_all(&dir);
        let exe = dir.join("game").join("bin").join("win64").join("dota2.exe");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();

        // no exe at all -> not running
        assert!(!game_running(&dir));

        // exe present and writable -> not running
        std::fs::write(&exe, b"exe").unwrap();
        assert!(!game_running(&dir));

        // read-only exe (write denied with ERROR_ACCESS_DENIED, not a sharing violation) ->
        // NOT running — a restrictive attribute/ACL must not brick the UI into "In game"
        let mut perm = std::fs::metadata(&exe).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&exe, perm.clone()).unwrap();
        assert!(!game_running(&dir));
        perm.set_readonly(false);
        std::fs::set_permissions(&exe, perm).unwrap();

        // exe held with no sharing (a running image looks like this) -> running
        use std::os::windows::fs::OpenOptionsExt;
        let lock = std::fs::OpenOptions::new().read(true).share_mode(0).open(&exe).unwrap();
        assert!(game_running(&dir));

        drop(lock);
        assert!(!game_running(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
