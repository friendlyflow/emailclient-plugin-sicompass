//! IMAP IDLE in the background.
//!
//! Watches one folder over its own IMAP connection. When the server reports
//! EXISTS, EXPUNGE or VANISHED, the provider's `notify` flag is raised so it
//! refreshes on the next render.
//!
//! Blocking: IDLE waits in rounds of [`IDLE_ROUND`], and between rounds it
//! looks whether it has been stopped, so a stop takes effect within a round
//! without anything waiting on it. Natively the watcher is a thread. In the
//! sandbox it is one long-lived task ([`IDLE_TASK`]) for the plugin's life:
//! a task blocked in a socket read only notices a cancel when the read
//! returns, so tasks started per folder would pile up against the host's cap
//! on concurrent tasks. What to watch, a stop and a refreshed OAuth token
//! reach it through its inbox ([`Command`]), and it emits [`CHANGED`].

use crate::EmailClientConfig;
use crate::connection::connect_imap;
use imap::extensions::idle::WaitOutcome;
use imap::types::UnsolicitedResponse;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const RECONNECT_DELAY: Duration = Duration::from_secs(10);

/// One round of waiting in IDLE. Short, so a stop or a folder switch is
/// noticed soon: RFC 2177 only asks that IDLE be re-issued within 29 minutes.
const IDLE_ROUND: Duration = Duration::from_secs(10);

/// The task that watches a folder in the sandbox.
pub const IDLE_TASK: &str = "idle";

/// What the IDLE task emits when the mailbox changed.
pub const CHANGED: &[u8] = b"changed";

/// What the UI tells the IDLE task.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub enum Command {
    /// Watch `folder` on the server `config` names, instead of whatever it
    /// watched before.
    Watch {
        config: EmailClientConfig,
        folder: String,
    },
    /// Watch nothing, and wait for the next `Watch`.
    Stop,
    /// A refreshed OAuth access token, for the next connection.
    Token(String),
}

// ---------------------------------------------------------------------------
// IdleController
// ---------------------------------------------------------------------------

pub struct IdleController {
    /// Raised when new mail arrives.
    notify: Arc<AtomicBool>,
    /// What is being watched: the folder, and the server and account, so
    /// re-rendering the same folder does not restart the watch.
    watching: Option<(String, String, String)>,
    /// Stops the running watcher; `None` when nothing is running.
    #[cfg(not(target_arch = "wasm32"))]
    stop: Option<Arc<AtomicBool>>,
    /// The task watching, in the sandbox.
    #[cfg(target_arch = "wasm32")]
    task: Option<u64>,
    /// The OAuth access token the IDLE session should authenticate with.
    ///
    /// Shared rather than copied into the watcher: the token refresh in
    /// `lib.rs` replaces the access token roughly hourly, and an IDLE session
    /// that captured it at start-up would keep reconnecting with a dead
    /// credential until the user re-entered the folder.
    token: Arc<Mutex<String>>,
}

impl IdleController {
    pub fn new(notify: Arc<AtomicBool>) -> Self {
        IdleController {
            notify,
            watching: None,
            #[cfg(not(target_arch = "wasm32"))]
            stop: None,
            #[cfg(target_arch = "wasm32")]
            task: None,
            token: Arc::new(Mutex::new(String::new())),
        }
    }

    /// Watch `folder`. Watching it already, on the same server and account,
    /// changes nothing.
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

        #[cfg(not(target_arch = "wasm32"))]
        {
            let stop = Arc::new(AtomicBool::new(false));
            self.stop = Some(Arc::clone(&stop));
            let notify = Arc::clone(&self.notify);
            let token = Arc::clone(&self.token);
            let _ = std::thread::Builder::new()
                .name("email-idle".to_owned())
                .spawn(move || {
                    idle_loop(
                        &config,
                        &folder,
                        &|| stop.load(Ordering::Acquire),
                        &|| notify.store(true, Ordering::Relaxed),
                        &|| Some(token.lock().expect("token mutex").clone()),
                    )
                });
        }

        #[cfg(target_arch = "wasm32")]
        self.tell(&Command::Watch { config, folder });
    }

    /// Publish a freshly refreshed OAuth access token to the running session.
    ///
    /// Takes effect on the watcher's next reconnect; the current IDLE continues
    /// on the old token until the server drops it, which is the same behaviour
    /// as any other long-lived IMAP connection.
    pub fn update_token(&mut self, access_token: &str) {
        *self.token.lock().expect("token mutex") = access_token.to_owned();
        #[cfg(target_arch = "wasm32")]
        if self.task.is_some() {
            self.tell(&Command::Token(access_token.to_owned()));
        }
    }

    /// Stop watching. Returns at once: the watcher notices within a round.
    pub fn stop(&mut self) {
        if self.watching.take().is_none() {
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(stop) = self.stop.take() {
            stop.store(true, Ordering::Release);
        }
        #[cfg(target_arch = "wasm32")]
        if self.task.is_some() {
            self.tell(&Command::Stop);
        }
    }

    /// Hand the IDLE task `command`, starting the task with it if there is
    /// none (or the one there was has gone).
    #[cfg(target_arch = "wasm32")]
    fn tell(&mut self, command: &Command) {
        let Ok(bytes) = serde_json::to_vec(command) else {
            return;
        };
        if let Some(id) = self.task {
            if sicompass_pdk::tasks::send(id, &bytes).is_ok() {
                return;
            }
            sicompass_pdk::tasks::cancel(id);
            self.task = None;
        }
        // Only a watch starts a task: there is nothing to stop, and a token
        // arrives with the next watch anyway.
        if matches!(command, Command::Watch { .. }) {
            match sicompass_pdk::tasks::spawn(IDLE_TASK, &bytes) {
                Ok(id) => self.task = Some(id),
                Err(e) => {
                    self.watching = None;
                    sicompass_pdk::host::log(&format!("emailclient: no IDLE task: {e}"));
                }
            }
        }
    }

    /// An event from the IDLE task. `true` when it was this watcher's.
    #[cfg(target_arch = "wasm32")]
    pub fn on_task_event(&mut self, id: u64, event: &sicompass_pdk::TaskEvent) -> bool {
        if self.task != Some(id) {
            return false;
        }
        match event {
            sicompass_pdk::TaskEvent::Progress(b) if b.as_slice() == CHANGED => {
                self.notify.store(true, Ordering::Relaxed);
            }
            sicompass_pdk::TaskEvent::Done(_) => {
                // Gone: the next folder render starts another.
                self.task = None;
                self.watching = None;
            }
            _ => {}
        }
        true
    }
}

impl Drop for IdleController {
    fn drop(&mut self) {
        self.stop();
        #[cfg(target_arch = "wasm32")]
        if let Some(id) = self.task.take() {
            sicompass_pdk::tasks::cancel(id);
        }
    }
}

// ---------------------------------------------------------------------------
// The IDLE loop
// ---------------------------------------------------------------------------

/// Watch `folder` until `stopped` says so: reconnect after a failure, raise
/// `changed` on every mailbox change. `token` is asked for the current access
/// token before each connection.
pub fn idle_loop(
    config: &EmailClientConfig,
    folder: &str,
    stopped: &dyn Fn() -> bool,
    changed: &dyn Fn(),
    token: &dyn Fn() -> Option<String>,
) {
    while !stopped() {
        if let Err(e) = run_idle_session(config, folder, stopped, changed, token) {
            eprintln!("emailclient_idle: session error: {e}");
        }
        // Back off before reconnecting, looking at `stopped` every second.
        for _ in 0..RECONNECT_DELAY.as_secs() {
            if stopped() {
                return;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

/// Connect, authenticate, select the folder, then IDLE in rounds.
fn run_idle_session(
    config: &EmailClientConfig,
    folder: &str,
    stopped: &dyn Fn() -> bool,
    changed: &dyn Fn(),
    token: &dyn Fn() -> Option<String>,
) -> Result<(), String> {
    // Re-read the token on every reconnect so a refresh that happened while
    // we were idling is picked up here.
    let mut config = config.clone();
    if let Some(current) = token().filter(|t| !t.is_empty()) {
        config.oauth_access_token = current;
    }

    let mut session = connect_imap(&config)?;
    session.select(folder).map_err(|e| e.to_string())?;

    while !stopped() {
        let outcome = {
            let mut handle = session.idle();
            handle.timeout(IDLE_ROUND).keepalive(false);
            // Keep waiting through anything that is not a mailbox change.
            handle
                .wait_while(|r| !is_mailbox_change(&r))
                .map_err(|e| e.to_string())?
            // Dropping the handle sends DONE.
        };
        if outcome == WaitOutcome::MailboxChanged {
            changed();
        }
    }

    let _ = session.logout();
    Ok(())
}

/// Whether an unsolicited response means the mailbox contents changed.
///
/// Anything else (notably the `* OK Still here` keepalive some servers send)
/// must not trigger a refresh.
fn is_mailbox_change(response: &UnsolicitedResponse) -> bool {
    matches!(
        response,
        UnsolicitedResponse::Expunge(_)
            | UnsolicitedResponse::Vanished { .. }
            | UnsolicitedResponse::Exists(_)
    )
}

/// The IDLE task itself, in the sandbox: its input is the first
/// [`Command::Watch`], its inbox brings the ones after it, and it emits
/// [`CHANGED`]. It lives as long as the plugin.
#[cfg(target_arch = "wasm32")]
pub fn run_idle_task(input: &[u8]) -> Result<Vec<u8>, String> {
    use sicompass_pdk::tasks;
    use std::cell::RefCell;

    let first: Command = serde_json::from_slice(input).map_err(|e| e.to_string())?;
    let target: RefCell<Option<(EmailClientConfig, String)>> = RefCell::new(None);
    let token = RefCell::new(String::new());
    // Set when a watch or a stop arrived, so the session in progress ends.
    let retarget = RefCell::new(false);
    let apply = |command: Command| match command {
        Command::Watch { config, folder } => {
            *token.borrow_mut() = config.oauth_access_token.clone();
            *target.borrow_mut() = Some((config, folder));
            *retarget.borrow_mut() = true;
        }
        Command::Stop => {
            *target.borrow_mut() = None;
            *retarget.borrow_mut() = true;
        }
        Command::Token(t) => *token.borrow_mut() = t,
    };
    let drain = || {
        while let Some(bytes) = tasks::receive(0) {
            if let Ok(command) = serde_json::from_slice(&bytes) {
                apply(command);
            }
        }
    };
    apply(first);

    while !tasks::cancelled() {
        *retarget.borrow_mut() = false;
        let Some((config, folder)) = target.borrow().clone() else {
            // Nothing to watch: wait for a command.
            if let Some(bytes) = tasks::receive(1000)
                && let Ok(command) = serde_json::from_slice(&bytes)
            {
                apply(command);
            }
            continue;
        };
        idle_loop(
            &config,
            &folder,
            &|| {
                drain();
                tasks::cancelled() || *retarget.borrow()
            },
            &|| tasks::emit(CHANGED),
            &|| {
                drain();
                Some(token.borrow().clone())
            },
        );
    }
    Ok(Vec::new())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_idle_controller_start_stop_noop_without_config() {
        // With an empty IMAP URL the watcher should fail fast without panicking.
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
        // Simulate what the watcher does on new mail.
        notify.store(true, Ordering::Relaxed);
        assert!(notify.load(Ordering::Relaxed));
        // Simulate provider calling clear_needs_refresh.
        notify.store(false, Ordering::Relaxed);
        assert!(!notify.load(Ordering::Relaxed));
    }

    #[test]
    fn test_update_token_is_visible_to_the_session() {
        let notify = Arc::new(AtomicBool::new(false));
        let mut ctrl = IdleController::new(notify);
        ctrl.update_token("refreshed-token");
        assert_eq!(
            *ctrl.token.lock().expect("token mutex"),
            "refreshed-token",
            "a refreshed access token must reach the running IDLE session"
        );
    }

    /// Every uncached render of a folder asks to watch it. Only the first
    /// may start a watcher: in the sandbox each start is a task, and a task
    /// blocked in IDLE holds its slot until its round ends.
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
        let first = Arc::clone(ctrl.stop.as_ref().unwrap());
        ctrl.start(config.clone(), "INBOX".to_owned());
        assert!(Arc::ptr_eq(&first, ctrl.stop.as_ref().unwrap()));
        assert!(!first.load(Ordering::Acquire), "the watcher keeps running");

        // Another folder, or the same one after a stop, is a new watch.
        ctrl.start(config.clone(), "Archive".to_owned());
        assert!(
            first.load(Ordering::Acquire),
            "the old watcher is told to stop"
        );
        let second = Arc::clone(ctrl.stop.as_ref().unwrap());
        ctrl.stop();
        ctrl.start(config, "Archive".to_owned());
        assert!(!Arc::ptr_eq(&second, ctrl.stop.as_ref().unwrap()));
    }

    #[test]
    fn a_command_survives_the_trip_to_the_task() {
        let command = Command::Watch {
            config: EmailClientConfig {
                imap_url: "imaps://imap.example.com".to_owned(),
                oauth_access_token: "ya29".to_owned(),
                ..Default::default()
            },
            folder: "INBOX".to_owned(),
        };
        let back: Command = serde_json::from_slice(&serde_json::to_vec(&command).unwrap()).unwrap();
        match back {
            Command::Watch { config, folder } => {
                assert_eq!(folder, "INBOX");
                assert_eq!(config.imap_url, "imaps://imap.example.com");
                assert_eq!(config.oauth_access_token, "ya29");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn test_only_mailbox_changes_raise_the_flag() {
        assert!(is_mailbox_change(&UnsolicitedResponse::Expunge(3)));
        assert!(is_mailbox_change(&UnsolicitedResponse::Exists(7)));
        // A keepalive must not look like new mail.
        assert!(!is_mailbox_change(&UnsolicitedResponse::Ok {
            code: None,
            information: Some("Still here".into()),
        }));
    }
}
