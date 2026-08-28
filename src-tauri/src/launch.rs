//! Launches the game with the Phoenix launch options.
//!
//! The base options are hardcoded (the shim requires them); the renderer flag, the optional
//! flags (LAUNCH_FLAGS) and the user's own extras come from settings.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::fslock;

/// Always passed, before everything else. The `+convar` values are policy, not preference:
/// keybinds stay local (a Steam Cloud sync can overwrite real bindings on launch), and the
/// network rates are fixed at 60 because the Phoenix servers run at 60 ticks. The rates are
/// additionally PINNED (see `PINNED_CONVARS`): autoexec.cfg wins over command-line `+` options,
/// so launch strips their setters from the cfg and the editor flags them. The keybinds convar is
/// deliberately NOT pinned — an autoexec line is the one remaining way to opt back into cloud
/// sync now that its tweak switch is gone.
pub const BASE_OPTIONS: [&str; 10] = [
    "-insecure",
    "-console",
    "+exec",
    "autoexec.cfg",
    "+dota_keybindings_cloud_disable",
    "1",
    "+cl_updaterate",
    "60",
    "+cl_cmdrate",
    "60",
];

/// The BASE_OPTIONS convars whose value must actually HOLD: `strip_pinned` removes their setters
/// from autoexec.cfg before every launch, and the editor flags such lines as it highlights.
pub const PINNED_CONVARS: &[&str] = &["cl_updaterate", "cl_cmdrate"];

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
/// Currently empty (the former tweaks are baked into BASE_OPTIONS); the machinery stays for the
/// next genuinely optional flag.
pub const LAUNCH_FLAGS: &[LaunchFlag] = &[];

/// Is `id` on? An id settings never stored falls back to the flag's own default; an id that is
/// not in the table is off — only the table decides what can reach the command line, so a stale
/// key from another build cannot inject arguments.
pub fn flag_enabled(flags: &BTreeMap<String, bool>, id: &str) -> bool {
    let Some(f) = LAUNCH_FLAGS.iter().find(|f| f.id == id) else { return false };
    flags.get(id).copied().unwrap_or(f.default)
}

/// The user's autoexec.cfg for a game folder — one implementation, shared with the editor's
/// read/save commands.
pub fn autoexec_cfg(game_dir: &Path) -> PathBuf {
    game_dir.join("game").join("dota").join("cfg").join("autoexec.cfg")
}

/// Split a cfg line into code and its `//` comment, quote-aware (a `//` inside a quoted value,
/// e.g. a URL, stays value) — the same rule the editor's highlighter applies.
fn split_comment(line: &str) -> (&str, &str) {
    let b = line.as_bytes();
    let mut quoted = false;
    for i in 0..b.len().saturating_sub(1) {
        match b[i] {
            b'"' => quoted = !quoted,
            b'/' if !quoted && b[i + 1] == b'/' => return (&line[..i], &line[i..]),
            _ => {}
        }
    }
    (line, "")
}

/// The command a single `;`-separated statement issues: its first token, quote-trimmed and
/// lowercased (the console is case-insensitive). First token only — a convar named in an
/// argument position (a `bind`, an `echo`) is mentioned, not set.
fn stmt_command(stmt: &str) -> Option<String> {
    let tok = stmt.split_whitespace().next()?.trim_matches('"');
    (!tok.is_empty()).then(|| tok.to_ascii_lowercase())
}

/// The autoexec body without the statements that set a `PINNED_CONVARS` convar — `None` when
/// there is nothing to strip. Everything else survives verbatim, line endings included (`\r`
/// rides as trailing whitespace): a line reduced to nothing disappears, one reduced to its
/// comment keeps the comment.
pub fn strip_pinned(content: &str) -> Option<String> {
    let mut changed = false;
    let mut out: Vec<String> = Vec::new();
    for line in content.split('\n') {
        let (code, comment) = split_comment(line);
        let mut removed = false;
        let kept: Vec<&str> = code
            .split(';')
            .filter(|s| {
                let hit = stmt_command(s).is_some_and(|c| PINNED_CONVARS.contains(&c.as_str()));
                removed |= hit;
                !hit
            })
            .collect();
        if !removed {
            out.push(line.to_string());
            continue;
        }
        changed = true;
        let code = kept.join(";");
        if !code.trim().is_empty() || !comment.is_empty() {
            out.push(format!("{code}{comment}"));
        } // else: the line only set pinned convars — gone entirely
    }
    changed.then(|| out.join("\n"))
}

/// Make the pinned rates actually hold: rewrite autoexec.cfg without their setters, atomically
/// (temp + rename — this is the USER'S file). Deliberately quiet about failure: a missing cfg has
/// nothing to strip, a non-UTF-8 one must not be rewritten from a lossy decode (the editor's rule),
/// and a write error only means the pin doesn't hold this run — never a reason not to launch.
fn enforce_pins(game_dir: &Path) {
    let p = autoexec_cfg(game_dir);
    let Ok(bytes) = std::fs::read(&p) else { return };
    let Ok(content) = String::from_utf8(bytes) else { return };
    let Some(stripped) = strip_pinned(&content) else { return };
    let tmp = p.with_extension("cfg.tmp");
    if std::fs::write(&tmp, stripped).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
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
    enforce_pins(game_dir);
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
    fn args_are_base_then_renderer_then_extras() {
        let none = BTreeMap::new();
        let base = args_for("dx11", "", &none);
        assert_eq!(
            base,
            [
                "-insecure",
                "-console",
                "+exec",
                "autoexec.cfg",
                "+dota_keybindings_cloud_disable",
                "1",
                "+cl_updaterate",
                "60",
                "+cl_cmdrate",
                "60",
                "-dx11",
            ]
        );

        // a flag id the (currently empty) table doesn't know can never contribute args
        let mut junk = BTreeMap::new();
        junk.insert("notAFlag".to_string(), true);
        assert!(!flag_enabled(&junk, "notAFlag"));
        assert_eq!(args_for("dx11", "", &junk), base);

        // extras come last, quoted values intact
        let extras = args_for("dx9", "-novid \"a b\"", &none);
        assert_eq!(&extras[..4], ["-insecure", "-console", "+exec", "autoexec.cfg"]);
        assert_eq!(&extras[extras.len() - 3..], ["-dx9", "-novid", "a b"]);
    }

    #[test]
    fn strip_pinned_removes_only_setters() {
        // nothing to strip: mentions in comments and argument positions are not setters
        assert!(strip_pinned(
            "echo hi\n// cl_updaterate 30\nbind q \"say cl_cmdrate\"\nvolume 1\n"
        )
        .is_none());
        assert!(strip_pinned("").is_none());

        // setters go (case-insensitive, quoted, mid-line); everything else survives verbatim
        let s = strip_pinned(
            "volume 1\r\n\
             CL_UPDATERATE \"128\"\n\
             cl_cmdrate 128; echo hi // keep\n\
             \"cl_updaterate\" 1 // why\n\
             \n\
             last\n",
        )
        .unwrap();
        assert_eq!(s, "volume 1\r\n echo hi // keep\n// why\n\nlast\n");
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
