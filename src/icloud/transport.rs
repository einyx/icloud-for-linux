//! The HTTP layer.
//!
//! Everything the protocol client does goes through the [`Transport`] trait.
//! The trait exists so the protocol can be tested against scripted responses
//! without a network, and so the transport can be swapped (a proxy, a mock,
//! a different TLS stack) without touching the protocol code.
//!
//! Cookies are handled by a small jar of our own instead of the HTTP
//! client's: the session has to be *persisted* to disk between runs, and the
//! service hands out cookies for several hosts (`idmsa.apple.com`,
//! `setup.icloud.com`, `pXX-icloud-content.icloud.com`) that have to be
//! matched per host.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;

use super::error::{Error, Result};

/// HTTP methods used by the protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Delete,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
        }
    }
}

/// The body of a request.
#[derive(Debug)]
pub enum Body {
    Empty,
    Json(Value),
    Text(String),
    /// A single file upload, sent as multipart/form-data.
    File {
        path: std::path::PathBuf,
        file_name: String,
        content_type: String,
    },
}

pub struct HttpRequest {
    pub method: Method,
    pub url: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Body,
}

impl HttpRequest {
    pub fn new(method: Method, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            query: Vec::new(),
            headers: Vec::new(),
            body: Body::Empty,
        }
    }

    pub fn query(mut self, key: &str, value: impl Into<String>) -> Self {
        self.query.push((key.to_string(), value.into()));
        self
    }

    pub fn header(mut self, key: &str, value: impl Into<String>) -> Self {
        self.headers.push((key.to_string(), value.into()));
        self
    }

    pub fn json(mut self, value: Value) -> Self {
        self.body = Body::Json(value);
        self
    }

    pub fn text(mut self, value: impl Into<String>) -> Self {
        self.body = Body::Text(value.into());
        self
    }

    pub fn body(mut self, body: Body) -> Self {
        self.body = body;
        self
    }

    /// The body of the request as text, for inspection.
    pub fn body_text(&self) -> String {
        match &self.body {
            Body::Empty => String::new(),
            Body::Json(value) => value.to_string(),
            Body::Text(text) => text.clone(),
            Body::File { file_name, .. } => file_name.clone(),
        }
    }
}

pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> Result<Value> {
        serde_json::from_slice(&self.body).map_err(|e| Error::Protocol(e.to_string()))
    }
}

/// A transport performs HTTP requests.
pub trait Transport: Send + Sync {
    fn execute(&self, request: &HttpRequest) -> Result<HttpResponse>;
}

/// A cookie with the host it belongs to.
#[derive(Debug, Clone)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
}

impl Cookie {
    /// Does this cookie apply to `host`?
    fn matches(&self, host: &str) -> bool {
        let domain = self.domain.trim_start_matches('.').to_ascii_lowercase();
        let host = host.to_ascii_lowercase();
        host == domain || host.ends_with(&format!(".{}", domain))
    }
}

/// A minimal cookie jar with host matching.
#[derive(Debug, Default, Clone)]
pub struct CookieJar {
    cookies: Vec<Cookie>,
}

impl CookieJar {
    pub fn new() -> Self {
        Self::default()
    }

    /// Store the `Set-Cookie` headers of a response.
    pub fn store(&mut self, url: &str, set_cookie: &[String]) {
        let host = host_of(url);
        for header in set_cookie {
            let mut parts = header.split(';');
            let Some(pair) = parts.next() else { continue };
            let Some((name, value)) = pair.split_once('=') else {
                continue;
            };
            let name = name.trim().to_string();
            let value = value.trim().to_string();
            let mut domain = host.clone();
            let mut path = "/".to_string();
            for attr in parts {
                let attr = attr.trim();
                if let Some(v) = attr.strip_prefix("Domain=") {
                    domain = v.trim_start_matches('.').to_ascii_lowercase();
                } else if let Some(v) = attr.strip_prefix("Path=") {
                    path = v.to_string();
                }
            }
            self.cookies
                .retain(|c| !(c.name == name && c.domain == domain));
            self.cookies.push(Cookie {
                name,
                value,
                domain,
                path,
            });
        }
    }

    /// The `Cookie` header value for a URL, if any cookie applies.
    pub fn header_for(&self, url: &str) -> Option<String> {
        let host = host_of(url);
        let applicable: Vec<&Cookie> = self.cookies.iter().filter(|c| c.matches(&host)).collect();
        if applicable.is_empty() {
            return None;
        }
        Some(
            applicable
                .iter()
                .map(|c| format!("{}={}", c.name, c.value))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.cookies
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(name))
            .map(|c| c.value.as_str())
    }

    pub fn set(&mut self, name: &str, value: &str, domain: &str) {
        self.cookies.retain(|c| !c.name.eq_ignore_ascii_case(name));
        self.cookies.push(Cookie {
            name: name.to_string(),
            value: value.to_string(),
            domain: domain.trim_start_matches('.').to_ascii_lowercase(),
            path: "/".to_string(),
        });
    }

    pub fn remove(&mut self, name: &str) {
        self.cookies.retain(|c| !c.name.eq_ignore_ascii_case(name));
    }

    pub fn clear(&mut self) {
        self.cookies.clear();
    }

    /// Serialise for persistence.
    pub fn to_json(&self) -> Value {
        serde_json::Value::Array(
            self.cookies
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "name": c.name,
                        "value": c.value,
                        "domain": c.domain,
                        "path": c.path,
                    })
                })
                .collect(),
        )
    }

    /// Restore from a persisted jar.
    pub fn from_json(value: &Value) -> Self {
        let mut jar = CookieJar::new();
        if let Some(items) = value.as_array() {
            for item in items {
                let Some(name) = item.get("name").and_then(|v| v.as_str()) else {
                    continue;
                };
                jar.cookies.push(Cookie {
                    name: name.to_string(),
                    value: item
                        .get("value")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    domain: item
                        .get("domain")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    path: item
                        .get("path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("/")
                        .to_string(),
                });
            }
        }
        jar
    }
}

fn host_of(url: &str) -> String {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// The real transport, backed by `reqwest`.
pub struct HttpTransport {
    client: reqwest::blocking::Client,
    jar: Mutex<CookieJar>,
    user_agent: String,
}

impl HttpTransport {
    pub fn new(user_agent: &str) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(20))
            .danger_accept_invalid_certs(false)
            .build()
            .map_err(|e| Error::Transport(e.to_string()))?;
        Ok(Self {
            client,
            jar: Mutex::new(CookieJar::new()),
            user_agent: user_agent.to_string(),
        })
    }

    pub fn jar(&self) -> &Mutex<CookieJar> {
        &self.jar
    }

    /// Apply the stored cookies to a request and record the response cookies.
    fn prepare(&self, request: &HttpRequest) -> Result<reqwest::blocking::RequestBuilder> {
        let method = reqwest::Method::from_bytes(request.method.as_str().as_bytes())
            .map_err(|e| Error::Transport(e.to_string()))?;
        let mut builder = self
            .client
            .request(method, &request.url)
            .header("User-Agent", &self.user_agent)
            .header("Accept", "*/*");
        if let Some(cookie) = self
            .jar
            .lock()
            .ok()
            .and_then(|j| j.header_for(&request.url))
        {
            builder = builder.header("Cookie", cookie);
        }
        for (key, value) in &request.headers {
            builder = builder.header(key, value);
        }
        for (key, value) in &request.query {
            builder = builder.query(&[(key.as_str(), value.as_str())]);
        }
        Ok(builder)
    }

    fn absorb(&self, url: &str, response: reqwest::blocking::Response) -> HttpResponse {
        let headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_string(),
                    v.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        let set_cookie: Vec<String> = response
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap_or_default().to_string())
            .collect();
        if !set_cookie.is_empty() {
            if let Ok(mut jar) = self.jar.lock() {
                jar.store(url, &set_cookie);
            }
        }
        let status = response.status().as_u16();
        let body = response.bytes().map(|b| b.to_vec()).unwrap_or_default();
        HttpResponse {
            status,
            headers,
            body,
        }
    }
}

impl Transport for HttpTransport {
    fn execute(&self, request: &HttpRequest) -> Result<HttpResponse> {
        let mut builder = self.prepare(request)?;
        builder = match &request.body {
            Body::Empty => builder,
            Body::Json(value) => builder
                .header("Content-Type", "application/json")
                .body(serde_json::to_vec(value)?),
            Body::Text(text) => builder
                .header("Content-Type", "text/plain")
                .body(text.clone()),
            Body::File {
                path,
                file_name,
                content_type,
            } => {
                // The file is streamed from disk: a large upload never sits in
                // memory.
                let part = reqwest::blocking::multipart::Part::file(path)
                    .map_err(|e| Error::Transport(e.to_string()))?
                    .file_name(file_name.clone())
                    .mime_str(content_type)
                    .map_err(|e| Error::Transport(e.to_string()))?;
                builder.multipart(reqwest::blocking::multipart::Form::new().part("file", part))
            }
        };

        let response = builder
            .send()
            .map_err(|e| Error::Transport(e.to_string()))?;
        Ok(self.absorb(&request.url, response))
    }
}

/// Read a response body from a transport that supports streaming.
///
/// Downloads go through the transport twice: once for the small JSON answer
/// that carries the download URL, and once for the content itself. The second
/// response is written straight to disk.
pub fn download_to_file(
    transport: &dyn Transport,
    request: &HttpRequest,
    dest: &Path,
) -> Result<u64> {
    let response = transport.execute(request)?;
    if response.status >= 400 {
        return Err(Error::Api {
            status: response.status,
            code: None,
            message: "download failed".to_string(),
        });
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(dest, &response.body)?;
    Ok(response.body.len() as u64)
}

/// A transport that answers from a scripted list of responses. Used by the
/// protocol tests.
pub struct ScriptedTransport {
    pub responses: Mutex<Vec<Result<HttpResponse>>>,
    pub requests: Mutex<Vec<HttpRequest>>,
}

impl ScriptedTransport {
    pub fn new(responses: Vec<Result<HttpResponse>>) -> Self {
        Self {
            responses: Mutex::new(responses),
            requests: Mutex::new(Vec::new()),
        }
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().map(|r| r.len()).unwrap_or(0)
    }

    pub fn last_request(&self) -> Option<HttpRequest> {
        self.requests.lock().ok().and_then(|r| r.last().cloned())
    }
}

impl Transport for ScriptedTransport {
    fn execute(&self, request: &HttpRequest) -> Result<HttpResponse> {
        if let Ok(mut requests) = self.requests.lock() {
            requests.push(request.clone());
        }
        let mut responses = self
            .responses
            .lock()
            .map_err(|_| Error::Protocol("scripted transport is poisoned".into()))?;
        if responses.is_empty() {
            return Err(Error::Protocol("no scripted response left".into()));
        }
        responses.remove(0)
    }
}

impl Clone for HttpRequest {
    fn clone(&self) -> Self {
        Self {
            method: self.method,
            url: self.url.clone(),
            query: self.query.clone(),
            headers: self.headers.clone(),
            body: match &self.body {
                Body::Empty => Body::Empty,
                Body::Json(v) => Body::Json(v.clone()),
                Body::Text(t) => Body::Text(t.clone()),
                Body::File {
                    path,
                    file_name,
                    content_type,
                } => Body::File {
                    path: path.clone(),
                    file_name: file_name.clone(),
                    content_type: content_type.clone(),
                },
            },
        }
    }
}

/// Helper for building a JSON response in tests and for simple paths.
pub fn json_response(status: u16, value: Value) -> Result<HttpResponse> {
    Ok(HttpResponse {
        status,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: serde_json::to_vec(&value)?,
    })
}

/// Query parameters used by every request of the setup service.
pub fn client_params(client_id: &str) -> Vec<(String, String)> {
    vec![("clientId".to_string(), client_id.to_string())]
}

/// A map of header names to values, used when a response is inspected.
pub fn header_map(headers: &[(String, String)]) -> HashMap<String, String> {
    headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
        .collect()
}
