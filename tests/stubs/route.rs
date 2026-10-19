//! A tiny stateful HTTP stub layer on top of `wiremock`.
//!
//! `wiremock` normally returns canned responses. A vendor stub needs *state*:
//! the instance created by `POST /instances` must appear in the next
//! `GET /instances`, a delete must make it disappear, and so on. [`StubServer`]
//! keeps one `S` (the vendor's simulated account) behind a mutex and routes
//! each request to a handler `Fn(&Request, &mut S) -> ResponseTemplate`, so a
//! vendor stub reads like a small, synchronous fake of the real API.

use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::Value;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// A running stub of one vendor API with simulated account state `S`.
pub struct StubServer<S> {
    server: MockServer,
    state: Arc<Mutex<S>>,
    calls: Arc<Mutex<Vec<String>>>,
}

impl<S: Send + 'static> StubServer<S> {
    pub async fn start(state: S) -> Self {
        StubServer {
            server: MockServer::start().await,
            state: Arc::new(Mutex::new(state)),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Base URL to put in the adapter's config (`http://127.0.0.1:<port>`).
    pub fn uri(&self) -> String {
        self.server.uri()
    }

    /// Lock the simulated account (to seed it, inject faults, or inspect it).
    pub fn state(&self) -> MutexGuard<'_, S> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Every request served so far, as `METHOD /path`.
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    /// Route `http_method` + `path` (an anchored regex) to `handler`.
    /// Unrouted requests get wiremock's 404, which surfaces as an adapter
    /// error — so a stub that forgets an endpoint fails loudly.
    pub async fn route<F>(&self, http_method: &str, path: &str, handler: F)
    where
        F: Fn(&Request, &mut S) -> ResponseTemplate + Send + Sync + 'static,
    {
        Mock::given(method(http_method))
            .and(path_regex(path))
            .respond_with(Handler {
                state: self.state.clone(),
                calls: self.calls.clone(),
                handler,
            })
            .mount(&self.server)
            .await;
    }
}

struct Handler<S, F> {
    state: Arc<Mutex<S>>,
    calls: Arc<Mutex<Vec<String>>>,
    handler: F,
}

impl<S, F> Respond for Handler<S, F>
where
    S: Send + 'static,
    F: Fn(&Request, &mut S) -> ResponseTemplate + Send + Sync + 'static,
{
    fn respond(&self, request: &Request) -> ResponseTemplate {
        self.calls
            .lock()
            .unwrap()
            .push(format!("{} {}", request.method, request.url.path()));
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        (self.handler)(request, &mut state)
    }
}

// ---------------------------------------------------------------------------
// Request/response helpers shared by the vendor stubs
// ---------------------------------------------------------------------------

/// A JSON response.
pub fn json(status: u16, body: Value) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(body)
}

/// An empty response (e.g. `204 No Content`).
pub fn empty(status: u16) -> ResponseTemplate {
    ResponseTemplate::new(status)
}

/// The request body as JSON (`Value::Null` when absent or not JSON).
pub fn body(request: &Request) -> Value {
    serde_json::from_slice(&request.body).unwrap_or(Value::Null)
}

/// A query parameter (the first occurrence).
pub fn query(request: &Request, key: &str) -> Option<String> {
    request
        .url
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

/// All values of a repeated query parameter.
pub fn query_all(request: &Request, key: &str) -> Vec<String> {
    request
        .url
        .query_pairs()
        .filter(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
        .collect()
}

/// The last path segment, percent-decoded (e.g. the `{id}` of `/pods/{id}`).
pub fn last_segment(request: &Request) -> String {
    let raw = request
        .url
        .path_segments()
        .and_then(|mut s| s.next_back().map(str::to_string))
        .unwrap_or_default();
    percent_decode(&raw)
}

/// The path segment at `index` (0-based), percent-decoded.
pub fn segment(request: &Request, index: usize) -> String {
    let raw = request
        .url
        .path_segments()
        .and_then(|mut s| s.nth(index).map(str::to_string))
        .unwrap_or_default();
    percent_decode(&raw)
}

/// A request header value.
pub fn header(request: &Request, name: &str) -> Option<String> {
    request
        .headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&raw[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Sequential ids (`<prefix>-1`, `<prefix>-2`, …) for simulated resources.
#[derive(Debug, Default)]
pub struct Ids(u64);

impl Ids {
    pub fn next(&mut self, prefix: &str) -> String {
        self.0 += 1;
        format!("{prefix}-{}", self.0)
    }
}
