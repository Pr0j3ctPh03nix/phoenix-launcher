//! The network seam of the engine: everything the updater needs from "somewhere that serves
//! releases" behind one trait. The engine never talks HTTP directly — production uses the
//! GitHub backend (github.rs), tests use the in-memory fake below. New transports (a CDN
//! mirror, resumable downloads) slot in without touching engine/install logic.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::io::{Read, Seek};
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub name: String,
    /// API asset URL (used for private downloads with Accept: application/octet-stream).
    pub url: String,
    /// Direct download URL (used for public downloads).
    pub browser_download_url: String,
    /// Bytes, as the release index reports them. On a NAME-ADDRESSED backend (GitHub) that is the
    /// host's word, so it is used only to CHOOSE an asset — the mirror probe needs the BIGGEST one,
    /// since a throttled path serves a small file perfectly and would otherwise measure as healthy —
    /// and never to size a transfer: `download_to` learns the real length from the response.
    /// Defaulted, so an index that omits it still parses.
    ///
    /// On a CONTENT-ADDRESSED backend there is no index to report anything, and the field carries
    /// what the caller KNOWS instead: `install::Resolved::asset_for` synthesizes the asset with the
    /// signed manifest's declared size, and `Mirror::download` bounds its read at exactly that.
    #[serde(default)]
    pub size: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub tag_name: String,
    pub assets: Vec<Asset>,
    /// The release description, as a GitHub release index carries it — which is the only place it
    /// exists at all: a mirror publishes no index, so on one this is simply absent, and the
    /// histories built from a listing are GitHub-only by the same fact.
    ///
    /// DISPLAY TEXT, and nothing is ever decided by it. Everything a launcher acts on — the version
    /// offered, the notes shown beside it, every hash and size — comes out of the SIGNED manifest
    /// (`selfupdate::available`). Absent or JSON `null` both land as None.
    #[serde(default)]
    pub body: Option<String>,
    /// Unpublished. Only ever visible to a token with push access; absent from an anonymous
    /// listing entirely.
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
}

impl Release {
    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }

    /// Is this a release the updater would actually offer? The `/releases` LISTING carries drafts
    /// and prereleases; `/releases/latest` — which every check follows — carries neither. A
    /// "What's new" page built from the unfiltered listing therefore advertises versions that can
    /// never be installed, and dates its cache against a tag no check will ever report.
    pub fn is_published(&self) -> bool {
        !self.draft && !self.prerelease
    }

}

/// Why a network operation failed, in shell-actionable terms. Rooted in the anyhow chain at the
/// transport edge; the command layer downcasts to map it onto the wire error `kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetKind {
    /// Couldn't complete the HTTP exchange at all (DNS, TLS, stall, offline).
    Transport,
    /// The server answered with this status.
    Status(u16),
}

impl std::fmt::Display for NetKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NetKind::Transport => write!(f, "connection failed"),
            NetKind::Status(c) => write!(f, "HTTP {c}"),
        }
    }
}

impl std::error::Error for NetKind {}

/// Per-chunk download progress: bytes written so far, total size if known. Return `false` to
/// abort the download mid-stream (the partial file is kept — it resumes later); impls fail the
/// transfer with an error when aborted.
pub type ChunkProgress<'a> = &'a mut dyn FnMut(u64, Option<u64>) -> bool;

/// Send + Sync: install's phase 1 downloads several files at once from a worker pool, sharing
/// one Downloader reference across threads.
pub trait Downloader: Send + Sync {
    /// Does this backend address a payload entry by its CONTENT HASH rather than by the opaque
    /// release-asset `name` the manifest carries?
    ///
    /// One question, asked at two places in install.rs, and the reason it is a trait method rather
    /// than a downcast: both sites need the same fact and neither may know which concrete backend
    /// it holds.
    ///   - asset resolution: a name-addressed backend looks `name` up in the release's asset list
    ///     to learn the URL; a content-addressed one derives the URL from the hash and needs no
    ///     list at all.
    ///   - the release-index preflight: it refuses a run whose asset names the release does not
    ///     carry. A content-addressed backend HAS no release index, so there is nothing to check
    ///     against and the preflight can only ever say "addressable" — existence is learned when
    ///     the blob is fetched.
    /// Default false: a release-hosted backend is the ordinary case, and an impl that says nothing
    /// is that case.
    fn content_addressed(&self) -> bool {
        false
    }
    /// A release by tag (or the latest).
    fn fetch_release(&self, repo: &str, tag: Option<&str>) -> Result<Release>;
    /// All releases, newest first.
    fn fetch_releases(&self, repo: &str) -> Result<Vec<Release>>;
    /// A whole asset in memory (small files, e.g. manifest.json).
    fn download(&self, asset: &Asset) -> Result<Vec<u8>>;
    /// The same, refusing to buffer more than `max` bytes.
    ///
    /// For anything whose SIZE is a trust input — a document we are about to verify and believe —
    /// the index's `size` field is the host's word, not a fact, so the ceiling has to be enforced
    /// against the stream itself. The default impl reads first and checks after, which is honest
    /// for an in-memory double; `Github` overrides it to bound the read (see its `take`), because
    /// there the peer is the thing being distrusted.
    fn download_limited(&self, asset: &Asset, max: u64) -> Result<Vec<u8>> {
        let bytes = self.download(asset)?;
        if bytes.len() as u64 > max {
            anyhow::bail!("{} is larger than the {max} bytes allowed for it", asset.name);
        }
        Ok(bytes)
    }
    /// Stream an asset to `dest`, returning (bytes written, sha256 of the WHOLE file). Never
    /// buffers the body. `resume_from` > 0 continues an interrupted attempt: the existing prefix
    /// of `dest` is hashed (so the returned sha covers everything) and only the remainder is
    /// fetched; an impl that can't resume simply restarts from zero.
    fn download_to(&self, asset: &Asset, dest: &Path, resume_from: u64, progress: ChunkProgress) -> Result<(u64, String)>;
}

/// A response body as `stream_to_file` has to see it — deliberately not a `ureq::Response`, so the
/// resumable writer stays on this side of the engine's HTTP-free seam. Each HTTP backend builds one
/// through `transport::body_of`, which is the single place `Content-Range` is parsed.
pub struct Body {
    /// Byte offset the peer says this body starts at, from a 206's `Content-Range`. `None` when it
    /// answered 200 (declined the range) or sent nothing usable — see `stream_to_file` for why the
    /// two are treated alike.
    pub range_start: Option<u64>,
    /// The peer's claim about how many bytes follow, from `Content-Length`. Progress only.
    pub content_length: Option<u64>,
    pub reader: Box<dyn Read + Send + Sync + 'static>,
}

/// The resumable streaming download, shared by every HTTP backend: hash the prefix already on
/// disk, ask for the rest, append it, and return (bytes written, sha256 of the WHOLE file). Never
/// buffers the body.
///
/// `open` issues the request for one attempt and is handed the offset to resume from (`None` =
/// start over). It is a callback rather than a response because that request is the ONLY thing the
/// backends differ in: GitHub's carries auth and a hop to pre-signed storage, a mirror's is a plain
/// GET at a content-addressed URL. Everything after it — the range check, the truncate, the hash —
/// is identical, and one copy is what stops the two drifting into two sets of resume rules.
pub fn stream_to_file(
    dest: &Path,
    resume_from: u64,
    open: impl FnOnce(Option<u64>) -> Result<Body>,
    progress: ChunkProgress,
) -> Result<(u64, String)> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let mut prefix: u64 = 0;
    if resume_from > 0 {
        if let Ok(f) = std::fs::File::open(dest) {
            prefix = f.metadata()?.len().min(resume_from);
            std::io::copy(&mut f.take(prefix), &mut hasher)?;
        }
    }
    let body = open((prefix > 0).then_some(prefix))?;
    // A 206 is only usable if it is the range we ASKED for. The status alone says "partial", not
    // "partial from `prefix`" — a peer answering 206 with a different offset would have its bytes
    // appended at `prefix`, producing a plausibly-sized file that fails only at the final hash, and
    // burning the one clean restart a resume gets. Absent Content-Range is treated as a decline for
    // the same reason: unverifiable is not the same as correct.
    if prefix > 0 && body.range_start != Some(prefix) {
        prefix = 0; // server declined the Range, or answered a different one — restart at zero
        hasher = Sha256::new();
    }
    let total: Option<u64> = body.content_length.map(|l| l + prefix);
    let mut reader = body.reader;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false) // length is fixed explicitly via set_len below (resume keeps the prefix)
        .open(dest)
        .with_context(|| format!("creating {}", dest.display()))?;
    // drop any stale tail beyond the prefix we just hashed, then append
    file.set_len(prefix)?;
    file.seek(std::io::SeekFrom::Start(prefix))?;
    // 256 KiB reads, matching verify.rs's file hashing: the payload includes multi-hundred-MB
    // VPKs, and 64 KiB quadrupled the read syscalls per file for nothing
    let mut buf = vec![0u8; 256 * 1024];
    let mut written = prefix;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        std::io::Write::write_all(&mut file, &buf[..n])?;
        hasher.update(&buf[..n]);
        written += n as u64;
        if !progress(written, total) {
            // caller aborted (a sibling download failed, a warm was cancelled) — the
            // partial file stays behind as the resume source
            anyhow::bail!("download aborted");
        }
    }
    Ok((written, hex::encode(hasher.finalize())))
}

#[cfg(test)]
pub mod fake {
    //! In-memory Downloader for engine/install tests: one release of byte-vector assets.
    use super::*;
    use anyhow::Context;
    use std::collections::HashMap;

    pub struct Fake {
        pub tag: String,
        pub assets: HashMap<String, Vec<u8>>,
        pub prerelease: bool,
    }

    impl Fake {
        /// A release whose assets are `assets` plus `manifest.json` (the given JSON) and a valid
        /// `manifest.json.minisig` over exactly the bytes it serves.
        ///
        /// It SIGNS, because it stands in for a producer and a producer signs — a fake that served
        /// unsigned documents would test the reader against a release nobody will ever publish.
        /// For the same reason it fills in `payload_id`/`serial` when the caller's JSON omits
        /// them: those are constants of the release process, not of any individual test, and
        /// making thirty manifest literals restate them would be noise around the thing each test
        /// is actually about. A test that cares says so — `payload("game")`, `serial(n)`.
        ///
        /// The signing key is the suite's own (`trust::testing`), pinned only in test builds.
        pub fn new(tag: &str, manifest_json: &str, assets: Vec<(&str, &[u8])>) -> Self {
            let mut map: HashMap<String, Vec<u8>> =
                assets.into_iter().map(|(n, b)| (n.to_string(), b.to_vec())).collect();
            map.insert("manifest.json".to_string(), manifest_json.as_bytes().to_vec());
            let mut fake = Self { tag: tag.to_string(), assets: map, prerelease: false };
            fake.publish_manifest(|_| {});
            fake
        }

        /// Serve the manifest as a different payload — the base-game tests, which fetch through
        /// the same `manifest_of` gate as the shim.
        pub fn payload(mut self, id: &str) -> Self {
            let id = id.to_string();
            self.publish_manifest(|doc| {
                doc["payload_id"] = serde_json::Value::String(id.clone());
            });
            self
        }

        /// Serve the manifest at a given serial, for the freshness gate.
        pub fn serial(mut self, n: u64) -> Self {
            self.publish_manifest(|doc| doc["serial"] = serde_json::json!(n));
            self
        }

        /// Serve the manifest with a top-level key removed — the reader's own defaults and
        /// requirements are what several tests are about, and `new` fills two of those fields in.
        pub fn without(mut self, key: &str) -> Self {
            let key = key.to_string();
            self.publish_manifest(|doc| {
                if let Some(o) = doc.as_object_mut() {
                    o.remove(&key);
                }
            });
            self
        }

        /// Publish no manifest at all — the shape of every launcher release cut before signing
        /// existed, which self-update still has to be able to install from.
        pub fn no_manifest(mut self) -> Self {
            self.assets.remove("manifest.json");
            self.assets.remove("manifest.json.minisig");
            self
        }

        /// Publish the manifest with no signature beside it. A release that says something and
        /// declines to sign it is what an attacker stripping the signature produces, so it is a
        /// distinct case from publishing nothing.
        pub fn unsigned(mut self) -> Self {
            self.assets.remove("manifest.json.minisig");
            self
        }

        /// Rewrite `manifest.json` and re-sign it. The signature covers the bytes that are stored,
        /// so the edit and the signing cannot drift apart.
        ///
        /// A manifest that is not a JSON object is served (and signed) verbatim: several tests
        /// hand this garbage on purpose, and "the reader refuses it" is exactly what they assert.
        fn publish_manifest(&mut self, edit: impl FnOnce(&mut serde_json::Value)) {
            let raw = self.assets.get("manifest.json").cloned().unwrap_or_default();
            let bytes = match serde_json::from_slice::<serde_json::Value>(&raw) {
                Ok(mut doc) if doc.is_object() => {
                    if doc.get("payload_id").is_none() {
                        doc["payload_id"] = serde_json::json!("mod");
                    }
                    if doc.get("serial").is_none() {
                        // Never stale unless a test says so: a release double is not the subject
                        // of the freshness gate, and every test that is says so explicitly.
                        doc["serial"] = serde_json::json!(u64::MAX);
                    }
                    edit(&mut doc);
                    doc.to_string().into_bytes()
                }
                _ => raw,
            };
            let sig = crate::trust::testing::test_sig(&bytes);
            self.assets.insert("manifest.json".to_string(), bytes);
            self.assets.insert("manifest.json.minisig".to_string(), sig.into_bytes());
        }

        /// Mark it a prerelease: it stays in the `/releases` listing and drops out of the
        /// histories, which is exactly the case the exclusion exists for.
        pub fn prerelease(mut self) -> Self {
            self.prerelease = true;
            self
        }

        fn release(&self) -> Release {
            Release {
                tag_name: self.tag.clone(),
                body: None,
                draft: false,
                prerelease: self.prerelease,
                assets: self
                    .assets
                    .keys()
                    .map(|name| Asset {
                        size: self.assets.get(name).map_or(0, |b| b.len() as u64),
                        name: name.clone(),
                        url: String::new(),
                        browser_download_url: String::new(),
                    })
                    .collect(),
            }
        }
    }

    impl Downloader for Fake {
        fn fetch_release(&self, _repo: &str, _tag: Option<&str>) -> Result<Release> {
            Ok(self.release())
        }
        fn fetch_releases(&self, _repo: &str) -> Result<Vec<Release>> {
            Ok(vec![self.release()])
        }
        fn download(&self, asset: &Asset) -> Result<Vec<u8>> {
            self.assets
                .get(&asset.name)
                .cloned()
                .with_context(|| format!("no asset {}", asset.name))
        }
        fn download_to(&self, asset: &Asset, dest: &Path, resume_from: u64, progress: ChunkProgress) -> Result<(u64, String)> {
            use sha2::Digest;
            let bytes = self.download(asset)?;
            // honest resume, mirroring the real impl: keep a matching prefix, append the rest
            let mut out = Vec::new();
            if resume_from > 0 {
                if let Ok(existing) = std::fs::read(dest) {
                    let keep = (existing.len() as u64).min(resume_from) as usize;
                    out = existing;
                    out.truncate(keep);
                }
            }
            out.extend_from_slice(&bytes[out.len()..]);
            std::fs::write(dest, &out).with_context(|| format!("writing {}", dest.display()))?;
            // honor the abort contract like the real impl: false = fail the transfer, keep the
            // partial file (cancellation tests depend on this being honest)
            if !progress(out.len() as u64, Some(out.len() as u64)) {
                anyhow::bail!("download aborted");
            }
            Ok((out.len() as u64, hex::encode(sha2::Sha256::digest(&out))))
        }
    }
}
