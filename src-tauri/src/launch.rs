//! Launches the game with the Phoenix launch options.
//!
//! The base options are hardcoded (the shim requires them); the renderer flag, the optional
//! flags (LAUNCH_FLAGS) and the user's own extras come from settings.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

use crate::fslock;

/// Always passed, before everything else.
pub const BASE_OPTIONS: [&str; 4] = ["-insecure", "-console", "+exec", "autoexec.cfg"];

/// An optional launch option that settings expose as a switch.
pub struct LaunchFlag {
    /// Stable id: the settings key, and the i18n label key suffix (`set.flag.<id>`).
    pub id: &'static str,
    /// Appended to the command line when the flag is on.
    pub args: &'static [&'static str],
    /// Used when settings hold no entry for this id yet.
    pub default: bool,
}

/// The single source of truth for the optional flags. A new row here becomes a new settings
/// checkbox (the view, the save command and the spawn all read this table) — the frontend only
/// needs the matching `set.flag.<id>` string, and falls back to showing the raw args without it.
pub const LAUNCH_FLAGS: &[LaunchFlag] = &[LaunchFlag {
    // Dota syncs keybindings through Steam Cloud and can overwrite local ones on launch; this
    // convar keeps whatever is on disk.
    id: "noCloudKeybinds",
    args: &["+dota_keybindings_cloud_disable", "1"],
    default: false,
}];

/// Is `id` on? An id settings never stored falls back to the flag's own default; an id that is
/// not in the table is off — only the table decides what can reach the command line, so a stale
/// key from another build cannot inject arguments.
pub fn flag_enabled(flags: &BTreeMap<String, bool>, id: &str) -> bool {
    let Some(f) = LAUNCH_FLAGS.iter().find(|f| f.id == id) else { return false };
    flags.get(id).copied().unwrap_or(f.default)
}

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

/// The full argument list, in order: base options, renderer, the enabled optional flags, then
/// the user's extras — theirs come last so a duplicated option ends up on their value.
pub fn args_for(renderer: &str, extra: &str, flags: &BTreeMap<String, bool>) -> Vec<String> {
    let mut args: Vec<String> = BASE_OPTIONS.iter().map(|s| s.to_string()).collect();
    args.push(renderer_flag(renderer).to_string());
    for f in LAUNCH_FLAGS.iter().filter(|f| flag_enabled(flags, f.id)) {
        args.extend(f.args.iter().map(|s| s.to_string()));
    }
    args.extend(split_extra(extra));
    args
}

/// Spawn `game/bin/win64/dota2.exe` with the arguments from `args_for`.
pub fn launch(
    game_dir: &Path,
    renderer: &str,
    extra: &str,
    flags: &BTreeMap<String, bool>,
) -> Result<()> {
    let win64 = game_dir.join("game").join("bin").join("win64");
    let exe = win64.join("dota2.exe");
    if !exe.exists() {
        bail!("dota2.exe not found at {}", exe.display());
    }
    let mut cmd = std::process::Command::new(&exe);
    cmd.current_dir(&win64).args(args_for(renderer, extra, flags));
    cmd.spawn()
        .map(|_| ())
        .with_context(|| format!("launching {}", exe.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_flags_are_table_driven_and_ordered() {
        let none = BTreeMap::new();
        let base = args_for("dx11", "", &none);
        assert_eq!(base, ["-insecure", "-console", "+exec", "autoexec.cfg", "-dx11"]);

        // unset -> the flag's own default (all currently ship off)
        for f in LAUNCH_FLAGS {
            assert_eq!(flag_enabled(&none, f.id), f.default);
        }
        // an id that isn't in the table can never contribute args
        let mut junk = BTreeMap::new();
        junk.insert("notAFlag".to_string(), true);
        assert!(!flag_enabled(&junk, "notAFlag"));
        assert_eq!(args_for("dx11", "", &junk), base);

        // on -> args land after the renderer and before the user's extras
        let mut on = BTreeMap::new();
        on.insert("noCloudKeybinds".to_string(), true);
        assert_eq!(
            args_for("dx9", "-novid \"a b\"", &on),
            [
                "-insecure",
                "-console",
                "+exec",
                "autoexec.cfg",
                "-dx9",
                "+dota_keybindings_cloud_disable",
                "1",
                "-novid",
                "a b",
            ]
        );

        // explicitly off -> nothing added
        on.insert("noCloudKeybinds".to_string(), false);
        assert_eq!(args_for("dx11", "", &on), base);
    }

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
