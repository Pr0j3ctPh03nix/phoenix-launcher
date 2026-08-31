//! A minimal, single-purpose HTTP/1.1 test server. `#[cfg(test)]`-only, shared by github.rs and
//! mirror.rs's redirect-chain unit tests (see `main.rs`'s `mod test_http;`).
//!
//! It exists because what those tests prove — ureq re-checking a redirect's scheme on every hop,
//! capping the chain, and deciding per-hop whether `Authorization` survives a host change — is
//! ureq's OWN internal behavior, not code this crate writes. There is no way to prove it without a
//! real TCP round trip per hop: a mock at the `Downloader` level (as the rest of the engine tests
//! use) never exercises ureq's redirect loop at all. So this is intentionally not a general test
//! double — it understands just enough of the wire format to script a canned status/Location per
//! path and record whether `Authorization` arrived with the request that hit it.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// A scripted response for one path.
pub(crate) struct Canned {
    pub status: u16,
    /// Becomes the `Location` header when present; `None` for a terminal (non-redirect) response.
    /// Owned (not `&'static str`): a cross-host redirect target has to name this same server's
    /// PORT, which only exists once the listener has bound — a caller builds it with `format!`.
    pub location: Option<String>,
    /// The response body. Served whole for a plain request, and as a 206 `Content-Range` slice for
    /// a `Range: bytes=N-` — which is what makes a RESUME provable over a real socket rather than
    /// asserted about a mock.
    pub body: Vec<u8>,
}

impl Canned {
    pub fn redirect(location: impl Into<String>) -> Self {
        Self { status: 302, location: Some(location.into()), body: Vec::new() }
    }
    pub fn ok() -> Self {
        Self { status: 200, location: None, body: Vec::new() }
    }
    pub fn body(bytes: impl Into<Vec<u8>>) -> Self {
        Self { status: 200, location: None, body: bytes.into() }
    }
}

/// What a path's request(s) carried. One record per path, overwritten by each request except
/// `hits`, which counts them — "was this fetched once or twice" is the question a response CACHE
/// has to answer.
#[derive(Default)]
pub(crate) struct Seen {
    pub hits: u32,
    pub authorization: bool,
    pub range: Option<String>,
}

/// Runs until the test process exits — there is no shutdown handle because every caller is a
/// short-lived `#[test]` and the OS reclaims the socket on process exit; a listener nobody polls
/// again costs nothing.
pub(crate) struct TestServer {
    pub port: u16,
    hits: Arc<Mutex<HashMap<String, Seen>>>,
}

impl TestServer {
    /// Serve routes on `127.0.0.1`, built by `routes` from the port the OS just handed out — a
    /// cross-host redirect test needs to name THIS server's own port from a different host string
    /// (`http://localhost:{port}/...`), which does not exist to reference until after binding.
    /// Any path not listed answers 404.
    pub fn start(routes: impl FnOnce(u16) -> HashMap<&'static str, Canned>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test http listener");
        let port = listener.local_addr().expect("local_addr").port();
        let routes = routes(port);
        let hits = Arc::new(Mutex::new(HashMap::new()));
        let hits_bg = hits.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                handle_one(stream, &routes, &hits_bg);
            }
        });
        Self { port, hits }
    }

    /// Did the request that hit `path` carry an `Authorization` header? Panics if `path` was
    /// never hit — a test asserting on a hop that didn't happen has a wrong test, not a wrong
    /// answer of `false`.
    pub fn saw_authorization(&self, path: &str) -> bool {
        self.seen(path).authorization
    }

    /// How many requests reached `path`. Zero for a path never asked for — which, unlike the
    /// header question above, is a meaningful answer rather than a broken test.
    pub fn hits(&self, path: &str) -> u32 {
        self.hits.lock().unwrap().get(path).map_or(0, |s| s.hits)
    }

    /// The `Range` header of the last request to `path`.
    pub fn saw_range(&self, path: &str) -> Option<String> {
        self.seen(path).range
    }

    fn seen(&self, path: &str) -> Seen {
        let hits = self.hits.lock().unwrap();
        let s = hits
            .get(path)
            .unwrap_or_else(|| panic!("test server: {path} was never hit"));
        Seen { hits: s.hits, authorization: s.authorization, range: s.range.clone() }
    }
}

fn handle_one(mut stream: TcpStream, routes: &HashMap<&'static str, Canned>, hits: &Arc<Mutex<HashMap<String, Seen>>>) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone test stream"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return; // client closed without sending anything (pool probe, etc.)
    }
    // "GET /path HTTP/1.1\r\n"
    let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();

    let mut had_authorization = false;
    let mut range = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break; // end of headers (blank line) or a dropped connection
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("authorization:") {
            had_authorization = true;
        }
        if let Some(v) = lower.strip_prefix("range:") {
            range = Some(v.trim().to_string());
        }
    }
    {
        let mut seen = hits.lock().unwrap();
        let entry = seen.entry(path.clone()).or_default();
        entry.hits += 1;
        entry.authorization = had_authorization;
        entry.range = range.clone();
    }

    let (mut status, mut location, mut body) = match routes.get(path.as_str()) {
        Some(c) => (c.status, c.location.clone(), c.body.clone()),
        None => (404, None, Vec::new()),
    };
    // `bytes=N-` only — the one form `stream_to_file` ever sends. Anything else is served whole,
    // which is a legal answer (the client treats a 200 as "the range was declined").
    let mut content_range = None;
    if status == 200 && !body.is_empty() {
        if let Some(start) = range
            .as_deref()
            .and_then(|v| v.strip_prefix("bytes="))
            .and_then(|v| v.strip_suffix('-'))
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|s| *s < body.len())
        {
            content_range = Some(format!("bytes {start}-{}/{}", body.len() - 1, body.len()));
            body = body[start..].to_vec();
            status = 206;
        }
    }
    let mut resp = format!("HTTP/1.1 {status} {}\r\n", reason_phrase(status));
    if let Some(loc) = location.take() {
        resp.push_str(&format!("Location: {loc}\r\n"));
    }
    if let Some(cr) = content_range {
        resp.push_str(&format!("Content-Range: {cr}\r\n"));
    }
    resp.push_str(&format!("Content-Length: {}\r\nConnection: close\r\n\r\n", body.len()));
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.write_all(&body);
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        206 => "Partial Content",
        302 => "Found",
        404 => "Not Found",
        _ => "Other",
    }
}
