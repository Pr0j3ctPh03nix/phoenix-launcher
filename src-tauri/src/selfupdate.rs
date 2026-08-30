//! Launcher self-update: replace this executable with a newer one from the launcher repo's
//! Releases. UI-agnostic like the rest of the engine — network goes through `Downloader`, and the
//! restart itself is left to the caller (cmd/selfupdate.rs), since spawning processes is a shell
//! concern.
//!
//! Windows lets you RENAME a running executable, but not delete or overwrite it, and a process's
//! reported image path does NOT follow such a rename. That is the entire mechanism:
//!
//!   1. download the new exe beside the current one as `<stem>.new.exe`, and verify it;
//!   2. rename the running `<stem>.exe` -> `<stem>.old.exe`   (permitted while running);
//!   3. rename `<stem>.new.exe` -> `<stem>.exe`               (that name is free now);
//!   4. spawn `<stem>.exe` — which is the NEW file — and exit;
//!   5. the incoming process deletes `<stem>.old.exe` once the outgoing one is gone.
//!
//! Step 5 cannot happen in the outgoing process: its own image stays locked until it exits, so
//! the deletion has to outlive it (`cleanup_old`, called at startup).
//!
//! Nothing is swapped before the download is verified against the release's sha256 sidecar.
//! Writing a truncated or corrupt exe over a working launcher leaves the user with neither a
//! launcher nor a way back, so this mirrors install.rs: no unverified byte is ever committed.

use anyhow::{bail, Context, Result};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::downloader::{Asset, ChunkProgress, Downloader, Release};
use crate::engine::version_lt;

/// The launcher binary published by release.yml, and the checksum sidecar beside it.
const EXE_ASSET: &str = "phoenix-launcher.exe";
const SHA_ASSET: &str = "phoenix-launcher.exe.sha256";

/// Passed to the freshly swapped-in binary so it can confirm the update landed.
pub const UPDATED_FLAG: &str = "--updated";

/// The incoming process races the outgoing one's exit, so the first `.old` deletes are expected
/// to fail. ~5s of retries covers a normal shutdown; anything longer is left to the next launch.
const CLEANUP_TRIES: u32 = 20;
const CLEANUP_DELAY: Duration = Duration::from_millis(250);

/// A launcher release newer than this build.
pub struct Available {
    pub tag: String,
    /// `tag` without its leading "v" — what the UI shows.
    pub version: String,
    /// This build's version, so the UI can render "1.2.1 -> 1.3.0" without a second call.
    pub current: String,
    /// The GitHub release description, if it has one.
    pub notes: Option<String>,
}

fn exe_path() -> Result<PathBuf> {
    std::env::current_exe().context("locating the launcher executable")
}

/// `<stem><suffix>` next to `exe` — e.g. ".new.exe" -> `phoenix-launcher.new.exe`. Built from the
/// stem of the RUNNING file, so a launcher the user renamed still parks its siblings beside it.
fn sibling(exe: &Path, suffix: &str) -> PathBuf {
    let stem = exe.file_stem().unwrap_or(exe.as_os_str());
    let mut name = stem.to_os_string();
    name.push(suffix);
    exe.with_file_name(name)
}

/// Is `release` newer than this build? `None` means we are current (or ahead of it, which is
/// normal for a local dev build).
///
/// Takes an already-fetched release rather than a repo: WHICH repo and WHICH credentials can see
/// it is a shell decision (cmd/selfupdate.rs resolves it), and this way check and apply cost one
/// round trip each instead of two.
pub fn available(release: &Release) -> Option<Available> {
    let current = env!("CARGO_PKG_VERSION");
    let version = release.tag_name.trim_start_matches('v').to_string();
    if !version_lt(current, &version) {
        return None;
    }
    Some(Available {
        tag: release.tag_name.clone(),
        version,
        current: current.to_string(),
        // an empty release body is "no notes", not an empty section in the UI
        notes: release.body.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()).map(str::to_string),
    })
}

/// Download the launcher published by `release`, verify it, and swap it into place. Returns the
/// path to start — after the swap that path names the NEW binary.
///
/// Installing the release the user was SHOWN (rather than re-resolving "latest" here) is
/// deliberate: what the update button offers is what the update button installs.
pub fn apply(dl: &dyn Downloader, release: &Release, progress: ChunkProgress) -> Result<PathBuf> {
    apply_at(&exe_path()?, dl, release, progress)
}

/// `apply` with the target injected, so tests can drive the whole path — download, verify, swap,
/// rollback — against a scratch file instead of the running test binary.
fn apply_at(exe: &Path, dl: &dyn Downloader, release: &Release, progress: ChunkProgress) -> Result<PathBuf> {
    let dir = exe.parent().context("the launcher executable has no parent directory")?;
    ensure_dir_writable(dir)?;

    let asset = exe_asset(release)?;
    let expected = expected_sha(dl, release, &asset.name)?;

    let new = sibling(exe, ".new.exe");
    // Deliberately never resumed. A `.part` left by an earlier attempt at a DIFFERENT version
    // would be stitched onto the new bytes and produce a corrupt file of entirely plausible
    // length; the launcher is a few MB, so a clean re-fetch costs nothing worth that risk.
    let _ = std::fs::remove_file(&new);
    if let Err(e) = dl.download_to(asset, &new, 0, progress) {
        // a partial exe next to the launcher is AV bait and cleanup_old deliberately ignores
        // `.new` names, so it would sit there forever
        let _ = std::fs::remove_file(&new);
        return Err(e).with_context(|| format!("downloading {}", asset.name));
    }
    if let Err(e) = verify(&new, &expected) {
        let _ = std::fs::remove_file(&new); // never leave a rejected binary lying next to the exe
        return Err(e);
    }
    swap(exe, &new)?;
    Ok(exe.to_path_buf())
}

/// Reject anything we would not want to execute. The checksum is the real gate; the PE magic
/// catches the case a hash can't explain — a captive-portal or proxy error page delivered under a
/// perfectly healthy HTTP 200.
///
/// The hash is computed by RE-READING THE FILE, not taken from the download's in-memory digest.
/// The bytes that get renamed over the launcher are the bytes on disk, and those are what must be
/// proven: a second launcher instance updating concurrently writes the same `<stem>.new.exe`, and
/// two streams each hashing only their own output would both "verify" an interleaved file. This
/// matches the asset cache, which re-hashes from disk in `cache_ok` for the same reason.
fn verify(path: &Path, expected: &str) -> Result<()> {
    let size = std::fs::metadata(path)
        .with_context(|| format!("reading back {}", path.display()))?
        .len();
    if size == 0 {
        bail!("the downloaded launcher is empty");
    }
    let sha = crate::verify::sha256_file(path)
        .with_context(|| format!("hashing {}", path.display()))?;
    if !sha.eq_ignore_ascii_case(expected) {
        bail!("checksum mismatch: expected {expected}, got {sha}");
    }
    let mut magic = [0u8; 2];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .with_context(|| format!("reading back {}", path.display()))?;
    if &magic != b"MZ" {
        bail!("the downloaded file is not a Windows executable");
    }
    Ok(())
}

/// The published launcher binary: the canonical name first, else the release's single `.exe`
/// (the local file may have been renamed by the user, but the ASSET name is ours). Several
/// unnamed candidates is ambiguous — refuse rather than guess which one to run.
fn exe_asset(release: &Release) -> Result<&Asset> {
    if let Some(a) = release.asset(EXE_ASSET) {
        return Ok(a);
    }
    let mut exes = release.assets.iter().filter(|a| a.name.to_ascii_lowercase().ends_with(".exe"));
    let first = exes.next().context("the launcher release has no .exe asset")?;
    if exes.next().is_some() {
        bail!("the launcher release has several .exe assets and none named {EXE_ASSET}");
    }
    Ok(first)
}

/// The release's published sha256 for the CHOSEN exe asset. REQUIRED: a release without the
/// sidecar fails loudly (the user can still download it by hand) instead of swapping in an
/// unchecked binary. `<chosen>.sha256` first, so the renamed-exe fallback of `exe_asset` can
/// still verify; the canonical name stays as the fallback.
fn expected_sha(dl: &dyn Downloader, release: &Release, exe_name: &str) -> Result<String> {
    let sidecar = format!("{exe_name}.sha256");
    let asset = release.asset(&sidecar).or_else(|| release.asset(SHA_ASSET)).with_context(|| {
        format!(
            "release {} publishes no {sidecar} — refusing to install an unverified launcher",
            release.tag_name
        )
    })?;
    let bytes = dl.download(asset).context("downloading the launcher checksum")?;
    let text = String::from_utf8(bytes).context("the launcher checksum file is not valid UTF-8")?;
    // accepts a bare digest and `sha256sum` style ("<hex>  <name>") alike
    let hex = text.split_whitespace().next().unwrap_or_default().to_ascii_lowercase();
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("malformed checksum in {SHA_ASSET}");
    }
    Ok(hex)
}

/// Move the running exe aside and the verified one into its place. Both are metadata operations
/// in one directory, so the window in which no file sits at `exe` is as short as it can be — and
/// if the second rename fails, the first is undone, leaving a launcher that still starts.
fn swap(exe: &Path, new: &Path) -> Result<()> {
    let old = free_old_name(exe)?;
    std::fs::rename(exe, &old).with_context(|| format!("moving {} aside", exe.display()))?;
    if let Err(e) = std::fs::rename(new, exe) {
        // put the name back: an exe parked under `.old` would mean the next launch finds nothing
        let err = anyhow::Error::new(e)
            .context(format!("moving the new launcher into {}", exe.display()));
        if std::fs::rename(&old, exe).is_err() && !exe.exists() {
            // Both renames failed and nothing sits at the launcher's name any more. The user's
            // shortcut is dead, so the ONE thing worth saying is where their launcher actually
            // is — the generic message would send them looking for a file that moved.
            return Err(err.context(format!(
                "the launcher could not be put back — it is at {}, rename it to {} to restore it",
                old.display(),
                exe.file_name().unwrap_or_default().to_string_lossy()
            )));
        }
        return Err(err);
    }
    Ok(())
}

/// A `.old` name nothing holds. `fs::rename` replaces an existing target on Windows, but it can
/// never replace one a live process has open — a second copy of the previous build still running
/// would make a fixed `.old` name fail — so step aside to `.old1`, `.old2`, …
fn free_old_name(exe: &Path) -> Result<PathBuf> {
    for i in 0..10 {
        let suffix = if i == 0 { ".old.exe".to_string() } else { format!(".old{i}.exe") };
        let p = sibling(exe, &suffix);
        if !p.exists() || std::fs::remove_file(&p).is_ok() {
            return Ok(p);
        }
    }
    bail!("could not free a name to move the current launcher aside — is another copy running?")
}

/// Fail before spending a download on a folder we cannot write. A launcher in a protected
/// location (Program Files, unelevated) can never swap itself; saying so up front beats a
/// permission error several MB later. The io::Error rides the chain, so this surfaces as the
/// `io` wire kind — a permission problem, never "close the game".
fn ensure_dir_writable(dir: &Path) -> Result<()> {
    let probe = dir.join(".phoenix-update-probe");
    std::fs::write(&probe, b"")
        .with_context(|| format!("cannot write next to the launcher in {}", dir.display()))?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// Every `.old` leftover beside `exe`, from this or any earlier swap.
fn old_files(exe: &Path) -> Vec<PathBuf> {
    let (Some(dir), Some(stem)) = (exe.parent(), exe.file_stem().and_then(|s| s.to_str())) else {
        return Vec::new();
    };
    let prefix = format!("{stem}.old");
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    rd.filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|x| x.eq_ignore_ascii_case("exe"))
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&prefix))
        })
        .collect()
}

/// Clear leftovers from a previous swap. Detached and best-effort by design: right after an
/// update the outgoing process is often still exiting, and its image is locked until it finishes,
/// so early attempts failing is the expected path. Anything that survives every retry is inert
/// and gets collected by a later launch.
pub fn cleanup_old() {
    std::thread::spawn(|| {
        let Ok(exe) = exe_path() else { return };
        for _ in 0..CLEANUP_TRIES {
            if remove_leftovers(&exe) {
                return;
            }
            std::thread::sleep(CLEANUP_DELAY);
        }
    });
}

/// Try to delete every leftover; true when none remain. EVERY file gets an attempt each pass —
/// this was `.any(remove.is_err())`, which short-circuits, so one permanently locked `.old.exe`
/// (a second copy of the previous build still running) shielded every other leftover for the
/// whole retry window, and each pass burned itself on the same blocker first.
fn remove_leftovers(exe: &Path) -> bool {
    let mut all_gone = true;
    for p in old_files(exe) {
        if std::fs::remove_file(&p).is_err() {
            all_gone = false;
        }
    }
    all_gone
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, assets: &[&str], body: Option<&str>) -> Release {
        Release {
            tag_name: tag.to_string(),
            body: body.map(str::to_string),
            draft: false,
            prerelease: false,
            assets: assets
                .iter()
                .map(|n| Asset {
                    name: n.to_string(),
                    url: String::new(),
                    browser_download_url: String::new(),
                    size: 0,
                })
                .collect(),
        }
    }

    #[test]
    fn only_newer_tags_are_offered() {
        let cur = env!("CARGO_PKG_VERSION");
        assert!(available(&release("v999.0.0", &[], None)).is_some());
        assert!(available(&release("v0.0.1", &[], None)).is_none());
        // the running build itself is not an update — the common case, every launch
        assert!(available(&release(cur, &[], None)).is_none());
        assert!(available(&release(&format!("v{cur}"), &[], None)).is_none());
    }

    #[test]
    fn version_strips_the_tag_prefix() {
        let a = available(&release("v999.1.2", &[], None)).unwrap();
        assert_eq!(a.version, "999.1.2");
        assert_eq!(a.tag, "v999.1.2");
        assert_eq!(a.current, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn blank_release_body_is_not_notes() {
        assert!(available(&release("v999.0.0", &[], Some("  \n "))).unwrap().notes.is_none());
        assert_eq!(
            available(&release("v999.0.0", &[], Some(" fixed stuff "))).unwrap().notes.as_deref(),
            Some("fixed stuff")
        );
    }

    #[test]
    fn exe_asset_prefers_the_canonical_name() {
        let r = release("v9", &["other.exe", EXE_ASSET], None);
        assert_eq!(exe_asset(&r).unwrap().name, EXE_ASSET);
        // a lone differently-named exe is still unambiguous
        let r = release("v9", &["renamed.exe", "notes.txt"], None);
        assert_eq!(exe_asset(&r).unwrap().name, "renamed.exe");
        // two candidates and no canonical name: refuse rather than pick one to execute
        assert!(exe_asset(&release("v9", &["a.exe", "b.exe"], None)).is_err());
        assert!(exe_asset(&release("v9", &["notes.txt"], None)).is_err());
    }

    #[test]
    fn verify_rejects_bad_downloads() {
        use sha2::Digest;
        let digest = |b: &[u8]| hex::encode(sha2::Sha256::digest(b));
        let dir = std::env::temp_dir().join("phoenix-selfupdate-verify");
        let _ = std::fs::create_dir_all(&dir);

        // every digest below is the digest of what is ON DISK — verify re-reads the file rather
        // than trusting the download's in-memory hash, so these are the bytes that get executed
        let good = dir.join("good.exe");
        let payload: &[u8] = b"MZ\x90\x00payload";
        std::fs::write(&good, payload).unwrap();
        assert!(verify(&good, &"b".repeat(64)).is_err(), "checksum mismatch");
        assert!(verify(&good, &digest(payload)).is_ok());
        // hex case must not decide whether a launcher installs
        assert!(verify(&good, &digest(payload).to_uppercase()).is_ok());

        let empty = dir.join("empty.exe");
        std::fs::write(&empty, b"").unwrap();
        assert!(verify(&empty, &digest(b"")).is_err(), "empty download");

        // the checksum MATCHES here — a mirror or captive portal serving its own error page under
        // a 200 with a correct sidecar is exactly what the PE magic is the last line against
        let html = dir.join("portal.html.exe");
        let body: &[u8] = b"<!DOCTYPE html><html>nope";
        std::fs::write(&html, body).unwrap();
        assert!(verify(&html, &digest(body)).is_err(), "not a PE image");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The download's own digest must not be what decides: bytes that changed on disk after the
    /// stream finished (a second launcher instance writing the same `.new.exe`, an AV restore)
    /// have to be caught.
    #[test]
    fn verify_hashes_the_file_not_the_stream() {
        use sha2::Digest;
        let dir = std::env::temp_dir().join("phoenix-selfupdate-ondisk");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("swapped.exe");
        let downloaded: &[u8] = b"MZ\x90\x00the bytes we fetched";
        let expected = hex::encode(sha2::Sha256::digest(downloaded));
        // something else rewrote the file after the download reported success
        std::fs::write(&p, b"MZ\x90\x00tampered").unwrap();
        assert!(verify(&p, &expected).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_locked_leftover_does_not_shield_the_others() {
        let dir = std::env::temp_dir().join("phoenix-selfupdate-locked-old");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("phoenix-launcher.exe");
        std::fs::write(dir.join("phoenix-launcher.old.exe"), b"locked").unwrap();
        std::fs::write(dir.join("phoenix-launcher.old1.exe"), b"free").unwrap();

        // hold one open with no sharing — deleting it fails, like a still-running old build
        use std::os::windows::fs::OpenOptionsExt;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(dir.join("phoenix-launcher.old.exe"))
            .unwrap();
        assert!(!remove_leftovers(&exe), "the locked file still remains");
        assert!(
            !dir.join("phoenix-launcher.old1.exe").exists(),
            "the unlocked leftover must be collected even when another one is stuck"
        );
        drop(lock);
        assert!(remove_leftovers(&exe));
        assert!(!dir.join("phoenix-launcher.old.exe").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_files_matches_only_our_leftovers() {
        let dir = std::env::temp_dir().join("phoenix-selfupdate-old");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("phoenix-launcher.exe");
        for n in [
            "phoenix-launcher.exe",
            "phoenix-launcher.old.exe",
            "phoenix-launcher.old3.exe",
            "phoenix-launcher.new.exe", // a pending download is not a leftover
            "phoenix-launcher.old.txt", // only executables get collected
            "other.old.exe",            // a different launcher's leftovers stay untouched
        ] {
            std::fs::write(dir.join(n), b"x").unwrap();
        }
        let mut found: Vec<String> =
            old_files(&exe).iter().map(|p| p.file_name().unwrap().to_string_lossy().into()).collect();
        found.sort();
        assert_eq!(found, ["phoenix-launcher.old.exe", "phoenix-launcher.old3.exe"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A scratch dir holding a stand-in "installed launcher", plus a fake release serving `body`
    /// as the new exe with a matching (or deliberately wrong) checksum sidecar.
    fn stage(name: &str, body: &[u8], sha: Option<&str>) -> (PathBuf, crate::downloader::fake::Fake) {
        use sha2::Digest;
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("phoenix-launcher.exe"), b"OLD LAUNCHER").unwrap();
        let digest = sha.map(str::to_string).unwrap_or_else(|| hex::encode(sha2::Sha256::digest(body)));
        // sha256sum layout, exactly what release.yml writes
        let sidecar = format!("{digest}  {EXE_ASSET}\n");
        let fake = crate::downloader::fake::Fake::new(
            "v999.0.0",
            "{}",
            vec![(EXE_ASSET, body), (SHA_ASSET, sidecar.as_bytes())],
        );
        (dir, fake)
    }

    #[test]
    fn apply_downloads_verifies_and_swaps() {
        let (dir, fake) = stage("phoenix-selfupdate-apply", b"MZ\x90\x00NEW LAUNCHER", None);
        let exe = dir.join("phoenix-launcher.exe");
        let mut ticks = 0;
        let out = apply_at(&exe, &fake, &fake.fetch_release("r", None).unwrap(), &mut |_, _| {
            ticks += 1;
            true
        })
        .unwrap();

        assert_eq!(out, exe, "the returned path is the one to restart");
        assert_eq!(std::fs::read(&exe).unwrap(), b"MZ\x90\x00NEW LAUNCHER");
        assert_eq!(std::fs::read(sibling(&exe, ".old.exe")).unwrap(), b"OLD LAUNCHER");
        assert!(!sibling(&exe, ".new.exe").exists(), "staging is consumed");
        assert!(ticks > 0, "download progress reached the caller");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_download_never_reaches_the_exe() {
        // the sidecar disagrees with the bytes: a truncated/tampered download must be dropped
        // BEFORE the swap, or the user is left with a launcher that cannot start
        let (dir, fake) = stage("phoenix-selfupdate-corrupt", b"MZ\x90\x00NEW", Some(&"a".repeat(64)));
        let exe = dir.join("phoenix-launcher.exe");
        let err = apply_at(&exe, &fake, &fake.fetch_release("r", None).unwrap(), &mut |_, _| true).unwrap_err();

        assert!(format!("{err:#}").contains("checksum mismatch"), "got: {err:#}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"OLD LAUNCHER", "the working launcher is untouched");
        assert!(!sibling(&exe, ".new.exe").exists(), "the rejected binary is not left behind");
        assert!(!sibling(&exe, ".old.exe").exists(), "nothing was moved aside");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_release_without_a_checksum_is_refused() {
        let dir = std::env::temp_dir().join("phoenix-selfupdate-nosha");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("phoenix-launcher.exe");
        std::fs::write(&exe, b"OLD LAUNCHER").unwrap();
        let fake = crate::downloader::fake::Fake::new("v999.0.0", "{}", vec![(EXE_ASSET, b"MZ\x90\x00")]);

        let err = apply_at(&exe, &fake, &fake.fetch_release("r", None).unwrap(), &mut |_, _| true).unwrap_err();
        assert!(format!("{err:#}").contains(SHA_ASSET), "got: {err:#}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"OLD LAUNCHER");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stale_partial_download_is_discarded_not_resumed() {
        // a `.new.exe` left by an attempt at a DIFFERENT version must not be treated as a prefix
        // to append to — that stitches a corrupt file of entirely plausible length
        let (dir, fake) = stage("phoenix-selfupdate-stale", b"MZ\x90\x00NEW LAUNCHER", None);
        let exe = dir.join("phoenix-launcher.exe");
        std::fs::write(sibling(&exe, ".new.exe"), b"JUNK FROM AN OLDER ATTEMPT").unwrap();

        apply_at(&exe, &fake, &fake.fetch_release("r", None).unwrap(), &mut |_, _| true).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"MZ\x90\x00NEW LAUNCHER");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn swap_puts_the_new_binary_at_the_original_path() {
        let dir = std::env::temp_dir().join("phoenix-selfupdate-swap");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("phoenix-launcher.exe");
        let new = sibling(&exe, ".new.exe");
        std::fs::write(&exe, b"OLD").unwrap();
        std::fs::write(&new, b"NEW").unwrap();

        swap(&exe, &new).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"NEW");
        assert!(!new.exists(), "the staged file is consumed by the swap");
        assert_eq!(std::fs::read(sibling(&exe, ".old.exe")).unwrap(), b"OLD");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
