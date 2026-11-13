//! The server Kelpy changes, beyond the conversations: the secrets of each conversation, shared by its answers and
//! the administrator's operations, and the administrator's own, with the vault ([`secrets`]); what Kelpy expects to find on the server and the differences found since
//! ([`drift`]); the operations the administrator asks for ([`operations`]); the report and export of a conversation
//! ([`report`]).

mod drift;
mod operations;
mod report;
mod secrets;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use kelpy_common::lock;
use kelpy_storage::{Change, RecordKind, Storage};
use kelpy_tools::{Card, Executor, Registry, Secrets};
use zeroize::Zeroizing;

use crate::events::Events;
use crate::ssh_agent::SshAgent;
use crate::vault::{Vault, VaultError};

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

    /// The secrets, for the Kelpy restarted in this one's place: the values masked, the administrator's by name
    /// but for those kept in the vault, each conversation's by name, and the vault's key while it is unlocked.
    #[allow(clippy::type_complexity)]
    pub(super) fn handed_over(
        &self,
    ) -> (
        BTreeSet<String>,
        BTreeMap<String, String>,
        BTreeMap<i64, BTreeMap<String, String>>,
        Option<Zeroizing<Vec<u8>>>,
    ) {
        let generated = lock(&self.secrets)
            .iter()
            .map(|(&session_id, secrets)| (session_id, secrets.handed_over()))
            .filter(|(_, own)| !own.is_empty())
            .collect();
        // One copy under the vault's state: a lock in between cannot turn a kept secret into one not kept.
        let administrator = self.vault.handed_over();
        (
            administrator.masked,
            administrator.shared,
            generated,
            administrator.key,
        )
    }

    /// Takes over the secrets the Kelpy before this one handed over, then opens the vault with its key: only
    /// then are the secrets kept in it usable.
    pub(super) async fn take_over(
        &self,
        masked: BTreeSet<String>,
        shared: BTreeMap<String, String>,
        generated: BTreeMap<i64, BTreeMap<String, String>>,
        vault_key: Option<Zeroizing<Vec<u8>>>,
    ) -> Result<(), VaultError> {
        self.registry.take_over(masked, shared);
        {
            let mut secrets = lock(&self.secrets);
            for (session_id, own) in generated {
                secrets.insert(session_id, Secrets::taking_over(&self.registry, own));
            }
        }
        match vault_key {
            Some(key) => self.vault.take_over(&key).await,
            None => Ok(()),
        }
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
