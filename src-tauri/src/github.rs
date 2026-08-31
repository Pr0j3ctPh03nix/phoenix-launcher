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
/// Idle connections the pool keeps PER HOST. ureq's default is 1 — with a multi-worker download
/// pool hitting one host, every moment two workers were between files the pool closed one
/// connection and the next file paid a full DNS+TCP+TLS handshake again: exactly the
/// 663-vs-159 ms/file cost the pooled-agent design (below) exists to avoid, resurfacing across
/// thousands of base-game files. Sized past the biggest pool (install.rs DL_WORKERS = 8) plus
/// slack for a straggling background warm and the notes fetch — idle sockets cost nothing.
const POOL_PER_HOST: usize = 12;

/// The GitHub-backed `Downloader`. Holds the optional auth token and its pooled HTTP agents.
///
/// The agents are stored, not built per request, because ureq keeps its CONNECTION POOL inside
/// the Agent: a fresh one each time forces a full DNS + TCP + TLS handshake for every file.
/// Measured against this repo's small assets that is 663 ms/file versus 159 ms/file pooled — a
/// 4.2x difference, and the base game is thousands of small files where the handshake *is* the
/// transfer time. Construct one Github and reuse it (install/warm already do).
pub struct Github {
    token: Option<String>,
    /// Follows redirects — API calls and public asset URLs.
    agent: ureq::Agent,
    /// Never follows redirects: the private-asset path must see the 302 itself and re-issue
    /// WITHOUT the auth header, since storage URLs are pre-signed and 403 if they see one.
    no_redirect: ureq::Agent,
}

impl Github {
    pub fn new(token: Option<&str>) -> Self {
        Self {
            token: token.map(str::to_string),
            agent: agent(5),
            no_redirect: agent(0),
        }
    }

    /// The first `bytes` of an asset, for the mirror probe: whether the range was honoured (206)
    /// and the body reader.
    ///
    /// It goes through `asset_response` rather than fetching the URL directly so it takes the
    /// exact path a real download takes — including the authenticated API request and the hop to
    /// pre-signed storage. That hop is a different host from `api.github.com` and is where a
    /// throttled or filtered link actually bites, so a probe that skipped it would measure a route
    /// no download ever uses.
    pub fn ranged_asset(
        &self,
        asset: &Asset,
        bytes: u64,
    ) -> Result<(bool, Box<dyn Read + Send + Sync + 'static>)> {
        let resp = asset_response(self, asset, Some(format!("bytes=0-{}", bytes.saturating_sub(1))))?;
        Ok((resp.status() == 206, resp.into_reader()))
    }
}

fn agent(redirects: u32) -> ureq::Agent {
    ureq::builder()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(IO_TIMEOUT)
        .timeout_write(IO_TIMEOUT)
        .redirects(redirects)
        .max_idle_connections_per_host(POOL_PER_HOST)
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
            let trimmed = body.trim_start();
            // The snippet exists for the API's JSON errors ({"message": "Not Found"}), which are
            // worth showing. The CDN's error PAGES are HTML — 200 chars of doctype and meta tags
            // in the status line help nobody. The root NetKind already says "HTTP 500", so an
            // HTML body adds no context at all.
            if trimmed.starts_with('<') {
                return anyhow::Error::new(NetKind::Status(code));
            }
            let snippet: String = trimmed.chars().take(200).collect();
            anyhow::Error::new(NetKind::Status(code)).context(format!("HTTP {code}: {snippet}"))
        }
        ureq::Error::Transport(t) => {
            anyhow::Error::new(NetKind::Transport).context(format!("transport error: {t}"))
        }
    }
}

/// `net_err` for the pre-signed storage hop, where the URL must NOT reach the message.
///
/// That URL's query string is a time-limited read capability for the asset, and ureq's transport
/// errors include the URL they were fetching — which would put it in the UI's mono detail line and
/// in any log a user pastes into a bug report. The error kind is preserved (that is what the shell
/// classifies on); only the URL, and the response body, are dropped.
fn net_err_redacted(e: ureq::Error, what: &str) -> anyhow::Error {
    match e {
        ureq::Error::Status(code, _) => anyhow::Error::new(NetKind::Status(code))
            .context(format!("HTTP {code} fetching {what} from storage")),
        ureq::Error::Transport(t) => anyhow::Error::new(NetKind::Transport)
            .context(format!("transport error fetching {what} from storage: {}", t.kind())),
    }
}

/// Start an asset request and return the response whose body is the asset bytes.
///
/// Public (no token): the direct `browser_download_url`.
/// Private (token): the API asset URL with `Accept: application/octet-stream`, then follow the 302
/// to storage WITHOUT forwarding the Authorization header — the storage URL is pre-signed and 403s
/// if it sees one.
/// `range` is a ready `Range` header value (`bytes=N-`, `bytes=0-N`) or None for the whole asset.
/// With one set the answer is 206, or 200 if the server declined it (a resuming caller then
/// restarts from zero).
fn asset_response(gh: &Github, asset: &Asset, range: Option<String>) -> Result<ureq::Response> {
    let range = |req: ureq::Request| match &range {
        Some(v) => req.set("Range", v),
        None => req,
    };
    let Some(t) = gh.token.as_deref() else {
        return range(gh.agent.get(&asset.browser_download_url).set("User-Agent", UA)).call().map_err(net_err);
    };
    let resp = range(
        gh.no_redirect
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
            // no auth on the storage hop — but the Range header rides along (S3 honors it).
            // The error is re-contexted with the ASSET name: `loc` is a pre-signed URL whose
            // query string is a time-limited read capability, and ureq's transport errors carry
            // the URL they were fetching — which would put that capability in the UI's detail
            // line and in any log the user pastes into a bug report.
            range(gh.agent.get(&loc).set("User-Agent", UA))
                .call()
                .map_err(|e| net_err_redacted(e, &asset.name))
        }
        Ok(r) => bail!("asset download returned HTTP {}", r.status()),
        Err(e) => Err(net_err(e)),
    }
}

impl Downloader for Github {
    /// List releases, newest first (GitHub's order). One page, up to 100 — plenty for this project.
    fn fetch_releases(&self, repo: &str) -> Result<Vec<Release>> {
        let mut req = self
            .agent
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
        let mut req = self.agent.get(&api_url(repo, tag))
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
        asset_response(self, asset, None)?.into_reader().read_to_end(&mut buf)?;
        Ok(buf)
    }

    /// `download` with a hard ceiling, for bytes whose size is a trust input.
    ///
    /// `take(max + 1)` rather than a Content-Length check: the length header is the peer's claim,
    /// and a host that intends to exhaust this process's memory is not going to declare it. The
    /// extra byte is what distinguishes "exactly at the limit" from "there was more coming".
    fn download_limited(&self, asset: &Asset, max: u64) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        asset_response(self, asset, None)?.into_reader().take(max + 1).read_to_end(&mut buf)?;
        if buf.len() as u64 > max {
            bail!("{} is larger than the {max} bytes allowed for it", asset.name);
        }
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
        let resp = asset_response(self, asset, (prefix > 0).then(|| format!("bytes={prefix}-")))?;
        // A 206 is only usable if it is the range we ASKED for. The status alone says "partial",
        // not "partial from `prefix`" — a peer answering 206 with a different offset would have
        // its bytes appended at `prefix`, producing a plausibly-sized file that fails only at the
        // final hash, and burning the one clean restart a resume gets. Absent Content-Range is
        // treated as a decline for the same reason: unverifiable is not the same as correct.
        let range_ok = prefix == 0
            || (resp.status() == 206
                && resp
                    .header("Content-Range")
                    .and_then(|v| v.split_whitespace().nth(1))
                    .and_then(|v| v.split('-').next())
                    .and_then(|v| v.parse::<u64>().ok())
                    == Some(prefix));
        if !range_ok {
            prefix = 0; // server declined the Range, or answered a different one — restart at zero
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
                bail!("download aborted");
            }
        }
        Ok((written, hex::encode(hasher.finalize())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::time::Duration;

    /// `download_limited` has to cut a transfer off itself, against the REAL transport — not by
    /// trusting `Content-Length` (a peer's own claim) and not by ever needing to see the end of
    /// the body. Proven with a raw HTTP/1.1 server that streams several times the cap with NO
    /// `Content-Length` at all — the shape an endless, hostile response takes — so the read has to
    /// stop within `max + 1` bytes on its own regardless of how much more the peer has queued.
    #[test]
    fn download_limited_stops_reading_a_host_that_sends_past_the_cap() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback listener");
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else { return };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            // drain the request up to the blank line — its contents don't matter here
            let mut buf = [0u8; 4096];
            let mut req = Vec::new();
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                match Read::read(&mut stream, &mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => req.extend_from_slice(&buf[..n]),
                }
            }
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n");
            let chunk = vec![b'A'; 64 * 1024];
            // several times the test's cap below; a client that reads only `max + 1` bytes and
            // stops must not need any of this to finish sending
            for _ in 0..64 {
                if stream.write_all(&chunk).is_err() {
                    return; // the client stopped reading and closed its end — the success case
                }
            }
        });

        let gh = Github::new(None);
        let asset = Asset {
            name: "endless".into(),
            url: String::new(),
            browser_download_url: format!("http://{addr}/endless"),
            size: 0,
        };
        let cap = 256 * 1024; // well under the ~4 MiB the server offers
        let err = gh.download_limited(&asset, cap).expect_err("an endless body must be refused");
        assert!(
            err.to_string().contains("larger than"),
            "expected the size-cap refusal, got: {err}"
        );
    }
}
