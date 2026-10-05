//! The HTTP the email client speaks (Google's OAuth endpoints), in the shape
//! of `reqwest::blocking`.
//!
//! A blocking `ureq` client with rustls and bundled roots. `plugin.json`
//! declares the hosts it talks to (`allowedHosts`).

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

/// How long one request may take, start to end. A token refresh before a
/// send runs on a call from the app, which waits for it, so a server that
/// hangs becomes an error.
const TIMEOUT: Duration = Duration::from_secs(30);

/// The most a response may hold. Google's token and userinfo answers are a
/// few hundred bytes.
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;

/// Makes requests.
#[derive(Clone)]
pub struct Client {
    agent: ureq::Agent,
}

impl Default for Client {
    fn default() -> Self {
        use std::sync::OnceLock;
        static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
        let agent = AGENT.get_or_init(|| {
            ureq::Agent::config_builder()
                // Google answers errors as JSON with an error status, and the
                // callers read that JSON.
                .http_status_as_error(false)
                .timeout_global(Some(TIMEOUT))
                .build()
                .into()
        });
        Client {
            agent: agent.clone(),
        }
    }
}

impl Client {
    pub fn new() -> Self {
        Client::default()
    }

    fn request(&self, method: &str, url: &str) -> RequestBuilder {
        RequestBuilder {
            agent: self.agent.clone(),
            method: method.to_owned(),
            url: url.to_owned(),
            headers: Vec::new(),
            body: None,
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
    agent: ureq::Agent,
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
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

    pub fn send(self) -> Result<Response, Error> {
        let fail = |e: &dyn std::fmt::Display| Error(format!("{}: {e}", self.url));
        let mut builder = ureq::http::Request::builder()
            .method(self.method.as_str())
            .uri(&self.url);
        for (k, v) in &self.headers {
            builder = builder.header(k, v);
        }
        let result = match &self.body {
            Some(body) => self
                .agent
                .run(builder.body(body.as_slice()).map_err(|e| fail(&e))?),
            None => self.agent.run(builder.body(()).map_err(|e| fail(&e))?),
        };
        let mut resp = result.map_err(|e| fail(&e))?;
        let body = resp
            .body_mut()
            .with_config()
            .limit(MAX_RESPONSE_BYTES)
            .read_to_vec()
            .map_err(|e| fail(&e))?;
        Ok(Response { body })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    /// A server for one request: it answers `status` with `body`, and hands
    /// back the request line, the headers and the body it was sent.
    fn one_shot_server(
        status: u16,
        body: &'static str,
    ) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut head = String::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
                head.push_str(&line);
            }
            let mut sent = vec![0; length];
            reader.read_exact(&mut sent).unwrap();
            let mut stream = reader.into_inner();
            write!(
                stream,
                "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            head + &String::from_utf8(sent).unwrap()
        });
        (url, handle)
    }

    #[test]
    fn a_form_is_posted_encoded_with_its_content_type() {
        let (url, server) = one_shot_server(200, r#"{"access_token":"ya29"}"#);
        let resp = Client::new()
            .post(format!("{url}/token"))
            .form(&[("code", "4/a b"), ("grant_type", "authorization_code")])
            .send()
            .unwrap();
        let json: serde_json::Value = resp.json().unwrap();
        assert_eq!(json["access_token"], "ya29");
        let seen = server.join().unwrap();
        assert!(seen.starts_with("POST /token "), "{seen}");
        assert!(
            seen.to_ascii_lowercase()
                .contains("content-type: application/x-www-form-urlencoded"),
            "{seen}"
        );
        assert!(
            seen.ends_with("code=4%2Fa%20b&grant_type=authorization_code"),
            "{seen}"
        );
    }

    /// Google answers a refused token request with an error status and JSON
    /// saying why, which the callers read.
    #[test]
    fn an_error_status_still_hands_over_its_body() {
        let (url, server) = one_shot_server(400, r#"{"error":"invalid_grant"}"#);
        let text = Client::new()
            .get(format!("{url}/userinfo"))
            .bearer_auth("tok")
            .send()
            .unwrap()
            .text()
            .unwrap();
        assert_eq!(text, r#"{"error":"invalid_grant"}"#);
        let seen = server.join().unwrap();
        assert!(
            seen.to_ascii_lowercase()
                .contains("authorization: bearer tok"),
            "{seen}"
        );
    }

    #[test]
    fn no_server_is_an_error_that_names_the_url() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let err = Client::new()
            .get(format!("http://127.0.0.1:{port}/token"))
            .send()
            .err()
            .unwrap();
        assert!(err.to_string().contains("127.0.0.1"), "{err}");
    }
}
