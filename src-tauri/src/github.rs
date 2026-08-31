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
use crate::transport::{self, FetchError};

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

/// The GitHub-backed `Downloader`. Holds the optional auth token and its pooled HTTP agent.
///
/// The agent is stored, not built per request, because ureq keeps its CONNECTION POOL inside the
/// Agent: a fresh one each time forces a full DNS + TCP + TLS handshake for every file. Measured
/// against this repo's small assets that is 663 ms/file versus 159 ms/file pooled — a 4.2x
/// difference, and the base game is thousands of small files where the handshake *is* the
/// transfer time. Construct one Github and reuse it (install/warm already do).
pub struct Github {
    token: Option<String>,
    /// Built with ZERO auto-follow — every redirect, for every request (API calls, public asset
    /// URLs, and the private-asset path's own manual first hop), goes through `transport::fetch`
    /// instead. See that module's doc comment for why ureq's own `.redirects(N)` is not safe to
    /// hand a `Location` header from a response this process did not write.
    agent: ureq::Agent,
}

impl Github {
    pub fn new(token: Option<&str>) -> Self {
        Self { token: token.map(str::to_string), agent: agent() }
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

    /// Common headers for an API request, attaching `Authorization` only when `same_origin` — the
    /// bearer token this struct holds is minted for `api.github.com` and must not ride along to a
    /// redirect target that leaves it (a repo rename is the one real case the API might redirect
    /// on at all; storage redirects, which are exactly the case that must NOT keep it, go through
    /// `asset_response`'s own manual first hop instead and never call this).
    fn api_headers(&self, req: ureq::Request, same_origin: bool) -> ureq::Request {
        let req = req
            .set("User-Agent", UA)
            .set("Accept", "application/vnd.github+json")
            .set("X-GitHub-Api-Version", "2022-11-28");
        match (self.token.as_deref(), same_origin) {
            (Some(t), true) => req.set("Authorization", &format!("Bearer {t}")),
            _ => req,
        }
    }
}

fn agent() -> ureq::Agent {
    ureq::builder()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(IO_TIMEOUT)
        .timeout_write(IO_TIMEOUT)
        // 0, not a positive count: `transport::fetch` drives every hop itself — see its module doc
        // for why ureq's own auto-follow is not safe to hand a redirect to.
        .redirects(0)
        .max_idle_connections_per_host(POOL_PER_HOST)
        // https-only, and re-checked on EVERY hop `transport::fetch` issues, not just the URL we
        // start with (each hop is its own fresh request, so this same check runs again every
        // time): an asset's `url`/`browser_download_url` is response content (see
        // `downloader::Asset`), so a redirect to `http://`, `file://` or a `\\host\share` UNC path
        // must be refused before this process ever touches it — the last shape is the one that
        // matters most, since Windows treats it as an implicit SMB target and touching it alone
        // leaks this machine's NTLMv2 hash to whatever answers there.
        .https_only(true)
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
fn net_err_redacted(e: FetchError, what: &str) -> anyhow::Error {
    match e {
        FetchError::Http(ureq::Error::Status(code, _)) => anyhow::Error::new(NetKind::Status(code))
            .context(format!("HTTP {code} fetching {what} from storage")),
        FetchError::Http(ureq::Error::Transport(t)) => anyhow::Error::new(NetKind::Transport)
            .context(format!("transport error fetching {what} from storage: {}", t.kind())),
        FetchError::TooManyRedirects => anyhow::Error::new(NetKind::Transport)
            .context(format!("too many redirects fetching {what} from storage")),
        FetchError::BadRedirect(reason) => anyhow::Error::new(NetKind::Transport)
            .context(format!("{reason} fetching {what} from storage")),
    }
}

/// `net_err` for a request that went through `transport::fetch`: the hop-cap and bad-Location
/// cases never reach ureq at all, so they need their own `NetKind` root rather than being able to
/// reuse `net_err`.
fn net_err_fetch(e: FetchError) -> anyhow::Error {
    match e {
        FetchError::Http(inner) => net_err(inner),
        FetchError::TooManyRedirects => anyhow::Error::new(NetKind::Transport)
            .context(format!("too many redirects (max {})", transport::MAX_REDIRECTS)),
        FetchError::BadRedirect(reason) => anyhow::Error::new(NetKind::Transport).context(reason),
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
    let with_range = |req: ureq::Request| match &range {
        Some(v) => req.set("Range", v),
        None => req,
    };
    let Some(t) = gh.token.as_deref() else {
        return transport::fetch(&gh.agent, &asset.browser_download_url, |req, _same_origin| {
            with_range(req.set("User-Agent", UA))
        })
        .map_err(net_err_fetch);
    };
    // First hop, explicit and NOT run through `transport::fetch`: it is the only request that may
    // legitimately carry the bearer token, so it must not be treated as just another redirect
    // hop a generic `same_origin` policy could get wrong. `gh.agent` has zero auto-follow (see its
    // field doc), so this returns the raw 3xx itself rather than chasing it.
    let first = with_range(
        gh.agent
            .get(&asset.url)
            .set("User-Agent", UA)
            .set("Accept", "application/octet-stream")
            .set("Authorization", &format!("Bearer {t}")),
    )
    .call();
    match first {
        Ok(r) if (200..300).contains(&r.status()) => Ok(r),
        Ok(r) if (300..400).contains(&r.status()) => {
            let loc = r
                .header("Location")
                .context("redirect response without a Location header")?
                .to_string();
            // No auth from here on — the Range header rides along (S3 honors it), but
            // `transport::fetch`'s `configure` closure never attaches Authorization, whatever the
            // host does across any FURTHER redirect this hop might itself produce. The error is
            // re-contexted with the ASSET name: `loc` is a pre-signed URL whose query string is a
            // time-limited read capability, and a raw transport error would carry the URL it was
            // fetching — which would put that capability in the UI's detail line and in any log
            // the user pastes into a bug report.
            transport::fetch(&gh.agent, &loc, |req, _same_origin| with_range(req.set("User-Agent", UA)))
                .map_err(|e| net_err_redacted(e, &asset.name))
        }
        Ok(r) => bail!("asset download returned HTTP {}", r.status()),
        Err(e) => Err(net_err(e)),
    }
}

impl Downloader for Github {
    /// List releases, newest first (GitHub's order). One page, up to 100 — plenty for this project.
    fn fetch_releases(&self, repo: &str) -> Result<Vec<Release>> {
        let url = format!("https://api.github.com/repos/{repo}/releases?per_page=100");
        let resp = transport::fetch(&self.agent, &url, |req, same_origin| self.api_headers(req, same_origin))
            .map_err(net_err_fetch)?;
        resp.into_json().context("parsing the releases JSON")
    }

    /// Fetch a release by tag (or the latest). `token` is only required for private repos.
    fn fetch_release(&self, repo: &str, tag: Option<&str>) -> Result<Release> {
        let url = api_url(repo, tag);
        let resp = transport::fetch(&self.agent, &url, |req, same_origin| self.api_headers(req, same_origin))
            .map_err(net_err_fetch)?;
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
    use crate::test_http::{Canned, TestServer};
    use std::collections::HashMap;

    /// Same settings `agent()` builds, minus `https_only`: the redirect-chain and
    /// authorization-header tests below script a chain over a local plain-HTTP listener, which
    /// cannot speak TLS. `https_only` is proven separately, against the REAL `agent()` builder, by
    /// `github_agent_refuses_non_https_urls_cleanly` — the hop cap and the auth-header policy are
    /// independent checks from the scheme one, so testing them apart from it is not a gap.
    fn test_agent() -> ureq::Agent {
        ureq::builder()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout_read(IO_TIMEOUT)
            .timeout_write(IO_TIMEOUT)
            .redirects(0) // transport::fetch drives the loop; matches agent()'s real setting
            .build()
    }

    /// A URL that fails the https-only check must be refused as an ordinary error, never a panic
    /// — including the UNC shape (`\\host\share`), which Windows treats as an implicit SMB target
    /// and would leak this machine's NTLMv2 hash to whatever answers there the moment it is
    /// touched. `asset.url`/`asset.browser_download_url` are response content (`downloader::Asset`
    /// derives `Deserialize` with no scheme restriction of its own), so this is exactly the shape
    /// `asset_response` hands to `agent()` without further checking — the https-only flag on the
    /// agent IS the check. Goes through the real `agent()` builder, since that is what every
    /// request in this file uses.
    #[test]
    fn github_agent_refuses_non_https_urls_cleanly() {
        let a = agent();
        for url in [
            "http://example.com/asset",
            "file:///etc/passwd",
            "\\\\attacker\\share",
            "//attacker/share",
        ] {
            let result = a.get(url).call();
            assert!(result.is_err(), "expected {url} to be refused, got {result:?}");
        }
    }

    /// A redirect chain longer than `MAX_REDIRECTS` must fail cleanly rather than being followed
    /// forever — `transport::fetch`'s own hop count, proven over a genuine TCP round trip (see
    /// `test_http`'s doc comment for why a mock can't stand in for this).
    #[test]
    fn github_agent_refuses_a_redirect_chain_past_the_cap() {
        let server = TestServer::start(|_port| {
            let mut routes = HashMap::new();
            routes.insert("/loop", Canned::redirect("/loop"));
            routes
        });

        let url = format!("http://127.0.0.1:{}/loop", server.port);
        let err = transport::fetch(&test_agent(), &url, |req, _same_origin| req)
            .expect_err("must not follow forever");
        assert!(matches!(err, FetchError::TooManyRedirects), "expected TooManyRedirects, got {err:?}");
    }

    /// A redirect to a scheme with no host (`file:///nope`) must be refused cleanly, not crash the
    /// process — the exact bug `transport`'s module doc describes: ureq's own `.redirects(N)`
    /// auto-follow PANICS on this shape (`unit.rs::connect_inner`'s `host_str().unwrap()`), which
    /// is why `agent()` is built `.redirects(0)` and every hop instead goes through
    /// `transport::fetch`'s own loop, whose `agent.get(...).call()` per hop is guarded by ureq's
    /// `Request::parse_url()` (an empty host there is a clean `InvalidUrl`, not a panic — that
    /// guard is what fires here, before the request ever reaches the scheme-allowlist check
    /// `https_only` relies on for a well-formed http(s) URL). This test completing at all (Ok or
    /// Err) is half the proof.
    #[test]
    fn github_agent_refuses_a_bad_scheme_introduced_mid_chain() {
        let server = TestServer::start(|_port| {
            let mut routes = HashMap::new();
            routes.insert("/start", Canned::redirect("file:///nope"));
            routes
        });
        let url = format!("http://127.0.0.1:{}/start", server.port);
        let err = transport::fetch(&test_agent(), &url, |req, _same_origin| req)
            .expect_err("a redirect to a non-http(s) scheme must be refused, not followed");
        match err {
            FetchError::Http(e) => assert_eq!(e.kind(), ureq::ErrorKind::InvalidUrl),
            other => panic!("expected Http(InvalidUrl), got {other:?}"),
        }
    }

    /// The property item 3 exists for: a bearer token minted for this host must never be replayed
    /// to a redirect target on a DIFFERENT host, while a same-host redirect — the ordinary case,
    /// e.g. a repo rename — keeps working. Both hops hit the SAME physical listener; "127.0.0.1"
    /// and "localhost" are different HOST STRINGS naming it, which is all `api_headers`'s
    /// `same_origin` flag compares, so one server each proves both directions. Uses `api_headers`
    /// directly (not a bare closure) since that IS the policy under test — the same function
    /// `fetch_release`/`fetch_releases` call.
    #[test]
    fn authorization_survives_a_same_host_redirect_but_not_a_cross_host_one() {
        let gh = Github::new(Some("test-token"));
        // relative Location -> resolves against the SAME host:port the request was sent to.
        let same_host_server = TestServer::start(|_port| {
            let mut routes = HashMap::new();
            routes.insert("/same", Canned::redirect("/same-dest"));
            routes.insert("/same-dest", Canned::ok());
            routes
        });
        // absolute Location naming a DIFFERENT host string for this same listener's own port.
        let cross_host_server = TestServer::start(|port| {
            let mut routes = HashMap::new();
            routes.insert("/cross", Canned::redirect(format!("http://localhost:{port}/cross-dest")));
            routes.insert("/cross-dest", Canned::ok());
            routes
        });

        transport::fetch(
            &test_agent(),
            &format!("http://127.0.0.1:{}/same", same_host_server.port),
            |req, same_origin| gh.api_headers(req, same_origin),
        )
        .expect("same-host redirect should succeed");
        assert!(
            same_host_server.saw_authorization("/same-dest"),
            "a same-host redirect must keep the token"
        );

        transport::fetch(
            &test_agent(),
            &format!("http://127.0.0.1:{}/cross", cross_host_server.port),
            |req, same_origin| gh.api_headers(req, same_origin),
        )
        .expect("cross-host redirect should still succeed, just without the token");
        assert!(
            !cross_host_server.saw_authorization("/cross-dest"),
            "a cross-host redirect must NOT carry the token to the new host"
        );
    }
}
