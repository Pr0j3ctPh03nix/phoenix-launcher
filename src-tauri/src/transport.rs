//! A redirect-following HTTP fetch that never hands ureq's own auto-follow an un-vetted
//! `Location` header to chase on its own — used by mirror.rs (every mirror-sourced URL) and
//! github.rs (asset redirects, and the API's own auth-bearing requests).
//!
//! Why this exists, proven empirically rather than assumed from reading ureq's source: a redirect
//! target with a scheme but no host — `Location: file:///nope` — makes ureq 2.12.1 **panic**.
//! `Request::parse_url()` guards exactly this case for a URL a caller hands it directly (an empty
//! host becomes a clean `Err(ErrorKind::InvalidUrl)`), but `Agent::redirects(N)`'s own internal follow loop
//! builds the next hop with `Url::join` and never runs that guard — so `connect_inner`'s
//! `host_str().unwrap()` fires on whatever a PEER sent in `Location`. Every URL reached through
//! here is attacker-controlled the moment its source is hostile or compromised (a mirror's own
//! index or any asset URL it names — see mirror.rs's module doc; a GitHub redirect target, in
//! principle, for anyone that can intercept `api.github.com`), so that auto-follow path is not
//! safe to build on. Every agent used here is therefore built `.redirects(0)`, and this module
//! drives the chain itself: each hop is issued as its OWN fresh top-level request, which is what
//! routes it back through `parse_url`'s safe check on every hop, not just the first — the same
//! property that also makes `https_only` (set on both callers' agents) and the hop cap below
//! refuse a bad scheme or a too-long chain as an ordinary `Result::Err`, never a crash.

use std::fmt;

/// Redirect hops a fetch will follow before refusing outright. Everything reached through here —
/// a mirror's index and every asset URL it names, or a GitHub redirect to storage — is content a
/// peer controls, so an unbounded (or merely generous) follow count is a resource-exhaustion knob
/// at best. Five is headroom for a real CDN/load-balancer hop, not an invitation to walk further.
pub const MAX_REDIRECTS: u32 = 5;

/// Either the HTTP exchange itself failed (ureq's own `Status`/`Transport` split), or this
/// module's own redirect bookkeeping refused to continue. Kept separate from `ureq::Error`
/// because the latter has no variant for "we chose not to" — collapsing them would either lose
/// that distinction or force a fake status/transport error to carry it.
#[derive(Debug)]
pub enum FetchError {
    Http(ureq::Error),
    TooManyRedirects,
    /// A 3xx with no `Location` header, or a `Location` that could not be resolved against the
    /// current URL. Carries a short reason, never the URL itself — the caller decides how much of
    /// that to surface (see github.rs's `net_err_redacted`).
    BadRedirect(&'static str),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::Http(e) => write!(f, "{e}"),
            FetchError::TooManyRedirects => write!(f, "too many redirects (max {MAX_REDIRECTS})"),
            FetchError::BadRedirect(reason) => write!(f, "bad redirect: {reason}"),
        }
    }
}

/// Fetch `url`, following redirects ourselves.
///
/// `configure` builds the request for ONE hop, given the ureq request to start from and whether
/// this hop's URL is on the SAME ORIGIN (host AND scheme, an upgrade to `https` excepted) as the
/// ORIGINAL `url` — which is what lets a caller attach a header like `Authorization` only where it
/// should still travel (github.rs's API requests: kept across a same-origin redirect, dropped the
/// moment one leaves the origin it was minted for, and re-attached again if a later hop returns to
/// it — the comparison is always against the ORIGINAL request, never the previous hop). Checking
/// scheme too, not just host, matters here specifically because `same_host` is meaningful even
/// when this fetch is not itself https-only (mirror.rs's tests build a plain-HTTP agent): without
/// it, `https://host/a` -> `Location: http://host/b` would count as "same host" and reattach a
/// token onto a now-unencrypted connection. A caller with no host-sensitive header just ignores
/// the flag (mirror.rs's production code, which never attaches one at all).
pub fn fetch(
    agent: &ureq::Agent,
    url: &str,
    configure: impl Fn(ureq::Request, bool) -> ureq::Request,
) -> Result<ureq::Response, FetchError> {
    let start = origin_of(url);
    let mut current = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        let same_origin = is_same_or_more_secure_origin(&start, &current);
        let resp = configure(agent.get(&current), same_origin).call().map_err(FetchError::Http)?;
        if !(300..400).contains(&resp.status()) {
            return Ok(resp);
        }
        let loc = resp.header("Location").ok_or(FetchError::BadRedirect("no Location header"))?;
        current = resolve(&current, loc).ok_or(FetchError::BadRedirect("unresolvable redirect target"))?;
    }
    Err(FetchError::TooManyRedirects)
}

/// A response, as `downloader::stream_to_file` needs to see it. The ONE place `Content-Range` is
/// parsed: every backend that resumes a transfer reads the same header the same way, and this
/// module is where ureq stops — `downloader.rs` is the engine's HTTP-free seam and must not see a
/// `ureq::Response`.
///
/// `range_start` is filled only for a 206. A 200 is the peer DECLINING the range, and a 206 whose
/// `Content-Range` cannot be read is unverifiable, which is not the same as correct — both land as
/// `None`, and the caller restarts from zero.
pub fn body_of(resp: ureq::Response) -> crate::downloader::Body {
    let range_start = (resp.status() == 206)
        .then(|| {
            // "bytes 1234-5678/9999" — the offset is the first number after the unit
            resp.header("Content-Range")
                .and_then(|v| v.split_whitespace().nth(1))
                .and_then(|v| v.split('-').next())
                .and_then(|v| v.parse::<u64>().ok())
        })
        .flatten();
    let content_length = resp.header("Content-Length").and_then(|v| v.parse::<u64>().ok());
    crate::downloader::Body { range_start, content_length, reader: resp.into_reader() }
}

/// (scheme, host) — an origin is what deciding whether to keep a header like `Authorization` has
/// to compare, not just the host: `https://api.github.com` -> `http://api.github.com` is a
/// same-HOST redirect that downgraded transport security, and a token must not ride along onto a
/// plain connection just because the hostname matched.
fn origin_of(url: &str) -> Option<(String, String)> {
    let u = url::Url::parse(url).ok()?;
    Some((u.scheme().to_string(), u.host_str()?.to_string()))
}

/// Same host as `start`, and no less secure — same scheme, or an upgrade from `http` to `https`.
/// Mirrors ureq's own `RedirectAuthHeaders::SameHost` rule for the same reason: whatever replaced
/// it here should be at least as strict, not a regression dressed up as a rewrite.
fn is_same_or_more_secure_origin(start: &Option<(String, String)>, current_url: &str) -> bool {
    let (Some((s0, h0)), Some((s1, h1))) = (start, origin_of(current_url)) else {
        return false;
    };
    *h0 == h1 && (*s0 == s1 || (s0 != "https" && s1 == "https"))
}

/// Resolve a `Location` header against the URL it was received in reply to (it may be relative).
fn resolve(base: &str, location: &str) -> Option<String> {
    let base = url::Url::parse(base).ok()?;
    Some(base.join(location).ok()?.to_string())
}
