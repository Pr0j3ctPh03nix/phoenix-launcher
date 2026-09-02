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
//! property that makes the hop cap below refuse a too-long chain as an ordinary `Result::Err`,
//! never a crash.
//!
//! THE SCHEME RULES LIVE HERE, in `check_hop`, and they are this module's other half: which
//! schemes a fetch may touch at all, and the ban on a chain walking back down from https to http.
//! They used to be ureq's `https_only` flag on each caller's agent, which can only say "https or
//! nothing" — and mirrors may now be published on plain HTTP (`config::normalize_mirror_url`), so
//! the flag could no longer express the mirror policy without also dropping the guard that keeps
//! `file://` and UNC targets out. GitHub's own agent keeps the flag anyway; see `github::agent`.

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
    /// A hop `check_hop` would not issue: a scheme outside the caller's allowlist, or a step back
    /// down from https to http. Its own variant rather than a `BadRedirect` reason, because the
    /// INITIAL url is checked by the same rule and calling that a bad redirect would be a lie in
    /// the one message a user sees.
    RefusedScheme(&'static str),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::Http(e) => write!(f, "{e}"),
            FetchError::TooManyRedirects => write!(f, "too many redirects (max {MAX_REDIRECTS})"),
            FetchError::BadRedirect(reason) => write!(f, "bad redirect: {reason}"),
            FetchError::RefusedScheme(reason) => write!(f, "refused: {reason}"),
        }
    }
}

/// The schemes a fetch may reach, decided by the CALLER: the two backends have different answers
/// and neither is a sensible default. GitHub is an https service and nothing else; a mirror is an
/// ordinary static file host whose operator may publish it on plain HTTP, which the published list
/// and `config::normalize_mirror_url` both already allow.
///
/// Whichever is chosen, `check_hop` applies it to EVERY hop and bans an https -> http step within
/// one chain — so this widens what a chain may START on, never what it may walk into.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Schemes {
    /// `https`, and nothing else.
    HttpsOnly,
    /// `http` or `https`.
    HttpOrHttps,
}

/// Fetch `url`, following redirects ourselves, over the schemes `schemes` allows.
///
/// `configure` builds the request for ONE hop, given the ureq request to start from and whether
/// this hop's URL is on the SAME ORIGIN (host AND scheme, an upgrade to `https` excepted) as the
/// ORIGINAL `url` — which is what lets a caller attach a header like `Authorization` only where it
/// should still travel (github.rs's API requests: kept across a same-origin redirect, dropped the
/// moment one leaves the origin it was minted for, and re-attached again if a later hop returns to
/// it — the comparison is always against the ORIGINAL request, never the previous hop). Checking
/// scheme too, not just host, matters here specifically because a fetch is no longer https-only by
/// construction (`Schemes::HttpOrHttps`): without it, `https://host/a` -> `Location: http://host/b`
/// would count as "same host" and reattach a token onto a now-unencrypted connection — a hop
/// `check_hop` now refuses outright, which makes this the second lock on the same door rather than
/// the only one. A caller with no host-sensitive header just ignores the flag (mirror.rs's
/// production code, which never attaches one at all).
pub fn fetch(
    agent: &ureq::Agent,
    url: &str,
    schemes: Schemes,
    configure: impl Fn(ureq::Request, bool) -> ureq::Request,
) -> Result<ureq::Response, FetchError> {
    let start = origin_of(url);
    let mut current = url.to_string();
    let mut chain_used_https = false;
    for _ in 0..=MAX_REDIRECTS {
        if check_hop(schemes, chain_used_https, &current)? {
            chain_used_https = true;
        }
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

/// The refusal for a scheme no caller allows here.
const NOT_ALLOWED: &str = "the URL's scheme is not one this fetch may use";
/// The refusal for a chain that tried to walk back down to plain HTTP.
const DOWNGRADE: &str = "a redirect chain that has been on https may not go back to http";

/// BOTH SCHEME RULES, applied to ONE hop — the initial URL and every redirect target alike,
/// because each hop is its own fresh top-level request and a rule checked once is a rule the next
/// `Location` header gets to step around. Answers whether THIS hop is on https, which is the bit
/// the caller carries forward.
///
/// 1. THE ALLOWLIST. The scheme has to be one `schemes` names; everything else is refused before
///    the request is issued. `file://` is the obvious one, but the shape that matters most is a
///    Windows UNC path (`\\host\share`, and the `//host/share` it normalizes to): Windows treats
///    it as an implicit SMB target, and merely touching it leaks this machine's NTLMv2 hash to
///    whatever answers there, before a byte of content is read. A URL that does not parse at all
///    is refused here too — it names no scheme that could be allowed.
///
/// 2. NO DOWNGRADE. Once a chain has been on https it may not go back to http, so `http -> https
///    -> http` is refused exactly like `https -> http`. The rule is about the CHAIN and not merely
///    the previous hop, because otherwise anyone able to rewrite a `Location` would only have to
///    bounce through one more URL to undo it — and what an https hop bought (a URL, and any header
///    `configure` attached, that a network attacker could not read or rewrite) is spent the moment
///    the next request derived from it goes out in clear.
fn check_hop(schemes: Schemes, chain_used_https: bool, url: &str) -> Result<bool, FetchError> {
    let secure = match url::Url::parse(url).ok().as_ref().map(url::Url::scheme) {
        Some("https") => true,
        Some("http") if schemes == Schemes::HttpOrHttps => false,
        _ => return Err(FetchError::RefusedScheme(NOT_ALLOWED)),
    };
    if !secure && chain_used_https {
        return Err(FetchError::RefusedScheme(DOWNGRADE));
    }
    Ok(secure)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole of the scheme policy, as a table over `check_hop` — which is where both rules
    /// live, so this is the direct test of them rather than an inference from a socket.
    ///
    /// It is also the ONLY place the chain rule can be proven whole. Two of its cases
    /// (`https -> http` and `http -> https -> http`) need a chain that has completed a hop over
    /// TLS, and no test in this crate can serve one: every real-socket test here talks to a
    /// loopback listener, which speaks plain HTTP and cannot speak TLS. The socket tests in
    /// mirror.rs and github.rs cover what a live chain can show — that the check runs again on a
    /// redirect target, and that an http chain may still be sent UP to https.
    #[test]
    fn the_allowlist_and_the_no_downgrade_rule() {
        use Schemes::{HttpOrHttps, HttpsOnly};
        // the second argument is "the chain has already been on https"; the answer is "this hop
        // is https", which is what `fetch` accumulates into that same bit.
        assert!(check_hop(HttpsOnly, false, "https://example.com/a").unwrap());
        assert!(check_hop(HttpOrHttps, false, "https://example.com/a").unwrap());
        assert!(!check_hop(HttpOrHttps, false, "http://example.com/a").unwrap());

        // GitHub's policy: http is refused wherever it appears, the first URL included.
        assert!(check_hop(HttpsOnly, false, "http://example.com/a").is_err());

        // NO DOWNGRADE — and it is the CHAIN that is remembered, not the previous hop, so
        // http -> https -> http is refused exactly like https -> http.
        let err = check_hop(HttpOrHttps, true, "http://example.com/a")
            .expect_err("a chain that has been on https may not go back to http");
        assert!(matches!(err, FetchError::RefusedScheme(DOWNGRADE)), "got {err:?}");
        assert!(
            check_hop(HttpOrHttps, true, "https://example.com/a").unwrap(),
            "…while staying on https is the ordinary case"
        );

        // Everything else is refused under BOTH policies, however the chain got here. `//host` and
        // the UNC form parse as no URL at all, which is a refusal for the same reason: nothing in
        // them names a scheme that could be allowed.
        let refused =
            ["file:///etc/passwd", "ftp://example.com/a", "\\\\attacker\\share", "//attacker/share", ""];
        for url in refused {
            for schemes in [HttpsOnly, HttpOrHttps] {
                for chain in [false, true] {
                    let got = check_hop(schemes, chain, url);
                    assert!(
                        matches!(got, Err(FetchError::RefusedScheme(NOT_ALLOWED))),
                        "{url:?} must be refused under {schemes:?}, got {got:?}"
                    );
                }
            }
        }
    }
}
