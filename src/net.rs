//! Production IMAP and SMTP backends.
//!
//! `RealImap` implements `ImapBackend` on the blocking `imap` crate over the
//! TLS stream in `connection.rs`; `RealSmtp` implements `SmtpBackend` by
//! speaking SMTP itself over the same kind of stream, with lettre building the
//! message. Both are built from `EmailClientConfig`, and both block: in the
//! sandbox they run in the plugin's worker task, natively on its thread,
//! never in a call from the app.
//!
//! Every read and write is bounded by [`IMAP_TIMEOUT`] (the socket timeouts
//! `connection.rs` sets), so a server that accepted the connection and then
//! went silent fails the exchange instead of blocking it forever.

use crate::cache::EnvelopeCache;
use crate::connection::{ImapSession, RawImap, connect_imap};
use crate::{
    EmailAttachment, EmailClientConfig, EmailMessage, FolderInfo, ImapBackend, MailBody,
    MessageHeader, SmtpBackend,
};
use imap::types::Fetch;
use imap_proto::types::Address;
use lettre::Message;
use lettre::message::header::ContentType;
use lettre::message::{Attachment as LettreAttachment, MultiPart, SinglePart};
use std::time::Duration;

/// Upper bound on a single IMAP or SMTP read or write.
pub const IMAP_TIMEOUT: Duration = crate::connection::IO_TIMEOUT;

// ---------------------------------------------------------------------------
// RealImap
// ---------------------------------------------------------------------------

pub struct RealImap {
    config: EmailClientConfig,
    session: Option<ImapSession>,
    /// Opened on first use, so a copy that never lists a folder (the UI's,
    /// when the worker does the fetching) never opens the database: two
    /// instances writing one SQLite file from separate sandboxes is asking
    /// for a corrupt cache.
    cache: Option<EnvelopeCache>,
    cache_opened: bool,
    /// Separate connection used only for `UID THREAD`; see `fetch_threads`.
    /// Opened lazily on first use and reused across folders.
    thread_conn: Option<RawImap>,
}

impl RealImap {
    pub fn from_config(config: &EmailClientConfig) -> Self {
        RealImap {
            config: config.clone(),
            session: None,
            cache: None,
            cache_opened: false,
            thread_conn: None,
        }
    }

    /// Open the envelope cache the first time it is wanted.
    fn open_cache(&mut self) {
        if !self.cache_opened {
            self.cache_opened = true;
            if !self.config.username.is_empty() {
                self.cache = EnvelopeCache::open(&self.config.username);
            }
        }
    }

    /// Open the session if it is not already live.
    fn ensure_session(&mut self) -> Result<(), String> {
        if self.session.is_none() {
            self.session = Some(connect_imap(&self.config)?);
        }
        Ok(())
    }

    /// The live session. Only call after `ensure_session` has succeeded.
    fn session_mut(&mut self) -> &mut ImapSession {
        self.session.as_mut().expect("ensure_session succeeded")
    }

    /// Invalidate the cached session (called after errors and timeouts).
    fn reset_session(&mut self) {
        if let Some(mut s) = self.session.take() {
            // Best effort: the session is being discarded either way, and after
            // a timeout the server is by definition not answering.
            let _ = s.logout();
        }
    }

    /// Run `op`, dropping the session when it fails so the next call
    /// reconnects rather than reusing a half-open connection.
    fn guarded<R>(&mut self, op: impl FnOnce(&mut Self) -> Result<R, String>) -> Result<R, String> {
        let out = op(self);
        if out.is_err() {
            self.reset_session();
        }
        out
    }

    fn list_folders_inner(&mut self) -> Result<Vec<FolderInfo>, String> {
        self.ensure_session()?;
        let session = self.session_mut();
        let names = session.list(None, Some("*")).map_err(|e| e.to_string())?;

        let folders: Vec<FolderInfo> = names
            .iter()
            .filter_map(|n| {
                // Skip \Noselect folders (containers).
                if n.attributes()
                    .iter()
                    .any(|a| matches!(a, imap_proto::types::NameAttribute::NoSelect))
                {
                    return None;
                }
                // Collect SPECIAL-USE and system attributes as raw strings.
                //
                // imap-proto 0.16 promotes the RFC 6154 attributes to their own
                // variants, where 0.10 delivered every one of them as
                // `Custom("\\Trash")`. Map them back to the same raw strings the
                // rest of the crate matches on (`SpecialFolders`), so folder
                // routing is unaffected by the parser change.
                let attributes: Vec<String> = n
                    .attributes()
                    .iter()
                    .map(|a| {
                        use imap_proto::types::NameAttribute as NA;
                        match a {
                            NA::NoInferiors => "\\Noinferiors".to_owned(),
                            NA::NoSelect => "\\Noselect".to_owned(),
                            NA::Marked => "\\Marked".to_owned(),
                            NA::Unmarked => "\\Unmarked".to_owned(),
                            NA::All => "\\All".to_owned(),
                            NA::Archive => "\\Archive".to_owned(),
                            NA::Drafts => "\\Drafts".to_owned(),
                            NA::Flagged => "\\Flagged".to_owned(),
                            NA::Junk => "\\Junk".to_owned(),
                            NA::Sent => "\\Sent".to_owned(),
                            NA::Trash => "\\Trash".to_owned(),
                            // Already carries its leading backslash.
                            NA::Extension(s) => s.to_string(),
                            // `NameAttribute` is #[non_exhaustive].
                            other => format!("{other:?}"),
                        }
                    })
                    .collect();
                Some(FolderInfo {
                    name: n.name().to_owned(),
                    attributes,
                })
            })
            .collect();
        Ok(folders)
    }

    /// Inner implementation of `list_messages` that accepts the envelope cache
    /// as a separate parameter, allowing the caller to satisfy the borrow
    /// checker by taking the cache out of `self` first.
    fn list_messages_inner(
        &mut self,
        folder: &str,
        limit: usize,
        cache: &mut Option<EnvelopeCache>,
    ) -> Result<Vec<MessageHeader>, String> {
        self.ensure_session()?;
        let session = self.session_mut();

        let mailbox = session.select(folder).map_err(|e| e.to_string())?;
        let total = mailbox.exists as usize;
        let uid_validity = mailbox.uid_validity.unwrap_or(0);

        if total == 0 {
            if let &mut Some(ref c) = cache {
                c.invalidate_folder(folder, uid_validity);
            }
            return Ok(vec![]);
        }

        // --- Cache logic ---
        enum Plan {
            /// Cache already holds every message the server reports.
            ServeCached,
            /// Cache is valid but stale; fetch only UIDs above this one.
            Incremental(u32),
            /// No usable cache: fetch the whole window.
            Full,
        }

        let plan = match cache.as_ref() {
            Some(c) if c.get_uidvalidity(folder) == Some(uid_validity) => {
                if c.cached_count(folder) >= total {
                    Plan::ServeCached
                } else if let Some(max_uid) = c.max_uid(folder) {
                    Plan::Incremental(max_uid)
                } else {
                    c.invalidate_folder(folder, uid_validity);
                    Plan::Full
                }
            }
            Some(c) => {
                // UIDVALIDITY mismatch or first visit: flush and refetch.
                c.invalidate_folder(folder, uid_validity);
                Plan::Full
            }
            None => Plan::Full,
        };

        match plan {
            Plan::ServeCached => {
                let c = cache.as_ref().expect("ServeCached implies a cache");
                return Ok(c.get_latest(folder, limit));
            }
            Plan::Incremental(max_uid) => {
                let new_uid_range = format!("{}:*", max_uid + 1);
                let fetched = session
                    .uid_fetch(&new_uid_range, "(UID ENVELOPE FLAGS)")
                    .map_err(|e| e.to_string())?;
                let new_headers: Vec<MessageHeader> =
                    fetched.iter().filter_map(parse_fetch_to_header).collect();

                let c = cache.as_ref().expect("Incremental implies a cache");
                if !new_headers.is_empty() {
                    c.upsert_all(folder, &new_headers);
                }
                return Ok(c.get_latest(folder, limit));
            }
            Plan::Full => {}
        }

        // Full IMAP fetch (cache miss or no cache).
        let start = if total > limit { total - limit + 1 } else { 1 };
        let fetch_range = format!("{start}:{total}");
        let fetched = session
            .fetch(&fetch_range, "(UID ENVELOPE FLAGS)")
            .map_err(|e| e.to_string())?;

        let mut headers: Vec<MessageHeader> =
            fetched.iter().filter_map(parse_fetch_to_header).collect();

        headers.reverse(); // Most-recent-first.

        if let &mut Some(ref c) = cache {
            c.upsert_all(folder, &headers);
        }

        Ok(headers)
    }

    fn fetch_message_inner(
        &mut self,
        folder: &str,
        uid: u32,
    ) -> Result<Option<EmailMessage>, String> {
        self.ensure_session()?;
        let session = self.session_mut();

        session.select(folder).map_err(|e| e.to_string())?;
        let uid_str = uid.to_string();
        let fetched = session
            .uid_fetch(&uid_str, "BODY[]")
            .map_err(|e| e.to_string())?;

        let raw = fetched
            .iter()
            .find(|m| m.uid == Some(uid))
            .and_then(|m| m.body())
            .map(|b| b.to_vec());

        match raw {
            None => Ok(None),
            Some(bytes) => Ok(Some(parse_rfc2822(uid, &bytes))),
        }
    }

    fn fetch_by_message_id_inner(
        &mut self,
        folder: &str,
        message_id: &str,
    ) -> Result<Option<u32>, String> {
        self.ensure_session()?;
        let session = self.session_mut();

        session.select(folder).map_err(|e| e.to_string())?;
        let search = format!("HEADER Message-ID {message_id}");
        let uids = session.uid_search(&search).map_err(|e| e.to_string())?;
        Ok(uids.iter().next().copied())
    }

    fn set_flags_inner(
        &mut self,
        folder: &str,
        uid: u32,
        add: &[&str],
        remove: &[&str],
    ) -> Result<(), String> {
        self.ensure_session()?;
        let session = self.session_mut();

        session.select(folder).map_err(|e| e.to_string())?;
        let uid_str = uid.to_string();
        if !add.is_empty() {
            let query = format!("+FLAGS ({})", add.join(" "));
            session
                .uid_store(&uid_str, &query)
                .map_err(|e| e.to_string())?;
        }
        if !remove.is_empty() {
            let query = format!("-FLAGS ({})", remove.join(" "));
            session
                .uid_store(&uid_str, &query)
                .map_err(|e| e.to_string())?;
        }
        // Keep the envelope cache in sync.
        self.open_cache();
        if let Some(ref cache) = self.cache {
            let new_seen = if add.contains(&"\\Seen") {
                Some(true)
            } else if remove.contains(&"\\Seen") {
                Some(false)
            } else {
                None
            };
            let new_flagged = if add.contains(&"\\Flagged") {
                Some(true)
            } else if remove.contains(&"\\Flagged") {
                Some(false)
            } else {
                None
            };
            cache.patch_flags(folder, uid, new_seen, new_flagged);
        }
        Ok(())
    }

    fn copy_message_inner(&mut self, folder: &str, uid: u32, dest: &str) -> Result<(), String> {
        self.ensure_session()?;
        let session = self.session_mut();
        session.select(folder).map_err(|e| e.to_string())?;
        session
            .uid_copy(uid.to_string(), dest)
            .map_err(|e| e.to_string())
    }

    fn move_message_inner(&mut self, folder: &str, uid: u32, dest: &str) -> Result<(), String> {
        self.ensure_session()?;
        let session = self.session_mut();
        session.select(folder).map_err(|e| e.to_string())?;
        let uid_str = uid.to_string();

        // Try MOVE extension (RFC 6851) first; fall back to COPY + \Deleted + EXPUNGE.
        if session.uid_mv(&uid_str, dest).is_ok() {
            return Ok(());
        }
        // Fallback ordering matters: a failed COPY must not leave the message
        // marked \Deleted, or it would be destroyed without arriving.
        session
            .uid_copy(&uid_str, dest)
            .map_err(|e| e.to_string())?;
        session
            .uid_store(&uid_str, "+FLAGS (\\Deleted)")
            .map_err(|e| e.to_string())?;
        session.uid_expunge(&uid_str).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn expunge_uid_inner(&mut self, folder: &str, uid: u32) -> Result<(), String> {
        self.ensure_session()?;
        let session = self.session_mut();
        session.select(folder).map_err(|e| e.to_string())?;
        session
            .uid_expunge(uid.to_string())
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn append_inner(&mut self, folder: &str, message: &[u8]) -> Result<(), String> {
        self.ensure_session()?;
        let session = self.session_mut();
        session
            .append(folder, message)
            .finish()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// `fetch_threads` minus the error bookkeeping.
    fn threads_inner(&mut self, folder: &str) -> Result<Option<Vec<Vec<u32>>>, String> {
        if self.thread_conn.is_none() {
            self.thread_conn = Some(RawImap::connect(&self.config)?);
        }
        let raw = self.thread_conn.as_mut().expect("connected above");

        // Returns None (not an error) when the server cannot thread, so the
        // caller falls back to the per-Message-ID SEARCH path.
        let caps = raw.capabilities()?;
        let algo = if caps.iter().any(|c| c == "THREAD=REFERENCES") {
            "REFERENCES"
        } else if caps.iter().any(|c| c == "THREAD=ORDEREDSUBJECT") {
            "ORDEREDSUBJECT"
        } else {
            return Ok(None);
        };

        let response = raw.uid_thread(folder, algo)?;
        Ok(Some(parse_thread_response(&response)))
    }
}

impl ImapBackend for RealImap {
    fn list_folders(&mut self) -> Result<Vec<FolderInfo>, String> {
        self.guarded(|s| s.list_folders_inner())
    }

    fn list_messages(&mut self, folder: &str, limit: usize) -> Result<Vec<MessageHeader>, String> {
        // Take the cache out of self so we can hold a session borrow at the
        // same time (the borrow checker can't prove they're disjoint fields).
        self.open_cache();
        let mut cache = self.cache.take();
        let result = self.guarded(|s| s.list_messages_inner(folder, limit, &mut cache));
        self.cache = cache;
        result
    }

    fn fetch_message(&mut self, folder: &str, uid: u32) -> Result<Option<EmailMessage>, String> {
        self.guarded(|s| s.fetch_message_inner(folder, uid))
    }

    fn fetch_message_by_message_id(
        &mut self,
        folder: &str,
        message_id: &str,
    ) -> Result<Option<EmailMessage>, String> {
        let uid = self.guarded(|s| s.fetch_by_message_id_inner(folder, message_id))?;
        match uid {
            // Reuse the normal fetch path.
            Some(uid) => self.fetch_message(folder, uid),
            None => Ok(None),
        }
    }

    fn set_flags(
        &mut self,
        folder: &str,
        uid: u32,
        add: &[&str],
        remove: &[&str],
    ) -> Result<(), String> {
        self.guarded(|s| s.set_flags_inner(folder, uid, add, remove))
    }

    fn copy_message(&mut self, folder: &str, uid: u32, dest: &str) -> Result<(), String> {
        self.guarded(|s| s.copy_message_inner(folder, uid, dest))
    }

    fn move_message(&mut self, folder: &str, uid: u32, dest: &str) -> Result<(), String> {
        self.guarded(|s| s.move_message_inner(folder, uid, dest))
    }

    fn expunge_uid(&mut self, folder: &str, uid: u32) -> Result<(), String> {
        self.guarded(|s| s.expunge_uid_inner(folder, uid))
    }

    fn append(&mut self, folder: &str, message: &[u8]) -> Result<(), String> {
        self.guarded(|s| s.append_inner(folder, message))
    }

    fn fetch_threads(&mut self, folder: &str) -> Result<Option<Vec<Vec<u32>>>, String> {
        // Runs on a dedicated `RawImap` rather than the main session: no
        // imap-proto release can decode a `* THREAD` response (see `RawImap`'s
        // docs), and keeping it off the main session also stops it from
        // changing which mailbox that session has selected.
        let outcome = self.threads_inner(folder);
        if outcome.is_err() {
            // Force a reconnect on the next call — a half-open connection
            // would fail every subsequent fetch.
            self.thread_conn = None;
        }
        outcome
    }
}

// ---------------------------------------------------------------------------
// RealSmtp
// ---------------------------------------------------------------------------

pub struct RealSmtp {
    config: EmailClientConfig,
}

impl RealSmtp {
    pub fn from_config(config: &EmailClientConfig) -> Self {
        RealSmtp {
            config: config.clone(),
        }
    }
}

/// Parse `smtps://host` or `smtps://host:port` → `(host, port)`.
fn parse_smtp_url(url: &str) -> Option<(String, u16)> {
    let rest = url
        .strip_prefix("smtps://")
        .or_else(|| url.strip_prefix("smtp://"))?;
    if let Some(colon) = rest.rfind(':') {
        let host = rest[..colon].to_owned();
        let port: u16 = rest[colon + 1..].parse().ok()?;
        Some((host, port))
    } else {
        let port = if url.starts_with("smtps://") {
            465
        } else {
            587
        };
        Some((rest.to_owned(), port))
    }
}

impl SmtpBackend for RealSmtp {
    fn send(
        &mut self,
        from: &str,
        to: &[&str],
        cc: &[&str],
        bcc: &[&str],
        subject: &str,
        body: &MailBody,
        attachments: &[(&str, &[u8])],
    ) -> Result<Vec<u8>, String> {
        let (host, port) = parse_smtp_url(&self.config.smtp_url)
            .ok_or_else(|| format!("cannot parse SMTP URL: {}", self.config.smtp_url))?;

        if to.is_empty() {
            return Err("no recipients".to_owned());
        }
        let mut builder = Message::builder().from(
            from.parse()
                .map_err(|e: lettre::address::AddressError| e.to_string())?,
        );
        for addr in to {
            builder = builder.to(addr
                .parse()
                .map_err(|e: lettre::address::AddressError| e.to_string())?);
        }
        for addr in cc {
            builder = builder.cc(addr
                .parse()
                .map_err(|e: lettre::address::AddressError| e.to_string())?);
        }
        for addr in bcc {
            builder = builder.bcc(
                addr.parse()
                    .map_err(|e: lettre::address::AddressError| e.to_string())?,
            );
        }
        let builder = builder.subject(subject);

        let body_str = match body {
            MailBody::Text(s) => s.clone(),
            MailBody::Ffon(elems) => {
                sicompass_sdk::ffon::to_json_string(elems).map_err(|e| e.to_string())?
            }
        };

        let email = if attachments.is_empty() {
            builder
                .header(ContentType::TEXT_PLAIN)
                .body(body_str)
                .map_err(|e| e.to_string())?
        } else {
            let body_part = SinglePart::builder()
                .header(ContentType::TEXT_PLAIN)
                .body(body_str);
            let mut mp = MultiPart::mixed().singlepart(body_part);
            for (filename, bytes) in attachments {
                let ct = "application/octet-stream"
                    .parse::<ContentType>()
                    .map_err(|e| e.to_string())?;
                mp = mp.singlepart(
                    LettreAttachment::new(filename.to_string()).body(bytes.to_vec(), ct),
                );
            }
            builder.multipart(mp).map_err(|e| e.to_string())?
        };

        let raw = email.formatted();
        let envelope = email.envelope();
        let recipients: Vec<String> = envelope.to().iter().map(|a| a.to_string()).collect();
        let sender = envelope
            .from()
            .map(|a| a.to_string())
            .ok_or("the message has no sender")?;

        let mut smtp = Smtp::open(&self.config.smtp_url, &host, port)?;
        let auth = if self.config.oauth_access_token.is_empty() {
            let plain = format!("\0{}\0{}", self.config.username, self.config.password);
            format!("AUTH PLAIN {}", b64(plain.as_bytes()))
        } else {
            let payload = crate::connection::xoauth2_payload(
                &self.config.username,
                &self.config.oauth_access_token,
            );
            format!("AUTH XOAUTH2 {}", b64(payload.as_bytes()))
        };
        smtp.command(&auth, &[235])?;
        smtp.command(&format!("MAIL FROM:<{sender}>"), &[250])?;
        for rcpt in &recipients {
            smtp.command(&format!("RCPT TO:<{rcpt}>"), &[250, 251])?;
        }
        smtp.command("DATA", &[354])?;
        smtp.write(&dot_stuff(&raw))?;
        smtp.command(".", &[250])?;
        // The message is accepted; a server that drops the connection before
        // answering QUIT has not lost it.
        let _ = smtp.command("QUIT", &[221]);
        Ok(raw)
    }
}

/// Base64, standard alphabet.
fn b64(bytes: &[u8]) -> String {
    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes)
}

/// The message body of `DATA`: every line that starts with `.` gets a second
/// one (RFC 5321 4.5.2), and it ends with a line break so the lone `.` that
/// closes it is on a line of its own.
fn dot_stuff(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + 16);
    let mut at_line_start = true;
    for &b in raw {
        if at_line_start && b == b'.' {
            out.push(b'.');
        }
        out.push(b);
        at_line_start = b == b'\n';
    }
    if !out.ends_with(b"\r\n") {
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// One SMTP conversation, always encrypted before anything is sent: implicit
/// TLS for `smtps://` (465), STARTTLS for `smtp://` (587).
struct Smtp {
    io: std::io::BufReader<crate::connection::ImapStream>,
}

impl Smtp {
    /// Connect, read the greeting and say EHLO, upgrading a `smtp://`
    /// connection with STARTTLS first. A server that will not upgrade gets
    /// no credentials.
    fn open(url: &str, host: &str, port: u16) -> Result<Self, String> {
        use crate::connection::{ImapStream, open_tcp, open_tls, tls};
        if url.starts_with("smtps://") {
            let mut smtp = Smtp {
                io: std::io::BufReader::new(open_tls(host, port)?),
            };
            smtp.expect(&[220])?;
            smtp.command("EHLO sicompass", &[250])?;
            return Ok(smtp);
        }
        let mut plain = Smtp {
            io: std::io::BufReader::new(ImapStream::Plain(open_tcp(host, port)?)),
        };
        plain.expect(&[220])?;
        plain.command("EHLO sicompass", &[250])?;
        plain.command("STARTTLS", &[220])?;
        // Nothing follows the 220 until the handshake, so the reader holds
        // nothing that would be lost here.
        let ImapStream::Plain(tcp) = plain.io.into_inner() else {
            unreachable!("opened in the clear above");
        };
        let mut smtp = Smtp {
            io: std::io::BufReader::new(tls(host, tcp)?),
        };
        // The capabilities before the upgrade are not to be trusted.
        smtp.command("EHLO sicompass", &[250])?;
        Ok(smtp)
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
        use std::io::Write;
        let s = self.io.get_mut();
        s.write_all(bytes)
            .and_then(|_| s.flush())
            .map_err(|e| e.to_string())
    }

    /// Send `line` and read the reply, which must carry one of `ok`.
    fn command(&mut self, line: &str, ok: &[u16]) -> Result<String, String> {
        self.write(format!("{line}\r\n").as_bytes())?;
        self.expect(ok).map_err(|e| {
            // Never echo credentials back in an error.
            let verb = line
                .split_whitespace()
                .take(2)
                .collect::<Vec<_>>()
                .join(" ");
            let verb = if verb.starts_with("AUTH") {
                "AUTH".to_owned()
            } else {
                verb
            };
            format!("SMTP {verb}: {e}")
        })
    }

    /// Read one reply (all its continuation lines) and check its code.
    fn expect(&mut self, ok: &[u16]) -> Result<String, String> {
        use std::io::BufRead;
        let mut text = String::new();
        loop {
            let mut line = String::new();
            match self.io.read_line(&mut line) {
                Ok(0) => return Err("the server closed the connection".to_owned()),
                Ok(_) => {}
                Err(e) => return Err(e.to_string()),
            }
            let line = line.trim_end();
            text.push_str(line);
            text.push('\n');
            // `250-...` continues, `250 ...` (or a bare `250`) ends the reply.
            if line.len() < 4 || line.as_bytes()[3] != b'-' {
                let code: u16 = line.get(..3).and_then(|c| c.parse().ok()).unwrap_or(0);
                return if ok.contains(&code) {
                    Ok(text)
                } else {
                    Err(text.trim_end().to_owned())
                };
            }
        }
    }
}

// ---------------------------------------------------------------------------
// XOAUTH2 IMAP authenticator
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// RFC 2822 raw-message parser
// ---------------------------------------------------------------------------

/// Parse a raw RFC 2822 email (BODY[] response) into an `EmailMessage`.
fn parse_rfc2822(uid: u32, raw: &[u8]) -> EmailMessage {
    let text = String::from_utf8_lossy(raw);

    // Split headers from body at the first blank line.
    let (header_block, raw_body) = if let Some(pos) = text.find("\r\n\r\n") {
        (&text[..pos], &text[pos + 4..])
    } else if let Some(pos) = text.find("\n\n") {
        (&text[..pos], &text[pos + 2..])
    } else {
        (text.as_ref(), "")
    };

    let mut from = String::new();
    let mut to = String::new();
    let mut subject = String::new();
    let mut date = String::new();
    let mut message_id = String::new();
    let mut in_reply_to = String::new();
    let mut references = String::new();
    let mut content_type = String::new();
    let mut content_transfer_encoding = String::new();

    // Header parsing with folded-line support (RFC 2822 §2.2.3).
    let mut lines = header_block.lines().peekable();
    while let Some(line) = lines.next() {
        // Unfold continuation lines (lines starting with whitespace).
        let mut value = line.to_owned();
        while lines
            .peek()
            .is_some_and(|l| l.starts_with(' ') || l.starts_with('\t'))
        {
            if let Some(cont) = lines.next() {
                value.push(' ');
                value.push_str(cont.trim());
            }
        }
        let lc = value.to_ascii_lowercase();
        if lc.starts_with("from: ") {
            from = value[6..].to_owned();
        } else if lc.starts_with("to: ") {
            to = value[4..].to_owned();
        } else if lc.starts_with("subject: ") {
            subject = value[9..].to_owned();
        } else if lc.starts_with("date: ") {
            date = value[6..].to_owned();
        } else if lc.starts_with("message-id: ") {
            message_id = value[12..].to_owned();
        } else if lc.starts_with("in-reply-to: ") {
            in_reply_to = value[13..].to_owned();
        } else if lc.starts_with("references: ") {
            references = value[12..].to_owned();
        } else if lc.starts_with("content-type: ") {
            content_type = value[14..].to_owned();
        } else if lc.starts_with("content-transfer-encoding: ") {
            content_transfer_encoding = value[27..].trim().to_ascii_lowercase();
        }
    }

    let body = parse_body_part(raw_body, &content_type, &content_transfer_encoding);
    let attachments = parse_attachments(raw_body, &content_type);

    EmailMessage {
        uid,
        from,
        to,
        subject,
        date,
        body,
        message_id,
        in_reply_to,
        references,
        attachments,
    }
}

/// Walk a MIME body looking for attachment parts (Content-Disposition: attachment
/// or non-text, non-multipart parts in multipart/mixed).
fn parse_attachments(raw_body: &str, content_type: &str) -> Vec<EmailAttachment> {
    let ct_lc = content_type.to_ascii_lowercase();
    let mime = ct_lc.split(';').next().unwrap_or("").trim();
    if !mime.starts_with("multipart/") {
        return vec![];
    }
    let boundary = match extract_boundary(content_type) {
        Some(b) => b,
        None => return vec![],
    };
    let delimiter = format!("--{boundary}");
    let mut attachments = Vec::new();

    for chunk in raw_body.split(&delimiter) {
        let chunk = chunk.trim_start_matches('-').trim();
        if chunk.is_empty() {
            continue;
        }

        let (part_headers, part_body) = if let Some(pos) = chunk.find("\r\n\r\n") {
            (&chunk[..pos], &chunk[pos + 4..])
        } else if let Some(pos) = chunk.find("\n\n") {
            (&chunk[..pos], &chunk[pos + 2..])
        } else {
            continue;
        };

        let mut part_ct = String::new();
        let mut part_cte = String::new();
        let mut disposition = String::new();
        let mut filename = String::new();

        for line in part_headers.lines() {
            let lc = line.to_ascii_lowercase();
            if lc.starts_with("content-type: ") {
                part_ct = line[14..].to_owned();
            } else if lc.starts_with("content-transfer-encoding: ") {
                part_cte = line[27..].trim().to_ascii_lowercase();
            } else if let Some(rest) = lc.strip_prefix("content-disposition: ") {
                disposition = rest.to_owned();
                // Extract filename= from the same header line.
                for param in line[21..].split(';') {
                    let p = param.trim();
                    let pl = p.to_ascii_lowercase();
                    if pl.starts_with("filename=") || pl.starts_with("filename*=") {
                        filename = p
                            .split_once('=')
                            .map(|x| x.1)
                            .unwrap_or("")
                            .trim_matches('"')
                            .to_owned();
                    }
                }
            }
        }

        let is_attachment = disposition.trim_start().starts_with("attachment");
        let part_mime = part_ct
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let is_non_text = !part_mime.is_empty()
            && !part_mime.starts_with("text/")
            && !part_mime.starts_with("multipart/");

        if is_attachment || is_non_text {
            // Decode bytes.
            let data: Vec<u8> = match part_cte.as_str() {
                "base64" => {
                    use base64::Engine as _;
                    let compact: String =
                        part_body.chars().filter(|c| !c.is_whitespace()).collect();
                    base64::engine::general_purpose::STANDARD
                        .decode(compact.as_bytes())
                        .unwrap_or_default()
                }
                _ => part_body.as_bytes().to_vec(),
            };
            if filename.is_empty() {
                filename = "attachment".to_owned();
            }
            attachments.push(EmailAttachment {
                filename,
                content_type: part_mime,
                data,
            });
        }
    }
    attachments
}

/// Parse a MIME body part given its content-type and transfer-encoding headers.
fn parse_body_part(raw: &str, content_type: &str, cte: &str) -> MailBody {
    let ct_lc = content_type.to_ascii_lowercase();
    let mime = ct_lc.split(';').next().unwrap_or("").trim();

    // Handle multipart/* by extracting the best sub-part.
    if mime.starts_with("multipart/") {
        if let Some(boundary) = extract_boundary(content_type) {
            return parse_multipart(raw, &boundary);
        }
        return MailBody::Text(raw.to_owned());
    }

    let decoded = decode_transfer_encoding(raw, cte);

    match mime {
        "text/html" => {
            let elems = sicompass_sdk::ffon::html_to_ffon(&decoded, "");
            MailBody::Text(crate::flatten_ffon_to_text(&elems))
        }
        "application/json" => {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&decoded)
                && sicompass_sdk::ffon::is_ffon(&v)
                && let Ok(elems) = serde_json::from_value(v)
            {
                return MailBody::Ffon(elems);
            }
            MailBody::Text(decoded)
        }
        // text/plain or unknown/empty — treat as plain text, but promote to
        // Ffon if the content is valid FFON JSON (sicompass-sent bodies).
        _ => {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&decoded)
                && sicompass_sdk::ffon::is_ffon(&v)
                && let Ok(elems) = serde_json::from_value(v)
            {
                return MailBody::Ffon(elems);
            }
            MailBody::Text(decoded)
        }
    }
}

/// Decode a transfer-encoded body string.
fn decode_transfer_encoding(raw: &str, cte: &str) -> String {
    match cte.trim() {
        "quoted-printable" => {
            quoted_printable::decode(raw.as_bytes(), quoted_printable::ParseMode::Robust)
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_else(|_| raw.to_owned())
        }
        "base64" => {
            use base64::Engine as _;
            let compact: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
            base64::engine::general_purpose::STANDARD
                .decode(compact.as_bytes())
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_else(|_| raw.to_owned())
        }
        _ => raw.to_owned(),
    }
}

/// Extract the `boundary=` parameter from a Content-Type value.
fn extract_boundary(content_type: &str) -> Option<String> {
    for part in content_type.split(';').skip(1) {
        let p = part.trim();
        let lc = p.to_ascii_lowercase();
        if lc.starts_with("boundary=") {
            let val = &p[9..].trim_matches('"');
            return Some(val.to_string());
        }
    }
    None
}

/// Split a multipart body and return the best available part.
/// Preference order: FFON (application/json) > HTML > plain text.
fn parse_multipart(raw: &str, boundary: &str) -> MailBody {
    let delimiter = format!("--{boundary}");
    let mut parts: Vec<MailBody> = Vec::new();

    for chunk in raw.split(&delimiter) {
        let chunk = chunk.trim_start_matches('-').trim();
        if chunk.is_empty() {
            continue;
        }

        // Split chunk into its own headers and body.
        let (part_headers, part_body) = if let Some(pos) = chunk.find("\r\n\r\n") {
            (&chunk[..pos], &chunk[pos + 4..])
        } else if let Some(pos) = chunk.find("\n\n") {
            (&chunk[..pos], &chunk[pos + 2..])
        } else {
            continue;
        };

        let mut part_ct = String::new();
        let mut part_cte = String::new();
        for line in part_headers.lines() {
            let lc = line.to_ascii_lowercase();
            if lc.starts_with("content-type: ") {
                part_ct = line[14..].to_owned();
            } else if lc.starts_with("content-transfer-encoding: ") {
                part_cte = line[27..].trim().to_ascii_lowercase();
            }
        }
        parts.push(parse_body_part(part_body, &part_ct, &part_cte));
    }

    // Pick in preference order: Ffon > Text.
    let ffon = parts.iter().find(|p| matches!(p, MailBody::Ffon(_)));
    if let Some(f) = ffon {
        return f.clone();
    }
    parts
        .into_iter()
        .find(|p| matches!(p, MailBody::Text(_)))
        .unwrap_or_else(|| MailBody::Text(String::new()))
}

/// Format an IMAP address struct as "Name <mailbox@host>" or "mailbox@host".
/// Convert a single IMAP FETCH result into a `MessageHeader`, or `None` if
/// the fetch result is missing UID or ENVELOPE data.
fn parse_fetch_to_header(m: &Fetch) -> Option<MessageHeader> {
    let uid = m.uid?;
    let env = m.envelope()?;
    let subject = env
        .subject
        .as_deref()
        .and_then(|b| std::str::from_utf8(b).ok())
        .unwrap_or("")
        .to_owned();
    let from = env
        .from
        .as_deref()
        .and_then(|addrs| addrs.first())
        .map(|a| format_address(a))
        .unwrap_or_default();
    let date = env
        .date
        .as_deref()
        .and_then(|b| std::str::from_utf8(b).ok())
        .unwrap_or("")
        .to_owned();
    let seen = m
        .flags()
        .iter()
        .any(|f| matches!(f, imap::types::Flag::Seen));
    let flagged = m
        .flags()
        .iter()
        .any(|f| matches!(f, imap::types::Flag::Flagged));
    let message_id = env
        .message_id
        .as_deref()
        .and_then(|b| std::str::from_utf8(b).ok())
        .unwrap_or("")
        .to_owned();
    Some(MessageHeader {
        uid,
        from,
        subject,
        date,
        seen,
        flagged,
        message_id,
    })
}

fn format_address(addr: &Address<'_>) -> String {
    let name = addr
        .name
        .as_deref()
        .and_then(|b| std::str::from_utf8(b).ok())
        .unwrap_or("")
        .to_owned();
    let mailbox = addr
        .mailbox
        .as_deref()
        .and_then(|b| std::str::from_utf8(b).ok())
        .unwrap_or("");
    let host = addr
        .host
        .as_deref()
        .and_then(|b| std::str::from_utf8(b).ok())
        .unwrap_or("");

    if !name.is_empty() && !mailbox.is_empty() && !host.is_empty() {
        format!("{name} <{mailbox}@{host}>")
    } else if !mailbox.is_empty() && !host.is_empty() {
        format!("{mailbox}@{host}")
    } else {
        name
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Parse the raw bytes from `UID THREAD … ALL` into a list of threads.
///
/// Each thread is a flat `Vec<u32>` of all UIDs belonging to it (nested
/// children are flattened; ordering is depth-first).  Returns an empty vec
/// when the response contains no `* THREAD` line or no UIDs.
///
/// Example input line: `* THREAD (1 2 3)(4)(5 (6)(7 8))\r\n`
/// Returns: `[[1,2,3], [4], [5,6,7,8]]`
pub(crate) fn parse_thread_response(response: &str) -> Vec<Vec<u32>> {
    // Find the * THREAD untagged response.
    let data = response
        .lines()
        .find(|l| l.starts_with("* THREAD"))
        .and_then(|l| l.strip_prefix("* THREAD"))
        .unwrap_or("")
        .trim();

    let mut threads: Vec<Vec<u32>> = Vec::new();
    let mut current: Vec<u32> = Vec::new();
    let mut depth: usize = 0;
    let mut num_buf = String::new();

    let flush_num = |buf: &mut String, cur: &mut Vec<u32>| {
        if !buf.is_empty() {
            if let Ok(uid) = buf.parse::<u32>() {
                cur.push(uid);
            }
            buf.clear();
        }
    };

    for ch in data.chars() {
        match ch {
            '(' => {
                flush_num(&mut num_buf, &mut current);
                depth += 1;
            }
            ')' => {
                flush_num(&mut num_buf, &mut current);
                depth = depth.saturating_sub(1);
                if depth == 0 && !current.is_empty() {
                    threads.push(std::mem::take(&mut current));
                }
            }
            ' ' | '\t' => {
                flush_num(&mut num_buf, &mut current);
            }
            c if c.is_ascii_digit() => {
                num_buf.push(c);
            }
            _ => {
                flush_num(&mut num_buf, &mut current);
            }
        }
    }

    threads
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smtp_without_starttls_never_sends_the_password() {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut w = stream.try_clone().unwrap();
            let mut r = BufReader::new(stream);
            let mut heard = Vec::new();
            w.write_all(b"220 fake ESMTP\r\n").unwrap();
            let mut line = String::new();
            while r.read_line(&mut line).unwrap_or(0) > 0 {
                heard.push(line.trim_end().to_owned());
                let reply: &[u8] = if line.starts_with("EHLO") {
                    b"250 fake\r\n"
                } else if line.starts_with("STARTTLS") {
                    b"454 TLS not available\r\n"
                } else {
                    b"250 ok\r\n"
                };
                if w.write_all(reply).is_err() {
                    break;
                }
                line.clear();
            }
            heard
        });

        let config = EmailClientConfig {
            smtp_url: format!("smtp://127.0.0.1:{port}"),
            username: "me@example.com".to_owned(),
            password: "hunter2".to_owned(),
            ..Default::default()
        };
        let err = RealSmtp::from_config(&config)
            .send(
                "me@example.com",
                &["you@example.com"],
                &[],
                &[],
                "hi",
                &MailBody::Text("hello".to_owned()),
                &[],
            )
            .expect_err("a server that cannot upgrade must not be used");
        assert!(err.contains("STARTTLS"), "{err}");
        let heard = server.join().unwrap();
        assert_eq!(heard, ["EHLO sicompass", "STARTTLS"]);
    }

    #[test]
    fn test_parse_smtp_url_with_port() {
        assert_eq!(
            parse_smtp_url("smtps://smtp.gmail.com:465"),
            Some(("smtp.gmail.com".to_owned(), 465))
        );
    }

    #[test]
    fn test_parse_smtp_url_without_port_defaults_465() {
        assert_eq!(
            parse_smtp_url("smtps://smtp.gmail.com"),
            Some(("smtp.gmail.com".to_owned(), 465))
        );
    }

    #[test]
    fn test_parse_smtp_url_starttls_defaults_587() {
        assert_eq!(
            parse_smtp_url("smtp://smtp.example.com"),
            Some(("smtp.example.com".to_owned(), 587))
        );
    }

    #[test]
    fn test_parse_smtp_url_invalid_returns_none() {
        assert_eq!(parse_smtp_url(""), None);
        assert_eq!(parse_smtp_url("http://example.com"), None);
    }

    #[test]
    fn test_parse_rfc2822_extracts_fields() {
        let raw = b"From: Alice <alice@example.com>\r\n\
                    To: Bob <bob@example.com>\r\n\
                    Subject: Hello\r\n\
                    Date: Mon, 1 Jan 2025 00:00:00 +0000\r\n\
                    Message-ID: <abc@example.com>\r\n\
                    References: <prev@example.com>\r\n\
                    \r\n\
                    Hi there!\r\n";
        let msg = parse_rfc2822(42, raw);
        assert_eq!(msg.uid, 42);
        assert_eq!(msg.from, "Alice <alice@example.com>");
        assert_eq!(msg.to, "Bob <bob@example.com>");
        assert_eq!(msg.subject, "Hello");
        assert_eq!(msg.message_id, "<abc@example.com>");
        assert_eq!(msg.references, "<prev@example.com>");
        assert!(matches!(&msg.body, MailBody::Text(s) if s.contains("Hi there!")));
    }

    #[test]
    fn test_parse_rfc2822_lf_only_separator() {
        let raw = b"From: a@b.com\nSubject: Test\n\nBody text\n";
        let msg = parse_rfc2822(1, raw);
        assert_eq!(msg.subject, "Test");
        assert!(matches!(&msg.body, MailBody::Text(s) if s.contains("Body text")));
    }

    #[test]
    fn test_parse_rfc2822_no_body() {
        let raw = b"From: a@b.com\r\nSubject: Empty\r\n\r\n";
        let msg = parse_rfc2822(1, raw);
        assert_eq!(msg.subject, "Empty");
        assert!(matches!(&msg.body, MailBody::Text(s) if s.is_empty()));
    }

    #[test]
    fn test_parse_rfc2822_html_content_type() {
        let raw = b"From: a@b.com\r\nSubject: Html\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>Hello</p>\r\n";
        let msg = parse_rfc2822(1, raw);
        // HTML is flattened to plain text at parse time.
        assert!(matches!(&msg.body, MailBody::Text(s) if s.contains("Hello")));
    }

    #[test]
    fn test_parse_rfc2822_multipart_alternative_html_flattened_to_text() {
        let boundary = "bound1";
        let body = format!(
            "--{boundary}\r\nContent-Type: text/plain\r\n\r\nPlain text\r\n\
             --{boundary}\r\nContent-Type: text/html\r\n\r\n<p>Rich</p>\r\n\
             --{boundary}--\r\n"
        );
        let raw = format!(
            "From: a@b.com\r\nSubject: Multi\r\nContent-Type: multipart/alternative; boundary=\"{boundary}\"\r\n\r\n{body}"
        );
        let msg = parse_rfc2822(1, raw.as_bytes());
        // Both parts are Text after parsing; first Text match wins (plain text part).
        assert!(matches!(&msg.body, MailBody::Text(_)));
    }

    #[test]
    fn test_parse_rfc2822_application_json_ffon() {
        let ffon_json = r#"[{"Heading:":["line1","line2"]}]"#;
        let raw = format!(
            "From: a@b.com\r\nSubject: Ffon\r\nContent-Type: application/json; charset=utf-8\r\n\r\n{ffon_json}\r\n"
        );
        let msg = parse_rfc2822(1, raw.as_bytes());
        assert!(matches!(&msg.body, MailBody::Ffon(elems) if !elems.is_empty()));
    }

    #[test]
    fn test_parse_rfc2822_text_plain_ffon_promoted() {
        // sicompass sends FFON as text/plain JSON; receiver must promote it back to Ffon.
        let ffon_json = r#"[{"Heading:":["line1","line2"]}]"#;
        let raw = format!(
            "From: a@b.com\r\nSubject: Ffon\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{ffon_json}\r\n"
        );
        let msg = parse_rfc2822(1, raw.as_bytes());
        assert!(matches!(&msg.body, MailBody::Ffon(elems) if !elems.is_empty()));
    }

    #[test]
    fn test_parse_rfc2822_quoted_printable_decode() {
        // "café" in quoted-printable
        let raw = b"From: a@b.com\r\nSubject: QP\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\ncaf=C3=A9\r\n";
        let msg = parse_rfc2822(1, raw);
        assert!(matches!(&msg.body, MailBody::Text(s) if s.contains("café")));
    }

    #[test]
    fn test_format_address_with_name() {
        let addr = Address {
            name: Some(b"Alice".as_slice().into()),
            adl: None,
            mailbox: Some(b"alice".as_slice().into()),
            host: Some(b"example.com".as_slice().into()),
        };
        assert_eq!(format_address(&addr), "Alice <alice@example.com>");
    }

    #[test]
    fn test_format_address_without_name() {
        let addr = Address {
            name: None,
            adl: None,
            mailbox: Some(b"bob".as_slice().into()),
            host: Some(b"example.com".as_slice().into()),
        };
        assert_eq!(format_address(&addr), "bob@example.com");
    }

    /// Live integration test — skipped unless SICOMPASS_TEST_IMAP_URL is set.
    #[test]
    #[ignore]
    fn real_imap_smoke() {
        let imap_url = std::env::var("SICOMPASS_TEST_IMAP_URL").unwrap();
        let username = std::env::var("SICOMPASS_TEST_USERNAME").unwrap();
        let password = std::env::var("SICOMPASS_TEST_PASSWORD").unwrap();
        let config = EmailClientConfig {
            imap_url,
            username,
            password,
            ..Default::default()
        };

        let mut backend = RealImap::from_config(&config);
        let folders = backend.list_folders().expect("list_folders failed");
        assert!(!folders.is_empty(), "expected at least one folder");
        println!(
            "folders: {:?}",
            folders.iter().map(|f| &f.name).collect::<Vec<_>>()
        );

        let inbox = folders
            .iter()
            .find(|f| f.name.to_uppercase() == "INBOX")
            .expect("INBOX not found");
        let headers = backend
            .list_messages(&inbox.name, 5)
            .expect("list_messages failed");
        println!("inbox headers: {headers:?}");
    }

    // ---- parse_thread_response ----

    #[test]
    fn test_parse_thread_linear_threads() {
        let response = "* THREAD (1 2 3)(4)(5)\r\nA001 OK\r\n";
        let threads = parse_thread_response(response);
        assert_eq!(threads, vec![vec![1, 2, 3], vec![4], vec![5]]);
    }

    #[test]
    fn test_parse_thread_nested() {
        // (5 (6)(7 8)) → all four UIDs in one thread
        let response = "* THREAD (1)(2 3)(4)(5 (6)(7 8))\r\nA002 OK\r\n";
        let threads = parse_thread_response(response);
        assert_eq!(threads.len(), 4);
        assert_eq!(threads[0], vec![1]);
        assert_eq!(threads[1], vec![2, 3]);
        assert_eq!(threads[2], vec![4]);
        // 5, then two children (6) and (7 8) — flattened to [5,6,7,8]
        assert!(threads[3].contains(&5));
        assert!(threads[3].contains(&6));
        assert!(threads[3].contains(&7));
        assert!(threads[3].contains(&8));
    }

    #[test]
    fn test_parse_thread_empty_response() {
        let threads = parse_thread_response("A003 OK THREAD completed\r\n");
        assert!(threads.is_empty());
    }

    #[test]
    fn test_parse_thread_no_thread_line() {
        let threads = parse_thread_response("* OK [CAPABILITY IMAP4rev1]\r\n");
        assert!(threads.is_empty());
    }
}
