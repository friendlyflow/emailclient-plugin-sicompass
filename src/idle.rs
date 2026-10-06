//! IMAP IDLE background task.
//!
//! Runs an IMAP IDLE connection for a single folder on the shared email
//! runtime. When the server reports EXISTS, EXPUNGE or VANISHED, the shared
//! `notify` flag is set so the provider refreshes on the next render cycle.
//!
//! A `CancellationToken` under `tokio::select!` cancels the wait immediately,
//! so a stop or a folder switch takes effect at once, and the keepalive
//! interval is the RFC 2177 29 minutes. A refreshed OAuth token reaches the
//! running task through a shared slot.

use crate::EmailClientConfig;
use crate::connection::{ImapSession, connect_imap, runtime};
use async_imap::extensions::idle::IdleResponse;
use async_imap::imap_proto::{MailboxDatum, Response};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const RECONNECT_DELAY: Duration = Duration::from_secs(10);
/// RFC 2177 advises re-issuing IDLE at least every 29 minutes so the server
/// does not drop an apparently inactive client.
const IDLE_KEEPALIVE: Duration = Duration::from_secs(29 * 60);

// ---------------------------------------------------------------------------
// IdleController
// ---------------------------------------------------------------------------

pub struct IdleController {
    /// Shared flag written by the IDLE task when new mail arrives.
    notify: Arc<AtomicBool>,
    /// What is being watched: the folder, and the server and account, so
    /// re-rendering the same folder does not restart the watch.
    watching: Option<(String, String, String)>,
    /// Cancels the running task; `None` when nothing is running.
    cancel: Option<CancellationToken>,
    /// The OAuth access token the IDLE session should authenticate with.
    ///
    /// Shared rather than cloned into the task: the token refresh in `lib.rs`
    /// replaces the access token roughly hourly, and an IDLE session that
    /// captured it at start-up would keep reconnecting with a dead credential
    /// until the user re-entered the folder.
    token: Arc<Mutex<String>>,
}

impl IdleController {
    pub fn new(notify: Arc<AtomicBool>) -> Self {
        IdleController {
            notify,
            watching: None,
            cancel: None,
            token: Arc::new(Mutex::new(String::new())),
        }
    }

    /// Watch `folder`. Watching it already, on the same server and account,
    /// changes nothing.
    ///
    /// Otherwise stops any existing session first, then spawns a new task on
    /// the shared email runtime.
    pub fn start(&mut self, config: EmailClientConfig, folder: String) {
        let key = (
            folder.clone(),
            config.imap_url.clone(),
            config.username.clone(),
        );
        if self.watching.as_ref() == Some(&key) {
            return;
        }
        self.stop();
        self.watching = Some(key);

        *self.token.lock().expect("token mutex") = config.oauth_access_token.clone();

        let notify = Arc::clone(&self.notify);
        let token = Arc::clone(&self.token);
        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());

        runtime().spawn(async move {
            idle_loop(config, folder, notify, cancel, token).await;
        });
    }

    /// Publish a freshly refreshed OAuth access token to the running session.
    ///
    /// Takes effect on the IDLE task's next reconnect; the current IDLE
    /// continues on the old token until the server drops it, which is the same
    /// behaviour as any other long-lived IMAP connection.
    pub fn update_token(&self, access_token: &str) {
        *self.token.lock().expect("token mutex") = access_token.to_owned();
    }

    /// Stop the background IDLE task.
    ///
    /// Returns immediately: cancelling the token interrupts the IDLE wait at
    /// once, and the task sends DONE and logs out on its way down. Nothing here
    /// blocks the app's call.
    pub fn stop(&mut self) {
        if self.watching.take().is_none() {
            return;
        }
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
        }
    }
}

impl Drop for IdleController {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// IDLE loop
// ---------------------------------------------------------------------------

async fn idle_loop(
    config: EmailClientConfig,
    folder: String,
    notify: Arc<AtomicBool>,
    cancel: CancellationToken,
    token: Arc<Mutex<String>>,
) {
    while !cancel.is_cancelled() {
        if let Err(e) = run_idle_session(&config, &folder, &notify, &cancel, &token).await {
            eprintln!("emailclient_idle: session error: {e}");
        }

        if cancel.is_cancelled() {
            break;
        }

        // Back off before reconnecting, but wake immediately on cancellation.
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(RECONNECT_DELAY) => {}
        }
    }
}

/// Connect, authenticate, select the folder, then run the IDLE inner loop.
async fn run_idle_session(
    config: &EmailClientConfig,
    folder: &str,
    notify: &Arc<AtomicBool>,
    cancel: &CancellationToken,
    token: &Arc<Mutex<String>>,
) -> Result<(), String> {
    // Re-read the shared token on every reconnect so a refresh that happened
    // while we were idling is picked up here.
    let mut config = config.clone();
    {
        let current = token.lock().expect("token mutex");
        if !current.is_empty() {
            config.oauth_access_token = current.clone();
        }
    }

    let mut session: ImapSession = connect_imap(&config).await?;
    session.select(folder).await.map_err(|e| e.to_string())?;

    while !cancel.is_cancelled() {
        let mut handle = session.idle();
        handle.init().await.map_err(|e| e.to_string())?;

        // Scoped so the mutable borrow of `handle` ends before `done()`
        // consumes it.
        let outcome = {
            let (wait, _stop) = handle.wait_with_timeout(IDLE_KEEPALIVE);
            tokio::select! {
                result = wait => Some(result.map_err(|e| e.to_string())?),
                _ = cancel.cancelled() => None,
            }
        };

        // Always hand the session back, so a cancelled wait still leaves the
        // connection in a state we can log out of cleanly.
        session = handle.done().await.map_err(|e| e.to_string())?;

        match outcome {
            // Cancelled.
            None => break,
            Some(IdleResponse::ManualInterrupt) => break,
            // Keepalive expired with nothing to report; re-issue IDLE.
            Some(IdleResponse::Timeout) => {}
            Some(IdleResponse::NewData(data)) => {
                // A task told to stop may still see a change on its way down:
                // it is not about the folder on screen.
                if is_mailbox_change(data.parsed()) && !cancel.is_cancelled() {
                    notify.store(true, Ordering::Relaxed);
                }
            }
        }
    }

    let _ = session.logout().await;
    Ok(())
}

/// Whether an unsolicited response means the mailbox contents changed.
///
/// Anything else (notably the `* OK Still here` keepalive some servers send)
/// must not trigger a refresh.
fn is_mailbox_change(response: &Response<'_>) -> bool {
    matches!(
        response,
        Response::Expunge(_)
            | Response::Vanished { .. }
            | Response::MailboxData(MailboxDatum::Exists(_))
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_idle_controller_start_stop_noop_without_config() {
        // With an empty IMAP URL the task should fail fast without panicking.
        let notify = Arc::new(AtomicBool::new(false));
        let mut ctrl = IdleController::new(Arc::clone(&notify));
        ctrl.start(EmailClientConfig::default(), "INBOX".to_owned());
        std::thread::sleep(Duration::from_millis(100));
        ctrl.stop();
        // No panic is the success criterion.
    }

    #[test]
    fn test_needs_refresh_propagates_via_flag() {
        let notify = Arc::new(AtomicBool::new(false));
        let _ctrl = IdleController::new(Arc::clone(&notify));
        // Simulate what the IDLE task does on new mail.
        notify.store(true, Ordering::Relaxed);
        assert!(notify.load(Ordering::Relaxed));
        // Simulate provider calling clear_needs_refresh.
        notify.store(false, Ordering::Relaxed);
        assert!(!notify.load(Ordering::Relaxed));
    }

    #[test]
    fn test_update_token_is_visible_to_the_session() {
        let notify = Arc::new(AtomicBool::new(false));
        let ctrl = IdleController::new(notify);
        ctrl.update_token("refreshed-token");
        assert_eq!(
            *ctrl.token.lock().expect("token mutex"),
            "refreshed-token",
            "a refreshed access token must reach the running IDLE session"
        );
    }

    /// Every uncached render of a folder asks to watch it. Only the first
    /// may start a watcher: each start is a task and an IMAP connection.
    #[test]
    fn watching_the_same_folder_again_changes_nothing() {
        let notify = Arc::new(AtomicBool::new(false));
        let mut ctrl = IdleController::new(notify);
        let config = EmailClientConfig {
            imap_url: "imaps://127.0.0.1:1".to_owned(),
            username: "me@example.com".to_owned(),
            ..Default::default()
        };
        ctrl.start(config.clone(), "INBOX".to_owned());
        let first = ctrl.cancel.clone().unwrap();
        ctrl.start(config.clone(), "INBOX".to_owned());
        // A restart would have cancelled the first task to start a second.
        assert!(!first.is_cancelled(), "the watcher keeps running");

        // Another folder, or the same one after a stop, is a new watch.
        ctrl.start(config.clone(), "Archive".to_owned());
        assert!(first.is_cancelled(), "the old watcher is told to stop");
        let second = ctrl.cancel.clone().unwrap();
        ctrl.stop();
        assert!(second.is_cancelled());
        ctrl.start(config, "Archive".to_owned());
        assert!(!ctrl.cancel.as_ref().unwrap().is_cancelled());
    }

    #[test]
    fn test_only_mailbox_changes_raise_the_flag() {
        assert!(is_mailbox_change(&Response::Expunge(3)));
        assert!(is_mailbox_change(&Response::MailboxData(
            MailboxDatum::Exists(7)
        )));
        // A keepalive must not look like new mail.
        assert!(!is_mailbox_change(&Response::Data {
            status: async_imap::imap_proto::Status::Ok,
            code: None,
            information: Some("Still here".into()),
        }));
    }
}
