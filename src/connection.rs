//! Shared IMAP connection helpers used by both `net.rs` (RealImap) and
//! `idle.rs` (the IDLE watcher).
//!
//! Blocking, over TLS: rustls with ring and the webpki roots, so the same code
//! runs natively and in the sandbox. In the sandbox the TCP connection comes
//! from the host (`sicompass_pdk::sockets`), which reaches only public servers
//! on the ports `plugin.json` lists. Every read and write is bounded by
//! [`IO_TIMEOUT`], so a server that goes silent fails the exchange instead of
//! holding it forever.

use crate::EmailClientConfig;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Upper bound on one read or write on an IMAP or SMTP connection.
pub const IO_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// Transport carrying an IMAP (or SMTP) session.
///
/// Production traffic is always `Tls`. `Plain` exists so the test suite can
/// drive `RealImap` against a local fake IMAP server; [`open_stream`] refuses
/// it for anything that is not a loopback address, so credentials can never
/// leave the machine in the clear.
pub enum ImapStream {
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
    Plain(TcpStream),
}

impl std::fmt::Debug for ImapStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ImapStream::Tls(_) => "ImapStream::Tls",
            ImapStream::Plain(_) => "ImapStream::Plain",
        })
    }
}

impl ImapStream {
    fn tcp(&self) -> &TcpStream {
        match self {
            ImapStream::Tls(s) => s.get_ref(),
            ImapStream::Plain(s) => s,
        }
    }
}

impl Read for ImapStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            ImapStream::Tls(s) => s.read(buf),
            ImapStream::Plain(s) => s.read(buf),
        }
    }
}

impl Write for ImapStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            ImapStream::Tls(s) => s.write(buf),
            ImapStream::Plain(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            ImapStream::Tls(s) => s.flush(),
            ImapStream::Plain(s) => s.flush(),
        }
    }
}

/// What IDLE uses to wake up now and then (see `idle.rs`).
impl imap::extensions::idle::SetReadTimeout for ImapStream {
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> imap::Result<()> {
        self.tcp()
            .set_read_timeout(timeout)
            .map_err(imap::Error::Io)
    }
}

pub type ImapSession = imap::Session<ImapStream>;

// ---------------------------------------------------------------------------
// URL parser
// ---------------------------------------------------------------------------

/// Parse `imaps://host` or `imaps://host:port` into `(host, port)`.
pub fn parse_imap_url(url: &str) -> Option<(String, u16)> {
    let rest = url
        .strip_prefix("imaps://")
        .or_else(|| url.strip_prefix("imap://"))?;
    if let Some(colon) = rest.rfind(':') {
        let host = rest[..colon].to_owned();
        let port: u16 = rest[colon + 1..].parse().ok()?;
        Some((host, port))
    } else {
        let default_port = if url.starts_with("imaps://") {
            993
        } else {
            143
        };
        Some((rest.to_owned(), default_port))
    }
}

// ---------------------------------------------------------------------------
// XOAUTH2 authenticator
// ---------------------------------------------------------------------------

/// IMAP Authenticator implementing the XOAUTH2 SASL mechanism.
///
/// The `process` method returns the raw SASL initial response (the `imap`
/// crate base64-encodes it before sending).
pub struct XOAuth2Auth {
    pub user: String,
    pub token: String,
}

impl imap::Authenticator for XOAuth2Auth {
    type Response = String;
    fn process(&self, _challenge: &[u8]) -> Self::Response {
        xoauth2_payload(&self.user, &self.token)
    }
}

/// The raw XOAUTH2 SASL initial response.
///
/// Kept in one place because three callers send it: the `Authenticator` above
/// (which base64-encodes it for us), [`RawImap`] and SMTP (which encode it
/// themselves). Servers reject any deviation, so they must not drift apart.
pub fn xoauth2_payload(user: &str, token: &str) -> String {
    format!("user={user}\x01auth=Bearer {token}\x01\x01")
}

// ---------------------------------------------------------------------------
// Connecting
// ---------------------------------------------------------------------------

/// The addresses of `host:port`: through the host in the sandbox, which
/// answers only for public servers on the ports `plugin.json` lists.
fn resolve(host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    #[cfg(target_arch = "wasm32")]
    let addrs: Vec<SocketAddr> = sicompass_pdk::sockets::resolve(host, port)?
        .iter()
        .filter_map(|a| a.parse::<std::net::IpAddr>().ok())
        .map(|ip| SocketAddr::new(ip, port))
        .collect();
    #[cfg(not(target_arch = "wasm32"))]
    let addrs: Vec<SocketAddr> = {
        use std::net::ToSocketAddrs;
        (host, port)
            .to_socket_addrs()
            .map_err(|e| format!("cannot resolve {host}:{port}: {e}"))?
            .collect()
    };
    if addrs.is_empty() {
        Err(format!("cannot resolve {host}:{port}"))
    } else {
        Ok(addrs)
    }
}

/// Connect to the first of `addrs` that answers, bounded by [`IO_TIMEOUT`].
fn connect(addrs: &[SocketAddr]) -> Result<TcpStream, String> {
    let mut last = String::from("no address");
    for addr in addrs {
        #[cfg(target_arch = "wasm32")]
        let attempt = TcpStream::connect(addr);
        #[cfg(not(target_arch = "wasm32"))]
        let attempt = TcpStream::connect_timeout(addr, IO_TIMEOUT);
        match attempt {
            Ok(tcp) => {
                // Best effort: a stream that cannot take a timeout still works.
                let _ = tcp.set_read_timeout(Some(IO_TIMEOUT));
                let _ = tcp.set_write_timeout(Some(IO_TIMEOUT));
                return Ok(tcp);
            }
            Err(e) => last = format!("{addr}: {e}"),
        }
    }
    Err(last)
}

/// The TLS client configuration: the webpki roots, safe defaults.
fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let roots = rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            Arc::new(
                rustls::ClientConfig::builder_with_provider(Arc::new(
                    rustls::crypto::ring::default_provider(),
                ))
                .with_safe_default_protocol_versions()
                .expect("ring supports the default protocol versions")
                .with_root_certificates(roots)
                .with_no_client_auth(),
            )
        })
        .clone()
}

/// TLS over `tcp`, verified for `host`. The handshake runs on first use.
pub fn tls(host: &str, tcp: TcpStream) -> Result<ImapStream, String> {
    let name = rustls::pki_types::ServerName::try_from(host.to_owned())
        .map_err(|e| format!("{host}: {e}"))?;
    let conn = rustls::ClientConnection::new(tls_config(), name).map_err(|e| e.to_string())?;
    Ok(ImapStream::Tls(Box::new(rustls::StreamOwned::new(
        conn, tcp,
    ))))
}

/// A TLS connection to `host:port` (implicit TLS: IMAPS, SMTPS).
pub fn open_tls(host: &str, port: u16) -> Result<ImapStream, String> {
    let tcp = connect(&resolve(host, port)?)?;
    tls(host, tcp)
}

/// A plain TCP connection to `host:port`, for a protocol that upgrades it
/// itself (SMTP's STARTTLS) before anything secret is sent.
pub fn open_tcp(host: &str, port: u16) -> Result<TcpStream, String> {
    connect(&resolve(host, port)?)
}

/// Open the transport for `url`.
///
/// `imaps://` performs a TLS handshake. `imap://` stays in the clear and is
/// therefore only permitted when the resolved address is loopback — the fake
/// IMAP server in the test suite is the only intended user.
pub fn open_stream(url: &str, host: &str, port: u16) -> Result<ImapStream, String> {
    let addrs = resolve(host, port)?;
    let use_tls = url.starts_with("imaps://");

    // Decide before opening the socket, so a misconfigured plaintext URL fails
    // fast instead of hanging on a connect to a remote host.
    if !use_tls && !addrs.iter().all(|a| a.ip().is_loopback()) {
        return Err(format!(
            "refusing to send IMAP credentials in the clear to {host}; use imaps://"
        ));
    }

    let tcp = connect(&addrs)?;
    if use_tls {
        return tls(host, tcp);
    }
    Ok(ImapStream::Plain(tcp))
}

/// Open an authenticated IMAP session from `config`.
///
/// Uses XOAUTH2 when an access token is present, LOGIN otherwise.
pub fn connect_imap(config: &EmailClientConfig) -> Result<ImapSession, String> {
    let (host, port) = parse_imap_url(&config.imap_url)
        .ok_or_else(|| format!("cannot parse IMAP URL: {}", config.imap_url))?;

    let stream = open_stream(&config.imap_url, &host, port)?;
    let mut client = imap::Client::new(stream);
    client.read_greeting().map_err(|e| e.to_string())?;

    if config.oauth_access_token.is_empty() {
        client
            .login(&config.username, &config.password)
            .map_err(|(e, _)| e.to_string())
    } else {
        let auth = XOAuth2Auth {
            user: config.username.clone(),
            token: config.oauth_access_token.clone(),
        };
        client
            .authenticate("XOAUTH2", &auth)
            .map_err(|(e, _)| e.to_string())
    }
}

// ---------------------------------------------------------------------------
// RawImap — hand-rolled client for commands the `imap` crate cannot decode
// ---------------------------------------------------------------------------

/// A minimal IMAP client that returns server responses verbatim.
///
/// This exists for exactly one reason: every response line goes through
/// `imap_proto::parse_response`, and **no released imap-proto understands the
/// THREAD extension** (verified against 0.10.2, which the `imap` 2.x crate used,
/// and 0.16.7, which async-imap and the `imap` 3 crate use). `UID THREAD` therefore fails with
/// a parse error before its payload can be read, no matter which wire crate is
/// underneath, and the crates' raw readers are private.
///
/// It speaks only what THREAD needs: authenticate, `CAPABILITY`, `SELECT`,
/// `UID THREAD`. Responses are read line by line, which is safe because none of
/// those commands can return a literal (`{n}`) — do not extend this to `FETCH`,
/// which can.
///
/// Running THREAD on its own connection has a second benefit: `fetch_threads`
/// no longer issues a `SELECT` on the main session, so it cannot disturb the
/// mailbox that session has selected.
pub struct RawImap {
    io: BufReader<ImapStream>,
    tag: u32,
    /// Capabilities from the post-authentication `CAPABILITY`, fetched once.
    caps: Option<Vec<String>>,
}

impl RawImap {
    /// Connect and authenticate, using XOAUTH2 when a token is present and
    /// LOGIN otherwise — the same choice `connect_imap` makes.
    pub fn connect(config: &EmailClientConfig) -> Result<Self, String> {
        let (host, port) = parse_imap_url(&config.imap_url)
            .ok_or_else(|| format!("cannot parse IMAP URL: {}", config.imap_url))?;
        let stream = open_stream(&config.imap_url, &host, port)?;

        let mut raw = RawImap {
            io: BufReader::new(stream),
            tag: 0,
            caps: None,
        };

        let greeting = raw.read_line()?;
        if !greeting.starts_with("* OK") {
            return Err(format!("unexpected IMAP greeting: {greeting}"));
        }

        if config.oauth_access_token.is_empty() {
            raw.login(&config.username, &config.password)?;
        } else {
            raw.authenticate_xoauth2(&config.username, &config.oauth_access_token)?;
        }
        Ok(raw)
    }

    /// Cached `CAPABILITY` keywords, upper-cased.
    pub fn capabilities(&mut self) -> Result<&[String], String> {
        if self.caps.is_none() {
            let lines = self.command("CAPABILITY")?;
            let caps = lines
                .iter()
                .find_map(|l| l.strip_prefix("* CAPABILITY "))
                .map(|l| l.split_whitespace().map(|c| c.to_uppercase()).collect())
                .unwrap_or_default();
            self.caps = Some(caps);
        }
        Ok(self.caps.as_deref().unwrap_or(&[]))
    }

    /// `SELECT` then `UID THREAD <algo> UTF-8 ALL`, returning the raw response
    /// lines for [`crate::net::parse_thread_response`].
    pub fn uid_thread(&mut self, folder: &str, algo: &str) -> Result<String, String> {
        self.command(&format!("SELECT {}", quote(folder)))?;
        let lines = self.command(&format!("UID THREAD {algo} UTF-8 ALL"))?;
        Ok(lines.join("\r\n"))
    }

    fn login(&mut self, user: &str, password: &str) -> Result<(), String> {
        self.command(&format!("LOGIN {} {}", quote(user), quote(password)))?;
        Ok(())
    }

    fn authenticate_xoauth2(&mut self, user: &str, token: &str) -> Result<(), String> {
        let tag = self.next_tag();
        self.write(&format!("{tag} AUTHENTICATE XOAUTH2\r\n"))?;

        let cont = self.read_line()?;
        if !cont.starts_with('+') {
            return Err(format!("server refused XOAUTH2: {cont}"));
        }

        let payload = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            xoauth2_payload(user, token),
        );
        self.write(&format!("{payload}\r\n"))?;
        self.read_tagged(&tag)?;
        Ok(())
    }

    /// Run one command and return its untagged (`*`) response lines.
    fn command(&mut self, cmd: &str) -> Result<Vec<String>, String> {
        let tag = self.next_tag();
        self.write(&format!("{tag} {cmd}\r\n"))?;
        self.read_tagged(&tag)
    }

    /// Collect untagged lines until the tagged completion for `tag`.
    fn read_tagged(&mut self, tag: &str) -> Result<Vec<String>, String> {
        let mut untagged = Vec::new();
        loop {
            let line = self.read_line()?;
            match line.strip_prefix(&format!("{tag} ")) {
                Some(status) if status.starts_with("OK") => return Ok(untagged),
                Some(status) => return Err(status.to_owned()),
                None => untagged.push(line),
            }
        }
    }

    fn next_tag(&mut self) -> String {
        self.tag += 1;
        // A distinct prefix from the `imap` crate's, so a stray response is
        // obvious in a packet capture.
        format!("t{}", self.tag)
    }

    fn write(&mut self, s: &str) -> Result<(), String> {
        let stream = self.io.get_mut();
        stream.write_all(s.as_bytes()).map_err(|e| e.to_string())?;
        stream.flush().map_err(|e| e.to_string())
    }

    fn read_line(&mut self) -> Result<String, String> {
        let mut buf = Vec::new();
        match self.io.read_until(b'\n', &mut buf) {
            Ok(0) => Err("connection closed by server".to_owned()),
            Ok(_) => {
                while matches!(buf.last(), Some(b'\r') | Some(b'\n')) {
                    buf.pop();
                }
                Ok(String::from_utf8_lossy(&buf).into_owned())
            }
            Err(e) => Err(e.to_string()),
        }
    }
}

/// Quote an IMAP astring, escaping `\` and `"`.
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use imap::Authenticator;

    #[test]
    fn test_parse_imap_url_with_port() {
        assert_eq!(
            parse_imap_url("imaps://imap.gmail.com:993"),
            Some(("imap.gmail.com".to_owned(), 993))
        );
    }

    #[test]
    fn test_parse_imap_url_without_port_defaults_993() {
        assert_eq!(
            parse_imap_url("imaps://imap.gmail.com"),
            Some(("imap.gmail.com".to_owned(), 993))
        );
    }

    #[test]
    fn test_parse_imap_url_plain_defaults_143() {
        assert_eq!(
            parse_imap_url("imap://mail.example.com"),
            Some(("mail.example.com".to_owned(), 143))
        );
    }

    #[test]
    fn test_parse_imap_url_invalid_returns_none() {
        assert_eq!(parse_imap_url("http://example.com"), None);
    }

    #[test]
    fn test_xoauth2_process_builds_sasl_payload() {
        let auth = XOAuth2Auth {
            user: "user@example.com".to_owned(),
            token: "tok123".to_owned(),
        };
        assert_eq!(
            auth.process(b""),
            "user=user@example.com\x01auth=Bearer tok123\x01\x01"
        );
    }

    /// Plaintext IMAP must never be attempted against a remote host, or the
    /// login credentials would go out in the clear. The refusal happens before
    /// any socket is opened, so this test never touches the network.
    #[test]
    fn test_plaintext_to_non_loopback_is_refused() {
        // 198.51.100.0/24 is TEST-NET-2 (RFC 5737): reserved for documentation
        // and never routable, so `lookup_host` resolves it without a lookup.
        let err = open_stream("imap://198.51.100.7", "198.51.100.7", 143)
            .expect_err("plaintext to a remote host must be refused");
        assert!(
            err.contains("in the clear"),
            "expected a plaintext refusal, got: {err}"
        );
    }

    /// The loopback carve-out must not weaken `imaps://` — TLS is used for
    /// loopback too, so a local fake server cannot downgrade a secure config.
    #[test]
    fn test_imaps_to_loopback_still_attempts_tls() {
        // Nothing is listening, so this fails at connect; the point is that it
        // never reports the plaintext refusal, i.e. it took the TLS branch.
        let err = open_stream("imaps://127.0.0.1", "127.0.0.1", 1)
            .expect_err("nothing listens on port 1");
        assert!(
            !err.contains("in the clear"),
            "imaps:// must not take the plaintext branch, got: {err}"
        );
    }
}
