//! Per-install record of what the updater placed, stored next to the game so it is portable and
//! survives a lost config dir. Drives uninstall and future diffs.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const STATE_FILE: &str = ".phoenix-state.json";

#[derive(Debug, Serialize, Deserialize)]
pub struct InstalledState {
    pub version: String,
    pub files: Vec<InstalledFile>,
    /// LEGACY. True only if the updater itself created winmm_orig.dll (copied from System32) — a
    /// user's own pre-existing one leaves this false, so uninstall never deletes it.
    ///
    /// Launchers past 1.4.0 never set this true: the shim resolves the system DLL itself and
    /// nothing copies it any more (see `install::WINMM_ORIG`). It is still READ, and carried
    /// forward verbatim by every install, because folders an older launcher set up still hold the
    /// file and their uninstall still has to collect it. Zeroing it on update would strand a copy
    /// of a system DLL in the game folder permanently.
    #[serde(default)]
    pub winmm_orig_created: bool,
    /// Dests where a removal RESTORED a preserved vanilla original — the file sitting there now is
    /// stock, not ours. Without this record the next plan sees a file at a `remove[]` dest and
    /// flags it Remove again, so the very original the removal put back was then re-preserved and
    /// the dest emptied: the restore undone one release later, with a bogus "1 to change" in
    /// between. `plan` skips remove[] dests recorded here. `#[serde(default)]` keeps state files
    /// written before this field loading clean (empty = the old behavior, converging as before).
    #[serde(default)]
    pub restored: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InstalledFile {
    pub dest: String,
    pub sha256: String,
}

impl InstalledState {
    pub fn path(game_dir: &Path) -> PathBuf {
        game_dir.join(STATE_FILE)
    }

    pub fn load(game_dir: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(Self::path(game_dir)).ok()?;
        match serde_json::from_str(&text) {
            Ok(s) => Some(s),
            Err(_) => {
                // Corrupt state: quarantine it (moved aside, kept for forensics) and behave as
                // not-installed. Silently misreading it would misclassify our files as foreign
                // on the next install; a no-op install rewrites a fresh state (see install.rs).
                let _ = std::fs::rename(
                    Self::path(game_dir),
                    Self::path(game_dir).with_extension("json.bak"),
                );
                None
            }
        }
    }

    pub fn save(&self, game_dir: &Path) -> Result<()> {
        // temp + rename (atomic on the same volume): a crash mid-write can never leave a torn
        // state file — the quarantine path in `load` stays a last resort, not the normal one
        let p = Self::path(game_dir);
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, &p)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = std::env::temp_dir().join("phoenix-state-test-roundtrip");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = InstalledState {
            version: "1.0.0".into(),
            files: vec![InstalledFile { dest: "game/bin/win64/winmm.dll".into(), sha256: "aa".into() }],
            winmm_orig_created: true,
            restored: vec!["game/dota/old.vpk".into()],
        };
        s.save(&dir).unwrap();
        let loaded = InstalledState::load(&dir).unwrap();
        assert_eq!(loaded.version, "1.0.0");
        assert_eq!(loaded.files.len(), 1);
        assert!(loaded.winmm_orig_created);
        assert_eq!(loaded.restored, vec!["game/dota/old.vpk".to_string()]);

        // a state file from a build that predates `restored` still loads (serde default)
        let legacy = r#"{ "version": "0.9", "files": [], "winmm_orig_created": false }"#;
        std::fs::write(InstalledState::path(&dir), legacy).unwrap();
        assert!(InstalledState::load(&dir).unwrap().restored.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_state_is_quarantined() {
        let dir = std::env::temp_dir().join("phoenix-state-test-corrupt");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(InstalledState::path(&dir), b"{ not json").unwrap();
        assert!(InstalledState::load(&dir).is_none());
        // moved aside, not re-read: the original is gone, the .bak keeps the evidence
        assert!(!InstalledState::path(&dir).exists());
        assert!(dir.join(".phoenix-state.json.bak").exists());
        assert!(InstalledState::load(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
