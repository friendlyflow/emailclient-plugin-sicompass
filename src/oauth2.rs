//! Google OAuth2 authorization flow — port of `oauth2.c`.
//!
//! The sign-in is the desktop-app flow of RFC 8252: the app opens Google's
//! page in the user's browser and listens once on a loopback port for the
//! redirect (`sicompass_sdk::plugin::desktop::oauth_redirect`), and the
//! authorization code it brings back is exchanged for tokens at the Google
//! token endpoint. That waits for the user, so it runs on a thread of its own,
//! and [`PendingAuthorize::poll`] picks up the result.

use crate::http::Client;
use serde::{Deserialize, Serialize};
use sicompass_sdk::plugin::OauthReply;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

/// Seconds since the Unix epoch.
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GOOGLE_USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v2/userinfo";
const OAUTH2_SCOPE: &str = "https://mail.google.com/ email profile";

/// Result of an OAuth2 token operation — mirrors `OAuth2TokenResult` from C.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OAuth2TokenResult {
    pub success: bool,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
    pub error: String,
    /// The signed-in address, when the sign-in already asked for it.
    #[serde(default)]
    pub email: String,
}

// ---------------------------------------------------------------------------
// Non-blocking handle
// ---------------------------------------------------------------------------

/// An in-flight OAuth2 authorization request. Created by [`start`]; poll it
/// each frame with [`PendingAuthorize::poll`] until it returns `Some`.
pub struct PendingAuthorize {
    rx: std::sync::mpsc::Receiver<OAuth2TokenResult>,
    cancel: Arc<AtomicBool>,
    deadline: Instant,
}

impl std::fmt::Debug for PendingAuthorize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingAuthorize")
            .field("cancelled", &self.cancel.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl PendingAuthorize {
    /// Non-blocking check. Returns `Some(result)` once the worker finishes
    /// (success, error, or timeout), `None` while still waiting.
    pub fn poll(&self) -> Option<OAuth2TokenResult> {
        // Check deadline before try_recv so callers don't have to track time.
        if Instant::now() >= self.deadline {
            self.cancel.store(true, Ordering::Relaxed);
            // Drain whatever the worker might have sent right at the deadline.
            if let Ok(r) = self.rx.try_recv() {
                return Some(r);
            }
            return Some(OAuth2TokenResult {
                error: "timed out waiting for Google authorization".to_owned(),
                ..Default::default()
            });
        }
        match self.rx.try_recv() {
            Ok(r) => Some(r),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(OAuth2TokenResult {
                error: "authorization worker exited unexpectedly".to_owned(),
                ..Default::default()
            }),
        }
    }

    /// Give up on the sign-in: whatever the browser still brings back is
    /// dropped. The app's wait for the browser cannot be cut short, and the
    /// thread ends when it does.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Start the OAuth2 authorization flow asynchronously.
///
/// Spawns a thread that has the app run the browser sign-in and then
/// exchanges the code, and returns a [`PendingAuthorize`] handle immediately.
/// Call [`PendingAuthorize::poll`] each frame until it returns `Some`.
pub fn start(
    client_id: &str,
    client_secret: &str,
    timeout_secs: u64,
) -> Result<PendingAuthorize, OAuth2TokenResult> {
    if client_id.is_empty() || client_secret.is_empty() {
        return Err(OAuth2TokenResult {
            error: "client ID and client secret are required".to_owned(),
            ..Default::default()
        });
    }

    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel::<OAuth2TokenResult>();
    let cancel_worker = Arc::clone(&cancel);
    let client_id = client_id.to_owned();
    let client_secret = client_secret.to_owned();
    let wait = u32::try_from(timeout_secs).unwrap_or(u32::MAX);

    std::thread::Builder::new()
        .name("email-oauth".to_owned())
        .spawn(move || {
            let reply = sicompass_sdk::plugin::desktop::oauth_redirect(&auth_url(&client_id), wait);
            let result = if cancel_worker.load(Ordering::Relaxed) {
                OAuth2TokenResult {
                    error: "login cancelled".to_owned(),
                    ..Default::default()
                }
            } else {
                finish_redirect(reply, &client_id, &client_secret)
            };
            let _ = tx.send(result);
        })
        .map_err(|e| OAuth2TokenResult {
            error: format!("cannot start the sign-in: {e}"),
            ..Default::default()
        })?;

    Ok(PendingAuthorize {
        rx,
        cancel,
        // A little past the app's own wait, so its answer arrives first.
        deadline: Instant::now() + Duration::from_secs(timeout_secs + 15),
    })
}

/// Google's sign-in page for `client_id`. The app replaces `{redirect-uri}`
/// with its loopback address, percent-encoded.
fn auth_url(client_id: &str) -> String {
    format!(
        "{GOOGLE_AUTH_URL}?client_id={client_id}&redirect_uri={{redirect-uri}}&\
         response_type=code&scope={scope}&access_type=offline&prompt=consent",
        client_id = percent_encode(client_id),
        scope = percent_encode(OAUTH2_SCOPE),
    )
}

/// What the browser brought back, made into tokens: Google's refusal, a
/// missing code, or the code exchanged at the token endpoint (with the
/// redirect URI the app used, which Google checks), plus the signed-in
/// address.
fn finish_redirect(
    reply: Result<OauthReply, String>,
    client_id: &str,
    client_secret: &str,
) -> OAuth2TokenResult {
    let reply = match reply {
        Ok(reply) => reply,
        Err(e) => {
            return OAuth2TokenResult {
                error: e,
                ..Default::default()
            };
        }
    };
    let line = format!("GET /?{} HTTP/1.1", reply.query);
    if reply.query.split('&').any(|kv| kv.starts_with("error=")) {
        return OAuth2TokenResult {
            error: "Google returned an error response".to_owned(),
            ..Default::default()
        };
    }
    let Some(code) = extract_query_param(&line, "code") else {
        return OAuth2TokenResult {
            error: "no authorization code in redirect".to_owned(),
            ..Default::default()
        };
    };
    let mut tokens = exchange_code(&code, client_id, client_secret, &reply.redirect_uri);
    if tokens.success {
        tokens.email = fetch_email(&tokens.access_token).unwrap_or_default();
    }
    tokens
}

/// Start the OAuth2 authorization flow and block until completion or timeout.
///
/// Convenience wrapper for callers (and tests) that can afford to block.
pub fn authorize(client_id: &str, client_secret: &str, timeout_secs: u64) -> OAuth2TokenResult {
    match start(client_id, client_secret, timeout_secs) {
        Err(e) => e,
        Ok(handle) => {
            let sleep = Duration::from_millis(50);
            loop {
                if let Some(result) = handle.poll() {
                    return result;
                }
                std::thread::sleep(sleep);
            }
        }
    }
}

/// Fetch the authenticated user's email address from Google's userinfo endpoint.
/// Returns `None` on any network or parse error (caller treats it as optional).
pub fn fetch_email(access_token: &str) -> Option<String> {
    let response = Client::new()
        .get(GOOGLE_USERINFO_URL)
        .bearer_auth(access_token)
        .send()
        .ok()?;
    let json: serde_json::Value = response.json().ok()?;
    json.get("email")?.as_str().map(|s| s.to_owned())
}

/// Refresh an expired access token using a refresh token.
pub fn refresh_token(client_id: &str, client_secret: &str, refresh_tok: &str) -> OAuth2TokenResult {
    if client_id.is_empty() || client_secret.is_empty() || refresh_tok.is_empty() {
        return OAuth2TokenResult {
            error: "client ID, client secret, and refresh token are required".to_owned(),
            ..Default::default()
        };
    }

    let client = match Client::new()
        .post(GOOGLE_TOKEN_URL)
        .form(&[
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("refresh_token", refresh_tok),
            ("grant_type", "refresh_token"),
        ])
        .send()
    {
        Ok(r) => r,
        Err(e) => {
            return OAuth2TokenResult {
                error: format!("token refresh failed: {e}"),
                ..Default::default()
            };
        }
    };

    parse_token_response(client)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn extract_query_param(get_line: &str, param: &str) -> Option<String> {
    // GET /?code=XXXX&... HTTP/1.1
    let query_start = get_line.find('?')?;
    let query_end = get_line[query_start..]
        .find(' ')
        .map(|i| query_start + i)
        .unwrap_or(get_line.len());
    let query = &get_line[query_start + 1..query_end];
    for part in query.split('&') {
        if let Some(value) = part.strip_prefix(&format!("{param}=")) {
            return Some(value.to_owned());
        }
    }
    None
}

fn exchange_code(
    code: &str,
    client_id: &str,
    client_secret: &str,
    redirect_uri: &str,
) -> OAuth2TokenResult {
    let response = match Client::new()
        .post(GOOGLE_TOKEN_URL)
        .form(&[
            ("code", code),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("redirect_uri", redirect_uri),
            ("grant_type", "authorization_code"),
        ])
        .send()
    {
        Ok(r) => r,
        Err(e) => {
            return OAuth2TokenResult {
                error: format!("token exchange failed: {e}"),
                ..Default::default()
            };
        }
    };

    parse_token_response(response)
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    error: Option<String>,
    error_description: Option<String>,
}

fn parse_token_response(response: crate::http::Response) -> OAuth2TokenResult {
    let text = match response.text() {
        Ok(t) => t,
        Err(e) => {
            return OAuth2TokenResult {
                error: format!("failed to read response: {e}"),
                ..Default::default()
            };
        }
    };

    let parsed: TokenResponse = match serde_json::from_str(&text) {
        Ok(p) => p,
        Err(e) => {
            return OAuth2TokenResult {
                error: format!("invalid JSON response: {e}"),
                ..Default::default()
            };
        }
    };

    if let Some(err) = parsed.error {
        let desc = parsed.error_description.unwrap_or_default();
        return OAuth2TokenResult {
            error: format!("{err}: {desc}"),
            ..Default::default()
        };
    }

    let access_token = parsed.access_token.unwrap_or_default();
    if access_token.is_empty() {
        return OAuth2TokenResult {
            error: "no access_token in response".to_owned(),
            ..Default::default()
        };
    }

    OAuth2TokenResult {
        success: true,
        access_token,
        refresh_token: parsed.refresh_token.unwrap_or_default(),
        expires_in: parsed.expires_in.unwrap_or(3600),
        ..Default::default()
    }
}

/// Minimal percent-encoder for URL components (spaces, slashes, colons, etc.).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_authorize_empty_client_id_fails() {
        let r = authorize("", "secret", 1);
        assert!(!r.success);
        assert!(!r.error.is_empty());
    }

    #[test]
    fn test_authorize_empty_client_secret_fails() {
        let r = authorize("id", "", 1);
        assert!(!r.success);
        assert!(!r.error.is_empty());
    }

    #[test]
    fn test_refresh_empty_client_id_fails() {
        let r = refresh_token("", "secret", "refresh");
        assert!(!r.success);
        assert!(!r.error.is_empty());
    }

    #[test]
    fn test_refresh_empty_refresh_token_fails() {
        let r = refresh_token("id", "secret", "");
        assert!(!r.success);
        assert!(!r.error.is_empty());
    }

    #[test]
    fn test_percent_encode_special_chars() {
        assert_eq!(percent_encode("hello world"), "hello%20world");
        assert_eq!(percent_encode("a/b"), "a%2Fb");
        assert_eq!(percent_encode("a:b"), "a%3Ab");
    }

    #[test]
    fn test_percent_encode_unreserved_unchanged() {
        assert_eq!(percent_encode("abc-123_ABC.~"), "abc-123_ABC.~");
    }

    #[test]
    fn test_extract_query_param() {
        let line = "GET /?code=abc123&state=xyz HTTP/1.1";
        assert_eq!(extract_query_param(line, "code"), Some("abc123".to_owned()));
        assert_eq!(extract_query_param(line, "state"), Some("xyz".to_owned()));
        assert_eq!(extract_query_param(line, "missing"), None);
    }

    #[test]
    fn test_extract_query_param_no_query() {
        let line = "GET / HTTP/1.1";
        assert_eq!(extract_query_param(line, "code"), None);
    }

    #[test]
    fn test_start_empty_client_id_fails() {
        let r = start("", "secret", 5);
        assert!(r.is_err());
        assert!(!r.unwrap_err().error.is_empty());
    }

    #[test]
    fn test_pending_authorize_cancel_unblocks() {
        // start() with real credentials would open a browser — use a dummy
        // client_id/secret pair and cancel immediately.  We can't call start()
        // without the browser opening, so we test cancel on a PendingAuthorize
        // constructed manually from a channel pair.
        let (tx, rx) = std::sync::mpsc::channel::<OAuth2TokenResult>();
        let cancel = Arc::new(AtomicBool::new(false));
        let handle = PendingAuthorize {
            rx,
            cancel: Arc::clone(&cancel),
            deadline: Instant::now() + Duration::from_secs(60),
        };
        // Nothing sent yet — should be None.
        assert!(handle.poll().is_none());
        // Send a cancellation result on the channel (simulates the worker responding).
        tx.send(OAuth2TokenResult {
            error: "login cancelled".to_owned(),
            ..Default::default()
        })
        .unwrap();
        // Now poll should return Some.
        let result = handle.poll().unwrap();
        assert!(!result.success);
        assert!(!result.error.is_empty());
    }

    #[test]
    fn test_pending_authorize_timeout_returns_error() {
        let (_tx, rx) = std::sync::mpsc::channel::<OAuth2TokenResult>();
        let cancel = Arc::new(AtomicBool::new(false));
        let handle = PendingAuthorize {
            rx,
            cancel,
            // Already past the deadline.
            deadline: Instant::now() - Duration::from_secs(1),
        };
        let result = handle.poll().unwrap();
        assert!(!result.success);
        assert!(result.error.contains("timed out"));
    }

    /// The app runs the browser part. Outside sicompass there is no app, so
    /// the sign-in ends at once, saying why, and nothing reaches Google.
    #[test]
    fn a_sign_in_outside_sicompass_says_why() {
        let handle = start("id", "secret", 5).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let result = loop {
            if let Some(r) = handle.poll() {
                break r;
            }
            assert!(Instant::now() < deadline, "no answer");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(!result.success);
        assert!(
            result.error.contains("not running inside sicompass"),
            "{}",
            result.error
        );
    }

    #[test]
    fn the_sign_in_page_leaves_the_redirect_to_the_app() {
        let url = auth_url("123-abc.apps.googleusercontent.com");
        assert!(url.starts_with(GOOGLE_AUTH_URL), "{url}");
        assert!(
            url.contains("client_id=123-abc.apps.googleusercontent.com&"),
            "{url}"
        );
        assert!(url.contains("redirect_uri={redirect-uri}&"), "{url}");
        assert!(
            url.contains("scope=https%3A%2F%2Fmail.google.com%2F%20email%20profile"),
            "{url}"
        );
        assert!(url.contains("access_type=offline"), "{url}");
    }

    #[test]
    fn a_redirect_without_a_code_is_refused_before_any_exchange() {
        let reply = |query: &str| {
            Ok(OauthReply {
                query: query.to_owned(),
                redirect_uri: "http://127.0.0.1:4242".to_owned(),
            })
        };
        let refused = finish_redirect(reply("error=access_denied"), "id", "secret");
        assert!(!refused.success);
        assert_eq!(refused.error, "Google returned an error response");

        let no_code = finish_redirect(reply("state=xyz"), "id", "secret");
        assert!(!no_code.success);
        assert_eq!(no_code.error, "no authorization code in redirect");

        let failed = finish_redirect(Err("the browser was closed".to_owned()), "id", "secret");
        assert!(!failed.success);
        assert_eq!(failed.error, "the browser was closed");
    }
}
