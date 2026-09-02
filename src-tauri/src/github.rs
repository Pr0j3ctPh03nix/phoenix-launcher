//! GitHub Releases backend for the `Downloader` trait. Works for public repos with no auth and
//! private repos with a token, over the same code path (the REST API), so the updater can pull
//! from either.
//!
//! All requests carry timeouts (connect + per-read/write) so a dead link errors out instead of
//! hanging the UI forever; big assets stream to disk via `download_to`. Failures root a typed
//! `NetKind` in the anyhow chain so the command layer can classify them for the UI.

use anyhow::{bail, Context, Result};
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::config::{Measured, Settings};
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

/// The GitHub-backed `Downloader`. Holds the optional auth token, the rule for when to send it,
/// and its pooled HTTP agent.
///
/// The agent is stored, not built per request, because ureq keeps its CONNECTION POOL inside the
/// Agent: a fresh one each time forces a full DNS + TCP + TLS handshake for every file. Measured
/// against this repo's small assets that is 663 ms/file versus 159 ms/file pooled — a 4.2x
/// difference, and the base game is thousands of small files where the handshake *is* the
/// transfer time. Construct one Github and reuse it (install/warm already do).
///
/// THE CREDENTIAL RULE LIVES HERE, and this is the only type that can hold a credential at all —
/// a mirror has no field for one (`mirror::Mirror`). It used to live in the command layer, spread
/// over a `Candidate` and a walk, where every caller that reached for a backend directly bypassed
/// it; keeping it in the backend means there is nowhere left to bypass it from.
pub struct Github {
    token: Option<String>,
    /// Send the credential on the FIRST request. True only for the private source repo: anonymous
    /// there is a round trip spent to be refused, on every check. Everything else is public and a
    /// repo-scoped PAT can be REFUSED where anonymous succeeds.
    lead_with_token: bool,
    /// Latched once a token attempt succeeded where anonymous was refused, so the asset download
    /// that follows takes the same authenticated path the release lookup did. Atomic, not `&mut`,
    /// because `Downloader` is `&self` and the download pool shares one instance.
    authed: AtomicBool,
    /// Built with ZERO auto-follow — every redirect, for every request (API calls, public asset
    /// URLs, and the private-asset path's own manual first hop), goes through `transport::fetch`
    /// instead. See that module's doc comment for why ureq's own `.redirects(N)` is not safe to
    /// hand a `Location` header from a response this process did not write.
    agent: ureq::Agent,
}

impl Github {
    /// The backend for `repo`, credential rule applied. THE one place that rule is decided.
    pub fn for_repo(settings: &Settings, repo: &str) -> Self {
        let token = settings.token();
        Self {
            lead_with_token: lead_with_token(token, repo, &settings.source_repo),
            ..Self::new(token)
        }
    }

    /// A backend with an EXPLICIT credential, and no rule: whatever it is handed, it leads with.
    /// For tests, and for a caller that has already decided (there is one place a token is chosen
    /// — `Settings::token` — so "decided" only ever means `for_repo`).
    pub fn new(token: Option<&str>) -> Self {
        Self {
            token: token.map(str::to_string),
            lead_with_token: token.is_some(),
            authed: AtomicBool::new(false),
            agent: agent(),
        }
    }

    /// The first `bytes` of an asset, for the source probe: whether the range was honoured (206)
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

    /// The credential to send WITHOUT asking first: the private source repo's, or one a refusal
    /// has already proved is needed here. `None` means ask anonymously.
    fn upfront(&self) -> Option<&str> {
        (self.lead_with_token || self.authed.load(Ordering::Relaxed))
            .then_some(self.token.as_deref())
            .flatten()
    }

    /// One API call, anonymously then with credentials if the server REFUSED.
    ///
    /// A private repo answers 404, indistinguishable from missing, so an HTTP refusal earns the
    /// retry and nothing else does: credentials can turn a 404 into a 200 but cannot fix DNS, and
    /// an offline launcher must not pay two connect timeouts. A retry that WORKS latches
    /// (`authed`), so the asset download that follows takes the path the release lookup proved
    /// rather than repeating the refusal per file.
    fn with_credentials<T>(&self, call: impl Fn(Option<&str>) -> Result<T>) -> Result<T> {
        if let Some(t) = self.upfront() {
            return call(Some(t));
        }
        let e = match call(None) {
            Ok(v) => return Ok(v),
            Err(e) => e,
        };
        let refused =
            e.chain().any(|c| matches!(c.downcast_ref::<NetKind>(), Some(NetKind::Status(_))));
        let (Some(t), true) = (self.token.as_deref(), refused) else { return Err(e) };
        let v = call(Some(t)).context("tried anonymously and with a token")?;
        self.authed.store(true, Ordering::Relaxed);
        Ok(v)
    }

    /// Common headers for an API request, attaching `Authorization` only when `same_origin` — the
    /// bearer token is minted for `api.github.com` and must not ride along to a redirect target
    /// that leaves it (a repo rename is the one real case the API might redirect on at all;
    /// storage redirects, which are exactly the case that must NOT keep it, go through
    /// `asset_response`'s own manual first hop instead and never call this).
    ///
    /// `token` is a parameter rather than a field read, because whether THIS attempt authenticates
    /// is `with_credentials`'s decision and changes between the two attempts of one call.
    fn api_headers(
        &self,
        req: ureq::Request,
        same_origin: bool,
        token: Option<&str>,
    ) -> ureq::Request {
        let req = req
            .set("User-Agent", UA)
            .set("Accept", "application/vnd.github+json")
            .set("X-GitHub-Api-Version", "2022-11-28");
        match (token, same_origin) {
            (Some(t), true) => req.set("Authorization", &format!("Bearer {t}")),
            _ => req,
        }
    }
}

/// Time GitHub, through the real download path — API release lookup, then a ranged asset read that
/// follows the authenticated redirect to storage.
///
/// Deliberately not unified with `mirror::probe`: GitHub IS a release index, and that index is the
/// right thing to time here, while a mirror has none at all. What the two share is the transfer
/// itself (`source::time_read`), because throughput is the one number the ranking sorts on and two
/// ways of measuring it would be two rankings.
pub fn probe(settings: &Settings, repo: &str, now: u64) -> Measured {
    let gh = Github::for_repo(settings, repo);
    let mut m = Measured::blank(now);

    let started = Instant::now();
    let release = match gh.fetch_release(repo, None) {
        Ok(r) => r,
        Err(e) => return Measured::failed(now, format!("release lookup: {}", net_reason(&e))),
    };
    m.latency_ms = Some(started.elapsed().as_millis() as u64);
    m.tag = Some(release.tag_name.clone());

    let Some(asset) = probe_asset(&release) else {
        m.error = Some("the release carries no asset to test".to_string());
        return m;
    };
    match gh.ranged_asset(asset, crate::source::PROBE_BYTES) {
        Ok((range_ok, reader)) => {
            m.range_ok = range_ok;
            crate::source::time_read(&mut m, reader, &asset.name);
        }
        Err(e) => m.error = Some(format!("{}: {e}", asset.name)),
    }
    m
}

/// The asset to time, out of a release index that is a real index: the BIGGEST one that is not
/// release metadata.
///
/// Size is what makes the measurement honest. A release carries hundreds of small loose game files,
/// and a throttled path serves a 2 KB file flawlessly — so picking arbitrarily would let exactly
/// the link the probe exists to catch report itself healthy. `manifest.json` is excluded for the
/// same reason: it is the one transfer such a path can always complete.
fn probe_asset(release: &Release) -> Option<&Asset> {
    let usable = |a: &&Asset| {
        a.name != crate::mirror::MIRRORS_ASSET
            && a.name != crate::engine::MANIFEST_ASSET
            && !a.name.ends_with(".sha256")
    };
    match release.assets.iter().filter(usable).max_by_key(|a| a.size) {
        // an index that omits sizes leaves nothing to choose on; any real asset beats none
        Some(a) if a.size > 0 => Some(a),
        _ => release.assets.iter().find(usable).or_else(|| release.assets.first()),
    }
}

/// A compact reason for a probe row. Without this the GitHub row would carry the API's whole JSON
/// error body — message, documentation URL and all — on a line sized for "HTTP 404".
fn net_reason(e: &anyhow::Error) -> String {
    e.chain()
        .find_map(|c| c.downcast_ref::<NetKind>())
        .map_or_else(|| "failed".to_string(), NetKind::to_string)
}

/// Should the very first request carry the credential, instead of earning it after a refusal?
///
/// Only for the SOURCE repo. That is the one the baked credential is scoped to and the one that is
/// private, so anonymous there is a round trip that exists only to be refused. Everything else is
/// public, and a repo-scoped credential can be REFUSED where anonymous access succeeds — so those
/// keep asking anonymously first. A free function so the rule is testable without a credential
/// baked into the test binary (`Settings::token` is `option_env!`, i.e. `None` in every build a
/// test runs in).
fn lead_with_token(token: Option<&str>, repo: &str, source_repo: &str) -> bool {
    token.is_some() && repo == source_repo
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
    // `upfront`, not the raw token: an asset download takes the path the RELEASE LOOKUP already
    // proved — the private repo's, or the one a refusal latched. Anonymous is not merely the
    // fallback here, it is the better route for a public repo (the tokenless
    // `browser_download_url` rides free CDN bandwidth and no API rate budget), so a backend that
    // simply had a token must not spend it on every file.
    let Some(t) = gh.upfront() else {
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
        self.with_credentials(|token| {
            let resp =
                transport::fetch(&self.agent, &url, |req, same_origin| {
                    self.api_headers(req, same_origin, token)
                })
                .map_err(net_err_fetch)?;
            resp.into_json().context("parsing the releases JSON")
        })
    }

    /// Fetch a release by tag (or the latest). A private repo answers 404 anonymously, so this
    /// goes through `with_credentials` rather than deciding for itself.
    fn fetch_release(&self, repo: &str, tag: Option<&str>) -> Result<Release> {
        let url = api_url(repo, tag);
        self.with_credentials(|token| {
            let resp =
                transport::fetch(&self.agent, &url, |req, same_origin| {
                    self.api_headers(req, same_origin, token)
                })
                .map_err(net_err_fetch)?;
            resp.into_json().context("parsing the release JSON")
        })
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
    ///
    /// The resume/hash/write half lives in `downloader::stream_to_file`, shared with the mirror
    /// backend — the only thing that differs between them is the request this closure issues.
    fn download_to(&self, asset: &Asset, dest: &Path, resume_from: u64, progress: ChunkProgress) -> Result<(u64, String)> {
        crate::downloader::stream_to_file(
            dest,
            resume_from,
            |prefix| {
                let range = prefix.map(|p| format!("bytes={p}-"));
                Ok(transport::body_of(asset_response(self, asset, range)?))
            },
            progress,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{Canned, TestServer};
    use std::collections::HashMap;
    use std::io::Write;
    use std::net::TcpListener;
    use std::time::Duration;

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
            |req, same_origin| gh.api_headers(req, same_origin, gh.upfront()),
        )
        .expect("same-host redirect should succeed");
        assert!(
            same_host_server.saw_authorization("/same-dest"),
            "a same-host redirect must keep the token"
        );

        transport::fetch(
            &test_agent(),
            &format!("http://127.0.0.1:{}/cross", cross_host_server.port),
            |req, same_origin| gh.api_headers(req, same_origin, gh.upfront()),
        )
        .expect("cross-host redirect should still succeed, just without the token");
        assert!(
            !cross_host_server.saw_authorization("/cross-dest"),
            "a cross-host redirect must NOT carry the token to the new host"
        );
    }

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

        // `test_agent()`, not `Github::new(None)`: the real `agent()` is https-only and this
        // server is a plain-HTTP loopback listener, which cannot speak TLS. The cap under test is
        // a read-length bound, independent of the scheme check that refuses `http://` in
        // production (proven separately by `github_agent_refuses_non_https_urls_cleanly`).
        let gh = Github { agent: test_agent(), ..Github::new(None) };
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

    // ---- the credential rule, which now lives in this file ----

    /// A private repo answers 404, indistinguishable from missing, so an HTTP REFUSAL earns a
    /// second try with a token — and nothing else does, because credentials can turn a 404 into a
    /// 200 but cannot fix DNS, and an offline launcher must not pay two connect timeouts.
    ///
    /// Over `with_credentials` itself rather than a socket: the rule is about WHICH attempts are
    /// made, and the transport it would be made over has nothing to say about that.
    #[test]
    fn a_refusal_earns_the_credential_retry_and_being_offline_does_not() {
        let gh = Github { lead_with_token: false, ..Github::new(Some("t")) };
        let seen = std::sync::Mutex::new(Vec::<Option<String>>::new());
        let answer = |kind: NetKind| {
            move |token: Option<&str>| -> Result<&'static str> {
                match token {
                    Some(_) => Ok("served"),
                    None => Err(anyhow::Error::new(kind).context("scripted refusal")),
                }
            }
        };

        let refused = answer(NetKind::Status(404));
        let got = gh.with_credentials(|t| {
            seen.lock().unwrap().push(t.map(str::to_string));
            refused(t)
        });
        assert_eq!(got.unwrap(), "served");
        assert_eq!(
            *seen.lock().unwrap(),
            [None, Some("t".to_string())],
            "anonymously first, then with the credential"
        );

        let gh = Github { lead_with_token: false, ..Github::new(Some("t")) };
        let seen = std::sync::Mutex::new(Vec::<Option<String>>::new());
        let dark = answer(NetKind::Transport);
        let got = gh.with_credentials(|t| {
            seen.lock().unwrap().push(t.map(str::to_string));
            dark(t)
        });
        assert!(got.is_err(), "an unreachable host is not a credentials problem");
        assert_eq!(*seen.lock().unwrap(), [None], "…so it is asked exactly once");
    }

    /// The private source repo cannot answer anonymously, so asking it that way first is a round
    /// trip spent to be refused on every check. It gets the credential immediately — and nothing
    /// else does, because the launcher and game repos are public and a repo-scoped credential can
    /// be refused where anonymous access works.
    #[test]
    fn only_the_source_repo_leads_with_the_credential() {
        let (tok, src) = (Some("t"), "Pr0j3ctPh03nix/client-dist-staging");
        assert!(lead_with_token(tok, src, src), "the private source repo leads with the token");
        assert!(
            !lead_with_token(tok, "Pr0j3ctPh03nix/phoenix-launcher", src),
            "a public repo must still be asked anonymously first"
        );
        assert!(
            !lead_with_token(None, src, src),
            "with no credential there is nothing to lead with"
        );
    }

    /// Once a refusal has been answered by the credential, the ASSET download that follows takes
    /// the same path — `asset_response` reads `upfront()`, so a private release whose lookup only
    /// worked authenticated does not go on to fetch its files anonymously and 404 every one of
    /// them. The latch is per-backend, which is exactly the lifetime of one operation.
    #[test]
    fn the_credential_latches_for_the_asset_download_that_follows() {
        let gh = Github { lead_with_token: false, ..Github::new(Some("t")) };
        assert_eq!(gh.upfront(), None, "nothing has been proved yet");
        gh.with_credentials(|token| match token {
            Some(_) => Ok(()),
            None => Err(anyhow::Error::new(NetKind::Status(404)).context("private")),
        })
        .expect("the credential retry serves it");
        assert_eq!(gh.upfront(), Some("t"), "and the download that follows inherits it");

        // a backend that never had to authenticate stays anonymous: on a public repo the
        // tokenless browser_download_url is the better route, not merely the fallback
        let gh = Github { lead_with_token: false, ..Github::new(Some("t")) };
        gh.with_credentials(|_| Ok(())).unwrap();
        assert_eq!(gh.upfront(), None);
    }
}
