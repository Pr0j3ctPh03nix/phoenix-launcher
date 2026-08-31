//! Download sources: discovering mirrors, and measuring whether one actually *works*.
//!
//! A mirror is a base URL serving a release index at `<url>/releases.json` — GitHub's `/releases`
//! JSON shape — with each release's assets beside it. Mirrors are never authored by the user: they
//! are published in a `mirrors.json` and refreshed by `sweep`.
//!
//! The measurement is deliberately more than a reachability check, because the failure this exists
//! to catch is not an unreachable host. It is a network path that completes a handshake, serves a
//! few KB of JSON perfectly well, and then throttles or stalls the bulk transfer — a source that
//! passes every ping and cannot deliver a file. So every probe ends by pulling a chunk of a REAL
//! asset and timing it, and the number that matters is throughput, not latency.

use std::io::Read;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::config::{normalize_mirror_url, Settings, Source};
use crate::downloader::{Asset, Downloader, NetKind, Release};
use crate::github::Github;
use crate::transport::{self, FetchError};

const UA: &str = concat!("phoenix-launcher/", env!("CARGO_PKG_VERSION"));

/// The published mirror list, as a release asset and at every mirror's root.
pub const MIRRORS_ASSET: &str = "mirrors.json";

/// Whether a mirror can serve an INSTALL yet. False until a `Downloader` implementation for
/// mirrors exists — today the only one is `Github` (`github.rs`), so ranking mirrors by throughput
/// orders a list that no download consults. Flip this in the same change that adds that impl; it
/// exists so the boot-time sweep's cost (a 512 KiB transfer and up to `PROBE_BUDGET` per source,
/// every launch) is not paid for a result nothing can act on. A user-initiated sweep still
/// measures, because there the cost is asked for.
pub const MIRROR_DOWNLOADS_ENABLED: bool = false;

/// How much of a real asset to pull. Comfortably past the ~16 KiB that throttling middleboxes have
/// been observed to let through before choking a connection, so a path that only *looks* alive is
/// measured as slow rather than reported as healthy.
const PROBE_BYTES: u64 = 512 * 1024;

/// Wall-clock cap on the asset read.
///
/// A per-read timeout does not catch a trickle: a path that delivers a few bytes every second
/// keeps resetting the socket's read deadline and would hold the probe open for as long as it
/// cares to. The loop is therefore bounded by total elapsed time, and running out mid-chunk counts
/// as a failure — not a low score — because a source that cannot finish 512 KiB cannot finish a
/// release.
const PROBE_BUDGET: Duration = Duration::from_secs(8);

/// Shorter than the download path's timeouts: a probe's job is to fail fast and be re-run, not to
/// wait out a bad link. One blocking read may still overshoot `PROBE_BUDGET` by this much.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// One source's measurement. Every field is independently meaningful, so a partial result is still
/// worth showing: a source can be reachable, current, and far too slow to use.
#[derive(Debug, Clone)]
pub struct Probe {
    /// None for the primary — it has no URL, and the UI keys on `primary` instead.
    pub url: Option<String>,
    pub primary: bool,
    /// Milliseconds to fetch and parse the index. All a plain reachability check would have said.
    pub latency_ms: Option<u64>,
    /// Bytes per second over a real asset chunk. The number worth sorting on.
    pub bytes_per_sec: Option<u64>,
    /// Newest published tag the source advertises. A source that is fast, healthy and three
    /// releases behind is visible only here.
    pub tag: Option<String>,
    /// Did the asset request answer 206? Resume across a dropped connection depends on it, which
    /// on the links this feature exists for is the difference between a multi-GiB download
    /// finishing and never finishing.
    pub range_ok: bool,
    /// None when the source served everything asked of it; a short reason otherwise.
    pub error: Option<String>,
}

impl Probe {
    fn blank(source: &Source) -> Self {
        Self {
            url: source.url().map(str::to_string),
            primary: source.is_primary(),
            latency_ms: None,
            bytes_per_sec: None,
            tag: None,
            range_ok: false,
            error: None,
        }
    }

    fn failed(source: &Source, why: impl Into<String>) -> Self {
        Self { error: Some(why.into()), ..Self::blank(source) }
    }

    /// Did it deliver the whole requested chunk, within budget, without faulting? The sort's
    /// primary key, and what the list paints green. Deliberately strict: "answered" is not health.
    pub fn healthy(&self) -> bool {
        self.error.is_none() && self.bytes_per_sec.is_some()
    }

    /// Sort key: usable sources first, then fastest, latency only breaking ties between two that
    /// both deliver. Never latency-first — a latency figure is exactly what a throttled path
    /// still passes.
    fn rank(&self) -> (bool, std::cmp::Reverse<u64>, u64) {
        (
            !self.healthy(),
            std::cmp::Reverse(self.bytes_per_sec.unwrap_or(0)),
            self.latency_ms.unwrap_or(u64::MAX),
        )
    }
}

/// The result of one sweep: the source list as it now stands, and what each source measured.
///
/// `sources` and `probes` are PARALLEL — index `i` of each describes the same source, after the
/// sort. Callers may zip them. `probes` is empty when the sweep only refreshed the list.
pub struct Sweep {
    pub sources: Vec<Source>,
    pub probes: Vec<Probe>,
    /// Why the published list could not be read, if it could not. Never fatal — the sweep runs on
    /// whatever list we already had.
    pub refresh_error: Option<String>,
}

/// Is there a published mirror nobody has ever timed? The ONLY thing that triggers an automatic
/// measurement — there is no schedule, because re-timing costs a real download per source and
/// re-ordering the list unprompted is what would move a user off the source they chose.
///
/// The primary is not counted: with no mirrors there is nothing to rank it against, and measuring
/// it alone would be a download that answers no question.
pub fn has_new_mirror(sources: &[Source]) -> bool {
    sources.iter().any(|s| matches!(s, Source::Mirror { measured: false, .. }))
}

/// Refresh the published mirror list. Cheap — one small document — so this runs on every launch.
///
/// Returns the list as it should now be, plus why it could not be updated, if it could not.
pub fn refresh(settings: &Settings) -> (Vec<Source>, Option<String>) {
    match fetch_published_mirrors(settings) {
        // A published list REPLACES the mirrors — including an empty one, which is the publisher
        // saying there are none. Flags survive by URL; the primary is not in the document and so
        // is untouched by construction.
        Ok(Some(urls)) => (rebuild(&settings.sources, &urls), None),
        // No document published at all is not an error and not an empty list: it is silence. The
        // existing mirrors stay, because "could not ask" must never read as "there are none".
        Ok(None) => (settings.sources.clone(), None),
        Err(e) => (settings.sources.clone(), Some(format!("{e:#}"))),
    }
}

/// Time every source and order them fastest-first, marking each mirror measured.
pub fn measure(sources: Vec<Source>, repo: &str, token: Option<&str>) -> (Vec<Source>, Vec<Probe>) {
    let probes = probe_all(&sources, repo, token);
    // Kept zipped through the sort: `sources[i]` and `probes[i]` describe the same source, and
    // reordering one without the other would silently mislabel every measurement.
    let mut paired: Vec<(Source, Probe)> = sources.into_iter().zip(probes).collect();
    // Unconditional. The head of the list is what gets used when nothing is pinned, so it should
    // be the fastest that works — there is no reading of "I would like the slow one first".
    paired.sort_by(|(_, a), (_, b)| a.rank().cmp(&b.rank()));
    let (mut sources, probes): (Vec<Source>, Vec<Probe>) = paired.into_iter().unzip();
    for s in &mut sources {
        if let Source::Mirror { measured, .. } = s {
            *measured = true;
        }
    }
    (sources, probes)
}

/// Refresh, then measure — the test button's whole job.
///
/// Pure: it reads settings and returns what they should become, so the caller decides whether to
/// persist (the GUI does; the CLI only on `--save`).
pub fn sweep(settings: &Settings, do_measure: bool) -> Sweep {
    let (sources, refresh_error) = refresh(settings);
    if !do_measure {
        return Sweep { sources, probes: Vec::new(), refresh_error };
    }
    let (sources, probes) = measure(sources, &settings.source_repo, settings.token());
    Sweep { sources, probes, refresh_error }
}

/// Did the measurement find anything usable at all?
///
/// The gate on replacing a user's pin automatically. If EVERY source is down — including the main
/// one, which is an ordinary outage on the networks this feature exists for — then dropping the
/// pin buys nothing: the fallback is dead too. Keeping it means the choice is still there when
/// the outage clears, instead of having been quietly spent on a moment when nothing worked.
pub fn any_healthy(probes: &[Probe]) -> bool {
    probes.iter().any(Probe::healthy)
}

/// Merge the published mirror list into the existing one, PRESERVING ORDER.
///
/// Order is a measurement result, not a property of the document: a plain list refresh happens on
/// every launch and must not throw away the speed ranking that decides which source is used. So
/// known entries keep their positions, their switches AND their measured flag, mirrors the
/// document has dropped are removed, and newly published ones land at the end — unmeasured, which
/// is precisely the signal that an automatic measurement is due.
///
/// The primary is preserved from `existing` wherever it sits, and re-inserted if somehow absent.
/// It is never drawn from the document, which is what makes it unremovable by one.
fn rebuild(existing: &[Source], urls: &[String]) -> Vec<Source> {
    let published: Vec<String> = {
        let mut seen = std::collections::HashSet::new();
        urls.iter().filter_map(|u| normalize_mirror_url(u)).filter(|u| seen.insert(u.clone())).collect()
    };

    let mut out: Vec<Source> = existing
        .iter()
        .filter(|s| s.is_primary() || s.url().is_some_and(|u| published.iter().any(|p| p == u)))
        .cloned()
        .collect();
    if !out.iter().any(Source::is_primary) {
        out.insert(0, Source::Primary);
    }
    for url in published {
        if !out.iter().any(|s| s.url() == Some(url.as_str())) {
            out.push(Source::Mirror { url, enabled: true, measured: false });
        }
    }
    out
}

/// The published mirror list: a JSON array of base URLs.
///
/// `Ok(None)` means no list is published — a different thing from `Ok(Some(vec![]))`, which is a
/// publisher stating there are no mirrors.
///
/// THE PRIMARY IS AUTHORITATIVE WHENEVER IT ANSWERS. Mirrors are consulted only when it could not
/// be REACHED, which is the case the fallback was written for. It used to fall through whenever
/// the primary merely published no list — the ordinary state today — and since `refresh` replaces
/// the source set wholesale with whatever comes back, any one enabled mirror could evict every
/// sibling and install its own hosts, permanently, with the primary perfectly healthy. This
/// document is unverified (there is no `mirrors` payload reader yet), so until there is one, who
/// is allowed to author it is the only control available.
///
/// The document describes MIRRORS ONLY. There is no element in it that could name, reorder or
/// remove the primary source.
fn fetch_published_mirrors(settings: &Settings) -> Result<Option<Vec<String>>> {
    // primary: the release asset
    let gh = Github::new(settings.token());
    let last_err = match gh.fetch_release(&settings.source_repo, None) {
        Ok(release) => {
            return match release.asset(MIRRORS_ASSET) {
                // Bounded like every other trust-adjacent fetch (see engine::manifest_of): this is
                // a list of hosts the installer will later take bytes from, so its size is a trust
                // input.
                Some(a) => gh
                    .download_limited(a, crate::trust::MAX_DOC_BYTES)
                    .and_then(|b| parse_list(&b))
                    .map(Some),
                // Reached, and publishes no list. That is an ANSWER, not a gap.
                None => Ok(None),
            };
        }
        Err(e) => e,
    };

    let mut published = false;
    for url in settings.sources.iter().filter(|s| s.enabled()).filter_map(|s| s.url()) {
        match fetch_list_from_mirror(url) {
            Ok(Some(urls)) => return Ok(Some(urls)),
            Ok(None) => published = true,
            Err(_) => {}
        }
    }

    // The primary was unreachable and no mirror had a list either: report the primary's failure,
    // which is the one worth showing, unless a mirror positively answered "there is none".
    if published {
        Ok(None)
    } else {
        Err(last_err)
    }
}

fn fetch_list_from_mirror(base: &str) -> Result<Option<Vec<String>>> {
    let agent = probe_agent();
    let url = format!("{base}/{MIRRORS_ASSET}");
    match transport::fetch(&agent, &url, |req, _same_origin| req.set("User-Agent", UA)) {
        Ok(r) => Ok(Some(parse_list(&read_all(r)?)?)),
        Err(FetchError::Http(ureq::Error::Status(404, _))) => Ok(None),
        Err(e) => Err(anyhow::anyhow!("{base}: {}", short_fetch(e))),
    }
}

fn read_all(resp: ureq::Response) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    resp.into_reader().take(1 << 20).read_to_end(&mut buf)?;
    Ok(buf)
}

fn parse_list(bytes: &[u8]) -> Result<Vec<String>> {
    serde_json::from_slice::<Vec<String>>(bytes).context("mirrors.json is not a list of URLs")
}

fn probe_agent() -> ureq::Agent {
    ureq::builder()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(IO_TIMEOUT)
        .timeout_write(IO_TIMEOUT)
        // 0, not a positive count: `transport::fetch` drives every hop itself — see its module doc
        // for why ureq's own auto-follow is not safe to hand a mirror's redirects to.
        .redirects(0)
        // https-only, and re-checked on EVERY hop of a redirect chain, not just the URL we start
        // with (each hop `transport::fetch` issues is its own fresh request, so this same check
        // runs again every time). A mirror's `releases.json` names its own asset URLs (see
        // `probe_mirror` below) — so a hostile or compromised mirror can point a probe at
        // `http://`, `file://`, or a `\\host\share` UNC path. The last one is the one that matters
        // most: Windows treats that shape as an implicit SMB target, and touching it is enough to
        // leak this machine's NTLMv2 hash to whatever server answers, before a single byte of
        // content is read.
        .https_only(true)
        .build()
}

/// `net_reason`-style compaction for the redirect-following path: `short` already strips a
/// `ureq::Error` down to its kind; this covers the two ways `transport::fetch` can fail without
/// ever reaching ureq at all.
fn short_fetch(e: FetchError) -> String {
    match e {
        FetchError::Http(inner) => short(inner),
        other => other.to_string(),
    }
}

/// Probe every source at once.
///
/// Parallel because a probe is almost entirely network wait: sweeping N sources serially costs
/// N × `PROBE_BUDGET`, which is the multi-minute freeze a user would read as a broken button. The
/// count is published and small, so a thread each is simpler than a pool and costs nothing worth
/// managing. Disabled mirrors are probed too — that is how one that has recovered gets noticed.
pub fn probe_all(sources: &[Source], repo: &str, token: Option<&str>) -> Vec<Probe> {
    std::thread::scope(|scope| {
        let handles: Vec<_> =
            sources.iter().map(|s| scope.spawn(move || probe_one(s, repo, token))).collect();
        sources
            .iter()
            .zip(handles)
            .map(|(s, h)| h.join().unwrap_or_else(|_| Probe::failed(s, "the probe crashed")))
            .collect()
    })
}

pub fn probe_one(source: &Source, repo: &str, token: Option<&str>) -> Probe {
    match source {
        Source::Primary => probe_primary(source, repo, token),
        Source::Mirror { url, .. } => probe_mirror(source, url),
    }
}

/// The primary, measured through the real GitHub download path — API release lookup, then a ranged
/// asset read that follows the authenticated redirect to storage.
fn probe_primary(source: &Source, repo: &str, token: Option<&str>) -> Probe {
    let gh = Github::new(token);
    let mut p = Probe::blank(source);

    let started = Instant::now();
    let release = match gh.fetch_release(repo, None) {
        Ok(r) => r,
        Err(e) => return Probe::failed(source, format!("release lookup: {}", net_reason(&e))),
    };
    p.latency_ms = Some(started.elapsed().as_millis() as u64);
    p.tag = Some(release.tag_name.clone());

    let Some(asset) = probe_asset(&release) else {
        p.error = Some("the release carries no asset to test".to_string());
        return p;
    };
    match gh.ranged_asset(asset, PROBE_BYTES) {
        Ok((range_ok, reader)) => {
            p.range_ok = range_ok;
            time_read(&mut p, reader, &asset.name);
        }
        Err(e) => p.error = Some(format!("{}: {e}", asset.name)),
    }
    p
}

fn probe_mirror(source: &Source, url: &str) -> Probe {
    let agent = probe_agent();
    let mut p = Probe::blank(source);

    // 1. the index — proves it is a mirror at all, and yields the assets to time against.
    let started = Instant::now();
    let index_url = format!("{url}/releases.json");
    let resp = match transport::fetch(&agent, &index_url, |req, _same_origin| req.set("User-Agent", UA)) {
        Ok(r) => r,
        Err(e) => return Probe::failed(source, format!("releases.json: {}", short_fetch(e))),
    };
    // Through `read_all`'s ceiling, NOT `into_json`: ureq bounds `into_string` but not
    // `into_json`, which hands the whole body to serde as a stream — so an endless
    // `releases.json` from a host we have not authenticated would run the process out of memory
    // before the probe ever judged it. This is the least trusted input the launcher reads.
    let releases: Vec<Release> = match read_all(resp).and_then(|b| Ok(serde_json::from_slice(&b)?)) {
        Ok(v) => v,
        Err(e) => return Probe::failed(source, format!("releases.json is not readable: {e}")),
    };
    p.latency_ms = Some(started.elapsed().as_millis() as u64);

    let Some(release) = releases.iter().find(|r| r.is_published()) else {
        p.error = Some("no published release is advertised".to_string());
        return p;
    };
    p.tag = Some(release.tag_name.clone());

    let Some(asset) = probe_asset(release) else {
        p.error = Some("the release carries no asset to test".to_string());
        return p;
    };

    // 2. a real transfer. Ranged so the cost is bounded on the mirror's side too, and so the
    //    answer doubles as a resume-support check. `asset.browser_download_url` is itself content
    //    the mirror's `releases.json` named — see `transport`'s module doc for why that URL, and
    //    every redirect it might send back, goes through the same guarded fetch as the index did.
    let resp = match transport::fetch(&agent, &asset.browser_download_url, |req, _same_origin| {
        req.set("User-Agent", UA).set("Range", &format!("bytes=0-{}", PROBE_BYTES - 1))
    }) {
        Ok(r) => r,
        Err(e) => {
            p.error = Some(format!("{}: {}", asset.name, short_fetch(e)));
            return p;
        }
    };
    p.range_ok = resp.status() == 206;
    time_read(&mut p, resp.into_reader(), &asset.name);
    p
}

/// Read up to `PROBE_BYTES` under a wall-clock budget and record the throughput.
fn time_read(p: &mut Probe, mut reader: impl Read, asset: &str) {
    let started = Instant::now();
    let mut buf = vec![0u8; 32 * 1024];
    let mut got: u64 = 0;
    let mut out_of_budget = false;
    loop {
        if started.elapsed() >= PROBE_BUDGET {
            out_of_budget = true;
            break;
        }
        match reader.read(&mut buf) {
            Ok(0) => break, // asset smaller than the chunk we asked for
            Ok(n) => {
                got += n as u64;
                if got >= PROBE_BYTES {
                    break;
                }
            }
            Err(e) => {
                if got == 0 {
                    p.error = Some(format!("{asset}: the transfer failed: {e}"));
                    return;
                }
                // A transfer that dies partway is broken, not slow — but the bytes that did arrive
                // are a real measurement, so keep both and let the caller weigh them.
                p.error = Some(format!("the transfer stalled after {} KiB", got / 1024));
                break;
            }
        }
    }
    if got == 0 {
        p.error = Some("answered, but delivered no data".to_string());
        return;
    }
    p.bytes_per_sec = Some((got as f64 / started.elapsed().as_secs_f64().max(0.001)) as u64);
    // Running out of budget mid-chunk is a FAILURE, not merely a low score. This is the exact
    // shape of the throttled path this module exists for — index instant, first few KiB instant,
    // then a drip — and reporting it as healthy-but-slow would paint a green row on a source that
    // cannot finish a download this decade. The measured rate rides along in the message.
    if out_of_budget && got < PROBE_BYTES {
        p.error =
            Some(format!("too slow — {} KiB in {}s", got / 1024, PROBE_BUDGET.as_secs()));
    }
}

/// The asset to time: the BIGGEST one that is not release metadata.
///
/// Size is what makes the measurement honest. A release carries hundreds of small loose game
/// files, and a throttled path serves a 2 KB file flawlessly — so picking arbitrarily would let
/// exactly the link this module exists to catch report itself healthy. `manifest.json` is excluded
/// for the same reason: it is the one transfer such a path can always complete.
fn probe_asset(release: &Release) -> Option<&Asset> {
    let usable = |a: &&Asset| a.name != MIRRORS_ASSET && a.name != "manifest.json" && !a.name.ends_with(".sha256");
    match release.assets.iter().filter(usable).max_by_key(|a| a.size) {
        // an index that omits sizes leaves nothing to choose on; any real asset beats none
        Some(a) if a.size > 0 => Some(a),
        _ => release.assets.iter().find(usable).or_else(|| release.assets.first()),
    }
}

/// The same compaction for the GitHub path, whose errors arrive as an anyhow chain rooted at a
/// `NetKind`. Without this the primary's row would carry GitHub's whole JSON error body — API
/// message, documentation URL and all — on a line sized for "HTTP 404".
fn net_reason(e: &anyhow::Error) -> String {
    e.chain()
        .find_map(|c| c.downcast_ref::<NetKind>())
        .map_or_else(|| "failed".to_string(), NetKind::to_string)
}

/// A compact reason for the UI. ureq's transport errors carry the URL they were fetching, which
/// would put a full asset URL in a settings row; only the kind survives.
fn short(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, _) => format!("HTTP {code}"),
        ureq::Error::Transport(t) => t.kind().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mirror(url: &str, measured: bool) -> Source {
        Source::Mirror { url: url.to_string(), enabled: true, measured }
    }

    fn probe(source: &Source, healthy: bool) -> Probe {
        let mut p = Probe::blank(source);
        if healthy {
            p.bytes_per_sec = Some(1);
        } else {
            p.error = Some("down".into());
        }
        p
    }

    /// The one and only trigger for an automatic measurement.
    #[test]
    fn a_newly_published_mirror_is_the_trigger() {
        assert!(has_new_mirror(&[Source::Primary, mirror("https://a", false)]));
        assert!(!has_new_mirror(&[Source::Primary, mirror("https://a", true)]));
    }

    /// With no mirrors there is nothing to rank the primary against, so timing it alone would be
    /// a download that answers no question.
    #[test]
    fn the_primary_alone_never_triggers_a_measurement() {
        assert!(!has_new_mirror(&[Source::Primary]));
    }

    /// Measuring marks every mirror, so the same list never triggers a second automatic pass.
    #[test]
    fn measuring_clears_the_trigger() {
        let sources = vec![mirror("https://a", false)];
        // no network in a test: mark them the way `measure` does and check the trigger is gone
        let marked: Vec<Source> = sources
            .into_iter()
            .map(|s| match s {
                Source::Mirror { url, enabled, .. } => Source::Mirror { url, enabled, measured: true },
                p => p,
            })
            .collect();
        assert!(!has_new_mirror(&marked));
    }

    /// Nothing usable anywhere — main source included — is an ordinary outage on these networks,
    /// and the one case where the automatic switch must NOT spend the user's pin on a moment when
    /// no alternative worked.
    #[test]
    fn everything_offline_is_not_a_reason_to_repick() {
        let sources = vec![Source::Primary, mirror("https://a", true)];
        let all_dead = vec![probe(&sources[0], false), probe(&sources[1], false)];
        assert!(!any_healthy(&all_dead));

        let one_alive = vec![probe(&sources[0], false), probe(&sources[1], true)];
        assert!(any_healthy(&one_alive));
    }

    /// A refresh must not disturb the ranking: known mirrors keep their slots and their measured
    /// flag, and only genuinely new ones arrive — unmeasured, which is what triggers a pass.
    #[test]
    fn rebuild_preserves_rank_and_marks_only_new_mirrors() {
        let existing = vec![mirror("https://fast", true), Source::Primary, mirror("https://slow", true)];
        // the document lists them in a different order and adds one
        let out = rebuild(
            &existing,
            &["https://slow".into(), "https://new".into(), "https://fast".into()],
        );
        assert_eq!(out[0].url(), Some("https://fast"));
        assert!(out[1].is_primary());
        assert_eq!(out[2].url(), Some("https://slow"));
        assert_eq!(out[3].url(), Some("https://new"));
        assert!(has_new_mirror(&out));
        assert!(matches!(out[0], Source::Mirror { measured: true, .. }));
        assert!(matches!(out[3], Source::Mirror { measured: false, .. }));
    }

    /// A mirror the publisher dropped leaves the list; one that is merely OFFLINE does not — the
    /// document says what exists, not what is reachable.
    #[test]
    fn rebuild_removes_only_unpublished_mirrors() {
        let existing = vec![Source::Primary, mirror("https://gone", true), mirror("https://kept", true)];
        let out = rebuild(&existing, &["https://kept".into()]);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|s| s.url() != Some("https://gone")));
    }

    /// A URL that fails the https-only check must be refused as an ordinary error, never a panic
    /// — including the UNC shape (`\\host\share`), which Windows treats as an implicit SMB target
    /// and would leak this machine's NTLMv2 hash to whatever answers there the moment it is
    /// touched. Goes through `fetch_list_from_mirror` itself (not just `probe_agent` in
    /// isolation), since that is the real call site a published mirror's base URL reaches.
    #[test]
    fn mirror_fetch_refuses_non_https_urls_cleanly() {
        for base in [
            "http://mirror.example",
            "file:///etc/passwd",
            "\\\\attacker\\share",
            "//attacker/share",
        ] {
            let result = fetch_list_from_mirror(base);
            assert!(result.is_err(), "expected {base} to be refused, got {result:?}");
        }
    }

    /// A redirect chain longer than `MAX_REDIRECTS` must fail cleanly rather than being followed
    /// forever — `transport::fetch`'s own hop count, proven over a genuine TCP round trip (there
    /// is no way to prove this without one; see `test_http`'s doc comment). Built with
    /// `.redirects(0)`, matching `probe_agent`'s real setting: production never lets ureq's OWN
    /// auto-follow run at all (see `transport`'s module doc for why), so the cap enforced here is
    /// `transport::fetch`'s loop, not ureq's.
    #[test]
    fn mirror_fetch_refuses_a_redirect_chain_past_the_cap() {
        use crate::test_http::{Canned, TestServer};
        let server = TestServer::start(|_port| {
            let mut routes = std::collections::HashMap::new();
            routes.insert("/loop", Canned::redirect("/loop"));
            routes
        });

        let agent = ureq::builder()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout_read(IO_TIMEOUT)
            .timeout_write(IO_TIMEOUT)
            .redirects(0)
            .build();
        let url = format!("http://127.0.0.1:{}/loop", server.port);
        let err = transport::fetch(&agent, &url, |req, _same_origin| req)
            .expect_err("an endless redirect must not be followed forever");
        assert!(matches!(err, FetchError::TooManyRedirects), "expected TooManyRedirects, got {err:?}");
    }

    /// The scheme check runs on every hop, not just the URL a caller starts with: a mirror can
    /// answer its INDEX fine and then redirect an asset somewhere else entirely. This is also the
    /// test that proves the fix for the panic `transport`'s module doc describes — a redirect
    /// naming a scheme with no host (`file:///nope`) used to crash the process via ureq's own
    /// auto-follow; routing every hop through `transport::fetch` (a fresh top-level `.call()` each
    /// time, safely guarded by `Request::parse_url()`) is what turns that into an ordinary `Err`
    /// (`InvalidUrl` — `parse_url` refuses the empty host before ever reaching the scheme check
    /// `https_only` relies on; that check is what would fire for a well-formed http(s) URL, and is
    /// exercised by the initial-URL test above). `https_only` itself needs TLS to exercise, which
    /// this local plain-HTTP server has none of, but the empty-host check applies unconditionally,
    /// so a `file://` Location still proves the RE-CHECK happens on the SECOND hop, which the
    /// initial-URL test (`mirror_fetch_refuses_non_https_urls_cleanly`) cannot show by itself.
    /// (A `\\host\share` Location does not serve this test — WHATWG URL joining normalizes its
    /// backslashes into a protocol-relative `//host/share`, i.e. it becomes an ordinary
    /// same-scheme redirect to a new HOST, not a scheme change; that shape is exactly what
    /// `https_only` on the initial-URL tests already covers.)
    #[test]
    fn mirror_fetch_refuses_a_bad_scheme_introduced_mid_chain() {
        use crate::test_http::{Canned, TestServer};
        let server = TestServer::start(|_port| {
            let mut routes = std::collections::HashMap::new();
            routes.insert("/start", Canned::redirect("file:///nope"));
            routes
        });
        let agent = ureq::builder()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout_read(IO_TIMEOUT)
            .timeout_write(IO_TIMEOUT)
            .redirects(0)
            .build();
        let url = format!("http://127.0.0.1:{}/start", server.port);
        // The call itself completing (whether Ok or Err) is half the proof: the OLD code path
        // (ureq's own `.redirects(N)` auto-follow) panicked the whole test process here instead.
        let err = transport::fetch(&agent, &url, |req, _same_origin| req)
            .expect_err("a redirect to a non-http(s) scheme must be refused, not followed");
        match err {
            FetchError::Http(e) => assert_eq!(e.kind(), ureq::ErrorKind::InvalidUrl),
            other => panic!("expected Http(InvalidUrl), got {other:?}"),
        }
    }
}
