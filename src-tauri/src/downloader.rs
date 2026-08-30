//! The network seam of the engine: everything the updater needs from "somewhere that serves
//! releases" behind one trait. The engine never talks HTTP directly — production uses the
//! GitHub backend (github.rs), tests use the in-memory fake below. New transports (a CDN
//! mirror, resumable downloads) slot in without touching engine/install logic.

use anyhow::Result;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub name: String,
    /// API asset URL (used for private downloads with Accept: application/octet-stream).
    pub url: String,
    /// Direct download URL (used for public downloads).
    pub browser_download_url: String,
    /// Bytes, as the release index reports them. Used only to CHOOSE an asset — the mirror probe
    /// needs the BIGGEST one, since a throttled path serves a small file perfectly and would
    /// otherwise measure as healthy. Never used to size a transfer: `download_to` learns the real
    /// length from the response. Defaulted, so an index that omits it still parses.
    #[serde(default)]
    pub size: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub tag_name: String,
    pub assets: Vec<Asset>,
    /// The release description. It is the LAUNCHER's "What's new" — both the pending-update
    /// banner (selfupdate.rs) and the launcher history page (`engine::launcher_notes_history`)
    /// render it. The dist repo's comes from its manifests instead. Absent or JSON `null` both
    /// land as None.
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

    /// Name -> asset, built once. `asset()` is a linear scan, which is fine for the handful of
    /// lookups the shim does but quadratic for the base game: 4,635 jobs against a merged release
    /// carrying 4,636 assets is ~10 million string comparisons.
    pub fn asset_index(&self) -> std::collections::HashMap<&str, &Asset> {
        self.assets.iter().map(|a| (a.name.as_str(), a)).collect()
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
                        // Never stale unless a test says so. A release double is not the subject
                        // of the freshness gate, and a floor baked into the build under test
                        // (PHOENIX_MIN_SERIAL_*, which persists in whatever terminal a release was
                        // built from) would otherwise turn most of the suite red for a reason
                        // nothing on screen would name.
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
