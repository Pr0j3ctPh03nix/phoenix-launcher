//! Base-game files the user has told us to LEAVE ALONE, stored next to the game like
//! `InstalledState` so it is portable, per-folder, and survives a lost config dir.
//!
//! Why this exists: `game_verify` compares the folder against the vanilla manifest, and a mod that
//! replaces a stock file is a hash mismatch exactly like a corrupted one. Without a record, every
//! verify re-reports the user's mods as damage and every repair offers to destroy them.
//!
//! PINNED TO CONTENT, never to a path. A pin is `dest -> the sha256 that was there when the user
//! approved it`, so it says "these bytes are intentional", not "never check this file again". The
//! difference is the whole safety story: a path-only pin silently disables integrity checking for
//! that dest forever — including long after the mod is gone and the file has genuinely rotted —
//! while a content pin expires the moment the bytes change, putting the file back in front of the
//! user as a difference they have not seen yet. A mod UPDATE therefore asks once more, which is
//! the honest outcome: the thing they approved is not the thing that is there now.
//!
//! Kept files are never hidden — `game_verify` counts them and the files view lists them in their
//! own state. "Silently skipped" is the failure mode this design exists to avoid.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const KEEP_FILE: &str = ".phoenix-keep.json";

/// One approval. Two hashes, because a pin is a decision about a COMPARISON, not about a file:
/// "keep mine instead of theirs". Recording only mine made the approval outlive the thing it was
/// weighed against — a later release could change that file and the pin would go on suppressing
/// it forever, silently, while the launcher reported "up to date".
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Pin {
    /// A pin written before `theirs` existed: content only. Held rather than expired — we cannot
    /// tell whether the release moved on, and re-asking about every file somebody already decided
    /// would be the wrong way to be wrong.
    Mine(String),
    Full { mine: String, theirs: Option<String> },
}

impl Pin {
    pub fn mine(&self) -> &str {
        match self {
            Pin::Mine(m) => m,
            Pin::Full { mine, .. } => mine,
        }
    }
    pub fn theirs(&self) -> Option<&str> {
        match self {
            Pin::Mine(_) => None,
            Pin::Full { theirs, .. } => theirs.as_deref(),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct KeepList {
    /// dest (manifest form, `/`-separated) -> the approval recorded for it.
    ///
    /// A map, not a Vec of pairs: one dest can only have one approved content, and a BTreeMap
    /// makes that unrepresentable-otherwise as well as giving the file a stable diff-friendly
    /// order.
    #[serde(default)]
    pub files: BTreeMap<String, Pin>,
}

impl KeepList {
    pub fn path(game_dir: &Path) -> PathBuf {
        game_dir.join(KEEP_FILE)
    }

    /// Load, treating both "no file" and "unreadable file" as an empty list.
    ///
    /// A corrupt keep list is quarantined rather than tolerated, exactly like `InstalledState`:
    /// half-parsing it could drop some pins and keep others, and a pin that vanishes is far less
    /// dangerous than one that survives wrong. Losing every pin costs the user one round of
    /// re-approving files that are still sitting right there in the view.
    pub fn load(game_dir: &Path) -> Self {
        let p = Self::path(game_dir);
        let Ok(text) = std::fs::read_to_string(&p) else { return Self::default() };
        match serde_json::from_str(&text) {
            Ok(k) => k,
            Err(_) => {
                let _ = std::fs::rename(&p, p.with_extension("json.bak"));
                Self::default()
            }
        }
    }

    /// Write via temp + rename (atomic on one volume) — a crash mid-write can never leave a torn
    /// list that the next load would quarantine.
    pub fn save(&self, game_dir: &Path) -> Result<()> {
        let p = Self::path(game_dir);
        // An empty list has nothing to say: remove the file instead of leaving `{"files":{}}`
        // behind, so a folder with no kept files looks like one.
        if self.files.is_empty() {
            let _ = std::fs::remove_file(&p);
            return Ok(());
        }
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, &p)?;
        Ok(())
    }

    /// Does the approval still hold?
    ///
    /// BOTH sides must still be what they were. `mine` changing means the bytes the user approved
    /// are not the bytes that are there now. `theirs` changing means the release now ships
    /// something different from what the decision was weighed against — the user chose their
    /// version over a specific other version, and that other version is gone. Either way the
    /// premise expired and the file goes back in front of them as a difference they have not
    /// ruled on. Symmetry is the point: an approval that survived one side changing but not the
    /// other would be arbitrary.
    ///
    /// `theirs_now` is `None` for callers with no manifest to compare against (the offline
    /// verdict). Unknown is not "changed": the pin holds.
    pub fn is_kept(&self, dest: &str, mine: &str, theirs_now: Option<&str>) -> bool {
        let Some(p) = self.files.get(dest) else { return false };
        if p.mine() != mine {
            return false;
        }
        match (p.theirs(), theirs_now) {
            (Some(was), Some(now)) => was == now,
            _ => true,
        }
    }

    /// The user kept THIS content here, and the authority has since changed what it ships.
    ///
    /// `is_kept` is already false in that case — the pin expired — but "expired because they
    /// changed it" and "never ruled on" are different things to say to a user, and defaulting
    /// them the same way silently reverses a decision they made. This is the difference.
    pub fn superseded(&self, dest: &str, mine: &str, theirs_now: Option<&str>) -> bool {
        let Some(p) = self.files.get(dest) else { return false };
        match (p.mine() == mine, p.theirs(), theirs_now) {
            (true, Some(was), Some(now)) => was != now,
            _ => false,
        }
    }

    pub fn pin(&mut self, dest: &str, mine: &str, theirs: Option<String>) {
        self.files
            .insert(dest.to_string(), Pin::Full { mine: mine.to_string(), theirs });
    }

    pub fn unpin(&mut self, dest: &str) {
        self.files.remove(dest);
    }
}

/// Drop the pins on `dests` — what a restore does after the fact. Restoring a file the user had
/// pinned IS them taking the approval back, and a pin left behind would go on describing bytes
/// that are no longer there (harmless, since pins are content-matched, but it would also mean the
/// next mod at that dest inherited an approval nobody gave it).
pub fn unpin_all(game_dir: &Path, dests: &std::collections::HashSet<String>) -> Result<()> {
    let mut k = KeepList::load(game_dir);
    let before = k.files.len();
    k.files.retain(|d, _| !dests.contains(d));
    if k.files.len() != before {
        k.save(game_dir)?;
    }
    Ok(())
}

/// Pin each dest to whatever is there RIGHT NOW, against what the release currently ships.
///
/// The hash is read here rather than taken from the caller on purpose: a pin is a promise about
/// bytes on disk, and the only moment that promise can be made truthfully is the moment it is
/// written. A hash round-tripped through the UI describes what was there when the view was built,
/// which may be minutes and one mod installation ago. Reads are memoized, so re-hashing files that
/// were just examined costs a stat.
///
/// `path_of` resolves a dest to the file the PLAN compared, which is not always `game_dir/dest`:
/// for a base dest the shim owns (or relocated), the evidence lives at the preserved original
/// under `.phoenix-vanilla/`. Hashing a different file than the plan read would record an approval
/// that can never match, so the user's "leave this alone" answer would silently never take.
///
/// `expected` gives the authority's current hash for a dest; `None` means it could not be
/// established (offline, or a dest that authority does not carry). A dest that has become intact
/// in the meantime is NOT pinned — there is nothing to approve.
pub fn pin_all(
    game_dir: &Path,
    dests: &[String],
    path_of: impl Fn(&str) -> PathBuf,
    expected: impl Fn(&str) -> Option<String>,
) -> Result<u32> {
    let mut k = KeepList::load(game_dir);
    let mut n = 0;
    for dest in dests {
        let Ok(mine) = crate::verify::sha256_file_cached(&path_of(dest)) else { continue };
        let theirs = expected(dest);
        if theirs.as_deref() == Some(mine.as_str()) {
            k.unpin(dest);
            continue;
        }
        // NEVER null out a `theirs` we already had. Unknown is not "there is nothing on the other
        // side": re-approving a file while the manifest is unreachable would quietly turn a
        // two-sided pin into a legacy one-sided one, and those hold against ANY future release —
        // so the file would stop receiving updates for good, silently, which is the failure the
        // second hash was added to prevent.
        let theirs = theirs.or_else(|| k.files.get(dest).and_then(|p| p.theirs().map(str::to_string)));
        k.pin(dest, &mine, theirs);
        n += 1;
    }
    k.save(game_dir)?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("phoenix-keep-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn roundtrip_and_content_pinning() {
        let dir = tempdir("roundtrip");
        let mut k = KeepList::default();
        k.pin("game/dota/pak01_dir.vpk", "aa", Some("theirs-v1".into()));
        k.save(&dir).unwrap();

        let k = KeepList::load(&dir);
        assert!(k.is_kept("game/dota/pak01_dir.vpk", "aa", Some("theirs-v1")));
        // the pin is on the BYTES: the same dest holding anything else is not approved, which is
        // what makes a mod update (or later corruption) surface again instead of staying hidden
        assert!(!k.is_kept("game/dota/pak01_dir.vpk", "bb", Some("theirs-v1")));
        assert!(!k.is_kept("game/dota/other.vpk", "aa", Some("theirs-v1")));
        // and the OTHER side of the comparison expires it just the same: the user chose their
        // bytes over a specific release version, and that version is gone
        assert!(!k.is_kept("game/dota/pak01_dir.vpk", "aa", Some("theirs-v2")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_list_removes_the_file() {
        let dir = tempdir("empty");
        let mut k = KeepList::default();
        k.pin("a", "aa", None);
        k.save(&dir).unwrap();
        assert!(KeepList::path(&dir).exists());

        k.unpin("a");
        k.save(&dir).unwrap();
        assert!(!KeepList::path(&dir).exists(), "no pins left = no file");
        assert!(KeepList::load(&dir).files.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_is_quarantined_not_half_read() {
        let dir = tempdir("corrupt");
        std::fs::write(KeepList::path(&dir), b"{ not json").unwrap();
        assert!(KeepList::load(&dir).files.is_empty());
        assert!(!KeepList::path(&dir).exists());
        assert!(dir.join(".phoenix-keep.json.bak").exists(), "the evidence is kept");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
