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
}

impl Canned {
    pub fn redirect(location: impl Into<String>) -> Self {
        Self { status: 302, location: Some(location.into()) }
    }
    pub fn ok() -> Self {
        Self { status: 200, location: None }
    }
}

/// Runs until the test process exits — there is no shutdown handle because every caller is a
/// short-lived `#[test]` and the OS reclaims the socket on process exit; a listener nobody polls
/// again costs nothing.
pub(crate) struct TestServer {
    pub port: u16,
    hits: Arc<Mutex<HashMap<String, bool>>>,
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
        *self.hits.lock().unwrap().get(path).unwrap_or_else(|| panic!("test server: {path} was never hit"))
    }
}

fn handle_one(mut stream: TcpStream, routes: &HashMap<&'static str, Canned>, hits: &Arc<Mutex<HashMap<String, bool>>>) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone test stream"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return; // client closed without sending anything (pool probe, etc.)
    }
    // "GET /path HTTP/1.1\r\n"
    let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();

    let mut had_authorization = false;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break; // end of headers (blank line) or a dropped connection
        }
        if line.to_ascii_lowercase().starts_with("authorization:") {
            had_authorization = true;
        }
    }
    hits.lock().unwrap().insert(path.clone(), had_authorization);

    let (status, reason, location) = match routes.get(path.as_str()) {
        Some(c) => (c.status, reason_phrase(c.status), c.location.clone()),
        None => (404, "Not Found", None),
    };
    let mut resp = format!("HTTP/1.1 {status} {reason}\r\n");
    if let Some(loc) = location {
        resp.push_str(&format!("Location: {loc}\r\n"));
    }
    resp.push_str("Content-Length: 0\r\nConnection: close\r\n\r\n");
    let _ = stream.write_all(resp.as_bytes());
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        302 => "Found",
        404 => "Not Found",
        _ => "Other",
    }
}
