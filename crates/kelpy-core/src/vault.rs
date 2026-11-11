//! The vault: the administrator's secrets kept across restarts, encrypted in `data/vault.json`.
//!
//! The secrets are encrypted with XChaCha20-Poly1305 under a random key of the vault. That key is stored once per
//! unlock method, encrypted under a key derived from it: a passphrase through Argon2id, or an SSH key through its
//! agent, which signs a challenge of the vault (a key whose signatures are always the same for the same data, see
//! [`ssh_agent`](crate::ssh_agent)), the signature then going through HKDF-SHA256. Any one method opens the vault;
//! a lost passphrase with no other method loses the secrets.
//!
//! The vault starts locked. Unlocked, its secrets join the administrator's in the [`Registry`], where every
//! conversation can use them by placeholder; locked again, they leave it. The secrets added without keeping them in
//! the vault are in the registry only, and last until Kelpy stops. Every change is written before it is applied:
//! a failed write leaves the vault and the registry as they were.
//!
//! Changes come one at a time. Their slow parts (deriving a key, waiting for the agent's confirmation, writing the
//! file) hold no lock on the state, and the file is written off the async runtime, so meanwhile the vault can be
//! shown and locked: locking cancels an unlock under way, and a change written meanwhile is kept in the locked vault
//! (a secret it adds or keeps is then not in the registry: it joins it at the next unlock). A change that writes runs
//! as a task of its own and ends even if its caller stops waiting, so the next change finds the file and the state
//! in agreement. Lock order: the state, then the registry.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use kelpy_tools::Registry;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use kelpy_common::lock;
use tokio_util::task::TaskTracker;

use crate::events::{Event, Events};
use crate::private_file;
use crate::ssh_agent::{self, AgentError, AgentKey, SshAgent};

/// The version of the vault's file this Kelpy writes.
const FORMAT: u32 = 1;
const KEY_LENGTH: usize = 32;
const NONCE_LENGTH: usize = 24;
const SALT_LENGTH: usize = 32;
/// Argon2id with 64 MiB, 3 passes and 1 lane: the second choice of RFC 9106 for memory-constrained systems, about
/// a second on a small server. Tests use 64 KiB, as the settings are stored with each passphrase.
const MEMORY_KIB: u32 = if cfg!(test) { 64 } else { 64 * 1024 };
const ITERATIONS: u32 = 3;
const LANES: u32 = 1;
/// The shortest passphrase accepted.
pub const MIN_PASSPHRASE: usize = 12;
/// What the SSH agent signs, followed by the unlock's salt.
const CHALLENGE: &[u8] = b"Kelpy vault unlock, version 1\n";
/// What each encryption is bound to, so that one part of the file cannot stand in for another.
const KEY_CONTEXT: &[u8] = b"kelpy vault key";
const SECRETS_CONTEXT: &[u8] = b"kelpy vault secrets";
/// The shortest secret the administrator can add: a shorter value would be masked wherever it appears.
pub const MIN_SECRET: usize = 6;
/// The longest secret the administrator can add.
pub const MAX_SECRET: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("there is no vault yet")]
    Missing,
    #[error("there is a vault already")]
    Exists,
    #[error("the vault is locked")]
    Locked,
    #[error("the vault cannot be read ({0}): delete it to create a new one")]
    Unreadable(String),
    #[error("wrong passphrase")]
    WrongPassphrase,
    #[error("no key of the SSH agent unlocks the vault")]
    NoAgentKey,
    #[error("the SSH agent holds no key {0}")]
    UnknownKey(String),
    #[error(
        "the key {0} cannot unlock a vault: its signatures change each time (use an Ed25519 or RSA key)"
    )]
    Unsuitable(String),
    #[error("the vault was not unlocked: {0}")]
    Damaged(&'static str),
    #[error(transparent)]
    Agent(#[from] AgentError),
    #[error("{0}")]
    Invalid(String),
    #[error("cannot write the vault: {0}")]
    Write(io::Error),
    #[error("the vault was locked before it opened")]
    Cancelled,
    #[error("Kelpy is stopping")]
    Stopping,
    #[error("no random numbers on this system: {0}")]
    Random(getrandom::Error),
}

/// How to open the vault.
pub enum Unlock {
    Passphrase(Zeroizing<String>),
    /// The SSH agent's key with this fingerprint; to unlock, `None` tries every key of the agent.
    SshKey(Option<String>),
}

/// The state of the vault, as the interfaces show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VaultView {
    pub state: VaultState,
    /// Why the vault cannot be read, when it cannot.
    pub problem: Option<String>,
    /// How it can be unlocked, in order.
    pub unlocks: Vec<UnlockView>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VaultState {
    None,
    Locked,
    Unlocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UnlockView {
    Passphrase,
    SshKey { label: String, fingerprint: String },
}

/// Writes a file only the current user can open, replacing it.
type WriteFile = Arc<dyn Fn(&Path, &[u8]) -> io::Result<()> + Send + Sync>;

pub struct Vault {
    inner: Arc<Inner>,
    /// The changes under way, each a task of its own: [`Vault::stopped`] waits for them.
    changes: TaskTracker,
    /// Held while a change is let in, so that none starts once [`Vault::stop`] has run.
    admitting: Mutex<()>,
}

struct Inner {
    path: PathBuf,
    /// [`private_file::write`]; tests put a slow disk in its place.
    write_file: WriteFile,
    /// The SSH agent of Kelpy's session, if it has one.
    agent: Option<SshAgent>,
    registry: Registry,
    events: Events,
    /// One change at a time, held across its slow part.
    changes: tokio::sync::Mutex<()>,
    /// Held briefly, never across an await.
    state: Mutex<Guarded>,
}

struct Guarded {
    state: State,
    /// How many times the vault was locked: an unlock under way when this changes does not open it.
    locks: u64,
}

/// What [`Vault::handed_over`] gives, as one consistent copy.
pub(crate) struct HandedOver {
    /// Every value masked.
    pub masked: BTreeSet<String>,
    /// The administrator's secrets by name, not kept in the vault.
    pub shared: BTreeMap<String, String>,
    /// The vault's key, while it is unlocked.
    pub key: Option<Zeroizing<Vec<u8>>>,
}

/// Secrets by name, their values wiped when dropped.
type Plain = BTreeMap<String, Zeroizing<String>>;

enum State {
    None,
    Unreadable(String),
    Locked(Sealed),
    Open {
        sealed: Sealed,
        key: Zeroizing<[u8; KEY_LENGTH]>,
        /// The names of the secrets kept in the vault; their values are in the registry.
        kept: BTreeSet<String>,
    },
}

/// The vault's file.
#[derive(Clone, Serialize, Deserialize)]
struct Sealed {
    format: u32,
    unlocks: Vec<Slot>,
    nonce: Bytes,
    /// The secrets by name, as JSON, encrypted under the vault's key.
    secrets: Bytes,
}

/// The vault's key, encrypted under the key one unlock method derives.
#[derive(Clone, Serialize, Deserialize)]
struct Slot {
    #[serde(flatten)]
    method: Method,
    nonce: Bytes,
    key: Bytes,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
enum Method {
    Passphrase {
        salt: Bytes,
        memory_kib: u32,
        iterations: u32,
        lanes: u32,
    },
    SshKey {
        public_key: Bytes,
        label: String,
        salt: Bytes,
    },
}

/// Bytes written in base64.
#[derive(Clone, PartialEq, Eq)]
struct Bytes(Vec<u8>);

impl Serialize for Bytes {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        STANDARD
            .decode(text)
            .map(Bytes)
            .map_err(serde::de::Error::custom)
    }
}

impl Vault {
    /// The vault in `path`, locked, sharing its secrets through `registry` once unlocked. `agent` signs for the
    /// SSH keys that unlock it.
    pub fn open(
        path: PathBuf,
        agent: Option<SshAgent>,
        registry: Registry,
        events: Events,
    ) -> Self {
        Self {
            inner: Arc::new(Inner::open(path, agent, registry, events)),
            changes: TaskTracker::new(),
            admitting: Mutex::new(()),
        }
    }

    /// Refuses changes from now on: Kelpy is stopping. The changes under way go on; [`Vault::stopped`] waits for
    /// them.
    pub fn stop(&self) {
        let _admitting = lock(&self.admitting);
        self.changes.close();
    }

    /// Stops the vault and waits until the changes under way have written what they write.
    pub async fn stopped(&self) {
        self.stop();
        self.changes.wait().await;
    }

    /// Runs a change that writes the vault's file as a task of its own, so that it ends even if its caller stops
    /// waiting: until the file and the state agree it holds the lock on changes, and nothing else can write or
    /// delete the file meanwhile. Refused once Kelpy is stopping.
    async fn change<T, F>(&self, change: impl FnOnce(Arc<Inner>) -> F) -> Result<T, VaultError>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, VaultError>> + Send + 'static,
    {
        let running = {
            let _admitting = lock(&self.admitting);
            if self.changes.is_closed() {
                return Err(VaultError::Stopping);
            }
            self.changes.spawn(change(Arc::clone(&self.inner)))
        };
        match running.await {
            Ok(changed) => changed,
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Err(error) => Err(VaultError::Write(io::Error::other(error))),
        }
    }

    pub fn view(&self) -> VaultView {
        self.inner.view()
    }

    /// The keys of the SSH agent of Kelpy's session.
    pub async fn agent_keys(&self) -> Result<Vec<AgentKey>, VaultError> {
        self.inner.agent_keys().await
    }

    /// The names of the administrator's secrets kept in the vault, while it is unlocked.
    pub fn kept(&self) -> BTreeSet<String> {
        self.inner.kept()
    }

    /// Creates the vault, empty and unlocked, opened by `unlock`.
    pub async fn create(&self, unlock: Unlock) -> Result<(), VaultError> {
        self.change(|inner| async move { inner.create(unlock).await })
            .await
    }

    /// Unlocks the vault: its secrets join the administrator's, in place of any added with the same name.
    pub async fn unlock(&self, unlock: Unlock) -> Result<(), VaultError> {
        self.inner.unlock(unlock).await
    }

    /// Locks the vault, cancelling an unlock under way: its secrets leave the administrator's, but stay masked.
    pub fn lock(&self) {
        self.inner.lock();
    }

    /// The administrator's secrets for the Kelpy restarted in this one's place, read at once under the vault's state,
    /// which a lock changes together with the registry: the values masked; the secrets by name, but for those kept
    /// in the vault, which come back only by opening it; and, while it is unlocked, the key that opens it.
    pub(crate) fn handed_over(&self) -> HandedOver {
        let guarded = self.inner.guarded();
        let (masked, mut shared) = self.inner.registry.handed_over();
        let key = match &guarded.state {
            State::Open { key, kept, .. } => {
                for name in kept {
                    shared.remove(name);
                }
                Some(Zeroizing::new(key.to_vec()))
            }
            _ => None,
        };
        HandedOver {
            masked,
            shared,
            key,
        }
    }

    /// Unlocks the vault with the key the Kelpy before this one handed over: its secrets join the administrator's.
    pub(crate) async fn take_over(&self, key: &[u8]) -> Result<(), VaultError> {
        let key: [u8; KEY_LENGTH] = key
            .try_into()
            .map_err(|_| VaultError::Damaged("the key handed over is not a vault key"))?;
        self.inner.take_over(Zeroizing::new(key)).await
    }

    /// Adds the administrator's secret `name`, kept in the vault if `keep`, or else until Kelpy stops.
    pub async fn add(
        &self,
        name: &str,
        value: Zeroizing<String>,
        keep: bool,
    ) -> Result<(), VaultError> {
        let name = name.to_owned();
        self.change(move |inner| async move { inner.add(&name, value, keep).await })
            .await
    }

    /// Keeps the administrator's secret `name` in the vault, from now on across restarts.
    pub async fn keep(&self, name: &str) -> Result<(), VaultError> {
        let name = name.to_owned();
        self.change(move |inner| async move { inner.keep(&name).await })
            .await
    }

    /// Removes the administrator's secret `name`, from the vault too. Its value stays masked: it may still be on
    /// the server.
    pub async fn remove(&self, name: &str) -> Result<(), VaultError> {
        let name = name.to_owned();
        self.change(move |inner| async move { inner.remove(&name).await })
            .await
    }

    /// Adds a way to unlock the vault, which must be unlocked.
    pub async fn add_unlock(&self, unlock: Unlock) -> Result<(), VaultError> {
        self.change(|inner| async move { inner.add_unlock(unlock).await })
            .await
    }

    /// Removes the way to unlock the vault at `index`, unless it is the last one. The vault must be unlocked.
    pub async fn remove_unlock(&self, index: usize) -> Result<(), VaultError> {
        self.change(move |inner| async move { inner.remove_unlock(index).await })
            .await
    }

    /// Deletes the vault and the secrets kept in it; those added only until Kelpy stops stay.
    pub async fn delete(&self) -> Result<(), VaultError> {
        self.change(|inner| async move { inner.delete().await })
            .await
    }

    /// Puts a slow disk in place of the real one.
    #[cfg(test)]
    fn write_with(&mut self, write_file: WriteFile) {
        Arc::get_mut(&mut self.inner)
            .expect("no change under way")
            .write_file = write_file;
    }
}

impl Inner {
    fn open(path: PathBuf, agent: Option<SshAgent>, registry: Registry, events: Events) -> Self {
        let state = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Sealed>(&bytes) {
                Ok(sealed) if sealed.format == FORMAT && !sealed.unlocks.is_empty() => {
                    State::Locked(sealed)
                }
                Ok(sealed) if sealed.format != FORMAT => State::Unreadable(format!(
                    "it is in format {}, written by another version of Kelpy",
                    sealed.format
                )),
                Ok(_) => State::Unreadable("it has no way to unlock it".into()),
                Err(error) => State::Unreadable(error.to_string()),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => State::None,
            Err(error) => State::Unreadable(error.to_string()),
        };
        Self {
            path,
            write_file: Arc::new(private_file::write),
            agent,
            registry,
            events,
            changes: tokio::sync::Mutex::new(()),
            state: Mutex::new(Guarded { state, locks: 0 }),
        }
    }

    fn guarded(&self) -> MutexGuard<'_, Guarded> {
        kelpy_common::lock(&self.state)
    }

    fn view(&self) -> VaultView {
        let guarded = self.guarded();
        let (state, problem, sealed) = match &guarded.state {
            State::None => (VaultState::None, None, None),
            State::Unreadable(problem) => (VaultState::None, Some(problem.clone()), None),
            State::Locked(sealed) => (VaultState::Locked, None, Some(sealed)),
            State::Open { sealed, .. } => (VaultState::Unlocked, None, Some(sealed)),
        };
        let unlocks = sealed.map_or_else(Vec::new, |sealed| {
            sealed
                .unlocks
                .iter()
                .map(|slot| match &slot.method {
                    Method::Passphrase { .. } => UnlockView::Passphrase,
                    Method::SshKey {
                        public_key, label, ..
                    } => UnlockView::SshKey {
                        label: label.clone(),
                        fingerprint: ssh_agent::fingerprint(&public_key.0),
                    },
                })
                .collect()
        });
        VaultView {
            state,
            problem,
            unlocks,
        }
    }

    async fn agent_keys(&self) -> Result<Vec<AgentKey>, VaultError> {
        Ok(self.agent()?.keys().await?)
    }

    fn agent(&self) -> Result<&SshAgent, VaultError> {
        self.agent
            .as_ref()
            .ok_or(VaultError::Agent(AgentError::Missing))
    }

    fn kept(&self) -> BTreeSet<String> {
        match &self.guarded().state {
            State::Open { kept, .. } => kept.clone(),
            _ => BTreeSet::new(),
        }
    }

    async fn create(&self, unlock: Unlock) -> Result<(), VaultError> {
        let _change = self.changes.lock().await;
        if !matches!(self.guarded().state, State::None) {
            return Err(VaultError::Exists);
        }
        let key = Zeroizing::new(random::<KEY_LENGTH>()?);
        let sealed = Sealed {
            format: FORMAT,
            unlocks: vec![slot(self.agent.as_ref(), &key, unlock).await?],
            nonce: Bytes(Vec::new()),
            secrets: Bytes(Vec::new()),
        };
        let sealed = self.persist(sealed, key.clone(), Plain::new()).await?;
        // Only locking can have run meanwhile, and it leaves a missing vault as it is.
        self.guarded().state = State::Open {
            sealed,
            key,
            kept: BTreeSet::new(),
        };
        self.events.publish(Event::SecretsChanged);
        Ok(())
    }

    async fn unlock(&self, unlock: Unlock) -> Result<(), VaultError> {
        let _change = self.changes.lock().await;
        let (sealed, locks) = {
            let guarded = self.guarded();
            match &guarded.state {
                State::Locked(sealed) => (sealed.clone(), guarded.locks),
                State::Open { .. } => return Ok(()),
                State::None => return Err(VaultError::Missing),
                State::Unreadable(problem) => {
                    return Err(VaultError::Unreadable(problem.clone()));
                }
            }
        };
        let key = match unlock {
            Unlock::Passphrase(passphrase) => open_with_passphrase(&sealed, &passphrase).await?,
            Unlock::SshKey(fingerprint) => {
                open_with_agent(self.agent()?, &sealed, fingerprint.as_deref()).await?
            }
        };
        let secrets = decrypt_secrets(&sealed, &key)?;
        let mut guarded = self.guarded();
        if guarded.locks != locks {
            return Err(VaultError::Cancelled);
        }
        let kept = secrets.keys().cloned().collect();
        for (name, mut value) in secrets {
            self.registry.set(&name, std::mem::take(&mut *value));
        }
        guarded.state = State::Open { sealed, key, kept };
        drop(guarded);
        self.events.publish(Event::SecretsChanged);
        Ok(())
    }

    async fn take_over(&self, key: Zeroizing<[u8; KEY_LENGTH]>) -> Result<(), VaultError> {
        let _change = self.changes.lock().await;
        let mut guarded = self.guarded();
        let sealed = match &guarded.state {
            State::Locked(sealed) => sealed.clone(),
            State::Open { .. } => return Ok(()),
            State::None => return Err(VaultError::Missing),
            State::Unreadable(problem) => return Err(VaultError::Unreadable(problem.clone())),
        };
        let secrets = decrypt_secrets(&sealed, &key)?;
        let kept = secrets.keys().cloned().collect();
        for (name, mut value) in secrets {
            self.registry.set(&name, std::mem::take(&mut *value));
        }
        guarded.state = State::Open { sealed, key, kept };
        drop(guarded);
        self.events.publish(Event::SecretsChanged);
        Ok(())
    }

    fn lock(&self) {
        let mut guarded = self.guarded();
        guarded.locks += 1;
        let state = std::mem::replace(&mut guarded.state, State::None);
        let State::Open { sealed, kept, .. } = state else {
            guarded.state = state;
            return;
        };
        for name in &kept {
            self.registry.remove(name);
        }
        guarded.state = State::Locked(sealed);
        drop(guarded);
        self.events.publish(Event::SecretsChanged);
    }

    async fn add(
        &self,
        name: &str,
        value: Zeroizing<String>,
        keep: bool,
    ) -> Result<(), VaultError> {
        let name = kelpy_tools::secret::name(name)
            .map_err(|error| VaultError::Invalid(error.to_string()))?;
        if !(MIN_SECRET..=MAX_SECRET).contains(&value.chars().count()) {
            return Err(VaultError::Invalid(format!(
                "a secret must have from {MIN_SECRET} to {MAX_SECRET} characters: a shorter one would be masked \
                 wherever it appears"
            )));
        }
        let _change = self.changes.lock().await;
        if self.registry.value(&name).is_some() {
            return Err(VaultError::Invalid(format!(
                "there is already a secret named `{name}`"
            )));
        }
        let mut value = value;
        if keep {
            let mut open = self.snapshot()?;
            open.secrets.insert(name.clone(), value.clone());
            let written = self.persist(open.sealed, open.key, open.secrets).await?;
            // Kept, it serves only if the vault is still open once written; the state stays locked until the
            // registry agrees with it, so a lock cannot come in between.
            let (guarded, open) = self.settle(written, |kept| {
                kept.insert(name.clone());
            });
            if open {
                self.registry.set(&name, std::mem::take(&mut *value));
            }
            drop(guarded);
        } else {
            self.registry.set(&name, std::mem::take(&mut *value));
        }
        self.events.publish(Event::SecretsChanged);
        Ok(())
    }

    async fn keep(&self, name: &str) -> Result<(), VaultError> {
        let _change = self.changes.lock().await;
        let value = self
            .registry
            .value(name)
            .map(Zeroizing::new)
            .ok_or_else(|| VaultError::Invalid(format!("there is no secret named `{name}`")))?;
        let mut open = self.snapshot()?;
        if open.kept.contains(name) {
            return Ok(());
        }
        open.secrets.insert(name.to_owned(), value);
        let written = self.persist(open.sealed, open.key, open.secrets).await?;
        let (guarded, open) = self.settle(written, |kept| {
            kept.insert(name.to_owned());
        });
        // Locked meanwhile: the secret is now one of the locked vault's, and leaves the registry as they did.
        if !open {
            self.registry.remove(name);
        }
        drop(guarded);
        self.events.publish(Event::SecretsChanged);
        Ok(())
    }

    async fn remove(&self, name: &str) -> Result<(), VaultError> {
        let _change = self.changes.lock().await;
        let in_vault =
            matches!(&self.guarded().state, State::Open { kept, .. } if kept.contains(name));
        if in_vault {
            let mut open = self.snapshot()?;
            open.secrets.remove(name);
            let written = self.persist(open.sealed, open.key, open.secrets).await?;
            drop(self.settle(written, |kept| {
                kept.remove(name);
            }));
        }
        // Locked meanwhile, a secret kept in the vault left the registry already.
        let removed = self.registry.remove(name) || in_vault;
        if !removed {
            return Err(VaultError::Invalid(format!(
                "there is no secret named `{name}`"
            )));
        }
        self.events.publish(Event::SecretsChanged);
        Ok(())
    }

    async fn add_unlock(&self, unlock: Unlock) -> Result<(), VaultError> {
        let _change = self.changes.lock().await;
        let mut open = self.snapshot()?;
        let slot = slot(self.agent.as_ref(), &open.key, unlock).await?;
        if let Method::SshKey { public_key, .. } = &slot.method
            && open.sealed.unlocks.iter().any(|existing| {
                matches!(&existing.method, Method::SshKey { public_key: other, .. } if other == public_key)
            })
        {
            return Err(VaultError::Invalid("that SSH key unlocks the vault already".into()));
        }
        open.sealed.unlocks.push(slot);
        let written = self.persist(open.sealed, open.key, open.secrets).await?;
        drop(self.settle(written, |_| {}));
        self.events.publish(Event::SecretsChanged);
        Ok(())
    }

    async fn remove_unlock(&self, index: usize) -> Result<(), VaultError> {
        let _change = self.changes.lock().await;
        let mut open = self.snapshot()?;
        if index >= open.sealed.unlocks.len() {
            return Err(VaultError::Invalid(
                "there is no such way to unlock the vault".into(),
            ));
        }
        if open.sealed.unlocks.len() == 1 {
            return Err(VaultError::Invalid(
                "the last way to unlock the vault cannot be removed: delete the vault instead"
                    .into(),
            ));
        }
        open.sealed.unlocks.remove(index);
        let written = self.persist(open.sealed, open.key, open.secrets).await?;
        drop(self.settle(written, |_| {}));
        self.events.publish(Event::SecretsChanged);
        Ok(())
    }

    async fn delete(&self) -> Result<(), VaultError> {
        let _change = self.changes.lock().await;
        if matches!(self.guarded().state, State::None) {
            return Err(VaultError::Missing);
        }
        let path = self.path.clone();
        let removed = tokio::task::spawn_blocking(move || match std::fs::remove_file(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        })
        .await
        .map_err(io::Error::other)
        .and_then(|removed| removed);
        removed.map_err(VaultError::Write)?;
        let mut guarded = self.guarded();
        if let State::Open { kept, .. } = &guarded.state {
            for name in kept {
                self.registry.remove(name);
            }
        }
        guarded.state = State::None;
        drop(guarded);
        self.events.publish(Event::SecretsChanged);
        Ok(())
    }

    /// A copy of the open vault, for a change: or why it is not open.
    fn snapshot(&self) -> Result<Snapshot, VaultError> {
        match &self.guarded().state {
            State::Open { sealed, key, kept } => Ok(Snapshot {
                sealed: sealed.clone(),
                key: key.clone(),
                kept: kept.clone(),
                secrets: kept
                    .iter()
                    .filter_map(|name| {
                        Some((name.clone(), Zeroizing::new(self.registry.value(name)?)))
                    })
                    .collect(),
            }),
            State::Locked(_) => Err(VaultError::Locked),
            State::None => Err(VaultError::Missing),
            State::Unreadable(problem) => Err(VaultError::Unreadable(problem.clone())),
        }
    }

    /// Writes `sealed` with `secrets` encrypted under `key`, off the async runtime and holding no lock on the
    /// state, and returns it as written.
    async fn persist(
        &self,
        sealed: Sealed,
        key: Zeroizing<[u8; KEY_LENGTH]>,
        secrets: Plain,
    ) -> Result<Sealed, VaultError> {
        let (path, write_file) = (self.path.clone(), Arc::clone(&self.write_file));
        tokio::task::spawn_blocking(move || write(&*write_file, &path, sealed, &key, &secrets))
            .await
            .map_err(|error| VaultError::Write(io::Error::other(error)))?
    }

    /// Puts the vault as `written` in the state, and changes the names kept in it with `kept` if it is still open.
    /// Returns whether it is (if it was locked meanwhile, the change is in the locked vault), with the state still
    /// locked: what goes with the change in the registry is done before a lock can run.
    fn settle(
        &self,
        written: Sealed,
        kept: impl FnOnce(&mut BTreeSet<String>),
    ) -> (MutexGuard<'_, Guarded>, bool) {
        let mut guarded = self.guarded();
        let open = match &mut guarded.state {
            State::Open {
                sealed,
                kept: names,
                ..
            } => {
                *sealed = written;
                kept(names);
                true
            }
            // Only locking runs during a change.
            State::Locked(sealed) => {
                *sealed = written;
                false
            }
            State::None | State::Unreadable(_) => false,
        };
        (guarded, open)
    }
}

/// An open vault as it was when a change began.
struct Snapshot {
    sealed: Sealed,
    key: Zeroizing<[u8; KEY_LENGTH]>,
    kept: BTreeSet<String>,
    secrets: Plain,
}

/// Writes `sealed` with `secrets` encrypted under `key`, with a new nonce, to `path` with `write_file`, and returns
/// it as written.
fn write(
    write_file: &dyn Fn(&Path, &[u8]) -> io::Result<()>,
    path: &Path,
    mut sealed: Sealed,
    key: &[u8; KEY_LENGTH],
    secrets: &Plain,
) -> Result<Sealed, VaultError> {
    let plain = Zeroizing::new(
        serde_json::to_vec(secrets)
            .map_err(io::Error::other)
            .map_err(VaultError::Write)?,
    );
    let (nonce, encrypted) = encrypt(key, &plain, SECRETS_CONTEXT)?;
    sealed.nonce = Bytes(nonce.to_vec());
    sealed.secrets = Bytes(encrypted);
    let bytes = serde_json::to_vec_pretty(&sealed)
        .map_err(io::Error::other)
        .map_err(VaultError::Write)?;
    write_file(path, &bytes).map_err(VaultError::Write)?;
    Ok(sealed)
}

/// The vault's `key`, encrypted for `unlock`.
async fn slot(
    agent: Option<&SshAgent>,
    key: &[u8; KEY_LENGTH],
    unlock: Unlock,
) -> Result<Slot, VaultError> {
    let salt = random::<SALT_LENGTH>()?.to_vec();
    let (method, wrapping) = match unlock {
        Unlock::Passphrase(passphrase) => {
            if passphrase.chars().count() < MIN_PASSPHRASE {
                return Err(VaultError::Invalid(format!(
                    "the passphrase must have at least {MIN_PASSPHRASE} characters"
                )));
            }
            let wrapping = stretch(passphrase, salt.clone(), MEMORY_KIB, ITERATIONS, LANES).await?;
            let method = Method::Passphrase {
                salt: Bytes(salt),
                memory_kib: MEMORY_KIB,
                iterations: ITERATIONS,
                lanes: LANES,
            };
            (method, wrapping)
        }
        Unlock::SshKey(fingerprint) => {
            let fingerprint = fingerprint.ok_or(VaultError::NoAgentKey)?;
            let agent = agent.ok_or(AgentError::Missing)?;
            let key = agent
                .keys()
                .await?
                .into_iter()
                .find(|key| key.fingerprint() == fingerprint)
                .ok_or(VaultError::UnknownKey(fingerprint))?;
            if !key.signs_the_same() {
                return Err(VaultError::Unsuitable(key.label()));
            }
            let wrapping = sign(agent, &key, &salt).await?;
            // An agent may still sign at random: the vault would never open again.
            if sign(agent, &key, &salt).await? != wrapping {
                return Err(VaultError::Unsuitable(key.label()));
            }
            let method = Method::SshKey {
                public_key: Bytes(key.blob.clone()),
                label: key.label(),
                salt: Bytes(salt),
            };
            (method, wrapping)
        }
    };
    let (nonce, encrypted) = encrypt(&wrapping, key, KEY_CONTEXT)?;
    Ok(Slot {
        method,
        nonce: Bytes(nonce.to_vec()),
        key: Bytes(encrypted),
    })
}

/// The vault's key, from the first passphrase slot `passphrase` opens.
async fn open_with_passphrase(
    sealed: &Sealed,
    passphrase: &Zeroizing<String>,
) -> Result<Zeroizing<[u8; KEY_LENGTH]>, VaultError> {
    for slot in &sealed.unlocks {
        let Method::Passphrase {
            salt,
            memory_kib,
            iterations,
            lanes,
        } = &slot.method
        else {
            continue;
        };
        let wrapping = stretch(
            passphrase.clone(),
            salt.0.clone(),
            *memory_kib,
            *iterations,
            *lanes,
        )
        .await?;
        if let Some(key) = unwrap_key(slot, &wrapping) {
            return Ok(key);
        }
    }
    Err(VaultError::WrongPassphrase)
}

/// The vault's key, from the slot of an SSH key the agent holds: the one with `fingerprint`, or else any.
async fn open_with_agent(
    agent: &SshAgent,
    sealed: &Sealed,
    fingerprint: Option<&str>,
) -> Result<Zeroizing<[u8; KEY_LENGTH]>, VaultError> {
    let keys = agent.keys().await?;
    for slot in &sealed.unlocks {
        let Method::SshKey {
            public_key, salt, ..
        } = &slot.method
        else {
            continue;
        };
        let Some(key) = keys.iter().find(|key| {
            key.blob == public_key.0 && fingerprint.is_none_or(|wanted| key.fingerprint() == wanted)
        }) else {
            continue;
        };
        let wrapping = sign(agent, key, &salt.0).await?;
        return unwrap_key(slot, &wrapping).ok_or(VaultError::Damaged(
            "the SSH key signed differently from when it was added",
        ));
    }
    Err(fingerprint.map_or(VaultError::NoAgentKey, |fingerprint| {
        VaultError::UnknownKey(fingerprint.to_owned())
    }))
}

/// The key a passphrase derives, with Argon2id, off the async runtime: it takes about a second.
async fn stretch(
    passphrase: Zeroizing<String>,
    salt: Vec<u8>,
    memory_kib: u32,
    iterations: u32,
    lanes: u32,
) -> Result<Zeroizing<[u8; KEY_LENGTH]>, VaultError> {
    tokio::task::spawn_blocking(move || {
        let params = argon2::Params::new(memory_kib, iterations, lanes, Some(KEY_LENGTH))
            .map_err(|_| VaultError::Damaged("its passphrase settings are not valid"))?;
        let argon =
            argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
        let mut key = Zeroizing::new([0; KEY_LENGTH]);
        argon
            .hash_password_into(passphrase.as_bytes(), &salt, key.as_mut())
            .map_err(|_| VaultError::Damaged("its passphrase settings are not valid"))?;
        Ok(key)
    })
    .await
    .map_err(|_| VaultError::Damaged("deriving the key failed"))?
}

/// The key the SSH agent's signature of the vault's challenge derives, with HKDF-SHA256.
async fn sign(
    agent: &SshAgent,
    key: &AgentKey,
    salt: &[u8],
) -> Result<Zeroizing<[u8; KEY_LENGTH]>, VaultError> {
    let challenge = [CHALLENGE, salt].concat();
    let signature = Zeroizing::new(agent.sign(&key.blob, &challenge).await?);
    let mut derived = Zeroizing::new([0; KEY_LENGTH]);
    ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, salt)
        .extract(&signature)
        .expand(&[KEY_CONTEXT], ring::hkdf::HKDF_SHA256)
        .and_then(|okm| okm.fill(derived.as_mut()))
        // The output is 32 bytes, well within HKDF-SHA256's limit.
        .map_err(|_| VaultError::Damaged("deriving the key failed"))?;
    Ok(derived)
}

/// The vault's key from `slot`, if `wrapping` opens it.
fn unwrap_key(slot: &Slot, wrapping: &[u8; KEY_LENGTH]) -> Option<Zeroizing<[u8; KEY_LENGTH]>> {
    let plain = Zeroizing::new(decrypt(wrapping, &slot.nonce.0, &slot.key.0, KEY_CONTEXT)?);
    let key: [u8; KEY_LENGTH] = plain.as_slice().try_into().ok()?;
    Some(Zeroizing::new(key))
}

fn decrypt_secrets(sealed: &Sealed, key: &[u8; KEY_LENGTH]) -> Result<Plain, VaultError> {
    let plain = Zeroizing::new(
        decrypt(key, &sealed.nonce.0, &sealed.secrets.0, SECRETS_CONTEXT)
            .ok_or(VaultError::Damaged("its secrets do not match its key"))?,
    );
    serde_json::from_slice(&plain).map_err(|_| VaultError::Damaged("its secrets cannot be read"))
}

/// `plain` encrypted under `key` for `context`, with a new random nonce.
fn encrypt(
    key: &[u8; KEY_LENGTH],
    plain: &[u8],
    context: &[u8],
) -> Result<([u8; NONCE_LENGTH], Vec<u8>), VaultError> {
    let nonce = random::<NONCE_LENGTH>()?;
    let cipher = XChaCha20Poly1305::new(key.into());
    let encrypted = cipher
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: plain,
                aad: context,
            },
        )
        // Only past the cipher's limit, 256 GiB.
        .map_err(|_| VaultError::Write(io::Error::other("the secrets are too large to encrypt")))?;
    Ok((nonce, encrypted))
}

fn decrypt(
    key: &[u8; KEY_LENGTH],
    nonce: &[u8],
    encrypted: &[u8],
    context: &[u8],
) -> Option<Vec<u8>> {
    let nonce: [u8; NONCE_LENGTH] = nonce.try_into().ok()?;
    XChaCha20Poly1305::new(key.into())
        .decrypt(
            &XNonce::from(nonce),
            Payload {
                msg: encrypted,
                aad: context,
            },
        )
        .ok()
}

/// `N` bytes from the system's random source.
fn random<const N: usize>() -> Result<[u8; N], VaultError> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).map_err(VaultError::Random)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passphrase(text: &str) -> Unlock {
        Unlock::Passphrase(Zeroizing::new(text.into()))
    }

    /// From now on each write of `vault` says it started, then waits until the test lets it go on.
    fn slow_disk(
        vault: &mut Vault,
    ) -> (
        tokio::sync::mpsc::UnboundedReceiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (started, writing) = tokio::sync::mpsc::unbounded_channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let released = Mutex::new(released);
        vault.write_with(Arc::new(move |path, bytes| {
            let _ = started.send(());
            let _ = kelpy_common::lock(&released).recv();
            private_file::write(path, bytes)
        }));
        (writing, release)
    }

    fn vault(folder: &tempfile::TempDir) -> (Vault, Registry) {
        let registry = Registry::default();
        let vault = Vault::open(
            folder.path().join("vault.json"),
            None,
            registry.clone(),
            Events::new(),
        );
        (vault, registry)
    }

    #[tokio::test]
    async fn what_is_handed_over_never_turns_a_kept_secret_into_one_not_kept() {
        let folder = tempfile::tempdir().unwrap();
        let (vault, _) = vault(&folder);
        vault
            .create(passphrase("correct horse battery"))
            .await
            .unwrap();
        vault
            .add("smtp", Zeroizing::new("smtp-password".into()), true)
            .await
            .unwrap();
        vault
            .add("api", Zeroizing::new("until-kelpy-stops".into()), false)
            .await
            .unwrap();
        let key = vault.handed_over().key.unwrap();
        for _ in 0..3000 {
            vault.take_over(&key).await.unwrap();
            // Copies taken while another thread locks the vault: each one is all before the lock, or all after.
            std::thread::scope(|threads| {
                let copying = threads.spawn(|| {
                    loop {
                        let copy = vault.handed_over();
                        assert!(
                            !copy.shared.contains_key("smtp"),
                            "a kept secret handed over as not kept"
                        );
                        assert!(copy.shared.contains_key("api"));
                        assert!(copy.masked.contains("smtp-password"));
                        if copy.key.is_none() {
                            break;
                        }
                    }
                });
                std::thread::yield_now();
                vault.lock();
                copying.join().unwrap();
            });
        }
    }

    #[tokio::test]
    async fn kept_secrets_survive_a_restart_and_need_the_passphrase() {
        let folder = tempfile::tempdir().unwrap();
        let (vault, registry) = vault(&folder);
        assert_eq!(vault.view().state, VaultState::None);
        assert!(matches!(
            vault
                .add("smtp", Zeroizing::new("smtp-password".into()), true)
                .await,
            Err(VaultError::Missing)
        ));
        assert!(matches!(
            vault.create(passphrase("short")).await,
            Err(VaultError::Invalid(_))
        ));

        vault
            .create(passphrase("correct horse battery"))
            .await
            .unwrap();
        vault
            .add("smtp", Zeroizing::new("smtp-password".into()), true)
            .await
            .unwrap();
        vault
            .add("api", Zeroizing::new("until-kelpy-stops".into()), false)
            .await
            .unwrap();
        assert_eq!(registry.names(), ["api", "smtp"]);
        assert!(
            vault
                .add("api", Zeroizing::new("another-one".into()), false)
                .await
                .is_err()
        );
        assert!(
            vault
                .add("tiny", Zeroizing::new("12345".into()), false)
                .await
                .is_err()
        );
        let file = std::fs::read_to_string(folder.path().join("vault.json")).unwrap();
        assert!(!file.contains("smtp"), "{file}");

        // Kelpy starts again: the vault is locked, the secret added without keeping it is gone.
        let (vault, registry) = self::vault(&folder);
        let view = vault.view();
        assert_eq!(view.state, VaultState::Locked);
        assert_eq!(view.unlocks, [UnlockView::Passphrase]);
        assert!(registry.names().is_empty());
        assert!(matches!(
            vault.unlock(passphrase("wrong horse battery")).await,
            Err(VaultError::WrongPassphrase)
        ));
        vault
            .unlock(passphrase("correct horse battery"))
            .await
            .unwrap();
        assert_eq!(registry.value("smtp").as_deref(), Some("smtp-password"));
        assert_eq!(vault.kept(), BTreeSet::from(["smtp".to_owned()]));

        // Locked, its secrets leave the registry.
        vault.lock();
        assert_eq!(registry.value("smtp"), None);
        assert!(matches!(
            vault.keep("smtp").await,
            Err(VaultError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn a_secret_added_for_now_can_be_kept_and_removed() {
        let folder = tempfile::tempdir().unwrap();
        let (vault, registry) = vault(&folder);
        vault
            .add("db_root", Zeroizing::new("root-password".into()), false)
            .await
            .unwrap();
        assert!(matches!(
            vault.keep("db_root").await,
            Err(VaultError::Missing)
        ));
        vault
            .create(passphrase("correct horse battery"))
            .await
            .unwrap();
        vault.keep("db_root").await.unwrap();

        // A second passphrase opens it too; the last way cannot be removed.
        vault
            .add_unlock(passphrase("another long passphrase"))
            .await
            .unwrap();
        vault.remove_unlock(0).await.unwrap();
        assert!(vault.remove_unlock(0).await.is_err());

        let (again, _) = self::vault(&folder);
        assert!(
            again
                .unlock(passphrase("correct horse battery"))
                .await
                .is_err()
        );
        again
            .unlock(passphrase("another long passphrase"))
            .await
            .unwrap();
        assert_eq!(again.kept().len(), 1);

        vault.remove("db_root").await.unwrap();
        assert_eq!(registry.value("db_root"), None);
        let (third, registry) = self::vault(&folder);
        third
            .unlock(passphrase("another long passphrase"))
            .await
            .unwrap();
        assert!(registry.names().is_empty());
        third.delete().await.unwrap();
        assert!(!folder.path().join("vault.json").exists());
        assert_eq!(third.view().state, VaultState::None);
    }

    #[tokio::test]
    async fn a_failed_write_changes_nothing() {
        let folder = tempfile::tempdir().unwrap();
        let (vault, registry) = vault(&folder);
        vault
            .create(passphrase("correct horse battery"))
            .await
            .unwrap();
        let before = std::fs::read(folder.path().join("vault.json")).unwrap();
        // A folder where the file's staged copy goes makes the write fail.
        std::fs::create_dir(folder.path().join("vault.json.new")).unwrap();
        assert!(matches!(
            vault
                .add("smtp", Zeroizing::new("smtp-password".into()), true)
                .await,
            Err(VaultError::Write(_))
        ));
        assert_eq!(registry.value("smtp"), None);
        assert!(vault.kept().is_empty());
        assert_eq!(
            std::fs::read(folder.path().join("vault.json")).unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn while_the_agent_is_slow_to_sign_the_vault_can_be_shown_and_locked() {
        let folder = tempfile::tempdir().unwrap();
        let mut fake = crate::ssh_agent::fake::start();
        let registry = Registry::default();
        let vault = std::sync::Arc::new(Vault::open(
            folder.path().join("vault.json"),
            Some(fake.agent.clone()),
            registry.clone(),
            Events::new(),
        ));
        vault
            .create(Unlock::SshKey(Some(fake.fingerprint.clone())))
            .await
            .unwrap();
        vault
            .add("smtp", Zeroizing::new("smtp-password".into()), true)
            .await
            .unwrap();
        vault.lock();

        // The agent waits for its user to confirm.
        fake.signing.send_replace(false);
        let unlocking = tokio::spawn({
            let vault = vault.clone();
            async move { vault.unlock(Unlock::SshKey(None)).await }
        });
        fake.waiting.wait_for(|count| *count == 1).await.unwrap();
        assert_eq!(vault.view().state, VaultState::Locked);
        // Locking meanwhile cancels the unlock.
        vault.lock();
        fake.signing.send_replace(true);
        assert!(matches!(
            unlocking.await.unwrap(),
            Err(VaultError::Cancelled)
        ));
        assert_eq!(vault.view().state, VaultState::Locked);
        assert_eq!(registry.value("smtp"), None);

        vault.unlock(Unlock::SshKey(None)).await.unwrap();
        assert_eq!(registry.value("smtp").as_deref(), Some("smtp-password"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn while_the_disk_is_slow_to_write_the_vault_can_be_shown_and_locked() {
        let folder = tempfile::tempdir().unwrap();
        let (mut vault, registry) = vault(&folder);
        vault
            .create(passphrase("correct horse battery"))
            .await
            .unwrap();
        vault
            .add("smtp", Zeroizing::new("smtp-password".into()), true)
            .await
            .unwrap();

        let (mut writing, release) = slow_disk(&mut vault);
        let vault = Arc::new(vault);
        let adding = tokio::spawn({
            let vault = Arc::clone(&vault);
            async move {
                vault
                    .add("api", Zeroizing::new("api-token-value".into()), true)
                    .await
            }
        });
        writing.recv().await.unwrap();

        // A write holding the state's lock would keep these waiting.
        let shown_and_locked = tokio::task::spawn_blocking({
            let vault = Arc::clone(&vault);
            move || {
                let shown = vault.view().state;
                vault.lock();
                shown
            }
        });
        let shown = tokio::time::timeout(std::time::Duration::from_secs(5), shown_and_locked)
            .await
            .expect("the vault waited for its write")
            .unwrap();
        assert_eq!(shown, VaultState::Unlocked);
        assert_eq!(vault.view().state, VaultState::Locked);
        assert_eq!(registry.value("smtp"), None);

        // The secret written meanwhile is kept in the locked vault, not in the registry.
        release.send(()).unwrap();
        adding.await.unwrap().unwrap();
        assert_eq!(registry.value("api"), None);
        let (again, registry) = self::vault(&folder);
        again
            .unlock(passphrase("correct horse battery"))
            .await
            .unwrap();
        assert_eq!(registry.value("api").as_deref(), Some("api-token-value"));
        assert_eq!(registry.value("smtp").as_deref(), Some("smtp-password"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_change_whose_caller_stops_waiting_ends_before_the_next_one() {
        let folder = tempfile::tempdir().unwrap();
        let (mut vault, registry) = vault(&folder);
        vault
            .create(passphrase("correct horse battery"))
            .await
            .unwrap();
        let (mut writing, release) = slow_disk(&mut vault);
        let vault = Arc::new(vault);
        let adding = tokio::spawn({
            let vault = Arc::clone(&vault);
            async move {
                vault
                    .add("api", Zeroizing::new("api-token-value".into()), true)
                    .await
            }
        });
        writing.recv().await.unwrap();
        // The caller stops waiting while the file is written; the vault is deleted next.
        adding.abort();
        assert!(adding.await.unwrap_err().is_cancelled());
        let mut deleting = tokio::spawn({
            let vault = Arc::clone(&vault);
            async move { vault.delete().await }
        });
        // The deletion waits for the write its caller left.
        let waited = std::time::Duration::from_millis(200);
        assert!(tokio::time::timeout(waited, &mut deleting).await.is_err());
        release.send(()).unwrap();
        deleting.await.unwrap().unwrap();

        // The write ended before the deletion: no file, and nothing of the vault in memory.
        assert!(!folder.path().join("vault.json").exists());
        assert_eq!(vault.view().state, VaultState::None);
        assert_eq!(registry.value("api"), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stopping_waits_for_the_changes_let_in_and_refuses_the_next() {
        let folder = tempfile::tempdir().unwrap();
        let (mut vault, registry) = vault(&folder);
        vault
            .create(passphrase("correct horse battery"))
            .await
            .unwrap();
        // A change queued behind one that is writing.
        let (mut writing, release) = slow_disk(&mut vault);
        let vault = Arc::new(vault);
        let add = |name: &'static str| {
            let vault = Arc::clone(&vault);
            tokio::spawn(async move {
                vault
                    .add(name, Zeroizing::new(format!("{name}-value")), true)
                    .await
            })
        };
        let first = add("first");
        writing.recv().await.unwrap();
        let second = add("second");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let mut stopped = tokio::spawn({
            let vault = Arc::clone(&vault);
            async move { vault.stopped().await }
        });
        let waited = std::time::Duration::from_millis(200);
        assert!(tokio::time::timeout(waited, &mut stopped).await.is_err());
        assert!(matches!(
            vault
                .add("late", Zeroizing::new("late-value".into()), true)
                .await,
            Err(VaultError::Stopping)
        ));
        // Both writes go on and end before the vault says it stopped.
        release.send(()).unwrap();
        writing.recv().await.unwrap();
        release.send(()).unwrap();
        stopped.await.unwrap();
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
        assert_eq!(registry.value("second").as_deref(), Some("second-value"));
        assert_eq!(registry.value("late"), None);
        let written = std::fs::read_to_string(folder.path().join("vault.json")).unwrap();
        let reopened = Vault::open(
            folder.path().join("vault.json"),
            None,
            Registry::default(),
            Events::new(),
        );
        reopened
            .unlock(passphrase("correct horse battery"))
            .await
            .unwrap();
        assert_eq!(
            reopened.kept(),
            BTreeSet::from(["first".into(), "second".into()]),
            "{written}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_secret_kept_while_the_vault_is_locked_leaves_the_registry() {
        let folder = tempfile::tempdir().unwrap();
        let (mut vault, registry) = vault(&folder);
        vault
            .create(passphrase("correct horse battery"))
            .await
            .unwrap();
        vault
            .add("db_root", Zeroizing::new("root-password".into()), false)
            .await
            .unwrap();
        let (mut writing, release) = slow_disk(&mut vault);
        let vault = Arc::new(vault);
        let keeping = tokio::spawn({
            let vault = Arc::clone(&vault);
            async move { vault.keep("db_root").await }
        });
        writing.recv().await.unwrap();
        vault.lock();
        release.send(()).unwrap();
        keeping.await.unwrap().unwrap();

        // Kept in the locked vault, it is not available until the vault is unlocked again.
        assert_eq!(vault.view().state, VaultState::Locked);
        assert_eq!(registry.value("db_root"), None);
        let (again, registry) = self::vault(&folder);
        again
            .unlock(passphrase("correct horse battery"))
            .await
            .unwrap();
        assert_eq!(registry.value("db_root").as_deref(), Some("root-password"));
    }

    #[tokio::test]
    async fn a_changed_file_does_not_open() {
        let folder = tempfile::tempdir().unwrap();
        let (vault, _) = vault(&folder);
        vault
            .create(passphrase("correct horse battery"))
            .await
            .unwrap();
        vault
            .add("smtp", Zeroizing::new("smtp-password".into()), true)
            .await
            .unwrap();
        let path = folder.path().join("vault.json");
        let mut sealed: Sealed = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        sealed.secrets.0[0] ^= 1;
        std::fs::write(&path, serde_json::to_vec(&sealed).unwrap()).unwrap();
        let (again, registry) = self::vault(&folder);
        assert!(matches!(
            again.unlock(passphrase("correct horse battery")).await,
            Err(VaultError::Damaged(_))
        ));
        assert!(registry.names().is_empty());

        std::fs::write(&path, "not json").unwrap();
        let (unreadable, _) = self::vault(&folder);
        let view = unreadable.view();
        assert_eq!(view.state, VaultState::None);
        assert!(view.problem.is_some());
        assert!(matches!(
            unreadable.create(passphrase("correct horse battery")).await,
            Err(VaultError::Exists)
        ));
        unreadable.delete().await.unwrap();
        unreadable
            .create(passphrase("correct horse battery"))
            .await
            .unwrap();
    }

    /// With a private ssh-agent holding a new Ed25519 key, where OpenSSH is installed.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_ssh_key_of_the_agent_unlocks_the_vault() {
        use std::process::{Command, Stdio};

        let folder = tempfile::tempdir().unwrap();
        let socket = folder.path().join("agent.sock");
        let Ok(mut agent) = Command::new("ssh-agent")
            .arg("-D")
            .arg("-a")
            .arg(&socket)
            .stdout(Stdio::null())
            .spawn()
        else {
            eprintln!("skipped: no ssh-agent");
            return;
        };
        let key = folder.path().join("key");
        let made = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "admin@laptop", "-f"])
            .arg(&key)
            .status()
            .unwrap();
        assert!(made.success());
        while !socket.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let added = Command::new("ssh-add")
            .arg(&key)
            .env("SSH_AUTH_SOCK", &socket)
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(added.success());

        let ssh = SshAgent::at(socket.clone());
        let keys = ssh.keys().await.unwrap();
        assert_eq!(keys.len(), 1);
        assert!(keys[0].label().ends_with("(admin@laptop)"));
        let fingerprint = keys[0].fingerprint();

        let open = |agent| {
            Vault::open(
                folder.path().join("vault.json"),
                agent,
                Registry::default(),
                Events::new(),
            )
        };
        let vault = open(Some(ssh.clone()));
        vault
            .create(Unlock::SshKey(Some(fingerprint.clone())))
            .await
            .unwrap();
        vault
            .add("smtp", Zeroizing::new("smtp-password".into()), true)
            .await
            .unwrap();
        assert!(
            vault
                .add_unlock(Unlock::SshKey(Some(fingerprint.clone())))
                .await
                .is_err()
        );

        let again = open(Some(ssh.clone()));
        again.unlock(Unlock::SshKey(None)).await.unwrap();
        assert_eq!(again.kept().len(), 1);
        let view = again.view();
        assert!(
            matches!(&view.unlocks[..], [UnlockView::SshKey { fingerprint: shown, .. }] if *shown == fingerprint)
        );

        // Without the agent, or with the key gone from it, the vault stays locked.
        assert!(matches!(
            open(None).unlock(Unlock::SshKey(None)).await,
            Err(VaultError::Agent(_))
        ));
        let removed = Command::new("ssh-add")
            .arg("-D")
            .env("SSH_AUTH_SOCK", &socket)
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(removed.success());
        assert!(matches!(
            open(Some(ssh)).unlock(Unlock::SshKey(None)).await,
            Err(VaultError::NoAgentKey)
        ));
        agent.kill().unwrap();
        agent.wait().unwrap();
    }
}
