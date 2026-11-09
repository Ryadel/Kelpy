//! Incoming remote connections, such as SSH sessions to Kelpy's own server. Each one, once authenticated, waits for
//! the administrator's approval in an interface already signed in (the Web UI, the terminal UI), unless its key was
//! allowed when Kelpy started; nobody deciding within [`APPROVAL_WAIT`] counts as a refusal. Every connection, its
//! decision and its end are written to `logs/remote.log`: Kelpy is never a backdoor.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kelpy_common::{lock, unix_now, utc};
use serde::Serialize;
use tokio::sync::oneshot;

use crate::events::{Event, Events};

/// How long a connection waits for a decision.
pub const APPROVAL_WAIT: Duration = Duration::from_secs(120);
/// The most characters of a user name, and of a key with its comment, shown: the client chooses both.
const MAX_USER: usize = 64;
const MAX_KEY: usize = 200;

/// A connection, as the interfaces show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConnectionView {
    pub id: u64,
    /// Where it comes from, such as `203.0.113.5:51234`.
    pub address: String,
    /// The user name it gave.
    pub user: String,
    /// Its key, as `ssh-ed25519 SHA256:… (comment)`.
    pub key: String,
    pub fingerprint: String,
    /// Unix time, in seconds.
    pub since: i64,
    /// Who allowed it, once allowed.
    pub allowed_by: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConnectionsView {
    pub waiting: Vec<ConnectionView>,
    pub open: Vec<ConnectionView>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectionError {
    #[error("this connection is not waiting for a decision")]
    NotWaiting,
}

pub struct Connections {
    state: Mutex<State>,
    events: Events,
    log: PathBuf,
}

#[derive(Default)]
struct State {
    next: u64,
    /// The fingerprints of the keys allowed without asking.
    allowed: Vec<String>,
    /// Each with where its decision goes: whether it is allowed, and by whom.
    waiting: BTreeMap<u64, (ConnectionView, oneshot::Sender<(bool, String)>)>,
    open: BTreeMap<u64, ConnectionView>,
}

/// What asks to connect.
pub struct Request {
    pub address: String,
    pub user: String,
    pub key: String,
    pub fingerprint: String,
}

impl Connections {
    /// Connections logged to `log`.
    pub fn new(log: PathBuf, events: Events) -> Self {
        Self {
            state: Mutex::default(),
            events,
            log,
        }
    }

    /// Lets the keys with these fingerprints connect without asking, such as for unattended use.
    pub fn allow_keys(&self, fingerprints: Vec<String>) {
        lock(&self.state).allowed = fingerprints;
    }

    pub fn view(&self) -> ConnectionsView {
        let state = lock(&self.state);
        ConnectionsView {
            waiting: state
                .waiting
                .values()
                .map(|(view, _)| view.clone())
                .collect(),
            open: state.open.values().cloned().collect(),
        }
    }

    /// Asks for `request` to be allowed, within [`APPROVAL_WAIT`]. Allowed, the connection is open until the
    /// returned guard is dropped.
    pub async fn ask(self: &Arc<Self>, request: Request) -> Option<Open> {
        let (answer, answered) = oneshot::channel();
        let view = {
            let mut state = lock(&self.state);
            state.next += 1;
            let view = ConnectionView {
                id: state.next,
                address: request.address,
                user: shown(&request.user, MAX_USER),
                key: shown(&request.key, MAX_KEY),
                fingerprint: request.fingerprint,
                since: unix_now(),
                allowed_by: None,
            };
            if state.allowed.contains(&view.fingerprint) {
                drop(state);
                return Some(self.open(view, "--allow-key".into()));
            }
            state.waiting.insert(view.id, (view.clone(), answer));
            view
        };
        self.log(&view, "asks to connect");
        self.events.publish(Event::ConnectionsChanged);
        let decision = tokio::time::timeout(APPROVAL_WAIT, answered).await;
        // Taken out under the lock: a decision arriving now finds it gone.
        lock(&self.state).waiting.remove(&view.id);
        match decision {
            Ok(Ok((true, by))) => Some(self.open(view, by)),
            Ok(Ok((false, by))) => {
                self.log(&view, &format!("refused {by}"));
                self.events.publish(Event::ConnectionsChanged);
                None
            }
            _ => {
                self.log(&view, "refused: nobody decided in time");
                self.events.publish(Event::ConnectionsChanged);
                None
            }
        }
    }

    /// Allows or refuses the connection `id`, from the interface `by`, such as `in the Web UI`.
    pub fn decide(&self, id: u64, allow: bool, by: &str) -> Result<(), ConnectionError> {
        let (_, answer) = lock(&self.state)
            .waiting
            .remove(&id)
            .ok_or(ConnectionError::NotWaiting)?;
        // The connection may have ended meanwhile: then nothing waits for the answer.
        let _ = answer.send((allow, by.to_owned()));
        Ok(())
    }

    fn open(self: &Arc<Self>, mut view: ConnectionView, by: String) -> Open {
        view.allowed_by = Some(by.clone());
        self.log(&view, &format!("allowed {by}"));
        lock(&self.state).open.insert(view.id, view.clone());
        self.events.publish(Event::ConnectionsChanged);
        Open {
            connections: Arc::clone(self),
            id: view.id,
        }
    }

    /// One line in `logs/remote.log`; best effort, as a full disk must not let a connection in unlogged nor keep
    /// one out: the decision shows in the interfaces either way.
    fn log(&self, view: &ConnectionView, what: &str) {
        let line = format!(
            "{} connection {} from {} as {} with {}: {what}\n",
            utc(unix_now()),
            view.id,
            view.address,
            view.user,
            view.key
        );
        if let Some(folder) = self.log.parent() {
            let _ = std::fs::create_dir_all(folder);
        }
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&self.log) {
            let _ = file.write_all(line.as_bytes());
        }
    }
}

/// `text`, chosen by the client, as it can be shown and logged: control characters, which could act on a terminal
/// showing it or forge a line of the log, become `?`, and past `max` characters it is cut, with `…`.
fn shown(text: &str, max: usize) -> String {
    let mut shown: String = text
        .chars()
        .take(max)
        .map(|char| if char.is_control() { '?' } else { char })
        .collect();
    if text.chars().nth(max).is_some() {
        shown.push('…');
    }
    shown
}

/// An open connection, until dropped.
pub struct Open {
    connections: Arc<Connections>,
    id: u64,
}

impl Open {
    pub fn id(&self) -> u64 {
        self.id
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        let view = lock(&self.connections.state).open.remove(&self.id);
        if let Some(view) = view {
            self.connections.log(&view, "closed");
            self.connections.events.publish(Event::ConnectionsChanged);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn what_the_client_chose_is_shown_without_control_characters_and_cut() {
        let folder = tempfile::tempdir().unwrap();
        let connections = Arc::new(Connections::new(
            folder.path().join("remote.log"),
            Events::new(),
        ));
        connections.allow_keys(vec!["SHA256:known".into()]);
        let mut request = request("SHA256:known");
        request.user = format!("\u{1b}[2Jroot\n{}", "a".repeat(200));
        request.key = "ssh-ed25519 SHA256:known (\u{7})".into();
        let open = connections.ask(request).await.unwrap();
        let view = connections.view().open[0].clone();
        assert!(view.user.starts_with("?[2Jroot?aaa"), "{}", view.user);
        assert_eq!(view.user.chars().count(), MAX_USER + 1);
        assert!(view.user.ends_with('…'));
        assert_eq!(view.key, "ssh-ed25519 SHA256:known (?)");
        drop(open);
    }

    fn request(fingerprint: &str) -> Request {
        Request {
            address: "203.0.113.5:51234".into(),
            user: "root".into(),
            key: format!("ssh-ed25519 {fingerprint} (laptop)"),
            fingerprint: fingerprint.into(),
        }
    }

    #[tokio::test]
    async fn a_connection_waits_for_a_decision_and_everything_is_logged() {
        let folder = tempfile::tempdir().unwrap();
        let log = folder.path().join("logs").join("remote.log");
        let connections = Arc::new(Connections::new(log.clone(), Events::new()));
        connections.allow_keys(vec!["SHA256:known".into()]);

        let asking = tokio::spawn({
            let connections = Arc::clone(&connections);
            async move { connections.ask(request("SHA256:new")).await }
        });
        while connections.view().waiting.is_empty() {
            tokio::task::yield_now().await;
        }
        let id = connections.view().waiting[0].id;
        connections.decide(id, true, "in the Web UI").unwrap();
        let open = asking.await.unwrap().unwrap();
        assert!(connections.view().waiting.is_empty());
        assert_eq!(
            connections.view().open[0].allowed_by.as_deref(),
            Some("in the Web UI")
        );
        assert!(connections.decide(id, false, "late").is_err());
        drop(open);
        assert!(connections.view().open.is_empty());

        // A refusal, and a key allowed when Kelpy started.
        let asking = tokio::spawn({
            let connections = Arc::clone(&connections);
            async move { connections.ask(request("SHA256:other")).await }
        });
        while connections.view().waiting.is_empty() {
            tokio::task::yield_now().await;
        }
        let id = connections.view().waiting[0].id;
        connections.decide(id, false, "in the terminal UI").unwrap();
        assert!(asking.await.unwrap().is_none());
        assert!(connections.ask(request("SHA256:known")).await.is_some());

        let logged = std::fs::read_to_string(&log).unwrap();
        for expected in [
            "connection 1 from 203.0.113.5:51234 as root with ssh-ed25519 SHA256:new (laptop): asks to connect",
            "connection 1 from 203.0.113.5:51234 as root with ssh-ed25519 SHA256:new (laptop): allowed in the Web UI",
            ": closed",
            "connection 2 from 203.0.113.5:51234 as root with ssh-ed25519 SHA256:other (laptop): refused in the terminal UI",
            "SHA256:known (laptop): allowed --allow-key",
        ] {
            assert!(logged.contains(expected), "{expected}\n{logged}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn nobody_deciding_is_a_refusal() {
        let folder = tempfile::tempdir().unwrap();
        let connections = Arc::new(Connections::new(
            folder.path().join("remote.log"),
            Events::new(),
        ));
        assert!(connections.ask(request("SHA256:new")).await.is_none());
        assert!(connections.view().waiting.is_empty());
    }
}
