//! The email client's background work: one worker that owns the IMAP session
//! and the SMTP connection, and runs what the UI asks of it in order.
//!
//! The app waits for every call to answer, and an IMAP folder listing or an
//! SMTP send can take a while, so none of it runs there. The UI side sends a
//! [`Job`]; the worker does it with no deadline and answers with a [`Done`],
//! which `poll` applies to the same result slots the rendering code reads.
//! The worker is a thread that lives as long as the plugin, fed through a
//! channel.

use crate::net::{RealImap, RealSmtp};
use crate::{
    EmailClientConfig, EmailMessage, FolderInfo, HistoryItem, ImapBackend, MailBody, MessageHeader,
    SmtpBackend,
};
use serde::{Deserialize, Serialize};

/// A write the UI already applied locally and the server still has to see.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BgOp {
    SetFlags {
        folder: String,
        uid: u32,
        add: Vec<String>,
        remove: Vec<String>,
    },
    Move {
        folder: String,
        uid: u32,
        dest: String,
    },
    Expunge {
        folder: String,
        uid: u32,
    },
    Append {
        folder: String,
        message: Vec<u8>,
    },
    /// Find the message with `msg_id` in `search_in` and move it to `dest`
    /// (an undo or redo of a trash, archive or move).
    MoveByMessageId {
        search_in: String,
        msg_id: String,
        dest: String,
    },
}

impl BgOp {
    /// Prefix for the user-facing error when the operation fails.
    pub fn label(&self) -> &'static str {
        match self {
            BgOp::SetFlags { .. } => "flag update failed",
            BgOp::Move { .. } => "move failed",
            BgOp::Expunge { .. } => "delete failed",
            BgOp::Append { .. } => "save failed",
            BgOp::MoveByMessageId { .. } => "move failed",
        }
    }
}

/// What the UI asks of the worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Job {
    /// Use these settings from now on (a refreshed token, changed settings):
    /// the open connection is dropped and the next job reconnects.
    Configure(EmailClientConfig),
    /// The folder list, and INBOX's latest 50 alongside it.
    Root,
    Envelopes {
        folder: String,
        limit: usize,
    },
    Threads {
        folder: String,
    },
    Message {
        folder: String,
        uid: u32,
    },
    History {
        key: String,
        plan: Vec<HistoryItem>,
        folders: Vec<String>,
    },
    Op(BgOp),
    Send {
        from: String,
        to: Vec<String>,
        cc: Vec<String>,
        bcc: Vec<String>,
        subject: String,
        body: MailBody,
        attachments: Vec<(String, Vec<u8>)>,
    },
    RefreshToken {
        client_id: String,
        client_secret: String,
        refresh_token: String,
    },
}

/// What the worker answers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Done {
    Root {
        folders: Result<Vec<FolderInfo>, String>,
        inbox: Result<Vec<MessageHeader>, String>,
    },
    Envelopes {
        folder: String,
        limit: usize,
        result: Result<Vec<MessageHeader>, String>,
    },
    Threads {
        folder: String,
        threads: Option<Vec<Vec<u32>>>,
    },
    Message {
        folder: String,
        uid: u32,
        result: Result<Option<EmailMessage>, String>,
    },
    History {
        key: String,
        labels: Vec<String>,
    },
    Op {
        label: String,
        error: Option<String>,
    },
    Sent {
        result: Result<Vec<u8>, String>,
    },
    /// A refreshed access token and when it expires (Unix seconds).
    Token {
        result: Result<(String, i64), String>,
    },
}

/// The worker's side: its settings and its connection.
pub struct WorkerState {
    config: EmailClientConfig,
    imap: Option<RealImap>,
}

impl WorkerState {
    pub fn new(config: EmailClientConfig) -> Self {
        WorkerState { config, imap: None }
    }

    fn imap(&mut self) -> &mut RealImap {
        let config = &self.config;
        self.imap
            .get_or_insert_with(|| RealImap::from_config(config))
    }

    /// Do one job. `None` for one that answers nothing (`Configure`).
    pub fn run(&mut self, job: Job) -> Option<Done> {
        Some(match job {
            Job::Configure(config) => {
                self.config = config;
                self.imap = None;
                return None;
            }
            Job::Root => Done::Root {
                folders: self.imap().list_folders(),
                inbox: self.imap().list_messages("INBOX", 50),
            },
            Job::Envelopes { folder, limit } => {
                let result = self.imap().list_messages(&folder, limit);
                Done::Envelopes {
                    folder,
                    limit,
                    result,
                }
            }
            Job::Threads { folder } => {
                // A THREAD failure is non-fatal and already covered by the
                // References path, so it is recorded as "no map", not an error.
                let threads = self.imap().fetch_threads(&folder).ok().flatten();
                Done::Threads { folder, threads }
            }
            Job::Message { folder, uid } => {
                let result = self.imap().fetch_message(&folder, uid);
                Done::Message {
                    folder,
                    uid,
                    result,
                }
            }
            Job::History { key, plan, folders } => {
                let mut labels = Vec::new();
                for item in plan {
                    match item {
                        HistoryItem::Label(l) => labels.push(l),
                        HistoryItem::FetchUid(uid) => {
                            let folder = folders.first().cloned().unwrap_or_default();
                            if let Ok(Some(msg)) = self.imap().fetch_message(&folder, uid) {
                                labels
                                    .push(format!("From: {} — Subject: {}", msg.from, msg.subject));
                            }
                        }
                        HistoryItem::FetchMessageId(msg_id) => {
                            for folder in &folders {
                                if let Ok(Some(msg)) =
                                    self.imap().fetch_message_by_message_id(folder, &msg_id)
                                {
                                    labels.push(format!(
                                        "From: {} — Subject: {}",
                                        msg.from, msg.subject
                                    ));
                                    break;
                                }
                            }
                        }
                    }
                }
                Done::History { key, labels }
            }
            Job::Op(op) => {
                let label = op.label().to_owned();
                let imap = self.imap();
                let result = match &op {
                    BgOp::SetFlags {
                        folder,
                        uid,
                        add,
                        remove,
                    } => {
                        let add: Vec<&str> = add.iter().map(String::as_str).collect();
                        let remove: Vec<&str> = remove.iter().map(String::as_str).collect();
                        imap.set_flags(folder, *uid, &add, &remove)
                    }
                    BgOp::Move { folder, uid, dest } => imap.move_message(folder, *uid, dest),
                    BgOp::Expunge { folder, uid } => imap.expunge_uid(folder, *uid),
                    BgOp::Append { folder, message } => imap.append(folder, message),
                    BgOp::MoveByMessageId {
                        search_in,
                        msg_id,
                        dest,
                    } => match imap.fetch_message_by_message_id(search_in, msg_id) {
                        Ok(Some(msg)) => imap.move_message(search_in, msg.uid, dest),
                        Ok(None) => Err(format!("the message is no longer in {search_in}")),
                        Err(e) => Err(e),
                    },
                };
                Done::Op {
                    label,
                    error: result.err(),
                }
            }
            Job::Send {
                from,
                to,
                cc,
                bcc,
                subject,
                body,
                attachments,
            } => {
                fn refs(v: &[String]) -> Vec<&str> {
                    v.iter().map(String::as_str).collect()
                }
                let attachment_refs: Vec<(&str, &[u8])> = attachments
                    .iter()
                    .map(|(n, b)| (n.as_str(), b.as_slice()))
                    .collect();
                let result = RealSmtp::from_config(&self.config).send(
                    &from,
                    &refs(&to),
                    &refs(&cc),
                    &refs(&bcc),
                    &subject,
                    &body,
                    &attachment_refs,
                );
                Done::Sent { result }
            }
            Job::RefreshToken {
                client_id,
                client_secret,
                refresh_token,
            } => {
                let r = crate::oauth2::refresh_token(&client_id, &client_secret, &refresh_token);
                let result = if r.success {
                    let expiry = crate::oauth2::now_secs() + r.expires_in;
                    Ok((r.access_token, expiry))
                } else {
                    Err("OAuth token refresh failed".to_owned())
                };
                Done::Token { result }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// The UI's handle on the worker
// ---------------------------------------------------------------------------

/// The UI side of the worker.
pub struct Worker {
    jobs: std::sync::mpsc::Sender<Vec<u8>>,
    done: std::sync::mpsc::Receiver<Vec<u8>>,
}

impl Worker {
    /// Start the worker with `config`.
    pub fn start(config: &EmailClientConfig) -> Result<Worker, String> {
        let (jobs, job_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let (done_tx, done) = std::sync::mpsc::channel::<Vec<u8>>();
        let mut state = WorkerState::new(config.clone());
        std::thread::Builder::new()
            .name("email-worker".to_owned())
            .spawn(move || {
                // Ends when the UI side (the sender) is dropped.
                for bytes in job_rx {
                    if let Some(answer) = serve(&mut state, &bytes)
                        && done_tx.send(answer).is_err()
                    {
                        return;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Worker { jobs, done })
    }

    /// Hand the worker a job.
    pub fn send(&self, job: &Job) -> Result<(), String> {
        let bytes = serde_json::to_vec(job).map_err(|e| e.to_string())?;
        self.jobs
            .send(bytes)
            .map_err(|_| "the email worker has stopped".to_owned())
    }

    /// What the worker has finished since the last call. `Err` once its
    /// thread has ended (a panic in a job), after the answers it gave before
    /// that: whatever it still had to do is not coming, so the UI starts
    /// another for the next job.
    pub fn drain(&self) -> Result<Vec<Done>, (Vec<Done>, String)> {
        use std::sync::mpsc::TryRecvError;
        let mut done = Vec::new();
        loop {
            match self.done.try_recv() {
                Ok(bytes) => done.extend(serde_json::from_slice(&bytes).ok()),
                Err(TryRecvError::Empty) => return Ok(done),
                Err(TryRecvError::Disconnected) => {
                    return Err((done, "the email worker stopped".to_owned()));
                }
            }
        }
    }
}

/// Run one serialized job, answering a serialized result.
fn serve(state: &mut WorkerState, bytes: &[u8]) -> Option<Vec<u8>> {
    let job: Job = serde_json::from_slice(bytes).ok()?;
    let done = state.run(job)?;
    serde_json::to_vec(&done).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_and_its_answer_survive_the_trip() {
        let job = Job::Send {
            from: "a@b.c".into(),
            to: vec!["d@e.f".into()],
            cc: vec![],
            bcc: vec![],
            subject: "hi".into(),
            body: MailBody::Text("two\nlines".into()),
            attachments: vec![("x.bin".into(), vec![0, 1, 255])],
        };
        let back: Job = serde_json::from_slice(&serde_json::to_vec(&job).unwrap()).unwrap();
        assert!(matches!(back, Job::Send { attachments, .. } if attachments[0].1 == [0, 1, 255]));
        let done = Done::Envelopes {
            folder: "INBOX".into(),
            limit: 50,
            result: Err("no".into()),
        };
        let back: Done = serde_json::from_slice(&serde_json::to_vec(&done).unwrap()).unwrap();
        assert!(matches!(
            back,
            Done::Envelopes {
                limit: 50,
                result: Err(_),
                ..
            }
        ));
    }

    #[test]
    fn configure_answers_nothing_and_a_failed_job_answers_its_error() {
        let mut state = WorkerState::new(EmailClientConfig::default());
        assert!(
            state
                .run(Job::Configure(EmailClientConfig::default()))
                .is_none()
        );
        // No IMAP URL: the listing fails, and says so rather than hanging.
        match state.run(Job::Envelopes {
            folder: "INBOX".into(),
            limit: 5,
        }) {
            Some(Done::Envelopes { result: Err(e), .. }) => assert!(e.contains("IMAP URL"), "{e}"),
            other => panic!("{other:?}"),
        }
    }

    /// Moved here from the provider's `test_reset_bg_imap_forces_a_reconnect`:
    /// the connection belongs to the worker now, and `reset_bg_imap` sends it
    /// `Configure`.
    #[test]
    fn configure_forces_a_reconnect() {
        let mut state = WorkerState::new(EmailClientConfig {
            imap_url: "imaps://imap.example.com".to_owned(),
            username: "user@example.com".to_owned(),
            ..Default::default()
        });
        let _ = state.imap();
        assert!(state.imap.is_some());

        // The open connection authenticated with the previous credentials.
        state.run(Job::Configure(state.config.clone()));
        assert!(state.imap.is_none());
    }

    #[test]
    fn the_worker_answers_through_its_channel() {
        let w = Worker::start(&EmailClientConfig::default()).unwrap();
        w.send(&Job::Threads {
            folder: "INBOX".into(),
        })
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(Done::Threads { folder, threads }) = w.drain().unwrap().into_iter().next() {
                assert_eq!(folder, "INBOX");
                assert!(threads.is_none());
                break;
            }
            assert!(std::time::Instant::now() < deadline, "no answer");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// A worker whose thread has gone says so once its answers are read, so
    /// nothing waits on the jobs it still had.
    #[test]
    fn a_worker_whose_thread_ended_says_so_after_its_answers() {
        let (jobs, _job_rx) = std::sync::mpsc::channel();
        let (done_tx, done) = std::sync::mpsc::channel();
        let w = Worker { jobs, done };
        let answer = Done::Op {
            label: "move failed".into(),
            error: None,
        };
        done_tx.send(serde_json::to_vec(&answer).unwrap()).unwrap();
        assert_eq!(w.drain().unwrap().len(), 1, "alive, one answer");
        done_tx.send(serde_json::to_vec(&answer).unwrap()).unwrap();
        drop(done_tx);
        let (answers, error) = w.drain().unwrap_err();
        assert_eq!(answers.len(), 1, "the answer before the end still counts");
        assert!(error.contains("stopped"), "{error}");
    }
}
