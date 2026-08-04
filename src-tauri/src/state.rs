//! Per-install record of what the updater placed, stored next to the game so it is portable and
//! survives a lost config dir. Drives uninstall and future diffs.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const STATE_FILE: &str = ".phoenix-state.json";

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct InstalledState {
    pub version: String,
    pub files: Vec<InstalledFile>,
    /// True only if the updater itself created winmm_orig.dll (copied from System32). A user's own
    /// pre-existing winmm_orig.dll leaves this false, so uninstall never deletes it.
    #[serde(default)]
    pub winmm_orig_created: bool,
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
        };
        s.save(&dir).unwrap();
        let loaded = InstalledState::load(&dir).unwrap();
        assert_eq!(loaded.version, "1.0.0");
        assert_eq!(loaded.files.len(), 1);
        assert!(loaded.winmm_orig_created);
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
