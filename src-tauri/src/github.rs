//! GitHub Releases backend for the `Downloader` trait. Works for public repos with no auth and
//! private repos with a token, over the same code path (the REST API), so the updater can pull
//! from either.
//!
//! All requests carry timeouts (connect + per-read/write) so a dead link errors out instead of
//! hanging the UI forever; big assets stream to disk via `download_to`. Failures root a typed
//! `NetKind` in the anyhow chain so the command layer can classify them for the UI.

use anyhow::{bail, Context, Result};
use std::io::{Read, Seek};
use std::path::Path;
use std::time::Duration;

use crate::downloader::{Asset, ChunkProgress, Downloader, NetKind, Release};

const UA: &str = concat!("phoenix-launcher/", env!("CARGO_PKG_VERSION"));
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Per socket read/write op — detects stalls without capping total transfer time of large assets.
const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// The GitHub-backed `Downloader`. Cheap to construct; holds only the optional auth token.
pub struct Github {
    token: Option<String>,
}

impl Github {
    pub fn new(token: Option<&str>) -> Self {
        Self { token: token.map(str::to_string) }
    }
}

fn agent(redirects: u32) -> ureq::Agent {
    ureq::builder()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(IO_TIMEOUT)
        .timeout_write(IO_TIMEOUT)
        .redirects(redirects)
        .build()
}

fn api_url(repo: &str, tag: Option<&str>) -> String {
    match tag {
        Some(t) => format!("https://api.github.com/repos/{repo}/releases/tags/{t}"),
        None => format!("https://api.github.com/repos/{repo}/releases/latest"),
    }
}

/// ureq's error into an anyhow chain rooted at a typed `NetKind` (the shell classifies on it),
/// keeping the HTTP status/body snippet as readable context.
fn net_err(e: ureq::Error) -> anyhow::Error {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            let snippet: String = body.chars().take(200).collect();
            anyhow::Error::new(NetKind::Status(code)).context(format!("HTTP {code}: {snippet}"))
        }
        ureq::Error::Transport(t) => {
            anyhow::Error::new(NetKind::Transport).context(format!("transport error: {t}"))
        }
    }
}

/// Start an asset request and return the response whose body is the asset bytes.
///
/// Public (no token): the direct `browser_download_url`.
/// Private (token): the API asset URL with `Accept: application/octet-stream`, then follow the 302
/// to storage WITHOUT forwarding the Authorization header — the storage URL is pre-signed and 403s
/// if it sees one.
/// `resume_from` > 0 adds a Range header; the answer is then 206, or 200 if the server declined
/// to resume (the caller restarts from zero in that case).
fn asset_response(asset: &Asset, token: Option<&str>, resume_from: u64) -> Result<ureq::Response> {
    let range = |req: ureq::Request| {
        if resume_from > 0 {
            req.set("Range", &format!("bytes={resume_from}-"))
        } else {
            req
        }
    };
    let Some(t) = token else {
        return range(agent(5).get(&asset.browser_download_url).set("User-Agent", UA)).call().map_err(net_err);
    };
    let resp = range(
        agent(0)
            .get(&asset.url)
            .set("User-Agent", UA)
            .set("Accept", "application/octet-stream")
            .set("Authorization", &format!("Bearer {t}")),
    )
    .call();
    match resp {
        Ok(r) if (200..300).contains(&r.status()) => Ok(r),
        Ok(r) if (300..400).contains(&r.status()) => {
            let loc = r
                .header("Location")
                .context("redirect response without a Location header")?
                .to_string();
            // no auth on the storage hop — but the Range header rides along (S3 honors it)
            range(agent(5).get(&loc).set("User-Agent", UA)).call().map_err(net_err)
        }
        Ok(r) => bail!("asset download returned HTTP {}", r.status()),
        Err(e) => Err(net_err(e)),
    }
}

impl Downloader for Github {
    /// List releases, newest first (GitHub's order). One page, up to 100 — plenty for this project.
    fn fetch_releases(&self, repo: &str) -> Result<Vec<Release>> {
        let mut req = agent(5)
            .get(&format!("https://api.github.com/repos/{repo}/releases?per_page=100"))
            .set("User-Agent", UA)
            .set("Accept", "application/vnd.github+json")
            .set("X-GitHub-Api-Version", "2022-11-28");
        if let Some(t) = self.token.as_deref() {
            req = req.set("Authorization", &format!("Bearer {t}"));
        }
        let resp = req.call().map_err(net_err)?;
        resp.into_json().context("parsing the releases JSON")
    }

    /// Fetch a release by tag (or the latest). `token` is only required for private repos.
    fn fetch_release(&self, repo: &str, tag: Option<&str>) -> Result<Release> {
        let mut req = agent(5)
            .get(&api_url(repo, tag))
            .set("User-Agent", UA)
            .set("Accept", "application/vnd.github+json")
            .set("X-GitHub-Api-Version", "2022-11-28");
        if let Some(t) = self.token.as_deref() {
            req = req.set("Authorization", &format!("Bearer {t}"));
        }
        let resp = req.call().map_err(net_err)?;
        resp.into_json().context("parsing the release JSON")
    }

    /// Download an asset into memory (small files, e.g. manifest.json).
    fn download(&self, asset: &Asset) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        asset_response(asset, self.token.as_deref(), 0)?.into_reader().read_to_end(&mut buf)?;
        Ok(buf)
    }

    /// Stream an asset to `dest`, returning (bytes written, sha256 of the WHOLE file). Never
    /// buffers the body. `resume_from` > 0 continues an interrupted attempt: the existing prefix
    /// is hashed (so the returned sha covers everything) and the rest fetched with a Range
    /// request; a 200 answer means the server declined to resume and we start over.
    fn download_to(&self, asset: &Asset, dest: &Path, resume_from: u64, progress: ChunkProgress) -> Result<(u64, String)> {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        let mut prefix: u64 = 0;
        if resume_from > 0 {
            if let Ok(f) = std::fs::File::open(dest) {
                prefix = f.metadata()?.len().min(resume_from);
                std::io::copy(&mut f.take(prefix), &mut hasher)?;
            }
        }
        let resp = asset_response(asset, self.token.as_deref(), prefix)?;
        if prefix > 0 && resp.status() != 206 {
            prefix = 0; // server declined the Range — restart from zero
            hasher = Sha256::new();
        }
        let total: Option<u64> = resp
            .header("Content-Length")
            .and_then(|v| v.parse::<u64>().ok())
            .map(|l| l + prefix);
        let mut reader = resp.into_reader();
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false) // length is fixed explicitly via set_len below (resume keeps the prefix)
            .open(dest)
            .with_context(|| format!("creating {}", dest.display()))?;
        // drop any stale tail beyond the prefix we just hashed, then append
        file.set_len(prefix)?;
        file.seek(std::io::SeekFrom::Start(prefix))?;
        let mut buf = [0u8; 64 * 1024];
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
                bail!("download aborted");
            }
        }
        Ok((written, hex::encode(hasher.finalize())))
    }
}
