//! Download sources: discovering mirrors, and measuring whether one actually *works*.
//!
//! A mirror is a base URL serving a payload by CONTENT: `<url>/<payload>/manifest.json`, and the
//! blobs that document names under `<url>/<payload>/blobs/<sha256>` (the layout, and why there is
//! no tag directory in it, is `Mirror`'s doc; `doc_url`/`blob_url` build it for both the probe and
//! the download backend). Mirrors are never authored by the user: they are published in a SIGNED
//! `mirrors.json` and refreshed by `sweep`. Believing that document is `signed`'s job, below — and
//! it is the only door to it, because a list that is merely parsed rewrites this machine's download
//! sources permanently.
//!
//! The measurement is deliberately more than a reachability check, because the failure this exists
//! to catch is not an unreachable host. It is a network path that completes a handshake, serves a
//! few KB of JSON perfectly well, and then throttles or stalls the bulk transfer — a source that
//! passes every ping and cannot deliver a file. So every probe ends by pulling a chunk of a REAL
//! asset and timing it, and the number that matters is throughput, not latency.

use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::config::{normalize_mirror_url, Settings, Source};
use crate::downloader::{Asset, Downloader, NetKind, Release};
use crate::github::Github;
use crate::manifest::Manifest;
use crate::transport::{self, FetchError};
use crate::trust::Payload;

const UA: &str = concat!("phoenix-launcher/", env!("CARGO_PKG_VERSION"));

/// The published mirror list, as a release asset and at every mirror's root.
pub const MIRRORS_ASSET: &str = "mirrors.json";

/// Ceiling on a mirror's `mirrors.json`. An entry is a couple of hundred bytes, so a megabyte is
/// already thousands of them — and it is a list of hosts the installer will later take bytes from,
/// which makes its size a trust input like any other. It also has to be buffered whole before its
/// signature can be checked, so this bounds a read that happens BEFORE anything is believed. The
/// payload MANIFEST the probe reads is bounded separately and far higher
/// (`trust::MAX_DOC_BYTES`); see `probe_mirror`.
const MIRRORS_MAX_BYTES: u64 = 1 << 20;

/// Whether a mirror can serve an INSTALL yet. The `Downloader` implementation now exists (see
/// `Mirror` below), so this is no longer a statement about missing code: it is the DEPLOYMENT
/// switch, and it stays false until the `mirrors.json` published for these hosts is signed and the
/// payload trees are actually mirrored. Nothing routes a byte through a mirror while it is false —
/// `cmd::download_sources` builds a chain of the primary alone — so the boot-time sweep's cost (a
/// 512 KiB transfer and up to `PROBE_BUDGET` per source, every launch) is not paid for a result
/// nothing can act on. A user-initiated sweep still measures, because there the cost is asked for.
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
    /// Milliseconds to fetch and read the document that names the release — the primary's release
    /// index, a mirror's manifest. All a plain reachability check would have said.
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
    /// The serial of the list this sweep accepted, if it accepted one. Private, and written only
    /// by `persist`, for the reason spelled out on `Refresh::persist`.
    serial: Option<u64>,
}

impl Sweep {
    /// Persist a measured sweep. The measured ORDER and the accepted SERIAL in one settings write —
    /// see `Refresh::persist`, of which this is the measured half.
    ///
    /// `clear_pin` drops the user's pinned source, which is the caller's policy and not this
    /// module's: the test button clears it (asking to be re-tested is asking for the answer the
    /// test gives) and the headless `--save` does not.
    pub fn persist(&self, clear_pin: bool) -> Result<()> {
        store(self.sources.clone(), self.serial, clear_pin)
    }
}

/// What a list refresh concluded: the sources as they should now stand, and — only when a list was
/// actually accepted — the serial to ratchet with.
pub struct Refresh {
    pub sources: Vec<Source>,
    /// Why the published list could not be read, if it could not. Never fatal.
    pub error: Option<String>,
    /// Private on purpose: it may only leave here through `persist`, together with the sources it
    /// came from.
    serial: Option<u64>,
}

impl Refresh {
    /// Time every source and reorder them fastest-first, keeping the accepted serial.
    ///
    /// The serial is a property of the DOCUMENT, so it survives any reordering of the hosts that
    /// document named — which is exactly why measuring must not be a path that quietly loses it.
    pub fn measured(self, settings: &Settings) -> (Self, Vec<Probe>) {
        let payload = settings.payload_of(&settings.source_repo);
        let (sources, probes) =
            measure(self.sources, &settings.source_repo, payload, settings.token());
        (Self { sources, error: self.error, serial: self.serial }, probes)
    }

    /// Persist a refresh: the sources it concluded AND the serial of the list they came from, in
    /// one settings write.
    ///
    /// Never one without the other, and that is why the field is private and this is a method. The
    /// ratchet is what makes checking a signature worth anything at all — a mirror can always serve
    /// an older list it once held a valid signature for, and the signature on it is still perfectly
    /// good — and it advances only when a list is actually APPLIED, which is here. A call site that
    /// stored the sources alone would leave the floor at zero forever: the rollback check would go
    /// on running against a floor nothing ever raised, and the only symptom of that is a rollback
    /// nobody notices.
    pub fn persist(&self, clear_pin: bool) -> Result<()> {
        store(self.sources.clone(), self.serial, clear_pin)
    }
}

/// The one settings write both of the above make.
fn store(sources: Vec<Source>, serial: Option<u64>, clear_pin: bool) -> Result<()> {
    Settings::update(move |s| {
        s.sources = sources;
        // Only when a list was ACCEPTED — `Ok(None)` and every refusal leave the floor alone, since
        // neither produced a document to be current with. `advance_serial` is monotonic, so this
        // can never walk the floor back either.
        if let Some(n) = serial {
            s.advance_serial(Payload::Mirrors, n);
        }
        if clear_pin {
            s.selected = None;
        }
    })
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

/// Refresh the published mirror list. Cheap — one small document and its signature — so this runs
/// on every launch.
pub fn refresh(settings: &Settings) -> Refresh {
    apply(&settings.sources, fetch_published_mirrors(settings))
}

/// What an answer DOES to the list. Three outcomes, none of them interchangeable, and the
/// differences between them are the whole safety property of this file:
///
/// * a VERIFIED list REPLACES the mirrors — including an empty one, which is the publisher saying
///   there are none. Flags survive by URL (`rebuild`); the primary is not in the document and so is
///   untouched by construction.
/// * `Ok(None)` — nothing published a list at all — is silence, not an empty list. The existing
///   mirrors stay, because "could not ask" must never read as "there are none".
/// * an ERROR is that same silence, and an error is what a document that FAILED TO VERIFY produces.
///   Refused and empty must not collapse into one outcome: if they did, a tampered or truncated
///   answer would wipe every mirror a user has, which is precisely the harm the signature is here
///   to prevent — and it would do it quietly, since a wiped list looks exactly like a publisher who
///   retired their mirrors.
///
/// Split out from `refresh` so all three can be exercised without a network; `refresh` is one line
/// over it.
fn apply(existing: &[Source], answer: Result<Option<signed::SignedList>>) -> Refresh {
    match answer {
        Ok(Some(list)) => Refresh {
            sources: rebuild(existing, list.hosts()),
            error: None,
            serial: Some(list.serial()),
        },
        Ok(None) => Refresh { sources: existing.to_vec(), error: None, serial: None },
        Err(e) => {
            Refresh { sources: existing.to_vec(), error: Some(format!("{e:#}")), serial: None }
        }
    }
}

/// Time every source and order them fastest-first, marking each mirror measured.
///
/// `payload` is `repo`'s (`Settings::payload_of`) — a mirror is addressed by payload directory, not
/// by repo, and only the settings know which is which.
pub fn measure(
    sources: Vec<Source>,
    repo: &str,
    payload: Option<Payload>,
    token: Option<&str>,
) -> (Vec<Source>, Vec<Probe>) {
    let probes = probe_all(&sources, repo, payload, token);
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
/// persist (the GUI does; the CLI only on `--save`) — through `Sweep::persist`, which is the only
/// thing that may write one.
pub fn sweep(settings: &Settings, do_measure: bool) -> Sweep {
    let refreshed = refresh(settings);
    if !do_measure {
        return Sweep {
            sources: refreshed.sources,
            probes: Vec::new(),
            refresh_error: refreshed.error,
            serial: refreshed.serial,
        };
    }
    let (refreshed, probes) = refreshed.measured(settings);
    Sweep {
        sources: refreshed.sources,
        probes,
        refresh_error: refreshed.error,
        serial: refreshed.serial,
    }
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
fn rebuild(existing: &[Source], hosts: &[signed::Host]) -> Vec<Source> {
    let published: Vec<(String, Vec<String>)> = {
        let mut seen = std::collections::HashSet::new();
        hosts
            .iter()
            .filter_map(|h| normalize_mirror_url(&h.url).map(|u| (u, h.payloads.clone())))
            .filter(|(u, _)| seen.insert(u.clone()))
            .collect()
    };

    let mut out: Vec<Source> = existing
        .iter()
        .filter(|s| s.is_primary() || s.url().is_some_and(|u| published.iter().any(|(p, _)| p == u)))
        .cloned()
        .collect();
    if !out.iter().any(Source::is_primary) {
        out.insert(0, Source::Primary);
    }
    // A surviving mirror keeps its flags and its rank — the whole point of matching by URL — but
    // takes the LIST's word on what it carries. Payloads are the publisher's statement about a
    // host, not a local preference, so an entry that gained or lost one must be re-read here or the
    // probe and the download chain go on believing a shape the list has stopped claiming.
    for s in &mut out {
        if let Source::Mirror { url, payloads, .. } = s {
            if let Some((_, fresh)) = published.iter().find(|(p, _)| p == url) {
                *payloads = fresh.clone();
            }
        }
    }
    for (url, payloads) in published {
        if !out.iter().any(|s| s.url() == Some(url.as_str())) {
            out.push(Source::Mirror { url, enabled: true, measured: false, payloads });
        }
    }
    out
}

/// The name of the detached signature published beside the list, at both sources.
fn mirrors_sig() -> String {
    format!("{MIRRORS_ASSET}{}", crate::trust::SIG_SUFFIX)
}

/// A `.minisig`'s bytes as the text `trust::verify` reads.
fn sig_text(bytes: Vec<u8>) -> Result<String> {
    String::from_utf8(bytes)
        .map_err(|_| anyhow::Error::new(crate::minisig::SigError::Malformed("not UTF-8")))
}

/// The published mirror list, VERIFIED — from the registry repo's release, or from a mirror.
///
/// `Ok(None)` means no list is published, which is a different thing from a verified document whose
/// `mirrors` array is empty: that one is a publisher stating there are none, and it replaces the
/// set. `Err` is "no answer", and it covers BOTH a source that could not be reached and a document
/// that failed to verify — see `apply` for why those two must land in the same place.
///
/// THE REGISTRY IS AUTHORITATIVE WHENEVER IT ANSWERS; mirrors are consulted only when it could not
/// be reached at all. The signature and the serial ratchet are what make a mirror's copy safe to
/// obey — before them, any one enabled mirror could evict every sibling and install its own hosts,
/// permanently, with the registry perfectly healthy, so "who is allowed to author it" was the only
/// control there was. What the ordering still buys, now that it is not the control, is FRESHNESS
/// and a useful error: the registry publishes first, so a mirror's copy can only ever be as new,
/// and a mirror's failure is not the one worth showing a user when the source of truth is down.
///
/// The document describes MIRRORS ONLY. There is no element in it that could name, reorder or
/// remove the primary source.
fn fetch_published_mirrors(settings: &Settings) -> Result<Option<signed::SignedList>> {
    let floor = settings.serial_floor(Payload::Mirrors);
    // `cmd::open_repo`, not a bare `Github`: this repo is PUBLIC, so the fetch takes the same
    // anonymous-first / token-on-refusal rule every other public repo takes rather than leading
    // with a credential scoped to the dist repo (see `Settings::mirrors_repo`). The source WALK
    // `open_repo` also performs degenerates to the primary alone here, because `payload_of` does
    // not know this repo and must not — a mirror's copy is reached below instead, at the root path
    // a mirror actually serves it from.
    let last_err = match crate::cmd::open_repo(settings.mirrors_repo(), settings) {
        Ok((dl, release)) => {
            // Reached, and publishes no list. That is an ANSWER, not a gap — and it is the only
            // "absent" this function recognises, which is why a missing SIGNATURE below is an error
            // instead: "unsigned" must never be spelled the same way as "not published".
            let Some(doc_asset) = release.asset(MIRRORS_ASSET) else { return Ok(None) };
            let sig_name = mirrors_sig();
            let sig_asset = release.asset(&sig_name).ok_or_else(|| {
                anyhow::Error::new(crate::trust::TrustError::Unsigned(MIRRORS_ASSET.to_string()))
            })?;
            // Bounded like every other trust-adjacent fetch (see engine::manifest_of): the document
            // is buffered whole in order to be verified, so its size is a trust input, and the read
            // happens before a single check has run.
            let doc = dl.download_limited(doc_asset, crate::trust::MAX_DOC_BYTES)?;
            let sig = sig_text(dl.download_limited(sig_asset, crate::trust::MAX_SIG_BYTES)?)?;
            // Everything past `open_repo` succeeding is the registry ANSWERING, so its verdict —
            // including a refusal — is returned rather than falling through to the mirrors below.
            return signed::verify(&doc, &sig, floor).map(Some);
        }
        Err(e) => e,
    };

    // One agent for the whole fallback: `probe_agent`'s pool is what keeps N mirrors from paying N
    // fresh handshakes for two small documents each.
    let agent = probe_agent();
    for url in settings.sources.iter().filter(|s| s.enabled()).filter_map(|s| s.url()) {
        if let Ok(Some(list)) = fetch_list_from_mirror(&agent, url, floor) {
            return Ok(Some(list));
        }
    }

    // NO MIRROR IS AUTHORITATIVE ABOUT WHETHER A LIST EXISTS — only the registry is, and it already
    // returned above if it answered at all. A mirror's 404 says one thing: that HOST does not carry
    // the document. It used to be counted as the publisher saying "there are none", and the
    // consequences were the reverse of harmless: `Ok(None)` reaches `apply` as a successful refresh
    // with `error: None`, so the settings pane reports nothing wrong while the mirror set silently
    // stops changing — for precisely the users whose registry is unreachable, which is everyone
    // this feature exists for. Any host that has not synced the list yet was enough to trigger it.
    //
    // So the registry's failure is what gets reported. Being unable to ask is not an answer.
    Err(last_err)
}

/// One mirror's copy of the list: `<base>/mirrors.json`, with `<base>/mirrors.json.minisig` beside
/// it.
///
/// THE LEAST TRUSTED SOURCE IN THE SYSTEM, and by some distance: this is a host we were told about
/// by a document, serving the document that decides which hosts we are told about next. Nothing
/// here can reach the parsed form without `signed::verify` — see that module's doc for why the
/// verified type cannot be built any other way.
///
/// A missing DOCUMENT is `Ok(None)`: this host does not carry the list, which is a true answer and
/// the one thing the caller may read as "no list published". A missing SIGNATURE is an error, for
/// exactly that reason — a mirror that could turn "I refuse to sign this" into "there is no list"
/// would still be choosing what the caller does, and here the caller does nothing at all.
///
/// The `agent` is a parameter for the same reason `probe_mirror`'s is: `probe_agent()` is
/// https-only and a loopback listener cannot speak TLS, so the end-to-end tests below could not
/// otherwise reach this. Production has exactly one caller and it passes `probe_agent()`.
fn fetch_list_from_mirror(
    agent: &ureq::Agent,
    base: &str,
    floor: u64,
) -> Result<Option<signed::SignedList>> {
    // `Ok(None)` here is a 404 and nothing else: every other failure is an error, so a host that
    // answers oddly is never mistaken for one that answers "no".
    let get = |name: &str, max: u64| -> Result<Option<Vec<u8>>> {
        let url = root_url(base, name);
        match transport::fetch(agent, &url, |req, _same_origin| req.set("User-Agent", UA)) {
            Ok(r) => Ok(Some(read_all(r, max)?)),
            Err(FetchError::Http(ureq::Error::Status(404, _))) => Ok(None),
            Err(e) => Err(anyhow::anyhow!("{base}: {}", short_fetch(e))),
        }
    };
    let Some(doc) = get(MIRRORS_ASSET, MIRRORS_MAX_BYTES)? else { return Ok(None) };
    let sig = get(&mirrors_sig(), crate::trust::MAX_SIG_BYTES)?.ok_or_else(|| {
        anyhow::Error::new(crate::trust::TrustError::Unsigned(MIRRORS_ASSET.to_string()))
    })?;
    signed::verify(&doc, &sig_text(sig)?, floor)
        .map(Some)
        .with_context(|| base.to_string())
}

/// Read a small document whole, under a ceiling the CALLER states — the two this module reads from
/// a mirror are not remotely the same size, and one ceiling for both would have to be the larger.
///
/// Over-limit is an ERROR rather than a silent truncation. A truncated document is still handed to
/// a parser, which reports it as malformed — blaming a host's syntax for its size, and on the
/// manifest path ranking a mirror dead for serving a release that merely grew.
fn read_all(resp: ureq::Response, max: u64) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    // `take(max + 1)` rather than trusting Content-Length, exactly as `Mirror::manifest_bytes`
    // does: the length header is the peer's claim, and a host that intends to exhaust this
    // process's memory is not going to declare it.
    resp.into_reader().take(max + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > max {
        anyhow::bail!("the document is larger than the {max} bytes allowed for it");
    }
    Ok(buf)
}

/// The published mirror list, and the ONLY door to it.
///
/// Everything that turns bytes into mirrors lives in here, and the only things it lets out are
/// `verify` and the type that function returns. The serde types are private, so no caller — not
/// another module, not the rest of this file — can deserialize the document without a signature in
/// hand, and `SignedList`'s own fields are private, so it cannot be assembled by hand either.
///
/// That is a deliberate use of the module system rather than a convention, because the thing it
/// prevents is not a bug in code that exists. `refresh` REPLACES the mirror set wholesale and the
/// result is persisted, so a document that reaches the parsed form unverified is not a bad answer
/// to one download — it is a permanent rewrite of where this machine fetches every future release
/// from. A free `parse_list(&[u8]) -> Vec<String>` used to sit here, called from both fetch paths;
/// making the verified form unconstructable is what stops a third path from being written that
/// forgets, and stops it at compile time rather than in review.
mod signed {
    use anyhow::{Context, Result};
    use serde::Deserialize;

    use crate::config::normalize_mirror_url;
    use crate::trust::{self, Payload};

    /// The document version this build reads. Its OWN number, unrelated to a manifest's `schema`:
    /// the two formats share a signing scheme and nothing else.
    ///
    /// An unknown one is REFUSED rather than read optimistically, and the trade that makes is the
    /// safe one: a refusal is silence, and silence leaves the mirrors this machine already has
    /// alone (`apply`), whereas guessing at a format we have never seen means acting on a document
    /// we cannot claim to have understood — in the one place where acting means handing a stranger
    /// every future download.
    const FORMAT: u64 = 1;

    /// The wire document, exactly as `phoenix-mirror-registry`'s `generate_mirror_list.py` renders
    /// it. Private, and it stays private: `Deserialize` is the bypass this module exists to remove.
    ///
    /// `signed_at` is deliberately NOT declared. It is advisory — nothing may fail on it and it may
    /// be absent entirely — and a field a reader parses but never acts on is one that invites the
    /// next reader to believe freshness is checked here. It is covered by the signature either way;
    /// `serial` is what orders these documents, and it is the only thing that does.
    #[derive(Deserialize)]
    struct Document {
        format: u64,
        /// Optional so an absent one fails as `TrustError::WrongPayload { found: None }` rather
        /// than as a syntax error: a signed document of ours always names its payload, so
        /// "somebody signed a document that does not say what it is" deserves the trust layer's
        /// answer and not serde's.
        #[serde(default)]
        payload_id: Option<String>,
        /// Optional for the same reason — absent is `StaleSerial { found: None }`, a document that
        /// cannot be shown to be current.
        #[serde(default)]
        serial: Option<u64>,
        mirrors: Vec<Entry>,
    }

    /// One registration. Unknown keys are IGNORED (serde's default), which is what lets the
    /// registry add a field without freezing the list of every launcher already installed.
    #[derive(Deserialize)]
    struct Entry {
        base_url: String,
        name: String,
        country: String,
        payloads: Vec<String>,
    }

    /// One published mirror, reduced to what the client acts on: where it is, and which payload
    /// trees it says it carries.
    ///
    /// `payloads` is kept — not merely validated and dropped — because a mirror need not carry all
    /// three, and both the probe and the download chain have to know. `name` and `country` are
    /// still deliberately discarded: nothing renders them, and a field carried but unused is one a
    /// later reader starts to believe is doing something.
    #[derive(Debug)]
    pub(super) struct Host {
        pub(super) url: String,
        pub(super) payloads: Vec<String>,
    }

    /// A mirror list that has been verified, identified and found current. `verify` is its sole
    /// constructor: the fields below are private, so nothing outside this module can produce one
    /// even by writing a struct literal.
    #[derive(Debug)]
    pub(super) struct SignedList {
        hosts: Vec<Host>,
        serial: u64,
    }

    impl SignedList {
        /// The published mirrors, in the order the publisher listed them.
        pub(super) fn hosts(&self) -> &[Host] {
            &self.hosts
        }

        /// The serial this document was accepted at — what the caller ratchets the floor forward
        /// with, once it has actually applied the list (`Refresh::persist`).
        pub(super) fn serial(&self) -> u64 {
            self.serial
        }
    }

    /// Bytes and a detached signature in; a list worth obeying out.
    ///
    /// The gates run in the order `engine::manifest_of` runs them, and for the same reasons: the
    /// caller has already BOUNDED the read, then the SIGNATURE comes before the parser (the parser
    /// is the largest attack surface here, and running it over unauthenticated bytes is the thing
    /// signing exists to stop), then the FORMAT, then IDENTITY AND FRESHNESS — a valid signature
    /// says we produced this document, not that it is the document that was asked for, nor that it
    /// is not one we published years ago and a mirror kept.
    pub(super) fn verify(doc: &[u8], sig: &str, floor: u64) -> Result<SignedList> {
        // WHICH key signed is deliberately not acted on, exactly as on the manifest path: a list
        // signed by the cold spare is a list signed by us, and the spare exists for the day the
        // active key is gone.
        trust::verify(doc, sig).map_err(anyhow::Error::new).context("verifying the mirror list")?;
        let parsed: Document =
            serde_json::from_slice(doc).context("the mirror list is not readable")?;
        if parsed.format != FORMAT {
            anyhow::bail!(
                "the mirror list is format {}, and this launcher reads format {FORMAT}",
                parsed.format
            );
        }
        let serial =
            trust::accept_ident(Payload::Mirrors, parsed.payload_id.as_deref(), parsed.serial, floor)
                .map_err(anyhow::Error::new)?;

        // ONE bad entry refuses the WHOLE document rather than being dropped. This list is signed,
        // so a malformed entry cannot be an attacker's doing — it is our own producer having
        // shipped what it promises it cannot — and quietly dropping it is the exact failure
        // `generate_mirror_list.py` exists to make impossible ("a well-formed document that
        // silently ships nothing"). A reader that repeats the drop is where that silence comes
        // back, with a green build at both ends and a mirror nobody can use.
        let mut hosts = Vec::with_capacity(parsed.mirrors.len());
        for m in &parsed.mirrors {
            // SHAPE only. The producer's vocabulary — the name charset, the two-letter country, the
            // set of payload names that exist — is checked where it is authored, and is not
            // transcribed here on purpose: a reader's copy of another repo's rules can only drift
            // from them, and the single power it would have is to refuse a legitimate future
            // registration. A payload kind added under format 1 must not freeze the mirror list of
            // every launcher already in the field.
            if m.name.is_empty()
                || m.country.is_empty()
                || m.payloads.is_empty()
                || m.payloads.iter().any(String::is_empty)
            {
                anyhow::bail!("the mirror list carries an entry with an empty field: {:?}", m.name);
            }
            // The published string must ALREADY be the one the client uses. Normalizing it here
            // would mean downloading from a URL nobody published and no reviewer read — and the
            // producer refuses a non-canonical `base_url` for that same reason, so one arriving
            // here means this is not the document that repo builds.
            if normalize_mirror_url(&m.base_url).as_deref() != Some(m.base_url.as_str()) {
                anyhow::bail!(
                    "the mirror list carries a base_url that is not canonical: {:?}",
                    m.base_url
                );
            }
            hosts.push(Host { url: m.base_url.clone(), payloads: m.payloads.clone() });
        }
        Ok(SignedList { hosts, serial })
    }
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
        // runs again every time). The probe DERIVES every URL it starts from (`doc_url`/`blob_url`,
        // see `probe_mirror` below), but a mirror still chooses every `Location` it answers with —
        // so a hostile or compromised one can point the next hop at `http://`, `file://`, or a
        // `\\host\share` UNC path. The last one is the one that matters most: Windows treats that
        // shape as an implicit SMB target, and touching it is enough to leak this machine's NTLMv2
        // hash to whatever server answers, before a single byte of content is read.
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
pub fn probe_all(
    sources: &[Source],
    repo: &str,
    payload: Option<Payload>,
    token: Option<&str>,
) -> Vec<Probe> {
    std::thread::scope(|scope| {
        let handles: Vec<_> =
            sources.iter().map(|s| scope.spawn(move || probe_one(s, repo, payload, token))).collect();
        sources
            .iter()
            .zip(handles)
            .map(|(s, h)| h.join().unwrap_or_else(|_| Probe::failed(s, "the probe crashed")))
            .collect()
    })
}

pub fn probe_one(
    source: &Source,
    repo: &str,
    payload: Option<Payload>,
    token: Option<&str>,
) -> Probe {
    match source {
        // Two backends, two measurements, deliberately not unified: the primary IS GitHub, so its
        // release index is the right thing to time there, and a mirror has no such index at all.
        Source::Primary => probe_primary(source, repo, token),
        Source::Mirror { url, .. } => {
            probe_mirror(&probe_agent(), source, url, measurable(source, payload))
        }
    }
}

/// Which payload directory to time this mirror through.
///
/// It has to be one the host actually CARRIES, and that is not always the one the caller asked
/// about: `measure` asks about the source repo's payload — always `mod`, since that is the repo
/// whose ranking it builds — and timing that fixed directory measured one which need not exist. A
/// game-only mirror 404'd, was painted unusable and dropped from `any_healthy` while serving the
/// 7.9 GiB payload perfectly; a mod-only mirror measured healthy and then headed the ranking used
/// for a game install it cannot serve at all.
///
/// Preferring the requested payload keeps the ordinary case measuring exactly what will be
/// downloaded. Falling back to another one the host carries is deliberate: throughput is a property
/// of the HOST, and a real transfer from it beats no number — while `candidates` is what keeps a
/// payload from being routed to a mirror lacking it, so a speed measured here can never on its own
/// send a download somewhere it cannot be served.
///
/// `None` means nothing measurable: a mirror advertising only payload names this build cannot name.
fn measurable(source: &Source, want: Option<Payload>) -> Option<Payload> {
    if want.is_some_and(|p| source.carries(p)) {
        return want;
    }
    source.payloads().iter().find_map(|id| Payload::from_id(id))
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

/// A mirror, measured through the layout a mirror actually serves (see this module's doc): its
/// payload manifest, then a ranged read of the biggest blob that document names.
///
/// NOTHING READ HERE IS VERIFIED, and nothing may come to depend on that changing. There is no
/// signature check on this manifest — the install path fetches and verifies its own copy
/// (`engine::manifest_of` -> `trust::accept`) before an installed byte rests on it — so the only
/// things taken from it are a hash, handed straight back as a URL, and a size, used only to decide
/// which blob to time. Every read stays bounded whatever the document claims: `MAX_DOC_BYTES` for
/// the manifest, `PROBE_BYTES` for the range, `PROBE_BUDGET` for the clock. A hostile mirror can
/// spend this probe's few seconds and mislead its own ranking, which is all a source can ever do.
///
/// The `agent` is a parameter for the same reason `Mirror`'s tests build their own: `probe_agent()`
/// is https-only and a loopback listener cannot speak TLS, so the end-to-end tests below could not
/// otherwise reach this at all. Production has exactly one caller and it passes `probe_agent()`.
fn probe_mirror(
    agent: &ureq::Agent,
    source: &Source,
    url: &str,
    payload: Option<Payload>,
) -> Probe {
    let mut p = Probe::blank(source);
    // A repo this build cannot name has no `<base>/<payload>/` to look under — `cmd::candidates`
    // drops mirrors from the download chain for the same reason, so there is nothing here worth
    // measuring: a number for a source that could never be used is worse than no number.
    let Some(payload) = payload else {
        return Probe::failed(source, "this build cannot name the payload for that repo");
    };

    // 1. the manifest — the one document a mirror must serve for a payload, so an answer here is
    //    what proves the host is a mirror of it at all (the role `releases.json` used to play).
    //    It is also the only way to address a blob: there is no index, and no listable directory.
    let started = Instant::now();
    let manifest_url = doc_url(url, payload.id(), crate::engine::MANIFEST_ASSET);
    let resp = match transport::fetch(agent, &manifest_url, |req, _same_origin| {
        req.set("User-Agent", UA)
    }) {
        Ok(r) => r,
        Err(e) => {
            let why = format!("{}: {}", crate::engine::MANIFEST_ASSET, short_fetch(e));
            return Probe::failed(source, why);
        }
    };
    // Bounded through `read_all`, NOT `into_json`: ureq bounds `into_string` but not `into_json`,
    // which hands the whole body to serde as a stream — so an endless `manifest.json` from a host
    // we have not authenticated would run the process out of memory before the probe ever judged
    // it. This is the least trusted input the launcher reads.
    //
    // The ceiling is the one this SAME document is read under on the install path
    // (`Mirror::manifest_bytes`), not `mirrors.json`'s: a payload manifest is a genuinely large
    // document — the base game's is ~4.6k entries of ~200 bytes (see `trust::MAX_DOC_BYTES`) —
    // and a megabyte would start reporting a healthy game mirror as unreadable as that grew.
    let doc = match read_all(resp, crate::trust::MAX_DOC_BYTES) {
        Ok(b) => b,
        Err(e) => return Probe::failed(source, format!("{}: {e}", crate::engine::MANIFEST_ASSET)),
    };
    // The strict reader, not a private permissive one: a second parser over the least trusted
    // document in the system is exactly the thing that drifts, and a document this one refuses is
    // a document the installer would refuse too.
    let manifest = match Manifest::parse(&doc) {
        Ok(m) => m,
        Err(e) => {
            let why = format!("{} is not readable: {e:#}", crate::engine::MANIFEST_ASSET);
            return Probe::failed(source, why);
        }
    };
    p.latency_ms = Some(started.elapsed().as_millis() as u64);
    p.tag = Some(tag_of(&manifest.version));

    let Some(sha256) = probe_blob(&manifest) else {
        p.error = Some("the release carries no blob to test".to_string());
        return p;
    };
    // 12 hex digits, the short form `manifest.rs`'s own validator prints: a 64-character name in a
    // settings row is noise. `probe_blob` returned it only after `is_content_hash`, so the slice
    // cannot land inside a character.
    let label = format!("blob {}", &sha256[..12]);

    // 2. a real transfer. Ranged so the cost is bounded on the mirror's side too, and so the answer
    //    doubles as a resume-support check. The URL is DERIVED from the hash, never a URL the
    //    document named — a content-addressed mirror advertises none, which is one attacker-chosen
    //    string fewer than the old release-index probe had to handle. What it does still choose is
    //    every `Location` it answers with, which is why this goes through the same guarded fetch as
    //    the manifest did — see `transport`'s module doc.
    let blob = blob_url(url, payload.id(), sha256);
    let resp = match transport::fetch(agent, &blob, |req, _same_origin| {
        req.set("User-Agent", UA).set("Range", &format!("bytes=0-{}", PROBE_BYTES - 1))
    }) {
        Ok(r) => r,
        Err(e) => {
            p.error = Some(format!("{label}: {}", short_fetch(e)));
            return p;
        }
    };
    p.range_ok = resp.status() == 206;
    time_read(&mut p, resp.into_reader(), &label);
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

/// The blob to time: the BIGGEST one the mirror could actually be asked for.
///
/// Size is what makes the measurement honest, exactly as in `probe_asset` below — a release carries
/// hundreds of small loose game files, and a throttled path serves a 2 KB file flawlessly, so
/// picking arbitrarily would let the very link this module exists to catch report itself healthy.
///
/// `probe_asset`'s name-based exclusions (`mirrors.json`, `manifest.json`, `*.sha256`) have no
/// analogue here and need none. They exist because GitHub's release index lists the release
/// DOCUMENTS alongside the payload's assets; a manifest lists no such thing — it describes payload
/// content only, and the documents live at the payload root, a different URL space from `blobs/`.
///
/// What replaces them is a ROUTE rule, because not every entry in the document HAS a blob.
/// `install::build_acqs` is the authority on the three routes, and only two of them ever cross the
/// wire: a bundle (addressed by `psha256`, `psize` bytes packed) and an entry carrying a `name`.
/// An entry with no `name` is carried inside a bundle and a zero-size one is materialized locally,
/// so neither has a blob of its own — timing one would 404 against a perfectly healthy mirror and
/// rank it dead, which is precisely the misjudgement the metadata exclusions were written to avoid.
fn probe_blob(m: &Manifest) -> Option<&str> {
    let bundles = m.bundles.iter().map(|b| (b.psha256.as_str(), b.psize));
    let named = m
        .payload_entries()
        .filter(|(name, _, size)| name.is_some() && *size > 0)
        .map(|(_, sha256, size)| (sha256, size));
    bundles
        .chain(named)
        // Stated where the URL is built, not left resting on `Manifest::parse` having checked it a
        // module away: the hash's FORM is what decides which of two URL shapes a name resolves to
        // (`Mirror::url_of`), so anything that is not one would be reached for as a DOCUMENT.
        .filter(|(sha256, _)| is_content_hash(sha256))
        .max_by_key(|&(_, size)| size)
        .map(|(sha256, _)| sha256)
}

/// The asset to time on GITHUB, whose release index is a real index: the BIGGEST one that is not
/// release metadata.
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

// ---- the layout, shared by the probe and the download backend ----

/// A release DOCUMENT beside the payload: `<base>/<payload>/<name>`.
///
/// A free function rather than a `Mirror` method because the PROBE builds the same URLs without a
/// `Mirror`: the two need different agents (a probe fails fast, a download waits out a slow link —
/// see `CONNECT_TIMEOUT` against `DL_CONNECT_TIMEOUT`), and that is the whole of the difference.
/// Two places formatting these paths by hand is how a layout change ships half-applied — measuring
/// one address while installing from another.
fn doc_url(base: &str, payload: &str, name: &str) -> String {
    format!("{}/{payload}/{name}", base.trim_end_matches('/'))
}

/// A document at a mirror's ROOT: `<base>/<name>`.
///
/// The mirror list lives here and not under a payload directory, because it is not any payload's
/// content: it describes the HOSTS, and a host serves one list whatever payload trees it happens to
/// carry. That is also why `Settings::payload_of` must never learn the registry repo — there is no
/// `<base>/mirrors/` to address.
fn root_url(base: &str, name: &str) -> String {
    format!("{}/{name}", base.trim_end_matches('/'))
}

/// A payload entry's bytes: `<base>/<payload>/blobs/<sha256>` — no extension, because the name IS
/// the hash and an extension would be a second, unsigned claim about the content.
fn blob_url(base: &str, payload: &str, sha256: &str) -> String {
    format!("{}/{payload}/blobs/{sha256}", base.trim_end_matches('/'))
}

/// 64 lowercase hex — the one shape a payload entry's `sha256` may take. The authority on the rule
/// is `manifest::Manifest::validate_hashes`, which REFUSES a manifest that breaks it; this is the
/// same test applied to decide which of two URL shapes a name belongs to.
fn is_content_hash(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The tag a mirror's release is NAMED by.
///
/// A mirror publishes no release index and no tag directory (see `Mirror`'s doc), so a release has
/// exactly one name there: the `version` its manifest carries. Shared with `fetch_release` so a
/// mirror cannot appear to advertise one release to the settings pane and another to the installer.
fn tag_of(version: &str) -> String {
    format!("v{version}")
}

// ---- the download backend ----

/// Connect/IO timeouts for the DOWNLOAD path, deliberately not the probe's. A probe's job is to
/// fail fast and be re-run (`CONNECT_TIMEOUT`/`IO_TIMEOUT` above are 5 s); a download's is to
/// survive a slow link, so these match github.rs's — 10 s to connect, 30 s per socket op, which
/// detects a stall without capping the total transfer time of a multi-GB asset.
const DL_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DL_IO_TIMEOUT: Duration = Duration::from_secs(30);
/// Idle connections kept per host, sized past install.rs's 8-worker pool plus slack — same
/// reasoning (and the same number) as github.rs's `POOL_PER_HOST`: below the worker count, every
/// moment two workers are between files the pool closes a connection and the next file pays a full
/// DNS+TCP+TLS handshake again.
const DL_POOL_PER_HOST: usize = 12;

/// A mirror as a `Downloader`: a plain static file host, addressed by CONTENT.
///
/// The layout is rooted at the host and split by payload:
/// ```text
/// <base>/<payload>/manifest.json          and manifest.json.minisig beside it
/// <base>/<payload>/blobs/<sha256>         no extension, no tag directory
/// ```
/// There is no tag directory on purpose. A tag path is a SECOND, unsigned name for a release, and
/// the signed manifest already carries `version` — so a client never needs a release index to
/// update, and a mirror can never advertise a release whose own manifest does not name it.
///
/// **This type cannot carry a token, structurally.** It has no such field, and that is the point
/// rather than an omission: the authenticated GitHub path keys on `token.is_some()` and then waits
/// for a 302 to pre-signed storage, which a static file host never sends — so a mirror holding a
/// token could only ever hang or fail obscurely. A field "for symmetry" would make that state
/// merely unlikely; having none makes it unrepresentable. Nothing here ever sets `Authorization`.
///
/// Every request goes through `transport::fetch`, so ureq's own auto-follow is never handed a
/// `Location` header this process did not write — see that module's doc for why. A mirror's index
/// and every URL it names are the least trusted input the launcher reads.
pub struct Mirror {
    /// Base URL, normalized (no trailing slash) — `config::normalize_mirror_url`'s output, which
    /// is the only shape that ever reaches settings.
    base: String,
    /// The payload directory: `trust::Payload::id()`.
    payload: &'static str,
    agent: ureq::Agent,
    /// The VERIFIED document pair, fetched once and kept — see `documents`.
    docs: std::sync::Mutex<Option<Arc<Documents>>>,
}

/// A mirror's release, as the two documents it consists of: the payload manifest and its detached
/// signature.
///
/// Only ever produced by `Mirror::documents`, which verifies before it returns — so holding one of
/// these IS the statement that these bytes carry a signature by a key we pinned. That is why the
/// fields are private to this file and there is no constructor beside it.
struct Documents {
    manifest: Vec<u8>,
    sig: String,
}

impl Mirror {
    pub fn new(base: &str, payload: Payload) -> Self {
        Self::with_agent(base, payload, download_agent())
    }

    /// `new` with the agent injected. In production `new` is its only caller, and there is exactly
    /// one right agent. It is `pub(crate)` for the TESTS in other modules: `download_agent()` is
    /// https-only and a loopback listener cannot speak TLS, so every end-to-end test of this
    /// backend — here and in the modules that consume it, self-update among them — swaps the agent
    /// and keeps every other field the real thing, the absence of anywhere to put a token included.
    pub(crate) fn with_agent(base: &str, payload: Payload, agent: ureq::Agent) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            payload: payload.id(),
            agent,
            docs: std::sync::Mutex::new(None),
        }
    }

    fn doc_url(&self, name: &str) -> String {
        doc_url(&self.base, self.payload, name)
    }

    /// The name of the detached signature published beside the payload manifest.
    fn sig_name() -> String {
        format!("{}{}", crate::engine::MANIFEST_ASSET, crate::trust::SIG_SUFFIX)
    }

    fn blob_url(&self, sha256: &str) -> String {
        blob_url(&self.base, self.payload, sha256)
    }

    /// Where an `Asset` this backend is handed actually lives.
    ///
    /// The rule is the whole of content addressing: a 64-lowercase-hex name is an entry's CONTENT
    /// HASH (which is what `Downloader::content_addressed` promises a caller synthesized), so it
    /// resolves to a blob; anything else is one of the release documents at the payload root. An
    /// asset belonging to some OTHER backend's release — a GitHub asset name, say — matches neither
    /// shape as a hash and would be looked for as a document, so it is refused by the fetch itself
    /// rather than silently mis-addressed. `Asset::url`/`browser_download_url` are ignored
    /// entirely: a mirror derives its own URLs and never follows one a release index named.
    fn url_of(&self, name: &str) -> String {
        if is_content_hash(name) {
            self.blob_url(name)
        } else {
            self.doc_url(name)
        }
    }

    /// One GET, redirects driven by `transport::fetch`, never carrying credentials.
    fn get(&self, url: &str, range: Option<&str>) -> Result<ureq::Response> {
        transport::fetch(&self.agent, url, |req, _same_origin| {
            let req = req.set("User-Agent", UA);
            match range {
                Some(v) => req.set("Range", v),
                None => req,
            }
        })
        .map_err(net_err_fetch)
    }

    /// One bounded document from the payload root.
    ///
    /// `take(max + 1)` rather than trusting Content-Length, exactly as github.rs does: the length
    /// header is the peer's claim, and a host that intends to exhaust this process's memory is not
    /// going to declare it.
    fn read_doc(&self, name: &str, max: u64) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        self.get(&self.doc_url(name), None)?
            .into_reader()
            .take(max + 1)
            .read_to_end(&mut buf)?;
        if buf.len() as u64 > max {
            anyhow::bail!("{name} is larger than the {max} bytes allowed for it");
        }
        Ok(buf)
    }

    /// The payload's manifest and its detached signature, fetched once, VERIFIED before either is
    /// looked at, and remembered.
    ///
    /// The check happens HERE, not in `fetch_release`, because this is where the bytes first exist:
    /// `fetch_release` reads `version` out of the document to name the release, and reading
    /// anything at all out of an unauthenticated document is the thing signing exists to stop. It
    /// used to pull `version` out with a `serde_json::Value` before a single check had run, on the
    /// least trusted input the launcher reads.
    ///
    /// It is also the only place that can cache the verified PAIR, which is what makes the tag this
    /// backend reports and the manifest the installer later verifies provably the same bytes — and
    /// what removes a second wire fetch of the signature as a side effect.
    ///
    /// Signature only, deliberately NOT `trust::accept`: the serial floor is a `Settings` value and
    /// a `Mirror` has no settings. That split is right — "we produced these bytes" is a fact about
    /// the document, and "this is the release you asked for, and it is not one we have already
    /// moved past" is a fact about this machine. `engine::manifest_of` asks the second one, over
    /// the very bytes this returns, and re-asks the first: one Ed25519 check over a ≤16 MiB
    /// document is microseconds to a few milliseconds, and a backend that could tell the trust gate
    /// to skip itself is exactly the seam an attacker wants. The double check is the cheaper of the
    /// two options.
    fn documents(&self) -> Result<Arc<Documents>> {
        if let Some(d) = self.docs.lock().unwrap().as_ref() {
            return Ok(d.clone());
        }
        let manifest = self.read_doc(crate::engine::MANIFEST_ASSET, crate::trust::MAX_DOC_BYTES)?;
        let sig = sig_text(self.read_doc(&Self::sig_name(), crate::trust::MAX_SIG_BYTES)?)?;
        crate::trust::verify(&manifest, &sig).map_err(anyhow::Error::new).with_context(|| {
            format!("verifying the mirror's {}", crate::engine::MANIFEST_ASSET)
        })?;
        let docs = Arc::new(Documents { manifest, sig });
        *self.docs.lock().unwrap() = Some(docs.clone());
        Ok(docs)
    }
}

/// The download agent. Same redirect and scheme policy as the probe's (`probe_agent`) and for the
/// same reasons — read its comments — with the download path's own timeouts and connection pool.
fn download_agent() -> ureq::Agent {
    ureq::builder()
        .timeout_connect(DL_CONNECT_TIMEOUT)
        .timeout_read(DL_IO_TIMEOUT)
        .timeout_write(DL_IO_TIMEOUT)
        .max_idle_connections_per_host(DL_POOL_PER_HOST)
        .redirects(0)
        .https_only(true)
        .build()
}

/// A mirror's fetch failure as an anyhow chain rooted at a typed `NetKind`.
///
/// Rooting it is load-bearing twice over: source failover advances on `NetKind::Transport`, and
/// `install::transient_net_failure` decides retries on the same type — a bare string error would
/// silently disable both. A sibling of github.rs's `net_err_fetch` rather than a shared copy,
/// because what may be SAID differs: no response-body snippet (a mirror's error body is arbitrary
/// content from the least trusted host the launcher talks to) and no URL (ureq's transport errors
/// carry the one they were fetching, which would put a full asset URL in the UI's detail line).
fn net_err_fetch(e: FetchError) -> anyhow::Error {
    match e {
        FetchError::Http(ureq::Error::Status(code, _)) => {
            anyhow::Error::new(NetKind::Status(code)).context(format!("HTTP {code} from the mirror"))
        }
        FetchError::Http(ureq::Error::Transport(t)) => anyhow::Error::new(NetKind::Transport)
            .context(format!("transport error from the mirror: {}", t.kind())),
        FetchError::TooManyRedirects => anyhow::Error::new(NetKind::Transport)
            .context(format!("too many redirects (max {})", transport::MAX_REDIRECTS)),
        FetchError::BadRedirect(reason) => anyhow::Error::new(NetKind::Transport).context(reason),
    }
}

impl Downloader for Mirror {
    /// Yes: this backend has no release index at all, and an entry's hash is its address.
    fn content_addressed(&self) -> bool {
        true
    }

    /// The one release a mirror serves for this payload, as a synthetic `Release` carrying the two
    /// documents the trust gate reads. It is a REAL request — the manifest is fetched here — which
    /// is exactly what makes this the probe that source failover needs: a mirror that is
    /// unreachable, or does not carry this payload, says so before anything downstream believes it
    /// was opened.
    ///
    /// `repo` is ignored: the base URL already names the host and the payload directory already
    /// names what is served, so there is no owner/name to resolve. `tag` is not ignored, but it
    /// cannot be RESOLVED either — there is no tag directory to look in (see the type's doc). It is
    /// instead CHECKED against the version the served manifest names, so "install the release the
    /// UI showed me" gets a truthful answer from a mirror that has since moved on, rather than
    /// quietly installing a different one.
    fn fetch_release(&self, _repo: &str, tag: Option<&str>) -> Result<Release> {
        let docs = self.documents()?;
        // `Manifest::parse`, the STRICT reader, over bytes `documents` has already verified. The
        // ad-hoc `serde_json::Value` read this replaces did neither: it ran a parser over an
        // unauthenticated document, and it was a second, permissive reader of the one format that
        // most needs exactly one. A document this reader refuses is a document the installer would
        // refuse too, so refusing it here is the earlier half of the same answer.
        let version = Manifest::parse(&docs.manifest)?.version;
        let tag_name = tag_of(&version);
        if let Some(want) = tag {
            if want != tag_name {
                // A refusal about THIS source, not about the release: rooted at a status so the
                // walk falls through to one that does serve it.
                return Err(anyhow::Error::new(NetKind::Status(404)).context(format!(
                    "the mirror serves {tag_name}, not {want}"
                )));
            }
        }
        let doc = |name: &str, size: u64| Asset {
            name: name.to_string(),
            // Both empty, and never read: `url_of` derives every URL from the name. An asset this
            // backend produced must not carry a second address that could disagree with it.
            url: String::new(),
            browser_download_url: String::new(),
            size,
        };
        Ok(Release {
            tag_name,
            assets: vec![
                doc(crate::engine::MANIFEST_ASSET, docs.manifest.len() as u64),
                doc(&Self::sig_name(), docs.sig.len() as u64),
            ],
            body: None,
            draft: false,
            prerelease: false,
        })
    }

    /// A mirror publishes exactly one current release per payload — there is no history to list,
    /// because there is no tag directory to list it from. Reported as the one-element list that is,
    /// rather than as an error: it is a true answer, and a manufactured failure would make a
    /// perfectly healthy mirror look broken.
    fn fetch_releases(&self, repo: &str) -> Result<Vec<Release>> {
        Ok(vec![self.fetch_release(repo, None)?])
    }

    /// A whole asset in memory, bounded at the size the CALLER declared for it — `asset.size`.
    ///
    /// On this backend that field is never a release index's claim, because there is no release
    /// index. An `Asset` a mirror is handed is one of exactly two things: a payload entry
    /// SYNTHESIZED by `install::Resolved::asset_for`, whose `size` is the signed manifest's declared
    /// size for that entry; or one of the two documents `fetch_release` produced itself, whose
    /// `size` is a length this backend measured. Either way it is the caller's knowledge of how
    /// long the answer may be, and an unbounded read here would hand the least trusted host in the
    /// system this process's memory on a path where the correct length was known all along. So the
    /// ceiling is per-entry, not a constant — one number for a 2 KB text file and a multi-hundred-MB
    /// VPK would have to be the larger, which bounds nothing useful for the smaller.
    ///
    /// Both release documents are sized by `fetch_release` from the bytes it has already read, so
    /// this is exact for them too.
    fn download(&self, asset: &Asset) -> Result<Vec<u8>> {
        self.download_limited(asset, asset.size)
    }

    /// `download` with a hard ceiling, for bytes whose size is a trust input. Overridden rather
    /// than left to the trait's read-then-check default: that default is honest for an in-memory
    /// double, and a mirror is the most distrusted peer in the system — the ceiling has to bound
    /// the READ, not describe it afterwards. `download` above is this with the asset's own declared
    /// size as `max`, so the two cannot drift into two ways of reading a body.
    fn download_limited(&self, asset: &Asset, max: u64) -> Result<Vec<u8>> {
        // BOTH release documents are already in hand and already bounded (`documents`) — and they
        // are the verified pair, so the trust gate downstream is handed exactly the bytes the tag
        // was read from rather than a second fetch that a host is free to answer differently. The
        // caller's own ceiling still applies, since it may be tighter than the one they were
        // fetched under.
        let buf = if asset.name == crate::engine::MANIFEST_ASSET {
            self.documents()?.manifest.clone()
        } else if asset.name == Self::sig_name() {
            self.documents()?.sig.clone().into_bytes()
        } else {
            let mut buf = Vec::new();
            self.get(&self.url_of(&asset.name), None)?
                .into_reader()
                .take(max + 1)
                .read_to_end(&mut buf)?;
            buf
        };
        if buf.len() as u64 > max {
            anyhow::bail!("{} is larger than the {max} bytes allowed for it", asset.name);
        }
        Ok(buf)
    }

    /// Stream to `dest`, resuming from `resume_from`. The resume/hash/write half is
    /// `downloader::stream_to_file`, shared with the GitHub backend so the two cannot drift into
    /// two sets of resume rules; all that differs is the request below.
    fn download_to(
        &self,
        asset: &Asset,
        dest: &std::path::Path,
        resume_from: u64,
        progress: crate::downloader::ChunkProgress,
    ) -> Result<(u64, String)> {
        let url = self.url_of(&asset.name);
        crate::downloader::stream_to_file(
            dest,
            resume_from,
            |prefix| {
                let range = prefix.map(|p| format!("bytes={p}-"));
                Ok(transport::body_of(self.get(&url, range.as_deref())?))
            },
            progress,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mirror(url: &str, measured: bool) -> Source {
        // No payloads: an entry that advertises nothing is read permissively, so these tests keep
        // measuring what they were written to measure. `carries_only` is for the cases that are
        // ABOUT a mirror holding some payloads and not others.
        Source::Mirror { url: url.to_string(), enabled: true, measured, payloads: Vec::new() }
    }

    fn carries_only(url: &str, payloads: &[&str]) -> Source {
        Source::Mirror {
            url: url.to_string(),
            enabled: true,
            measured: true,
            payloads: payloads.iter().map(|p| p.to_string()).collect(),
        }
    }

    /// `rebuild` takes what the signed list said; these build one entry of it.
    fn host(url: &str) -> signed::Host {
        signed::Host { url: url.to_string(), payloads: Vec::new() }
    }

    fn host_with(url: &str, payloads: &[&str]) -> signed::Host {
        signed::Host {
            url: url.to_string(),
            payloads: payloads.iter().map(|p| p.to_string()).collect(),
        }
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
                Source::Mirror { url, enabled, payloads, .. } => {
                    Source::Mirror { url, enabled, measured: true, payloads }
                }
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
            &[host("https://slow"), host("https://new"), host("https://fast")],
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
        let out = rebuild(&existing, &[host("https://kept")]);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|s| s.url() != Some("https://gone")));
    }

    /// A mirror is measured through a payload it actually holds.
    ///
    /// `measure` always asks about the source repo's payload — `mod` — so a fixed answer meant a
    /// game-only mirror was timed against a `mod/` directory it does not have: 404, painted
    /// UNUSABLE, dropped from `any_healthy`, while serving the 7.9 GiB payload perfectly.
    #[test]
    fn a_mirror_is_measured_through_a_payload_it_carries() {
        let game_only = carries_only("https://g", &["game"]);
        assert_eq!(measurable(&game_only, Some(Payload::Mod)), Some(Payload::Game));
        // …and the requested one still wins wherever the host has it, so the ordinary case measures
        // exactly what will be downloaded.
        let both = carries_only("https://b", &["mod", "game"]);
        assert_eq!(measurable(&both, Some(Payload::Mod)), Some(Payload::Mod));
    }

    /// An entry advertising nothing predates the field. Reading that as "carries nothing" would
    /// have made an upgrade silently unmeasurable — and unusable — for every mirror already saved.
    #[test]
    fn a_mirror_that_advertises_nothing_is_trusted_not_excluded() {
        let legacy = mirror("https://old", true);
        assert_eq!(measurable(&legacy, Some(Payload::Game)), Some(Payload::Game));
        assert!(legacy.carries(Payload::Mod) && legacy.carries(Payload::Game));
    }

    /// A payload kind added after this build ships is not a directory it can name, so there is
    /// nothing to time — and inventing a path for it would 404 a mirror that is perfectly healthy.
    #[test]
    fn a_mirror_carrying_only_unknown_payloads_is_not_measurable() {
        let future = carries_only("https://f", &["something-new"]);
        assert_eq!(measurable(&future, Some(Payload::Mod)), None);
        assert!(!future.carries(Payload::Mod));
    }

    /// Which payloads a host carries is the PUBLISHER's statement about it, not a local preference,
    /// so a refresh must re-read it — while flags and rank still survive by URL.
    #[test]
    fn rebuild_refreshes_payloads_but_keeps_flags() {
        let existing = vec![Source::Primary, carries_only("https://m", &["mod"])];
        let out = rebuild(&existing, &[host_with("https://m", &["mod", "game"])]);
        let m = out.iter().find(|s| s.url() == Some("https://m")).expect("kept by URL");
        assert!(m.carries(Payload::Game), "the list now says it carries the game");
        assert!(matches!(m, Source::Mirror { measured: true, .. }), "and it is still measured");
    }

    /// A URL that fails the https-only check must be refused as an ordinary error, never a panic
    /// — including the UNC shape (`\\host\share`), which Windows treats as an implicit SMB target
    /// and would leak this machine's NTLMv2 hash to whatever answers there the moment it is
    /// touched. Goes through `fetch_list_from_mirror` itself (not just `probe_agent` in
    /// isolation), since that is the real call site a published mirror's base URL reaches — and
    /// with the REAL agent, since `probe_agent`'s `https_only` flag IS the check under test.
    #[test]
    fn mirror_fetch_refuses_non_https_urls_cleanly() {
        for base in [
            "http://mirror.example",
            "file:///etc/passwd",
            "\\\\attacker\\share",
            "//attacker/share",
        ] {
            let result = fetch_list_from_mirror(&probe_agent(), base, 0);
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

    // ---- the published list, and the signature that makes it worth obeying ----

    /// Every payload the format defines, as the producer spells them.
    const ALL: &str = r#"["mod", "launcher", "game"]"#;

    /// The document exactly as `generate_mirror_list.py` renders it — the producer's literal field
    /// names, values and framing, never a Rust struct serialized back out. That is what makes these
    /// tests able to notice a cross-repo rename: `"payload_id": "mirrors"` here is compared against
    /// `Payload::Mirrors.id()` by the code under test, so a typo on either side fails below rather
    /// than in the field, where the symptom would be a mirror list nobody can read and no error
    /// anywhere.
    fn list_doc(payload_id: &str, serial: u64, mirrors: &str) -> String {
        format!(
            "{{\n  \"format\": 1,\n  \"payload_id\": \"{payload_id}\",\n  \"serial\": {serial},\n  \
             \"signed_at\": \"2026-09-01T11:00:00Z\",\n  \"mirrors\": [{mirrors}]\n}}\n"
        )
    }

    /// One registration. `payloads` is raw array text so a test can publish one the format does not
    /// describe.
    fn entry(name: &str, url: &str, payloads: &str) -> String {
        format!(r#"{{"base_url": "{url}", "name": "{name}", "country": "FI", "payloads": {payloads}}}"#)
    }

    /// A mirror serving the two documents at its ROOT — either of them optional, so a test can omit
    /// the signature or the list itself.
    fn list_server(doc: Option<Vec<u8>>, sig: Option<String>) -> crate::test_http::TestServer {
        use crate::test_http::{Canned, TestServer};
        TestServer::start(move |_port| {
            let mut routes = std::collections::HashMap::new();
            if let Some(d) = doc {
                routes.insert("/mirrors.json", Canned::body(d));
            }
            if let Some(s) = sig {
                routes.insert("/mirrors.json.minisig", Canned::body(s.into_bytes()));
            }
            routes
        })
    }

    /// `signed::verify` over a document this suite's key really signed — the shortcut every test
    /// below that has nothing to prove about the network takes.
    fn verify_signed(doc: &str, floor: u64) -> Result<signed::SignedList> {
        signed::verify(doc.as_bytes(), &crate::trust::testing::test_sig(doc.as_bytes()), floor)
    }

    /// The whole accept path over a real socket: the producer's document and its detached signature
    /// fetched from a mirror's root, verified, and turned into what a refresh applies.
    #[test]
    fn a_signed_list_from_a_mirror_is_accepted() {
        let doc = list_doc(
            "mirrors",
            5,
            &format!(
                "{}, {}",
                entry("phx-fi-1", "https://fi1.example", ALL),
                entry("phx-ru-1", "https://ru1.example", r#"["mod"]"#)
            ),
        );
        let sig = crate::trust::testing::test_sig(doc.as_bytes());
        let server = list_server(Some(doc.into_bytes()), Some(sig));
        let base = format!("http://127.0.0.1:{}", server.port);

        let list = fetch_list_from_mirror(&test_agent(), &base, 0)
            .expect("a correctly signed list must verify")
            .expect("…and must never read as \"this host publishes no list\"");
        let urls: Vec<&str> = list.hosts().iter().map(|h| h.url.as_str()).collect();
        assert_eq!(urls, ["https://fi1.example", "https://ru1.example"]);
        assert_eq!(list.serial(), 5, "the number the caller ratchets the floor with");
        // BOTH documents crossed the wire, from the mirror's ROOT — there is no payload directory
        // for a list that describes the hosts themselves — and the signature was fetched rather
        // than assumed absent.
        assert_eq!(server.hits("/mirrors.json"), 1);
        assert_eq!(server.hits("/mirrors.json.minisig"), 1);
    }

    /// A byte changed under a signature that was made over the honest bytes. The assertion that
    /// matters is `Err` and specifically NOT `Ok(None)`: the caller counts `Ok(None)` as a mirror
    /// positively answering "there is no list", which suppresses the registry's own error — so a
    /// refusal that could pass for one would turn a tampering attempt into silence.
    #[test]
    fn a_tampered_list_is_a_refusal_not_an_answer() {
        let doc = list_doc("mirrors", 5, &entry("phx-fi-1", "https://fi1.example", ALL));
        let sig = crate::trust::testing::test_sig(doc.as_bytes());
        let tampered = doc.replace("https://fi1.example", "https://evil.example");
        assert_ne!(tampered, doc, "the fixture must actually differ");

        let server = list_server(Some(tampered.into_bytes()), Some(sig));
        let base = format!("http://127.0.0.1:{}", server.port);
        let got = fetch_list_from_mirror(&test_agent(), &base, 0);
        assert!(got.is_err(), "a rewritten host must be refused, got {got:?}");
    }

    /// A signature this launcher is perfectly happy with, over a document that is not a mirror
    /// list. Our own key signs four payload lines, so "we produced this" cannot be the last
    /// question asked — and the document is shaped like a mirror list here on purpose, so that what
    /// refuses it is the `payload_id` check and not the parser tripping over a foreign shape.
    #[test]
    fn a_valid_signature_over_another_payloads_document_is_refused() {
        use crate::trust::TrustError;
        let doc = list_doc("mod", 5, &entry("phx-fi-1", "https://fi1.example", ALL));
        let err = verify_signed(&doc, 0).expect_err("a mod document is not a mirror list");
        assert!(
            matches!(
                err.downcast_ref::<TrustError>(),
                Some(TrustError::WrongPayload { expected: "mirrors", found: Some(got) }) if got == "mod"
            ),
            "expected a payload refusal, got: {err:#}"
        );
    }

    /// The rollback ratchet, on the one payload where a rollback rewrites the download sources
    /// rather than the game: a mirror can always serve an older list it once held a perfectly valid
    /// signature for, and that signature never stops verifying.
    #[test]
    fn a_list_older_than_this_machine_has_accepted_is_refused() {
        use crate::trust::TrustError;
        let doc = list_doc("mirrors", 5, &entry("phx-fi-1", "https://fi1.example", ALL));
        assert_eq!(
            verify_signed(&doc, 5).expect("the same list, checked again").serial(),
            5,
            "every launch re-reads the current list; refusing it would refuse the ordinary case"
        );
        let err = verify_signed(&doc, 9).expect_err("serial 5 is below a floor of 9");
        assert!(
            matches!(
                err.downcast_ref::<TrustError>(),
                Some(TrustError::StaleSerial { payload: "mirrors", found: Some(5), floor: 9 })
            ),
            "expected a stale-serial refusal, got: {err:#}"
        );
    }

    /// "Unsigned" and "absent" must not be spelled the same way. A mirror that could answer with an
    /// unsigned list and have it counted as "there is no list here" would still be choosing what
    /// the caller does; a mirror that genuinely carries no list is answering a question.
    #[test]
    fn an_unsigned_list_is_refused_while_a_missing_one_is_simply_absent() {
        let doc = list_doc("mirrors", 5, &entry("phx-fi-1", "https://fi1.example", ALL));
        let server = list_server(Some(doc.into_bytes()), None); // the list, with no signature beside it
        let base = format!("http://127.0.0.1:{}", server.port);
        let got = fetch_list_from_mirror(&test_agent(), &base, 0);
        assert!(got.is_err(), "an unsigned list must not pass for an absent one: {got:?}");

        let bare = list_server(None, None); // 404s both
        let base = format!("http://127.0.0.1:{}", bare.port);
        let got = fetch_list_from_mirror(&test_agent(), &base, 0);
        assert!(matches!(&got, Ok(None)), "a 404 on the list is an answer: {got:?}");
    }

    /// Rules this reader adds ON TOP of the signature, each proven over a document our own key
    /// really signed: "we produced this" is not "this is usable", and every refusal here leaves the
    /// mirrors already in settings alone rather than acting on a document half-understood.
    #[test]
    fn a_signed_document_this_reader_cannot_act_on_is_still_refused() {
        let good = entry("phx-fi-1", "https://fi1.example", ALL);
        // A format from the future. Refused rather than read optimistically — the cost is a list
        // that stops updating until the launcher does, and the alternative is guessing at the one
        // document that decides where every future download comes from.
        let future = list_doc("mirrors", 5, &good).replace("\"format\": 1", "\"format\": 2");
        assert!(verify_signed(&future, 0).is_err(), "an unknown format must not be read");

        // A `base_url` the client would have to rewrite before using it. The published string has
        // to BE the string that gets fetched, or the URL a reviewer approved and the URL this
        // machine downloads from are two different things.
        for bad in ["https://fi1.example/", " https://fi1.example", "fi1.example"] {
            let doc = list_doc("mirrors", 5, &entry("phx-fi-1", bad, ALL));
            assert!(verify_signed(&doc, 0).is_err(), "{bad:?} is not a canonical base_url");
        }

        // An entry that serves nothing, or names itself nothing. One bad entry refuses the WHOLE
        // document: this list is signed, so a broken one is our producer having shipped what it
        // promises it cannot, and dropping it quietly is the silent-non-publish failure the
        // registry repo exists to make impossible.
        let doc = list_doc("mirrors", 5, &entry("phx-fi-1", "https://fi1.example", "[]"));
        assert!(verify_signed(&doc, 0).is_err(), "a mirror that serves no payload is never used");
        let doc = list_doc("mirrors", 5, &entry("", "https://fi1.example", ALL));
        assert!(verify_signed(&doc, 0).is_err(), "a nameless registration is not one");
    }

    /// The three outcomes `apply` must keep apart — the whole of this module's failure semantics.
    ///
    /// A verified EMPTY list is the publisher stating there are no mirrors, and it replaces the
    /// set. A source that could not be asked, and a document that was refused, both leave the set
    /// exactly as it was: "could not ask" is not "there are none", and a refused document is not an
    /// empty one — collapsing that second pair is how a tampered answer would wipe a user's mirrors
    /// with nothing anywhere looking wrong.
    #[test]
    fn a_verified_empty_list_replaces_the_set_while_silence_leaves_it_alone() {
        let existing = vec![Source::Primary, mirror("https://kept", true)];

        let empty = verify_signed(&list_doc("mirrors", 5, ""), 0).expect("an empty list is valid");
        let applied = apply(&existing, Ok(Some(empty)));
        assert_eq!(applied.sources, vec![Source::Primary], "an empty list is an instruction");
        assert_eq!(applied.serial, Some(5), "…and it still ratchets — we accepted a document");
        assert!(applied.error.is_none());

        for silence in [Ok(None), Err(anyhow::anyhow!("the source is dark"))] {
            let applied = apply(&existing, silence);
            assert_eq!(applied.sources, existing, "silence must never read as \"there are none\"");
            assert_eq!(applied.serial, None, "nothing was accepted, so nothing may raise the floor");
        }
    }

    // ---- the probe, against the layout a mirror really serves ----

    /// `probe_agent()` minus `https_only`, which a loopback listener cannot satisfy — the same
    /// swap, for the same reason, as `test_mirror` below. Every other setting is the real one,
    /// `redirects(0)` included, so `transport::fetch` still drives the chain.
    fn test_agent() -> ureq::Agent {
        ureq::builder()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout_read(IO_TIMEOUT)
            .timeout_write(IO_TIMEOUT)
            .redirects(0)
            .build()
    }

    /// A manifest as a mirror serves it. Text rather than a hand-built struct so every test below
    /// goes through the real `Manifest::parse` — bundle invariants included, which is what makes a
    /// fixture that could not exist in a release fail here instead of quietly proving nothing.
    fn manifest_doc(files: &str, bundles: &str) -> String {
        format!(
            r#"{{"schema":3,"payload_id":"mod","version":"1.4.2","files":[{files}],"bundles":[{bundles}]}}"#
        )
    }

    fn leak(path: String) -> &'static str {
        Box::leak(path.into_boxed_str()) // routes are &'static
    }

    /// The whole probe over a real socket: the manifest names the blobs, the biggest one is pulled
    /// ranged, and what comes back is a throughput figure — the only number `rank` sorts on.
    ///
    /// The mirror here serves nothing at `releases.json`, which is the point: that document does
    /// not exist in this layout, and a probe that still asked for it measured every real mirror as
    /// dead.
    #[test]
    fn a_probe_measures_a_content_addressed_mirror() {
        use crate::test_http::{Canned, TestServer};
        let (bundle, member, loose) = ("b".repeat(64), "c".repeat(64), "d".repeat(64));
        let doc = manifest_doc(
            &format!(
                r#"{{"name":"notes.txt","dest":"game/notes.txt","sha256":"{loose}","size":24}},
                   {{"dest":"game/big.vpk","sha256":"{member}","size":65536}}"#
            ),
            &format!(
                r#"{{"name":"payload.phxb","codec":"zstd","psize":40960,
                     "psha256":"{bundle}","size":65536,"members":["{member}"]}}"#
            ),
        );
        let packed: Vec<u8> = (0..40960u32).map(|i| i as u8).collect();
        let (bundle_path, loose_path) =
            (leak(format!("/mod/blobs/{bundle}")), leak(format!("/mod/blobs/{loose}")));
        let body = packed.clone();
        let server = TestServer::start(move |_port| {
            let mut routes = std::collections::HashMap::new();
            routes.insert("/mod/manifest.json", Canned::body(doc.into_bytes()));
            routes.insert(bundle_path, Canned::body(body));
            routes.insert(loose_path, Canned::body(b"twenty-four bytes, here.".to_vec()));
            routes
        });

        let base = format!("http://127.0.0.1:{}", server.port);
        let source = mirror(&base, false);
        let p = probe_mirror(&test_agent(), &source, &base, Some(Payload::Mod));

        assert!(p.error.is_none(), "a healthy mirror must not report one: {:?}", p.error);
        assert!(p.healthy());
        assert!(p.bytes_per_sec.is_some(), "throughput is the measurement, not latency");
        assert!(p.latency_ms.is_some());
        // named by the manifest it serves, exactly as the download backend names it
        assert_eq!(p.tag.as_deref(), Some("v1.4.2"));
        // asked for a bounded chunk and got a 206 — the answer a resume across a dropped
        // connection depends on, and the reason the request is `bytes=0-N` and not `bytes=0-`
        assert_eq!(
            server.saw_range(bundle_path).as_deref(),
            Some(format!("bytes=0-{}", PROBE_BYTES - 1).as_str())
        );
        assert!(p.range_ok);
        assert_eq!(server.hits(bundle_path), 1);
        // the exact path the probe used to ask for, and the reason this test exists
        assert_eq!(server.hits("/releases.json"), 0, "no release index exists to ask for");
    }

    /// SIZE, over a real transfer: the 24-byte file is the one a throttled link would serve
    /// flawlessly, so a probe that picked it would paint the broken source green. It is not merely
    /// unranked — it is never requested at all.
    #[test]
    fn the_probe_never_times_a_small_file_when_a_large_one_exists() {
        use crate::test_http::{Canned, TestServer};
        let (bundle, member, loose) = ("b".repeat(64), "c".repeat(64), "d".repeat(64));
        let doc = manifest_doc(
            &format!(
                r#"{{"name":"notes.txt","dest":"game/notes.txt","sha256":"{loose}","size":24}},
                   {{"dest":"game/big.vpk","sha256":"{member}","size":65536}}"#
            ),
            &format!(
                r#"{{"name":"payload.phxb","codec":"zstd","psize":40960,
                     "psha256":"{bundle}","size":65536,"members":["{member}"]}}"#
            ),
        );
        let (bundle_path, loose_path) =
            (leak(format!("/mod/blobs/{bundle}")), leak(format!("/mod/blobs/{loose}")));
        let server = TestServer::start(move |_port| {
            let mut routes = std::collections::HashMap::new();
            routes.insert("/mod/manifest.json", Canned::body(doc.into_bytes()));
            routes.insert(bundle_path, Canned::body(vec![7u8; 40960]));
            routes.insert(loose_path, Canned::body(b"twenty-four bytes, here.".to_vec()));
            routes
        });

        let base = format!("http://127.0.0.1:{}", server.port);
        let p = probe_mirror(&test_agent(), &mirror(&base, false), &base, Some(Payload::Mod));
        assert!(p.healthy());
        assert_eq!(server.hits(loose_path), 0, "the small file must never be the measurement");
        assert_eq!(server.hits(bundle_path), 1);
    }

    /// Which entry can be timed at all, decided the way `install::build_acqs` decides what to
    /// download. The trap this pins: the BIGGEST thing in the document above is the 64 KiB member,
    /// and it has no blob of its own — its bytes exist only inside the bundle — so choosing on size
    /// alone would 404 against a mirror serving exactly what it should, and rank it dead.
    #[test]
    fn the_probe_picks_the_biggest_entry_that_has_a_blob_of_its_own() {
        let (bundle, member, loose) = ("b".repeat(64), "c".repeat(64), "d".repeat(64));
        let bundled = manifest_doc(
            &format!(
                r#"{{"name":"notes.txt","dest":"game/notes.txt","sha256":"{loose}","size":24}},
                   {{"dest":"game/big.vpk","sha256":"{member}","size":65536}}"#
            ),
            &format!(
                r#"{{"name":"payload.phxb","codec":"zstd","psize":40960,
                     "psha256":"{bundle}","size":65536,"members":["{member}"]}}"#
            ),
        );
        let m = Manifest::parse(bundled.as_bytes()).expect("a valid schema-3 manifest");
        assert_eq!(probe_blob(&m), Some(bundle.as_str()));

        // no bundles (a schema-2-shaped release): every entry names an asset, so the biggest of
        // those is both the honest choice and a fetchable one
        let loose_only = manifest_doc(
            &format!(
                r#"{{"name":"big.vpk","dest":"game/big.vpk","sha256":"{member}","size":9999}},
                   {{"name":"notes.txt","dest":"game/notes.txt","sha256":"{loose}","size":24}}"#
            ),
            "",
        );
        let m = Manifest::parse(loose_only.as_bytes()).expect("a valid bundle-less manifest");
        assert_eq!(probe_blob(&m), Some(member.as_str()));

        // a zero-size entry is materialized locally and stored nowhere, so there is nothing to
        // time — reported as "no blob", never as a request for a blob that cannot exist
        let empty_only = manifest_doc(
            &format!(r#"{{"name":"marker","dest":"game/marker","sha256":"{loose}","size":0}}"#),
            "",
        );
        let m = Manifest::parse(empty_only.as_bytes()).expect("a valid empty-entry manifest");
        assert_eq!(probe_blob(&m), None);
    }

    /// No manifest, no mirror. Answering that document is what proves a host serves this payload —
    /// the role `releases.json` played — so a host that cannot is reported as failed, with nothing
    /// measured for the ranking to mistake for a result.
    #[test]
    fn a_host_that_serves_no_manifest_fails_the_probe() {
        use crate::test_http::TestServer;
        let server = TestServer::start(|_port| std::collections::HashMap::new()); // 404s everything
        let base = format!("http://127.0.0.1:{}", server.port);
        let p = probe_mirror(&test_agent(), &mirror(&base, false), &base, Some(Payload::Mod));
        assert!(!p.healthy());
        assert!(p.bytes_per_sec.is_none() && p.latency_ms.is_none() && p.tag.is_none());
        let why = p.error.expect("a failed probe must say why");
        assert!(why.contains("manifest.json") && why.contains("404"), "unhelpful reason: {why}");
    }

    /// A repo this build cannot name has no `<base>/<payload>/` to look under, so there is no
    /// measurement to take — and the probe must not fall back to some other payload's directory,
    /// which would rank a source on a transfer the installer would never repeat.
    #[test]
    fn a_repo_with_no_payload_directory_is_refused_rather_than_guessed() {
        let source = mirror("https://mirror.example", false);
        let p = probe_mirror(&test_agent(), &source, "https://mirror.example", None);
        assert!(!p.healthy());
        assert!(p.error.is_some());
    }

    // ---- the download backend ----

    /// A `Mirror` over a plain-HTTP loopback server. `download_agent()` is https-only (proven
    /// separately by `mirror_fetch_refuses_non_https_urls_cleanly`, over the same builder settings)
    /// and a local listener cannot speak TLS, so the tests below swap the agent — every other field
    /// is the real thing, including the fact that there is nowhere to put a token.
    fn test_mirror(base: &str, payload: crate::trust::Payload) -> Mirror {
        Mirror::with_agent(
            base,
            payload,
            ureq::builder()
                .timeout_connect(CONNECT_TIMEOUT)
                .timeout_read(IO_TIMEOUT)
                .timeout_write(IO_TIMEOUT)
                .redirects(0) // transport::fetch drives the loop; matches download_agent()
                .build(),
        )
    }

    /// The layout, stated as URLs. Two things it pins that nothing else can: a payload entry is
    /// addressed by its CONTENT HASH with no extension, and there is NO TAG DIRECTORY anywhere —
    /// a tag path would be a second, unsigned name for a release, and the signed manifest already
    /// carries `version`.
    #[test]
    fn a_mirror_addresses_a_payload_by_hash_under_its_own_directory() {
        let hash = "a".repeat(64);
        let m = test_mirror("https://mirror.example/phx/", crate::trust::Payload::Mod);
        // the trailing slash is normalized away, so nothing downstream doubles a separator
        assert_eq!(m.doc_url("manifest.json"), "https://mirror.example/phx/mod/manifest.json");
        assert_eq!(
            m.doc_url("manifest.json.minisig"),
            "https://mirror.example/phx/mod/manifest.json.minisig"
        );
        assert_eq!(m.blob_url(&hash), format!("https://mirror.example/phx/mod/blobs/{hash}"));

        // and the resolution rule: a hash is a blob, anything else is a document beside it
        assert_eq!(m.url_of(&hash), m.blob_url(&hash));
        assert_eq!(m.url_of("manifest.json"), m.doc_url("manifest.json"));
        // a name from some OTHER backend's release index is not a hash, so it is never
        // mis-addressed as a blob — it is looked for where it plainly is not, and refused there
        assert_eq!(m.url_of("winmm.dll"), m.doc_url("winmm.dll"));
        assert!(!is_content_hash(&"A".repeat(64)), "uppercase is not the manifest's hash form");
        assert!(!is_content_hash(&"a".repeat(63)));

        // every payload gets its own directory off the same base
        let g = test_mirror("https://mirror.example/phx", crate::trust::Payload::Game);
        assert_eq!(g.blob_url(&hash), format!("https://mirror.example/phx/game/blobs/{hash}"));
    }

    /// The blob path, over a real socket — and the property the type exists to make impossible:
    /// no request it issues can carry credentials, because there is nowhere to keep them.
    #[test]
    fn a_mirror_fetches_a_blob_by_hash_and_never_authenticates() {
        use crate::downloader::{Asset, Downloader};
        use crate::test_http::{Canned, TestServer};
        let content = b"the payload bytes".to_vec();
        let hash = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&content));
        let path: &'static str =
            Box::leak(format!("/game/blobs/{hash}").into_boxed_str()); // routes are &'static
        let body = content.clone();
        let server = TestServer::start(move |_port| {
            let mut routes = std::collections::HashMap::new();
            routes.insert(path, Canned::body(body));
            routes
        });

        let m = test_mirror(&format!("http://127.0.0.1:{}", server.port), crate::trust::Payload::Game);
        let dest = std::env::temp_dir().join("phoenix-mirror-blob.bin");
        let _ = std::fs::remove_file(&dest);
        // exactly the asset install.rs synthesizes for a content-addressed source: the NAME is the
        // entry's hash, and both URL fields are empty
        let asset = Asset {
            name: hash.clone(),
            url: String::new(),
            browser_download_url: String::new(),
            size: content.len() as u64,
        };
        let (n, got) = m.download_to(&asset, &dest, 0, &mut |_, _| true).expect("blob download");
        assert_eq!(n, content.len() as u64);
        assert_eq!(got, hash, "the whole-file hash is what the caller verifies against");
        assert_eq!(std::fs::read(&dest).unwrap(), content);
        assert_eq!(server.hits(path), 1);
        assert!(!server.saw_authorization(path), "a mirror has no credentials to send");
        let _ = std::fs::remove_file(&dest);
    }

    /// `download` is bounded by the size the asset DECLARES, per entry — the manifest's signed size
    /// for a blob — and a host that sends past it is refused, not read to the end and then judged.
    /// A body exactly at the declared size still comes through whole: the bound is a ceiling on
    /// what the host may send, not a claim about what it will.
    #[test]
    fn a_mirror_refuses_a_body_longer_than_the_asset_declares() {
        use crate::downloader::{Asset, Downloader};
        use crate::test_http::{Canned, TestServer};
        let honest = b"the bytes the manifest signed for".to_vec();
        let hash = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&honest));
        // the same address, served with 100 KB tacked on past what the manifest declared
        let hostile: Vec<u8> = honest.iter().copied().chain(std::iter::repeat(b'X').take(100_000)).collect();
        let exact_path: &'static str = Box::leak(format!("/mod/blobs/{hash}").into_boxed_str());
        let long_path: &'static str = Box::leak(format!("/mod/blobs/{}", "e".repeat(64)).into_boxed_str());
        let (exact_body, long_body) = (honest.clone(), hostile);
        let server = TestServer::start(move |_port| {
            let mut routes = std::collections::HashMap::new();
            routes.insert(exact_path, Canned::body(exact_body));
            routes.insert(long_path, Canned::body(long_body));
            routes
        });
        let m = test_mirror(&format!("http://127.0.0.1:{}", server.port), crate::trust::Payload::Mod);
        // exactly what `install::Resolved::asset_for` synthesizes: the name is the hash, the size is
        // the manifest's declared size, and both URL fields are empty
        let asset = |name: &str| Asset {
            name: name.to_string(),
            url: String::new(),
            browser_download_url: String::new(),
            size: honest.len() as u64,
        };

        let err = m.download(&asset(&"e".repeat(64))).expect_err("an over-long body must be refused");
        assert!(
            format!("{err:#}").contains("larger than"),
            "the refusal must say it was the size, got: {err:#}"
        );
        assert_eq!(server.hits(long_path), 1);
        // …and a body that fits its declaration is read whole, at the exact ceiling
        assert_eq!(m.download(&asset(&hash)).expect("an honest body"), honest);
    }

    /// A resume asks for exactly the bytes it is missing, and the hash it returns still covers the
    /// WHOLE file — the prefix it inherited included. Proven over a real 206, because that is the
    /// half of `stream_to_file` a mock cannot exercise.
    #[test]
    fn a_mirror_resumes_from_the_prefix_it_is_handed() {
        use crate::downloader::{Asset, Downloader};
        use crate::test_http::{Canned, TestServer};
        let content: Vec<u8> = (0..200u8).collect();
        let hash = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&content));
        let path: &'static str = Box::leak(format!("/mod/blobs/{hash}").into_boxed_str());
        let body = content.clone();
        let server = TestServer::start(move |_port| {
            let mut routes = std::collections::HashMap::new();
            routes.insert(path, Canned::body(body));
            routes
        });

        let m = test_mirror(&format!("http://127.0.0.1:{}", server.port), crate::trust::Payload::Mod);
        let dest = std::env::temp_dir().join("phoenix-mirror-resume.bin");
        std::fs::write(&dest, &content[..80]).unwrap();
        let asset = Asset {
            name: hash.clone(),
            url: String::new(),
            browser_download_url: String::new(),
            size: content.len() as u64,
        };
        let (n, got) = m.download_to(&asset, &dest, 80, &mut |_, _| true).expect("resumed download");
        assert_eq!(server.saw_range(path).as_deref(), Some("bytes=80-"));
        assert_eq!(n, content.len() as u64);
        assert_eq!(got, hash);
        assert_eq!(std::fs::read(&dest).unwrap(), content);
        let _ = std::fs::remove_file(&dest);
    }

    /// A mirror serving a payload: its manifest, and the signature beside it.
    fn payload_server(doc: Vec<u8>, sig: Option<String>) -> crate::test_http::TestServer {
        use crate::test_http::{Canned, TestServer};
        TestServer::start(move |_port| {
            let mut routes = std::collections::HashMap::new();
            routes.insert("/mod/manifest.json", Canned::body(doc.clone()));
            if let Some(s) = sig.clone() {
                routes.insert("/mod/manifest.json.minisig", Canned::body(s.into_bytes()));
            }
            routes
        })
    }

    /// A mirror has no release index to read a tag out of, so the release it reports is NAMED by
    /// the manifest it serves — and by the very bytes that were signature-checked to get there,
    /// which is why the pair is fetched once and kept rather than fetched twice. A tag it does not
    /// serve is refused as a fact about this host (a status), so the walk moves on instead of
    /// installing something the user never saw.
    #[test]
    fn a_mirror_names_its_release_from_the_manifest_it_will_be_verified_by() {
        use crate::downloader::Downloader;
        let doc = br#"{"schema":2,"payload_id":"mod","version":"1.4.2","files":[]}"#.to_vec();
        let sig = crate::trust::testing::test_sig(&doc);
        let server = payload_server(doc.clone(), Some(sig.clone()));
        let m = test_mirror(&format!("http://127.0.0.1:{}", server.port), crate::trust::Payload::Mod);

        let release = m.fetch_release("ignored/repo", None).expect("the mirror's one release");
        assert_eq!(release.tag_name, "v1.4.2");
        let names: Vec<&str> = release.assets.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["manifest.json", "manifest.json.minisig"]);

        // BOTH documents the trust gate downloads are the ones the tag was read from, and neither
        // crossed the wire twice — which is what makes "the release this backend names" and "the
        // release the installer verifies" the same bytes rather than two fetches a host is free to
        // answer differently.
        let asset = release.asset("manifest.json").unwrap();
        assert_eq!(m.download_limited(asset, crate::trust::MAX_DOC_BYTES).unwrap(), doc);
        let asset = release.asset("manifest.json.minisig").unwrap();
        assert_eq!(
            m.download_limited(asset, crate::trust::MAX_SIG_BYTES).unwrap(),
            sig.into_bytes()
        );
        assert_eq!(server.hits("/mod/manifest.json"), 1, "the manifest is fetched once");
        assert_eq!(server.hits("/mod/manifest.json.minisig"), 1, "and so is its signature");

        // a tag this mirror does not serve is a refusal ABOUT THIS SOURCE, not about the release
        let err = m.fetch_release("ignored/repo", Some("v9.9.9")).unwrap_err();
        assert!(
            err.chain().any(|c| matches!(c.downcast_ref::<NetKind>(), Some(NetKind::Status(404)))),
            "expected a status in the chain so the source walk falls through, got: {err:#}"
        );
        // …and the tag it DOES serve resolves
        assert!(m.fetch_release("ignored/repo", Some("v1.4.2")).is_ok());
    }

    /// NO UNVERIFIED PARSE REMAINS. `fetch_release` reads `version` out of the manifest to name the
    /// release; it used to do that with an ad-hoc `serde_json::Value` BEFORE any signature check,
    /// over the least trusted document in the system. The document below is perfectly well-formed
    /// JSON and a perfectly valid manifest — the only thing wrong with it is the signature — so a
    /// reader that parsed first would answer "v1.4.2" and only notice afterwards.
    ///
    /// Nothing is cached either: a second call goes back to the wire rather than serving refused
    /// bytes out of memory, which is what stops one bad answer sticking for the process's life.
    #[test]
    fn fetch_release_verifies_before_it_parses() {
        use crate::downloader::Downloader;
        let doc = br#"{"schema":2,"payload_id":"mod","version":"1.4.2","files":[]}"#.to_vec();
        // a real signature over DIFFERENT bytes: the file is well-formed, it simply is not over this
        let sig = crate::trust::testing::test_sig(b"some other document");
        let server = payload_server(doc, Some(sig));
        let m = test_mirror(&format!("http://127.0.0.1:{}", server.port), crate::trust::Payload::Mod);

        let err = m.fetch_release("ignored/repo", None).unwrap_err();
        assert!(
            err.chain().any(|c| c.downcast_ref::<crate::minisig::SigError>().is_some()),
            "expected the signature refusal, not a parse result: {err:#}"
        );
        assert!(
            !err.chain().any(|c| c.downcast_ref::<serde_json::Error>().is_some()),
            "the document is valid JSON — a serde error here would mean it was parsed first: {err:#}"
        );

        assert!(m.fetch_release("ignored/repo", None).is_err());
        assert_eq!(server.hits("/mod/manifest.json"), 2, "refused bytes must not be remembered");
    }

    /// A payload with no signature beside it serves no release, and it is refused as a fact about
    /// THIS HOST (a status) so the walk moves on. "Unsigned" is not a weaker state a backend gets
    /// to fall back to — deleting one file would otherwise be all it took.
    #[test]
    fn a_mirror_that_publishes_no_signature_serves_no_release() {
        use crate::downloader::Downloader;
        let doc = br#"{"schema":2,"payload_id":"mod","version":"1.4.2","files":[]}"#.to_vec();
        let server = payload_server(doc, None);
        let m = test_mirror(&format!("http://127.0.0.1:{}", server.port), crate::trust::Payload::Mod);
        let err = m.fetch_release("ignored/repo", None).unwrap_err();
        assert!(
            err.chain().any(|c| matches!(c.downcast_ref::<NetKind>(), Some(NetKind::Status(404)))),
            "a missing signature is this host failing to serve the release: {err:#}"
        );
    }

    /// Every failure a mirror reports has to root a `NetKind`: source failover advances on
    /// `Transport`, and `install::transient_net_failure` decides retries on the same type. A bare
    /// string error would silently disable both, and nothing else would look wrong.
    #[test]
    fn a_mirror_failure_is_typed_so_failover_and_retries_can_read_it() {
        use crate::downloader::Downloader;
        use crate::test_http::TestServer;
        let server = TestServer::start(|_port| std::collections::HashMap::new()); // 404s everything
        let m = test_mirror(&format!("http://127.0.0.1:{}", server.port), crate::trust::Payload::Mod);
        let err = m.fetch_release("r", None).unwrap_err();
        assert!(
            err.chain().any(|c| matches!(c.downcast_ref::<NetKind>(), Some(NetKind::Status(404)))),
            "a refusal must carry its status: {err:#}"
        );

        // nothing listening at all is the Transport case — the one the source walk falls through on
        let dead = test_mirror("http://127.0.0.1:1", crate::trust::Payload::Mod);
        let err = dead.fetch_release("r", None).unwrap_err();
        assert!(
            err.chain().any(|c| matches!(c.downcast_ref::<NetKind>(), Some(NetKind::Transport))),
            "an unreachable host must be Transport: {err:#}"
        );
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
