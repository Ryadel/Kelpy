//! The secrets as the administrator sees them: their own, added in Secrets or kept in the vault, which every
//! conversation can use; and those each conversation generated. Their values are shown to the administrator on
//! request, never to the model, which is told only their names.

use serde::Serialize;

use super::Server;
use crate::events::Event;
use crate::vault::{VaultError, VaultState, VaultView};

/// Every secret, by name, and the vault.
#[derive(Debug, Clone, Serialize)]
pub struct SecretsView {
    pub vault: VaultView,
    /// The administrator's secrets, which every conversation can use.
    pub shared: Vec<SharedSecret>,
    /// The secrets the conversations generated, which last until Kelpy stops.
    pub generated: Vec<GeneratedSecret>,
    /// The keys of the SSH agent of Kelpy's session.
    pub agent_keys: Vec<AgentKeyView>,
    /// Why the agent's keys could not be listed.
    pub agent_problem: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SharedSecret {
    pub name: String,
    /// Whether it is kept in the vault, or else lasts until Kelpy stops.
    pub kept: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct GeneratedSecret {
    pub session_id: i64,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentKeyView {
    pub label: String,
    pub fingerprint: String,
    /// Whether it can unlock the vault: its signatures are always the same for the same data.
    pub usable: bool,
}

impl Server {
    pub(in crate::agent) async fn secrets_view(&self) -> SecretsView {
        let kept = self.vault.kept();
        let shared = self
            .registry
            .names()
            .into_iter()
            .map(|name| SharedSecret {
                kept: kept.contains(&name),
                name,
            })
            .collect();
        let mut generated: Vec<GeneratedSecret> = kelpy_common::lock(&self.secrets)
            .iter()
            .flat_map(|(&session_id, secrets)| {
                secrets
                    .names()
                    .into_iter()
                    .map(move |name| GeneratedSecret { session_id, name })
            })
            .collect();
        generated.sort_by(|a, b| (a.session_id, &a.name).cmp(&(b.session_id, &b.name)));
        let (agent_keys, agent_problem) = match self.vault.agent_keys().await {
            Ok(keys) => (
                keys.iter()
                    .map(|key| AgentKeyView {
                        label: key.label(),
                        fingerprint: key.fingerprint(),
                        usable: key.signs_the_same(),
                    })
                    .collect(),
                None,
            ),
            Err(error) => (Vec::new(), Some(error.to_string())),
        };
        SecretsView {
            vault: self.vault.view(),
            shared,
            generated,
            agent_keys,
            agent_problem,
        }
    }

    /// The value of a secret, for the administrator: one of theirs, or one `session_id` generated.
    pub(in crate::agent) fn secret_value(
        &self,
        session_id: Option<i64>,
        name: &str,
    ) -> Option<String> {
        match session_id {
            Some(id) => kelpy_common::lock(&self.secrets).get(&id)?.value(name),
            None => self.registry.value(name),
        }
    }

    /// Keeps the secret `name` that `session_id` generated in the vault, under the same name, for every
    /// conversation.
    pub(in crate::agent) async fn keep_generated(
        &self,
        session_id: i64,
        name: &str,
    ) -> Result<(), VaultError> {
        match self.vault.view().state {
            VaultState::Unlocked => {}
            VaultState::Locked => return Err(VaultError::Locked),
            VaultState::None => return Err(VaultError::Missing),
        }
        let secrets = kelpy_common::lock(&self.secrets)
            .get(&session_id)
            .cloned()
            .ok_or_else(|| {
                VaultError::Invalid(format!("this conversation has no secret named `{name}`"))
            })?;
        let value = secrets
            .share(name)
            .map_err(|error| VaultError::Invalid(error.to_string()))?;
        if let Err(error) = self.vault.keep(name).await {
            secrets.unshare(name, value);
            return Err(error);
        }
        self.events.publish(Event::SecretsChanged);
        Ok(())
    }

    /// What the model is told of the administrator's secrets and the vault.
    pub(in crate::agent) async fn secrets_note(&self) -> String {
        let names = self.registry.names();
        let mut note = String::from("## The administrator's secrets\n\n");
        if names.is_empty() {
            note.push_str("The administrator has added no secret.");
        } else {
            note.push_str(&format!(
                "The administrator's secrets, which you can use in every conversation as {{{{secret:name}}}} \
                 wherever generated secrets go: {}.",
                names.join(", ")
            ));
        }
        if self.vault.view().state == VaultState::Locked {
            note.push_str(
                " The vault is locked: the secrets kept in it are missing until the administrator unlocks it in \
                 Secrets.",
            );
        }
        note
    }
}
