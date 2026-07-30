//! Minimal GitHub Releases client. Works for public repos with no auth and private repos with a
//! token, over the same code path (the REST API), so the updater can pull from either.
//!
//! All requests carry timeouts (connect + per-read/write) so a dead link errors out instead of
//! hanging the UI forever; big assets stream to disk via `download_asset_to`.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

const UA: &str = concat!("phoenix-updater/", env!("CARGO_PKG_VERSION"));
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Per socket read/write op — detects stalls without capping total transfer time of large assets.
const IO_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Deserialize)]
pub struct Asset {
    pub name: String,
    /// API asset URL (used for private downloads with Accept: application/octet-stream).
    pub url: String,
    /// Direct download URL (used for public downloads).
    pub browser_download_url: String,
}

#[derive(Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    pub assets: Vec<Asset>,
}

impl Release {
    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
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

/// Fetch a release by tag (or the latest). `token` is only required for private repos.
pub fn fetch_release(repo: &str, tag: Option<&str>, token: Option<&str>) -> Result<Release> {
    let mut req = agent(5)
        .get(&api_url(repo, tag))
        .set("User-Agent", UA)
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28");
    if let Some(t) = token {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    let resp = req.call().map_err(net_err)?;
    resp.into_json().context("parsing the release JSON")
}

/// Start an asset request and return the response whose body is the asset bytes.
///
/// Public (no token): the direct `browser_download_url`.
/// Private (token): the API asset URL with `Accept: application/octet-stream`, then follow the 302
/// to storage WITHOUT forwarding the Authorization header — the storage URL is pre-signed and 403s
/// if it sees one.
fn asset_response(asset: &Asset, token: Option<&str>) -> Result<ureq::Response> {
    let Some(t) = token else {
        return agent(5).get(&asset.browser_download_url).set("User-Agent", UA).call().map_err(net_err);
    };
    let resp = agent(0)
        .get(&asset.url)
        .set("User-Agent", UA)
        .set("Accept", "application/octet-stream")
        .set("Authorization", &format!("Bearer {t}"))
        .call();
    match resp {
        Ok(r) if (200..300).contains(&r.status()) => Ok(r),
        Ok(r) if (300..400).contains(&r.status()) => {
            let loc = r
                .header("Location")
                .context("redirect response without a Location header")?
                .to_string();
            // no auth on the storage hop
            agent(5).get(&loc).set("User-Agent", UA).call().map_err(net_err)
        }
        Ok(r) => bail!("asset download returned HTTP {}", r.status()),
        Err(e) => Err(net_err(e)),
    }
}

/// Download an asset into memory (small files, e.g. manifest.json).
pub fn download_asset(asset: &Asset, token: Option<&str>) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    asset_response(asset, token)?.into_reader().read_to_end(&mut buf)?;
    Ok(buf)
}

/// Stream an asset to `dest`, returning (bytes written, sha256). Never buffers the whole body.
pub fn download_asset_to(asset: &Asset, token: Option<&str>, dest: &Path) -> Result<(u64, String)> {
    use sha2::{Digest, Sha256};
    let mut reader = asset_response(asset, token)?.into_reader();
    let mut file = std::fs::File::create(dest)
        .with_context(|| format!("creating {}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        std::io::Write::write_all(&mut file, &buf[..n])?;
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((total, hex::encode(hasher.finalize())))
}

fn net_err(e: ureq::Error) -> anyhow::Error {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            let snippet: String = body.chars().take(200).collect();
            anyhow!("HTTP {code}: {snippet}")
        }
        ureq::Error::Transport(t) => anyhow!("transport error: {t}"),
    }
}
