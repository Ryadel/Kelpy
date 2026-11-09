//! The server Kelpy changes, beyond the conversations: the secrets of each conversation, shared by its answers and
//! the administrator's operations, and the administrator's own, with the vault ([`secrets`]); what Kelpy expects to find on the server and the differences found since
//! ([`drift`]); the operations the administrator asks for ([`operations`]); the report and export of a conversation
//! ([`report`]).

mod drift;
mod operations;
mod report;
mod secrets;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use kelpy_common::lock;
use kelpy_storage::{Change, RecordKind, Storage};
use kelpy_tools::{Card, Executor, Registry, Secrets};

use crate::events::Events;
use crate::ssh_agent::SshAgent;
use crate::vault::Vault;

pub use drift::DifferenceView;
pub use operations::Operated;
pub use secrets::{AgentKeyView, GeneratedSecret, SecretsView, SharedSecret};

/// The secrets of each conversation, shared by its answers and the administrator's operations.
pub(super) type SessionSecrets = Arc<Mutex<HashMap<i64, Secrets>>>;

pub(super) struct Server {
    store: Storage,
    executor: Executor,
    events: Events,
    secrets: SessionSecrets,
    /// Every conversation's secrets, masked in all of them, and the administrator's, which all of them use.
    registry: Registry,
    /// Where the administrator keeps secrets across restarts.
    vault: Vault,
    /// Where the versions of files before a change are kept.
    backups: PathBuf,
    /// Kelpy's memory of the server, where it writes `server.md`.
    memory: PathBuf,
    /// When the records were last checked.
    checked: Mutex<Option<Instant>>,
    /// One check at a time.
    checking: tokio::sync::Mutex<()>,
    /// One writing of `server.md` at a time.
    writing: tokio::sync::Mutex<()>,
}

impl Server {
    pub(super) fn new(
        store: Storage,
        executor: Executor,
        events: Events,
        backups: PathBuf,
        memory: PathBuf,
        vault: PathBuf,
        agent: Option<SshAgent>,
    ) -> Self {
        let registry = Registry::default();
        Self {
            store,
            executor,
            vault: Vault::open(vault, agent, registry.clone(), events.clone()),
            events,
            secrets: SessionSecrets::default(),
            registry,
            backups,
            memory,
            checked: Mutex::new(None),
            checking: tokio::sync::Mutex::new(()),
            writing: tokio::sync::Mutex::new(()),
        }
    }

    /// The secrets of a conversation.
    pub(super) fn secrets(&self, session_id: i64) -> Secrets {
        lock(&self.secrets)
            .entry(session_id)
            .or_insert_with(|| Secrets::sharing(&self.registry))
            .clone()
    }

    pub(super) fn vault(&self) -> &Vault {
        &self.vault
    }

    /// Forgets the secrets of a conversation deleted: its placeholders no longer work, but the values stay masked
    /// in the others, as they may still be on the server.
    pub(super) fn forget(&self, session_id: i64) {
        lock(&self.secrets).remove(&session_id);
    }
}

/// A card's content as an action stores it.
pub(super) fn change_of(card: Card) -> Change {
    Change {
        diff: card.diff,
        notes: card.notes,
        floor: card.floor,
        path: card.path.map(|path| path.to_string_lossy().into_owned()),
        backup: None,
    }
}

fn kind_word(kind: RecordKind) -> &'static str {
    match kind {
        RecordKind::File => "The file",
        RecordKind::Service => "The service",
        RecordKind::Package => "The package",
    }
}
