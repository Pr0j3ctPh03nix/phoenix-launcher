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
//! Nothing is swapped before the download is verified against a hash the release committed to.
//! Writing a truncated or corrupt exe over a working launcher leaves the user with neither a
//! launcher nor a way back, so this mirrors install.rs: no unverified byte is ever committed.
//!
//! WHERE that hash comes from has two answers, and the order matters — see `resolve`. So does
//! WHERE the exe comes from: GitHub serves it under the asset NAME the release lists, a mirror
//! under the HASH the signed manifest names, and `resolve` is the one place self-update learns
//! which it is talking to without ever knowing which it is talking to.

use anyhow::{bail, Context, Result};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::Settings;
use crate::downloader::{Asset, ChunkProgress, Downloader, Release};
use crate::engine::{self, version_lt};
use crate::manifest::{FileEntry, Manifest};
use crate::source;
use crate::trust::Payload;

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
    /// The release's "What's new" text, out of the SIGNED manifest — the same document the version
    /// above is read from. `None` for a release whose manifest carries none, and for the legacy
    /// shape that carries no manifest at all.
    pub notes: Option<String>,
    /// The manifest this offer was judged by, VERIFIED, carried so the download that follows does
    /// not fetch and verify it a second time. `None` is the legacy shape — a release publishing no
    /// manifest at all — and it is what selects `resolve`'s sidecar branch, so the choice between
    /// the two paths is made once, here, where the question is first asked.
    pub manifest: Option<Manifest>,
}

fn exe_path() -> Result<PathBuf> {
    std::env::current_exe().context("locating the launcher executable")
}

/// Is this failure "we cannot believe that release" rather than "we could not get it"?
///
/// The one predicate that turns an exhausted walk back into `Ok(None)` — no update — so a launcher
/// whose sources all serve something unverifiable reports what it always did: nothing to install.
/// `UnsupportedSchema` is deliberately NOT in it: a release this build is too old to read IS an
/// update, and telling the user to update by hand is the only useful thing to say.
pub fn is_untrustworthy(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<crate::trust::TrustError>().is_some()
            || c.downcast_ref::<crate::minisig::SigError>().is_some()
    })
}

/// `<stem><suffix>` next to `exe` — e.g. ".new.exe" -> `phoenix-launcher.new.exe`. Built from the
/// stem of the RUNNING file, so a launcher the user renamed still parks its siblings beside it.
fn sibling(exe: &Path, suffix: &str) -> PathBuf {
    let stem = exe.file_stem().unwrap_or(exe.as_os_str());
    let mut name = stem.to_os_string();
    name.push(suffix);
    exe.with_file_name(name)
}

/// Is `release` newer than this build? `Ok(None)` means there is nothing to offer — we are current,
/// or ahead (normal for a local dev build).
///
/// THE VERSION COMPARED IS THE SIGNED ONE. A tag is a label the source chooses, so comparing
/// against it lets whoever serves the release pick the answer: publish a genuine OLD release under
/// a NEW tag and the comparison says "newer", the signature verifies (it is a real release), and
/// the user is silently downgraded. `manifest.version` is inside the signed document, so an
/// attacker cannot raise it without breaking the signature, and a replayed old release carries its
/// own old version and is correctly refused. The tag is still what we FETCH by; it is never what
/// we decide by.
///
/// A release with no manifest falls back to the tag — that is the pre-signing shape, and the same
/// deliberate gap `expected_sha` documents. It stops firing once every release carries one.
///
/// Untrustworthy is an ERROR, and `is_untrustworthy` is how the caller turns it back into "no
/// update". It used to be `Ok(None)` right here, which is the right thing to SHOW and the wrong
/// thing to decide: a source serving a release this launcher refuses would have ended the check as
/// "you are current" without the next source ever being asked. The walk needs a failure to fail
/// over on, and the caller — which is the thing that knows the whole ranking has answered the same
/// way — is where "we do not have it" becomes the honest silence it always was. Net UX unchanged,
/// failover gained. An UNREADABLE release (schema too new) stays an error the user must act on.
pub fn available(
    settings: &Settings,
    dl: &dyn Downloader,
    release: &Release,
) -> Result<Option<Available>> {
    let current = env!("CARGO_PKG_VERSION");
    let signed = signed_manifest(settings, dl, release)?;
    let version = match &signed {
        Some(m) => m.version.clone(),
        None => release.tag_name.trim_start_matches('v').to_string(),
    };
    if !version_lt(current, &version) {
        return Ok(None);
    }
    Ok(Some(Available {
        tag: release.tag_name.clone(),
        version,
        current: current.to_string(),
        // OUT OF THE SIGNED DOCUMENT, like the version beside it. This is prose the launcher
        // renders as its own changelog, in a banner that says a new launcher is available, with
        // links it will open in the user's browser — so it comes from the one place a source
        // cannot rewrite. An empty one is "no notes", not an empty section in the UI, and a
        // release without a changelog is an ordinary release: nothing here withholds an update
        // over its notes.
        notes: signed
            .as_ref()
            .and_then(|m| m.notes.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        manifest: signed,
    }))
}

/// The release's SIGNED manifest, or `None` when it publishes none.
///
/// THE one place the two shapes of a launcher release are told apart, so everything downstream
/// follows this single answer rather than asking the question again: which version is being
/// offered, what its notes say, and which entry names the exe to run. `resolve`'s legacy branch is
/// exactly the `None` here — read its doc for why that branch exists and what closes it.
///
/// A refusal — bad signature, unknown key, wrong payload, stale serial — is a release we do not
/// have, and a fact about the SOURCE serving it, so it propagates and the walk moves on.
fn signed_manifest(
    settings: &Settings,
    dl: &dyn Downloader,
    release: &Release,
) -> Result<Option<Manifest>> {
    match release.asset(engine::MANIFEST_ASSET) {
        Some(_) => Ok(Some(engine::manifest_of(settings, dl, release, Payload::Launcher)?)),
        None => Ok(None),
    }
}

/// A verified launcher binary, staged beside the running one and ready to be swapped in.
///
/// The two halves of an update are separate types of operation and only ONE of them may be retried
/// against another source. Downloading is transport: a host that serves a bad exe is a bad host,
/// and the next one deserves a turn. Renaming the running launcher is not — it is a local,
/// irreversible-ish pair of moves, and running it twice because a network failed is how a user ends
/// up with no launcher at all.
pub struct Staged {
    exe: PathBuf,
    new: PathBuf,
}

/// Download the launcher published by `release` and verify it — everything up to, and not
/// including, touching the running binary. Fail this and the next source is asked, from zero.
///
/// Installing the release the user was SHOWN (rather than re-resolving "latest") is deliberate:
/// what the update button offers is what the update button installs.
pub fn fetch_verified(
    dl: &dyn Downloader,
    release: &Release,
    manifest: Option<&Manifest>,
    progress: ChunkProgress,
) -> Result<Staged> {
    fetch_verified_at(&exe_path()?, dl, release, manifest, progress)
}

/// Move the verified binary into place and return the path to start — after the swap that path
/// names the NEW file.
pub fn swap_in(staged: &Staged) -> Result<PathBuf> {
    swap(&staged.exe, &staged.new)?;
    Ok(staged.exe.clone())
}

/// `fetch_verified` with the target injected, so tests can drive the whole path — download, verify,
/// swap, rollback — against a scratch file instead of the running test binary.
fn fetch_verified_at(
    exe: &Path,
    dl: &dyn Downloader,
    release: &Release,
    manifest: Option<&Manifest>,
    progress: ChunkProgress,
) -> Result<Staged> {
    let dir = exe.parent().context("the launcher executable has no parent directory")?;
    ensure_dir_writable(dir)?;

    let Target { asset, sha256: expected, size: expected_size } = resolve(dl, release, manifest)?;

    let new = sibling(exe, ".new.exe");
    // Deliberately never resumed. A `.part` left by an earlier attempt at a DIFFERENT version
    // would be stitched onto the new bytes and produce a corrupt file of entirely plausible
    // length; the launcher is a few MB, so a clean re-fetch costs nothing worth that risk.
    let _ = std::fs::remove_file(&new);
    // The signed manifest's `size` is a trust input exactly like its hash: in the target state
    // where mirrors serve this payload too, a third-party host must be cut off DURING the
    // transfer if it sends past it, not merely caught once an unbounded exe has already filled
    // the disk — hash-verification-before-swap runs on bytes that already landed, so it cannot
    // help here. Same idea as `install::obtain_to_cache`'s asset cap, reused rather than shared:
    // this path never retries or resumes, so it has no backoff loop to thread the guard through.
    // Only wired when `expected_size` came from the SIGNED manifest — the legacy `.sha256`
    // sidecar path has no trustworthy size at all (see `resolve`) and stays uncapped.
    let mut capped = false;
    let result = match expected_size {
        Some(size) => {
            let mut guarded = |written: u64, total: Option<u64>| -> bool {
                if written > size {
                    capped = true;
                    return false;
                }
                progress(written, total)
            };
            dl.download_to(&asset, &new, 0, &mut guarded)
        }
        None => dl.download_to(&asset, &new, 0, progress),
    };
    if let Err(e) = result {
        // a partial exe next to the launcher is AV bait and cleanup_old deliberately ignores
        // `.new` names, so it would sit there forever
        let _ = std::fs::remove_file(&new);
        // Distinct from an ordinary abort even though it rides the same callback — say so rather
        // than let it read as a cancel, and never retry it (there is no retry loop here to do
        // that by accident, unlike `obtain_to_cache`, but the explicit message keeps the two
        // call sites' refusals reading as one idea).
        if capped {
            let size = expected_size.expect("capped can only be set inside the Some(size) arm above");
            bail!(
                "{}: the source sent more than the signed {size} bytes — refusing (the host is \
                 misbehaving or hostile)",
                asset.name
            );
        }
        return Err(e).with_context(|| format!("downloading {}", asset.name));
    }
    if let Err(e) = verify(&new, &expected) {
        let _ = std::fs::remove_file(&new); // never leave a rejected binary lying next to the exe
        return Err(e);
    }
    Ok(Staged { exe: exe.to_path_buf(), new })
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

/// What `apply_at` fetches and what it must prove: the exe as THIS source addresses it, the hash
/// the release commits to for it, and — only when the SIGNED manifest is where both came from —
/// the size to cap the transfer at.
struct Target {
    asset: Asset,
    sha256: String,
    size: Option<u64>,
}

/// The launcher to install from `release`, resolved for whatever backend `dl` is. The hash is never
/// optional: a release that commits to nothing fails loudly (the user can still download it by
/// hand) instead of swapping in an unchecked binary. The size IS optional, and that is deliberate
/// — see below.
///
/// `manifest` is the one `available` already verified, HANDED here rather than fetched again: an
/// update used to download and Ed25519-check the same document twice on the one path that is also
/// downloading a multi-megabyte exe (three times counting the check that offered it). `None` is
/// the legacy branch, and `available` is where that is decided — see `signed_manifest`.
///
/// TWO sources of the hash, and the SIGNED one wins. A `.minisig` over the launcher's manifest is
/// a statement by a key we pinned; the `.sha256` sidecar is a statement by whoever served the
/// release, which is exactly the party a mirror lets somebody else be. The sidecar is nonetheless
/// still read, and release.yml must keep publishing it: launchers already installed check it and
/// nothing else, and self-update is the one path that has to keep working for builds that predate
/// every change made to it.
///
/// On the SIGNED branch the manifest also decides WHICH entry is the launcher and the source only
/// decides how to address it — through `source::asset_for`, the same rule every payload download
/// goes through. GitHub is name-addressed: the entry's `name` is looked up in the
/// release's asset list. A mirror is content-addressed: it has no asset list at all, and the
/// entry's `sha256` IS its address (`Mirror::url_of` sends a hash to `blobs/<sha256>`). Reading
/// the exe out of the release index, as this used to, worked on GitHub and could never work on a
/// mirror — the synthetic release a mirror opens carries only the two trust documents, so the exe
/// "was not published" there however faithfully it was mirrored. A second copy of the two-shape
/// rule here would be free to drift from install.rs's; reusing it is what keeps a launcher that
/// installs from one source from 404ing on the other.
///
/// The LEGACY branch (no manifest) stays name-addressed through `exe_asset`, and that is not a gap
/// left open for mirrors: a mirror's `fetch_release` always synthesizes `manifest.json` into the
/// release it reports, so on a mirror this branch is unreachable by construction. It exists for
/// GitHub releases cut before signing did.
///
/// Everything else on the signed branch — a signature that is missing, bad, from an unknown key,
/// or over a document that names a different payload — is an ERROR, not a fallback. Silently
/// dropping to the sidecar on a failed check would make the signature decorative: a downgrade
/// would cost an attacker one deleted file.
///
/// KNOWN AND DELIBERATE GAP: a release publishing no manifest at all falls back to the sidecar, so
/// an attacker who can choose what we see can strip the signed pair and be believed on the weaker
/// evidence. Closing it means refusing every release cut before signing existed — i.e. refusing to
/// self-update the builds that most need to.
///
/// It does NOT close by itself. The branch is chosen by the manifest's ABSENCE, before any version
/// or serial is read, so nothing downstream of that choice can make the unsigned path unreachable.
/// Closing it takes a deliberate switch, once every release in the wild carries a manifest. A
/// release that publishes a manifest WITHOUT a signature is a different matter and is refused
/// outright — that shape is only ever tampering.
///
/// The SAME gap is why the returned size is `Option`, not a fallback ceiling: the sidecar is one
/// line of hex with no size anywhere in it, so a value invented for that branch would not be a
/// trust input, it would be a made-up number dressed as one. `apply_at` leaves that branch's
/// download uncapped rather than pretend otherwise — the known gap stays exactly the shape it
/// already is, not silently narrowed by a number nobody signed.
///
/// The entry's `dest` is deliberately not consulted. A launcher payload installs nothing into a
/// game folder; the producer still emits a legal `dest` because the format demands one. `size`,
/// unlike `dest`, IS read — it is the same trust input as `sha256`, just enforced during the
/// transfer instead of after it (see `apply_at`).
fn resolve(dl: &dyn Downloader, release: &Release, manifest: Option<&Manifest>) -> Result<Target> {
    let Some(manifest) = manifest else {
        let asset = exe_asset(release)?.clone();
        let sha256 = legacy_sha(dl, release, &asset.name)?;
        return Ok(Target { asset, sha256, size: None });
    };
    let entry = exe_entry(manifest, &release.tag_name)?;
    let name = entry.name.as_deref().expect("exe_entry only ever returns a named entry");
    let asset = source::asset_for(dl, release, name, &entry.sha256, entry.size)
        .with_context(|| format!("release {} publishes no asset named {name}", release.tag_name))?;
    Ok(Target { asset, sha256: entry.sha256.clone(), size: Some(entry.size) })
}

/// The launcher entry of a SIGNED manifest: the canonical name first, else the document's single
/// `.exe`-named entry. Several unnamed candidates is ambiguous — refuse rather than guess which one
/// to run; none is a broken release, and guessing which hash it meant is the one thing we must
/// not do. The same rule `exe_asset` applies to a release index, applied to the signed document
/// instead — which is the one that exists on every source (a mirror publishes no index).
fn exe_entry<'m>(manifest: &'m Manifest, tag: &str) -> Result<&'m FileEntry> {
    let named = |f: &&FileEntry| f.name.is_some();
    if let Some(e) = manifest.files.iter().filter(named).find(|f| f.name.as_deref() == Some(EXE_ASSET)) {
        return Ok(e);
    }
    let mut exes = manifest.files.iter().filter(named).filter(|f| {
        f.name.as_deref().is_some_and(|n| n.to_ascii_lowercase().ends_with(".exe"))
    });
    let first = exes
        .next()
        .with_context(|| format!("the signed manifest of {tag} does not name a launcher executable"))?;
    if exes.next().is_some() {
        bail!("the signed manifest of {tag} names several .exe entries and none is {EXE_ASSET}");
    }
    Ok(first)
}

/// The published launcher binary of a release index — the LEGACY branch of `resolve`, for a
/// release with no signed manifest to ask instead. The canonical name first, else the release's
/// single `.exe` (the local file may have been renamed by the user, but the ASSET name is ours).
/// Several unnamed candidates is ambiguous — refuse rather than guess which one to run.
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

/// The release's published sha256 sidecar. `<chosen>.sha256` first, so the renamed-exe fallback of
/// `exe_asset` can still verify; the canonical name stays as the fallback.
fn legacy_sha(dl: &dyn Downloader, release: &Release, exe_name: &str) -> Result<String> {
    let sidecar = format!("{exe_name}.sha256");
    let asset = release.asset(&sidecar).or_else(|| release.asset(SHA_ASSET)).with_context(|| {
        format!(
            "release {} publishes no {sidecar} — refusing to install an unverified launcher",
            release.tag_name
        )
    })?;
    // bounded for the same reason a signature file is: this decides which bytes get executed,
    // and one line of hex is all it is ever allowed to be
    let bytes = dl
        .download_limited(asset, crate::trust::MAX_SIG_BYTES)
        .context("downloading the launcher checksum")?;
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

    /// These releases publish no manifest, so `available` falls back to the tag — the pre-signing
    /// shape. The downloader is therefore never reached; it only has to exist.
    fn offered(tag: &str, body: Option<&str>) -> Option<Available> {
        let dl = crate::downloader::fake::Fake::new(tag, r#"{"schema":2,"files":[]}"#, vec![]);
        available(&Settings::default(), &dl, &release(tag, &[], body)).unwrap()
    }

    #[test]
    fn only_newer_tags_are_offered() {
        let cur = env!("CARGO_PKG_VERSION");
        assert!(offered("v999.0.0", None).is_some());
        assert!(offered("v0.0.1", None).is_none());
        // the running build itself is not an update — the common case, every launch
        assert!(offered(cur, None).is_none());
        assert!(offered(&format!("v{cur}"), None).is_none());
    }

    /// A tag is a label whoever serves the release chooses; the signed version is not.
    ///
    /// The attack this closes: publish a GENUINE OLD launcher release under a NEW tag. Everything
    /// else passes — the signature is real, the key is pinned, the payload id matches — and a
    /// reader that compared against the tag would call it an upgrade and silently downgrade the
    /// user. Comparing `manifest.version` instead makes the old release state its own age.
    #[test]
    fn a_new_tag_over_an_old_signed_release_is_not_an_upgrade() {
        // A REAL manifest, not a sketch: `available` now reports a document it cannot believe as
        // an error (so a walk can fail the source over), which means a fixture the parser refuses
        // would make this pass for the wrong reason entirely.
        let old = format!(
            r#"{{"schema":2,"payload_id":"launcher","version":"0.0.1",
                 "files":[{{"dest":"x","name":"x","sha256":"{}","size":1}}]}}"#,
            "aa".repeat(32)
        );
        let dl = crate::downloader::fake::Fake::new("v999.0.0", &old, vec![]);
        let rel = dl.fetch_release("r", None).unwrap();
        assert!(
            rel.asset(engine::MANIFEST_ASSET).is_some(),
            "the fake must publish a manifest or this proves nothing"
        );
        assert!(
            available(&Settings::default(), &dl, &rel).unwrap().is_none(),
            "the tag says 999.0.0; the SIGNED document says 0.0.1, and that is the one that counts"
        );
    }

    #[test]
    fn version_strips_the_tag_prefix() {
        let a = offered("v999.1.2", None).unwrap();
        assert_eq!(a.version, "999.1.2");
        assert_eq!(a.tag, "v999.1.2");
        assert_eq!(a.current, env!("CARGO_PKG_VERSION"));
    }

    /// THE NOTES COME FROM THE SIGNED DOCUMENT, and from nowhere else.
    ///
    /// They are rendered as the launcher's own changelog, inside a banner whose chrome says a new
    /// launcher is available, with links the launcher itself opens in the user's browser. The
    /// release BODY is a string whoever serves the release writes, and on a mirror that is a third
    /// party who registered by pull request — so it is not what the banner shows, however good it
    /// looks. A blank one is "no notes" rather than an empty section, as before.
    #[test]
    fn the_offered_notes_come_from_the_signed_manifest() {
        let offered_with = |notes: serde_json::Value, body: Option<&str>| {
            let doc = serde_json::json!({
                "schema": 2, "payload_id": "launcher", "serial": 3, "version": "999.0.0",
                "notes": notes, "files": []
            })
            .to_string();
            let dl = crate::downloader::fake::Fake::new("v999.0.0", &doc, vec![]);
            let mut rel = dl.fetch_release("r", None).unwrap();
            rel.body = body.map(str::to_string);
            available(&Settings::default(), &dl, &rel).unwrap().expect("999.0.0 is an upgrade")
        };

        assert_eq!(
            offered_with(serde_json::json!("### Fixed\n- a thing"), None).notes.as_deref(),
            Some("### Fixed\n- a thing")
        );
        assert!(offered_with(serde_json::json!("  \n "), None).notes.is_none(), "blank is none");
        // …and the body is not a fallback: a signed document that carries no notes has none, even
        // when the source has plenty to say for itself.
        assert!(
            offered_with(serde_json::Value::Null, Some("### Click here for free hats")).notes.is_none(),
            "the release body is not where the banner's text comes from"
        );
        // the legacy shape carries no manifest at all, so it carries no notes either
        assert!(offered("v999.0.0", Some(" fixed stuff ")).unwrap().notes.is_none());
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
    ///
    /// LEGACY-SHAPED on purpose (`no_manifest`): this is every launcher release cut before signing
    /// existed, and the sidecar path these tests cover has to keep working for exactly those. The
    /// signed shape is `stage_signed` below.
    fn stage(name: &str, body: &[u8], sha: Option<&str>) -> (PathBuf, crate::downloader::fake::Fake) {
        let (dir, fake) = stage_raw(name, body, sha);
        (dir, fake.no_manifest())
    }

    fn stage_raw(name: &str, body: &[u8], sha: Option<&str>) -> (PathBuf, crate::downloader::fake::Fake) {
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

    /// A signed launcher release: the manifest names the exe asset and its hash, and the sidecar
    /// says something ELSE. Only a reader that actually prefers the signed manifest installs it.
    fn stage_signed(
        name: &str,
        body: &[u8],
        manifest_sha: &str,
        payload: &str,
    ) -> (PathBuf, crate::downloader::fake::Fake) {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("phoenix-launcher.exe"), b"OLD LAUNCHER").unwrap();
        // a sidecar that disagrees with the manifest: whichever hash the exe is checked against
        // decides whether this release installs, so the test can see which one was used
        let sidecar = format!("{}  {EXE_ASSET}\n", "a".repeat(64));
        let manifest = serde_json::json!({
            "schema": 2,
            "payload_id": payload,
            "serial": 3,
            "version": "999.0.0",
            "files": [{
                "name": EXE_ASSET,
                // meaningless for this payload and never read — the format simply requires one
                "dest": "phoenix-launcher.exe",
                "sha256": manifest_sha,
                "size": body.len(),
            }]
        })
        .to_string();
        let fake = crate::downloader::fake::Fake::new(
            "v999.0.0",
            &manifest,
            vec![(EXE_ASSET, body), (SHA_ASSET, sidecar.as_bytes())],
        );
        (dir, fake)
    }

    fn settings() -> Settings {
        Settings::default()
    }

    /// REQUIREMENT 3, for self-update: a source that serves a launcher whose bytes do not match the
    /// hash its release committed to is a BAD SOURCE, not the end of the update.
    ///
    /// The walk wraps the DOWNLOAD only. `fetch_verified` never resumes — a `.part` from a
    /// different version stitched onto new bytes is a corrupt file of plausible length — so the
    /// next source restarts from zero, and the rejected `.new.exe` is deleted before it does.
    /// The SWAP is outside the walk on purpose: renaming the running launcher is not an operation
    /// to retry against another host.
    #[test]
    fn a_source_that_serves_a_bad_launcher_hash_fails_over() {
        use crate::config::Source;
        use crate::downloader::Downloader;
        use std::sync::Arc;

        let good = b"MZ the launcher that was signed for";
        let sha = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(good));
        // Two releases naming the SAME hash: the first serves other bytes under it, the second
        // serves the bytes it promised.
        let (dir, honest) = stage_signed("phoenix-selfupdate-failover", good, &sha, "launcher");
        let (_, liar) = stage_signed("phoenix-selfupdate-failover-liar", b"MZ not this", &sha, "launcher");
        let exe = dir.join("phoenix-launcher.exe");

        let peers: Vec<Arc<dyn Downloader>> = vec![Arc::new(liar), Arc::new(honest)];
        let sources = vec![Source::default(), Source::at("https://b.example")];
        let by_key: std::collections::HashMap<Option<String>, Arc<dyn Downloader>> =
            sources.iter().map(|s| s.url.clone()).zip(peers).collect();
        let dial: crate::source::Dial = Box::new(move |s: &Source| by_key[&s.url].clone());

        let staged = crate::source::walk(&dial, &sources, "r", None, |dl, release| {
            let manifest = signed_manifest(&settings(), dl, release)?;
            fetch_verified_at(&exe, dl, release, manifest.as_ref(), &mut |_, _| true)
        })
        .expect("the second source serves what the first promised");
        let out = swap_in(&staged).expect("and the swap runs once, outside the walk");

        assert_eq!(out, exe);
        assert_eq!(std::fs::read(&exe).unwrap(), good);
        assert!(
            !dir.join("phoenix-launcher.new.exe").exists(),
            "nothing partial may be left beside the launcher — the rejected download is deleted \
             before the next source starts, and the accepted one is renamed away"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("phoenix-selfupdate-failover-liar"));
    }

    /// ONE fetch and one signature check of the launcher manifest per update.
    ///
    /// `available` verifies it to decide whether the release is newer at all — the signed version
    /// is what that judgement rests on — and the download then needs the same document to learn
    /// which entry names the exe. Those were two independent fetch-and-verify rounds of identical
    /// bytes, on the one path that is already pulling a multi-megabyte binary. The offer carries
    /// the document it was judged by, and this is the production sequence exactly.
    #[test]
    fn the_launcher_manifest_is_verified_once_per_update() {
        use std::sync::atomic::{AtomicU32, Ordering};
        struct Counting {
            inner: crate::downloader::fake::Fake,
            reads: AtomicU32,
        }
        impl Downloader for Counting {
            fn fetch_release(&self, r: &str, t: Option<&str>) -> Result<Release> {
                self.inner.fetch_release(r, t)
            }
            fn fetch_releases(&self, r: &str) -> Result<Vec<Release>> {
                self.inner.fetch_releases(r)
            }
            fn download(&self, a: &Asset) -> Result<Vec<u8>> {
                if a.name == engine::MANIFEST_ASSET {
                    self.reads.fetch_add(1, Ordering::SeqCst);
                }
                self.inner.download(a)
            }
            fn download_to(
                &self,
                a: &Asset,
                d: &Path,
                r: u64,
                p: ChunkProgress,
            ) -> Result<(u64, String)> {
                self.inner.download_to(a, d, r, p)
            }
        }

        let body: &[u8] = b"MZ\x90\x00ONE VERIFICATION";
        let sha = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(body));
        let (dir, fake) = stage_signed("phoenix-selfupdate-once", body, &sha, "launcher");
        let dl = Counting { inner: fake, reads: AtomicU32::new(0) };
        let exe = dir.join("phoenix-launcher.exe");

        let release = dl.fetch_release("r", None).unwrap();
        let offer = available(&settings(), &dl, &release).unwrap().expect("999.0.0 is an upgrade");
        let staged =
            fetch_verified_at(&exe, &dl, &release, offer.manifest.as_ref(), &mut |_, _| true)
                .expect("the offer's own manifest is what resolves the exe");
        swap_in(&staged).unwrap();

        assert_eq!(std::fs::read(&exe).unwrap(), body);
        assert_eq!(
            dl.reads.load(Ordering::SeqCst),
            1,
            "the manifest crosses the wire once for the whole update"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The two halves back to back, against a scratch exe: download + verify, then swap.
    ///
    /// They are separate in production because only the FIRST may be retried against another
    /// source (see `Staged`), and separating them here would mean spelling both out in twenty
    /// tests that are about the whole path — download, verify, swap, rollback — and not about the
    /// seam between them.
    fn apply_at(
        exe: &Path,
        settings: &Settings,
        dl: &dyn Downloader,
        release: &Release,
        progress: ChunkProgress,
    ) -> Result<PathBuf> {
        // The shell's own shape: the manifest is verified ONCE, up front, and handed on.
        let manifest = signed_manifest(settings, dl, release)?;
        let staged = fetch_verified_at(exe, dl, release, manifest.as_ref(), progress)?;
        swap_in(&staged)
    }

    #[test]
    fn apply_downloads_verifies_and_swaps() {
        let (dir, fake) = stage("phoenix-selfupdate-apply", b"MZ\x90\x00NEW LAUNCHER", None);
        let exe = dir.join("phoenix-launcher.exe");
        let mut ticks = 0;
        let out = apply_at(&exe, &settings(), &fake, &fake.fetch_release("r", None).unwrap(), &mut |_, _| {
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
        let err = apply_at(&exe, &settings(), &fake, &fake.fetch_release("r", None).unwrap(), &mut |_, _| true).unwrap_err();

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
        let fake = crate::downloader::fake::Fake::new("v999.0.0", "{}", vec![(EXE_ASSET, b"MZ\x90\x00")])
            .no_manifest();

        let err = apply_at(&exe, &settings(), &fake, &fake.fetch_release("r", None).unwrap(), &mut |_, _| true).unwrap_err();
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

        apply_at(&exe, &settings(), &fake, &fake.fetch_release("r", None).unwrap(), &mut |_, _| true).unwrap();
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

    // ---- signed launcher releases ----

    /// The signed manifest and the sidecar disagree, and the signed one decides. If it did not,
    /// signing the launcher payload would be decoration: whoever serves the release also serves
    /// the sidecar.
    #[test]
    fn a_signed_manifest_outranks_the_sha256_sidecar() {
        use sha2::Digest;
        let body: &[u8] = b"MZ\x90\x00NEW LAUNCHER";
        let (dir, fake) = stage_signed(
            "phoenix-selfupdate-signed",
            body,
            &hex::encode(sha2::Sha256::digest(body)),
            "launcher",
        );
        let exe = dir.join("phoenix-launcher.exe");
        apply_at(&exe, &settings(), &fake, &fake.fetch_release("r", None).unwrap(), &mut |_, _| true)
            .unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), body);

        // and the same release with a manifest hash that is WRONG does not install, however
        // agreeable the sidecar might have been — the sidecar is never consulted at all
        let (dir2, fake) =
            stage_signed("phoenix-selfupdate-signed-bad", body, &"b".repeat(64), "launcher");
        let exe2 = dir2.join("phoenix-launcher.exe");
        let err = apply_at(&exe2, &settings(), &fake, &fake.fetch_release("r", None).unwrap(), &mut |_, _| true)
            .unwrap_err();
        assert!(format!("{err:#}").contains("checksum mismatch"), "got: {err:#}");
        assert_eq!(std::fs::read(&exe2).unwrap(), b"OLD LAUNCHER");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    /// Stripping the signature must not be a way to be believed on the sidecar instead. A release
    /// that publishes a manifest and no `.minisig` is refused — that shape is only ever tampering,
    /// which is what makes it different from a release publishing no manifest at all (below).
    #[test]
    fn a_manifest_without_a_signature_is_refused_rather_than_downgraded() {
        use sha2::Digest;
        let body: &[u8] = b"MZ\x90\x00NEW LAUNCHER";
        let (dir, fake) = stage_signed(
            "phoenix-selfupdate-stripped",
            body,
            &hex::encode(sha2::Sha256::digest(body)),
            "launcher",
        );
        let fake = fake.unsigned();
        let exe = dir.join("phoenix-launcher.exe");
        let err = apply_at(&exe, &settings(), &fake, &fake.fetch_release("r", None).unwrap(), &mut |_, _| true)
            .unwrap_err();
        assert!(format!("{err:#}").contains("no signature"), "got: {err:#}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"OLD LAUNCHER");
        assert!(!sibling(&exe, ".new.exe").exists(), "nothing was even downloaded");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A manifest edited after it was signed fails the signature, not the exe checksum — the
    /// refusal has to happen before anything in that document is believed.
    #[test]
    fn an_edited_signed_manifest_is_refused() {
        use sha2::Digest;
        let body: &[u8] = b"MZ\x90\x00NEW LAUNCHER";
        let (dir, mut fake) = stage_signed(
            "phoenix-selfupdate-edited",
            body,
            &hex::encode(sha2::Sha256::digest(body)),
            "launcher",
        );
        // swap in a hash of the attacker's choosing, leaving the signature where it was
        let doc = String::from_utf8(fake.assets["manifest.json"].clone()).unwrap();
        let edited = doc.replace(&hex::encode(sha2::Sha256::digest(body)), &"c".repeat(64));
        assert_ne!(edited, doc);
        fake.assets.insert("manifest.json".into(), edited.into_bytes());

        let exe = dir.join("phoenix-launcher.exe");
        let err = apply_at(&exe, &settings(), &fake, &fake.fetch_release("r", None).unwrap(), &mut |_, _| true)
            .unwrap_err();
        assert!(format!("{err:#}").contains("does not match the file"), "got: {err:#}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"OLD LAUNCHER");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A validly signed manifest for a DIFFERENT payload is still one of ours. Serving the mod
    /// manifest here would otherwise hand self-update a document full of legitimate hashes for
    /// files that are not launchers.
    #[test]
    fn a_manifest_for_another_payload_is_refused() {
        use sha2::Digest;
        let body: &[u8] = b"MZ\x90\x00NEW LAUNCHER";
        let (dir, fake) = stage_signed(
            "phoenix-selfupdate-wrong-payload",
            body,
            &hex::encode(sha2::Sha256::digest(body)),
            "mod",
        );
        let exe = dir.join("phoenix-launcher.exe");
        let err = apply_at(&exe, &settings(), &fake, &fake.fetch_release("r", None).unwrap(), &mut |_, _| true)
            .unwrap_err();
        assert!(format!("{err:#}").contains("\"launcher\" was asked for"), "got: {err:#}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"OLD LAUNCHER");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The SIGNED manifest decides which entry is the launcher, and a release whose index does not
    /// carry that entry is a broken release — not an excuse to fall back to whatever `.exe` the
    /// index happens to list, whose hash the signed document never vouched for. Guessing which
    /// hash it meant is the one thing we must not do.
    #[test]
    fn a_signed_manifest_that_does_not_name_the_exe_is_refused() {
        let body: &[u8] = b"MZ\x90\x00NEW LAUNCHER";
        let (dir, fake) = stage_signed("phoenix-selfupdate-unnamed", body, &"d".repeat(64), "launcher");
        // the manifest's one entry names an asset the release does not publish. Re-signed rather
        // than edited in place, so this tests the missing ASSET and not a broken signature.
        let doc = String::from_utf8(fake.assets["manifest.json"].clone())
            .unwrap()
            .replace(EXE_ASSET, "something-else.exe");
        let fake = crate::downloader::fake::Fake::new("v999.0.0", &doc, vec![(EXE_ASSET, body)]);
        let exe = dir.join("phoenix-launcher.exe");
        let err = apply_at(&exe, &settings(), &fake, &fake.fetch_release("r", None).unwrap(), &mut |_, _| true)
            .unwrap_err();
        assert!(format!("{err:#}").contains("no asset named something-else.exe"), "got: {err:#}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"OLD LAUNCHER");
        assert!(!sibling(&exe, ".new.exe").exists(), "nothing was even downloaded");

        // …and a manifest naming NO executable at all is refused before any source is asked
        let doc = String::from_utf8(fake.assets["manifest.json"].clone())
            .unwrap()
            .replace("something-else.exe", "notes.txt");
        let fake = crate::downloader::fake::Fake::new("v999.0.0", &doc, vec![(EXE_ASSET, body)]);
        let err = apply_at(&exe, &settings(), &fake, &fake.fetch_release("r", None).unwrap(), &mut |_, _| true)
            .unwrap_err();
        assert!(format!("{err:#}").contains("does not name a launcher"), "got: {err:#}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"OLD LAUNCHER");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- which source, which address ----

    /// A launcher release as a MIRROR serves it: the signed manifest and its signature at the
    /// payload root, the exe under `blobs/<sha256>` — and nothing under the asset's NAME, because
    /// a content-addressed host has no such path. Returns the server, a `Mirror` over it, and the
    /// exe's hash.
    fn launcher_mirror(body: &[u8]) -> (crate::test_http::TestServer, crate::mirror::Mirror, String) {
        use crate::test_http::{Canned, TestServer};
        use sha2::Digest;
        let hash = hex::encode(sha2::Sha256::digest(body));
        let manifest = serde_json::json!({
            "schema": 2,
            "payload_id": "launcher",
            "serial": 3,
            "version": "999.0.0",
            "files": [{
                "name": EXE_ASSET,
                "dest": "phoenix-launcher.exe",
                "sha256": hash,
                "size": body.len(),
            }]
        })
        .to_string()
        .into_bytes();
        let sig = crate::trust::testing::test_sig(&manifest).into_bytes();
        let blob_path: &'static str = Box::leak(format!("/launcher/blobs/{hash}").into_boxed_str());
        let blob = body.to_vec();
        let server = TestServer::start(move |_port| {
            let mut routes = std::collections::HashMap::new();
            routes.insert("/launcher/manifest.json", Canned::body(manifest));
            routes.insert("/launcher/manifest.json.minisig", Canned::body(sig));
            routes.insert(blob_path, Canned::body(blob));
            routes
        });
        // `download_agent()` is https-only and a loopback listener cannot speak TLS — the same swap
        // mirror.rs's own tests make, with every other field the real thing
        let agent = ureq::builder()
            .timeout_connect(Duration::from_secs(5))
            .timeout_read(Duration::from_secs(5))
            .timeout_write(Duration::from_secs(5))
            .redirects(0)
            .build();
        let base = format!("http://127.0.0.1:{}", server.port);
        (server, crate::mirror::Mirror::with_agent(&base, Payload::Launcher, agent), hash)
    }

    /// The two shapes the launcher exe resolves to, from ONE signed manifest entry: on a
    /// name-addressed source (GitHub; the `Fake` here) the asset is the release's, under the
    /// entry's NAME; on a content-addressed one (a mirror) it is synthesized with the entry's HASH
    /// as its name, which `Mirror::url_of` sends to `blobs/<sha256>`. The hash and size come from
    /// the signed document either way — the source only ever decides the address.
    #[test]
    fn the_launcher_exe_is_addressed_by_name_on_github_and_by_hash_on_a_mirror() {
        use sha2::Digest;
        let body: &[u8] = b"MZ\x90\x00A LAUNCHER, ADDRESSED TWO WAYS";
        let hash = hex::encode(sha2::Sha256::digest(body));

        let (dir, fake) = stage_signed("phoenix-selfupdate-shape-name", body, &hash, "launcher");
        assert!(!fake.content_addressed(), "the Fake stands in for GitHub: a release index");
        let release = fake.fetch_release("r", None).unwrap();
        let m = signed_manifest(&settings(), &fake, &release).unwrap();
        let t = resolve(&fake, &release, m.as_ref()).expect("resolves on a name-addressed source");
        assert_eq!(t.asset.name, EXE_ASSET, "GitHub serves the exe under the asset name");
        assert_eq!(t.sha256, hash);
        assert_eq!(t.size, Some(body.len() as u64), "the signed size caps the transfer");
        let _ = std::fs::remove_dir_all(&dir);

        let (server, mirror, hash2) = launcher_mirror(body);
        assert_eq!(hash2, hash);
        assert!(mirror.content_addressed());
        let release = mirror.fetch_release("ignored/repo", None).expect("the mirror's one release");
        assert!(
            release.asset(EXE_ASSET).is_none(),
            "a mirror publishes no index, so the exe cannot be found by name — that was the bug"
        );
        let m = signed_manifest(&settings(), &mirror, &release).unwrap();
        let t =
            resolve(&mirror, &release, m.as_ref()).expect("resolves on a content-addressed source");
        assert_eq!(t.asset.name, hash, "a mirror serves the exe under its hash");
        assert_eq!(t.sha256, hash);
        assert_eq!(t.size, Some(body.len() as u64));
        // only the two trust documents crossed the wire so far — resolving is not downloading
        assert_eq!(server.hits("/launcher/manifest.json"), 1);
        assert_eq!(server.hits("/launcher/manifest.json.minisig"), 1);
        assert_eq!(server.hits(&format!("/launcher/blobs/{hash}")), 0);
    }

    /// The bug, end to end: a self-update from a mirror. The exe is fetched from `blobs/<sha256>`,
    /// verified against the signed hash, and swapped in — and the path the old code asked for, the
    /// asset's NAME at the payload root, is never requested, because it does not exist on any
    /// mirror.
    #[test]
    fn a_launcher_self_updates_from_a_mirror_by_hash() {
        let body: &[u8] = b"MZ\x90\x00A LAUNCHER BUILD SERVED BY A MIRROR";
        let (server, mirror, hash) = launcher_mirror(body);
        let dir = std::env::temp_dir().join("phoenix-selfupdate-from-mirror");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("phoenix-launcher.exe");
        std::fs::write(&exe, b"OLD LAUNCHER").unwrap();

        let release = mirror.fetch_release("ignored/repo", None).unwrap();
        assert!(
            available(&settings(), &mirror, &release).unwrap().is_some(),
            "the mirror's signed release is an upgrade, judged by the same gate as GitHub's"
        );
        let out = apply_at(&exe, &settings(), &mirror, &release, &mut |_, _| true).unwrap();
        assert_eq!(out, exe, "the returned path is the one to restart");
        assert_eq!(std::fs::read(&exe).unwrap(), body);
        assert_eq!(std::fs::read(sibling(&exe, ".old.exe")).unwrap(), b"OLD LAUNCHER");
        assert!(!sibling(&exe, ".new.exe").exists(), "staging is consumed");

        assert_eq!(server.hits(&format!("/launcher/blobs/{hash}")), 1, "fetched by hash, once");
        assert!(!server.saw_authorization(&format!("/launcher/blobs/{hash}")), "a mirror gets no credential");
        assert_eq!(server.hits(&format!("/launcher/{EXE_ASSET}")), 0, "the name is not a path a mirror has");
        assert_eq!(server.hits(&format!("/launcher/{SHA_ASSET}")), 0, "nor is the legacy sidecar");
        assert_eq!(server.hits("/launcher/manifest.json"), 1, "read once, verified once — never twice");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The legacy shape — no manifest at all — still installs from the sidecar. Every launcher in
    /// the wild predates signing, and refusing them would strand exactly the builds that most need
    /// to update. (See `resolve` for how that gap closes.)
    #[test]
    fn a_release_that_predates_signing_still_installs_from_the_sidecar() {
        let (dir, fake) = stage("phoenix-selfupdate-legacy", b"MZ\x90\x00NEW LAUNCHER", None);
        assert!(fake.fetch_release("r", None).unwrap().asset("manifest.json").is_none());
        let exe = dir.join("phoenix-launcher.exe");
        apply_at(&exe, &settings(), &fake, &fake.fetch_release("r", None).unwrap(), &mut |_, _| true)
            .unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"MZ\x90\x00NEW LAUNCHER");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- the signed size as a streaming cap on the exe download ----

    /// Streams real, small chunks — `Fake` writes a whole body in one call, which can't show a
    /// mid-transfer abort. Mirrors `install::tests::Overflowing`; wraps a `Fake` for everything
    /// except `download_to`, which serves `body` (not the Fake's own registered asset bytes) in
    /// pieces of `chunk`.
    struct OverflowingExe {
        inner: crate::downloader::fake::Fake,
        /// What the "host" actually sends — independent of whatever `inner` has registered.
        body: Vec<u8>,
        chunk: usize,
        calls: std::sync::atomic::AtomicU32,
        /// The last `written` value handed to the progress callback — the mid-transfer proof,
        /// read back after the call fails (the `.new.exe` it left is deleted by `apply_at`'s own
        /// cleanup and can't be inspected afterward).
        last_written: std::sync::atomic::AtomicU64,
    }

    impl crate::downloader::Downloader for OverflowingExe {
        fn fetch_release(&self, r: &str, t: Option<&str>) -> Result<Release> {
            self.inner.fetch_release(r, t)
        }
        fn fetch_releases(&self, r: &str) -> Result<Vec<Release>> {
            self.inner.fetch_releases(r)
        }
        fn download(&self, a: &Asset) -> Result<Vec<u8>> {
            self.inner.download(a)
        }
        fn download_to(
            &self,
            _asset: &Asset,
            dest: &Path,
            resume_from: u64,
            progress: ChunkProgress,
        ) -> Result<(u64, String)> {
            use sha2::Digest;
            use std::sync::atomic::Ordering;
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(resume_from, 0, "self-update never resumes");
            let mut file = std::fs::File::create(dest)?;
            let mut written = 0u64;
            for piece in self.body.chunks(self.chunk) {
                std::io::Write::write_all(&mut file, piece)?;
                written += piece.len() as u64;
                self.last_written.store(written, Ordering::SeqCst);
                if !progress(written, Some(self.body.len() as u64)) {
                    anyhow::bail!("download aborted");
                }
            }
            Ok((written, hex::encode(sha2::Sha256::digest(&self.body))))
        }
    }

    /// Item 2 in a second code path: `apply_at`'s exe download must be cut off mid-stream when the
    /// host sends past the SIGNED size, exactly like `obtain_to_cache`'s asset cap. Hash-
    /// verification-before-swap does not cover this — it runs on bytes already on disk.
    #[test]
    fn an_exe_stream_longer_than_its_signed_size_is_aborted_mid_transfer() {
        use sha2::Digest;
        use std::sync::atomic::Ordering;
        let declared: Vec<u8> = b"MZ\x90\x00HONEST LAUNCHER BUILD".to_vec(); // what the manifest signs for
        let hostile: Vec<u8> =
            declared.iter().copied().chain(std::iter::repeat(b'X').take(100_000)).collect();
        let (dir, fake) = stage_signed(
            "phoenix-selfupdate-exe-overcap",
            &declared,
            &hex::encode(sha2::Sha256::digest(&declared)),
            "launcher",
        );
        let dl = OverflowingExe {
            inner: fake,
            body: hostile,
            chunk: 16,
            calls: 0.into(),
            last_written: 0.into(),
        };
        let exe = dir.join("phoenix-launcher.exe");
        let release = dl.inner.fetch_release("r", None).unwrap();

        let err = apply_at(&exe, &settings(), &dl, &release, &mut |_, _| true).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("more than the signed"), "expected the size-cap refusal, got: {msg}");
        assert!(!msg.contains("aborted"), "must not read as an ordinary cancel: {msg}");
        assert_eq!(dl.calls.load(Ordering::SeqCst), 1, "no retry loop here to (mis)fire");
        assert_eq!(std::fs::read(&exe).unwrap(), b"OLD LAUNCHER", "the working launcher is untouched");
        assert!(!sibling(&exe, ".new.exe").exists(), "the oversized stream is not left behind");
        // the mid-transfer proof: writing stopped within one chunk of the signed size, nowhere
        // near the ~100,024-byte hostile body it would reach if the whole thing streamed through
        let last = dl.last_written.load(Ordering::SeqCst);
        assert!(
            (declared.len() as u64..declared.len() as u64 + dl.chunk as u64).contains(&last),
            "expected the abort within one chunk past the {}-byte signed size, got {last}",
            declared.len()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The companion the cap must not break: an update whose body is EXACTLY the signed size has
    /// to keep applying — streamed in real chunks (not `Fake`'s one-shot write) so the guard is
    /// actually exercised across several ticks, not just the size check that already ran before.
    #[test]
    fn an_exe_that_completes_exactly_at_the_signed_size_still_applies() {
        use sha2::Digest;
        let body: Vec<u8> = b"MZ\x90\x00A LAUNCHER BUILD OF A GIVEN LENGTH".to_vec();
        let (dir, fake) = stage_signed(
            "phoenix-selfupdate-exe-exact-cap",
            &body,
            &hex::encode(sha2::Sha256::digest(&body)),
            "launcher",
        );
        let dl = OverflowingExe {
            inner: fake,
            body: body.clone(),
            chunk: 7, // does not divide the body evenly — the last tick lands off-grid
            calls: 0.into(),
            last_written: 0.into(),
        };
        let exe = dir.join("phoenix-launcher.exe");
        let release = dl.inner.fetch_release("r", None).unwrap();

        let out = apply_at(&exe, &settings(), &dl, &release, &mut |_, _| true).unwrap();
        assert_eq!(out, exe, "the returned path is the one to restart");
        assert_eq!(std::fs::read(&exe).unwrap(), body);
        assert!(!sibling(&exe, ".new.exe").exists(), "staging is consumed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
