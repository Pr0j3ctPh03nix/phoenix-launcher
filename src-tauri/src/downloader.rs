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
        /// A release whose assets are `assets` plus `manifest.json` (the given JSON).
        pub fn new(tag: &str, manifest_json: &str, assets: Vec<(&str, &[u8])>) -> Self {
            let mut map: HashMap<String, Vec<u8>> =
                assets.into_iter().map(|(n, b)| (n.to_string(), b.to_vec())).collect();
            map.insert("manifest.json".to_string(), manifest_json.as_bytes().to_vec());
            Self { tag: tag.to_string(), assets: map, prerelease: false }
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
