//! The HTTP the email client speaks (Google's OAuth endpoints), in the shape
//! of `reqwest::blocking`.
//!
//! Inside the sandbox every request goes through the host's `net.fetch`, which
//! checks it against `allowedHosts`. Natively, for the unit tests, it is
//! reqwest.

use serde::de::DeserializeOwned;
use std::time::Duration;

/// A failed request, or a body that would not parse.
#[derive(Debug, Clone)]
pub struct Error(String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// Makes requests.
#[derive(Clone)]
pub struct Client {
    /// The whole-request timeout natively. Inside the sandbox the host sets it.
    timeout: Duration,
}

impl Default for Client {
    fn default() -> Self {
        Client {
            timeout: Duration::from_secs(30),
        }
    }
}

impl Client {
    pub fn new() -> Self {
        Client::default()
    }

    fn request(&self, method: &str, url: &str) -> RequestBuilder {
        RequestBuilder {
            method: method.to_owned(),
            url: url.to_owned(),
            headers: Vec::new(),
            body: None,
            timeout: self.timeout,
        }
    }

    pub fn get(&self, url: impl AsRef<str>) -> RequestBuilder {
        self.request("GET", url.as_ref())
    }

    pub fn post(&self, url: impl AsRef<str>) -> RequestBuilder {
        self.request("POST", url.as_ref())
    }
}

/// One request, being built.
pub struct RequestBuilder {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
    timeout: Duration,
}

impl RequestBuilder {
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// `Authorization: Bearer <token>`.
    pub fn bearer_auth(self, token: &str) -> Self {
        self.header("Authorization", format!("Bearer {token}"))
    }

    /// An `application/x-www-form-urlencoded` body.
    pub fn form(mut self, fields: &[(&str, &str)]) -> Self {
        let body: Vec<String> = fields
            .iter()
            .map(|(k, v)| format!("{}={}", form_encode(k), form_encode(v)))
            .collect();
        self.body = Some(body.join("&").into_bytes());
        self.header("Content-Type", "application/x-www-form-urlencoded")
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn send(self) -> Result<Response, Error> {
        let client = reqwest::blocking::Client::builder()
            .timeout(self.timeout)
            .build()
            .map_err(|e| Error(e.to_string()))?;
        let method = reqwest::Method::from_bytes(self.method.as_bytes())
            .map_err(|e| Error(e.to_string()))?;
        let mut req = client.request(method, &self.url);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        if let Some(body) = self.body {
            req = req.body(body);
        }
        let resp = req.send().map_err(|e| Error(e.to_string()))?;
        let body = resp.bytes().map_err(|e| Error(e.to_string()))?.to_vec();
        Ok(Response { body })
    }

    #[cfg(target_arch = "wasm32")]
    pub fn send(self) -> Result<Response, Error> {
        let _ = self.timeout;
        let resp = sicompass_pdk::net::fetch(&sicompass_pdk::net::HttpRequest {
            method: self.method,
            url: self.url,
            headers: self.headers,
            body: self.body,
        })
        .map_err(Error)?;
        Ok(Response { body: resp.body })
    }
}

/// Percent-encode a form field (RFC 3986 unreserved kept).
fn form_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A response, read whole. Google's token endpoint answers errors as JSON too,
/// so the status is not needed.
pub struct Response {
    body: Vec<u8>,
}

impl Response {
    pub fn json<T: DeserializeOwned>(self) -> Result<T, Error> {
        serde_json::from_slice(&self.body).map_err(|e| Error(e.to_string()))
    }

    pub fn text(self) -> Result<String, Error> {
        Ok(String::from_utf8_lossy(&self.body).into_owned())
    }
}
