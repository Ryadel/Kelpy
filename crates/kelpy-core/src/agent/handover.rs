//! What lives only in Kelpy's memory, handed over to the Kelpy restarted in its place (as administrator): the key
//! entered for the provider, the ChatGPT session, the administrator's secrets and those each conversation
//! generated, and the vault's key while it is unlocked. Everything else is on disk, where the new Kelpy reads it.
//!
//! It travels only over the channel between the two processes, never through a file.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use super::Agent;
use crate::chatgpt::SessionState;

/// The state an agent hands over; [`Agent::take_over`] takes it.
#[derive(Default, Serialize, Deserialize)]
pub struct AgentState {
    /// The key entered for the provider in this session.
    provider_key: Option<String>,
    chatgpt: Option<SessionState>,
    /// Every secret value masked so far, in any conversation.
    masked: BTreeSet<String>,
    /// The administrator's secrets, by name: those added without keeping them, and the vault's while unlocked.
    shared: BTreeMap<String, String>,
    /// The secrets each conversation generated, by name.
    generated: BTreeMap<i64, BTreeMap<String, String>>,
    /// The vault's key, while it is unlocked.
    vault_key: Option<Zeroizing<Vec<u8>>>,
}

impl Agent {
    /// What lives only in this agent's memory, for the Kelpy restarted in its place. Taken once no answer is being
    /// written: nothing changes it meanwhile but the administrator.
    pub fn handed_over(&self) -> AgentState {
        let (masked, shared, generated, vault_key) = self.server.handed_over();
        AgentState {
            provider_key: self.providers.handed_over(),
            chatgpt: self.providers.chatgpt().handed_over(),
            masked,
            shared,
            generated,
            vault_key,
        }
    }

    /// Takes over what the Kelpy before this one handed over, before anything uses this agent. Returns what could
    /// not be taken over, such as a vault changed on disk meanwhile.
    pub async fn take_over(&self, state: AgentState) -> Vec<String> {
        let mut problems = Vec::new();
        self.providers.take_over(state.provider_key);
        if let Some(session) = state.chatgpt {
            self.providers.chatgpt().take_over(session);
        }
        if let Err(problem) = self
            .server
            .take_over(state.masked, state.shared, state.generated, state.vault_key)
            .await
        {
            problems.push(format!("the vault stays locked: {problem}"));
        }
        problems
    }
}
