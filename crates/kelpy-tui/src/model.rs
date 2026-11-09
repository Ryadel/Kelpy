//! What the terminal UI reads from the runtime's API. Conversations, messages and actions are the runtime's own
//! types, as it sends them, so that nothing it sends is lost on the way: a conversation's revision, every detail of
//! an action to approve. The envelopes of the streams are the terminal UI's, as it follows only some events.

pub use kelpy_core::storage::{Action, ActionState, Message, Role, Session, Status};
use serde::Deserialize;

/// A conversation's messages, whether an answer is being written, and what the latest answer took of the model's
/// context window.
#[derive(Debug, Clone, Deserialize)]
pub struct Conversation {
    pub messages: Vec<Message>,
    pub running: bool,
    #[serde(default)]
    pub context: Option<ContextUse>,
}

/// How much of the model's context window an answer's request took, in tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct ContextUse {
    pub used: u64,
    pub window: u64,
}

/// What an answer's stream carries.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatEvent {
    User {
        message: Message,
    },
    Started {
        message: Message,
    },
    Delta {
        text: String,
    },
    Action {
        action: Box<Action>,
    },
    Done {
        message: Message,
        error: Option<String>,
        #[serde(default)]
        context: Option<ContextUse>,
    },
}

/// What the runtime's event channel carries, as far as the terminal UI follows it.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEvent {
    SessionCreated {
        session: Session,
    },
    SessionChanged {
        session: Session,
    },
    SessionDeleted {
        id: i64,
    },
    AnswerStarted {
        session_id: i64,
    },
    AnswerEnded {
        session_id: i64,
        #[serde(default)]
        context: Option<ContextUse>,
    },
    ProviderChanged,
    ConnectionsChanged,
    Resync,
    Stopping,
    /// Changes the terminal UI does not show, such as the secrets or the administrator's own operations.
    #[serde(other)]
    Other,
}

/// A remote connection waiting for the administrator's decision.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Connection {
    pub id: u64,
    pub address: String,
    pub user: String,
    pub key: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Connections {
    pub waiting: Vec<Connection>,
}

/// The provider, as far as the terminal UI shows it.
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderInfo {
    pub configured: Option<Configured>,
    pub needs: Option<String>,
    #[serde(default)]
    pub services: Vec<Service>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Configured {
    pub kind: String,
    pub service: Option<String>,
    pub model: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Service {
    pub id: String,
    pub name: String,
}

/// The provider as the terminal UI shows it: its name and model, and what keeps it from answering, if anything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderState {
    /// `Groq (llama-3.3-70b)`; empty until read.
    pub label: String,
    pub problem: Option<String>,
}

impl ProviderInfo {
    pub fn state(&self) -> ProviderState {
        let Some(configured) = &self.configured else {
            return ProviderState {
                label: "No AI provider".into(),
                problem: Some("choose one in the Web UI or run `kelpy provider`".into()),
            };
        };
        let name = |id: &str| {
            self.services
                .iter()
                .find(|service| service.id == id)
                .map(|service| service.name.as_str())
        };
        let service = configured
            .service
            .as_deref()
            .and_then(name)
            .or_else(|| name(&configured.kind))
            .unwrap_or(&configured.kind);
        let problem = match self.needs.as_deref() {
            Some("sign_in") => Some("not signed in: sign in from the Web UI"),
            Some("key") => Some("no API key: enter it in the Web UI"),
            _ => None,
        };
        ProviderState {
            label: format!("{service} ({})", configured.model),
            problem: problem.map(str::to_owned),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_events_and_fields_are_ignored() {
        let event: RuntimeEvent = serde_json::from_str(r#"{"type":"secrets_changed"}"#).unwrap();
        assert!(matches!(event, RuntimeEvent::Other));
        let event: RuntimeEvent = serde_json::from_str(
            r#"{"type":"answer_started","session_id":4,"user":{},"answer":{}}"#,
        )
        .unwrap();
        assert!(matches!(
            event,
            RuntimeEvent::AnswerStarted { session_id: 4 }
        ));
    }

    #[test]
    fn the_provider_is_named_with_its_state() {
        let info: ProviderInfo = serde_json::from_str(
            r#"{"configured":{"kind":"openai_compatible","service":"groq","model":"llama"},"needs":"key",
                "services":[{"id":"groq","name":"Groq"}]}"#,
        )
        .unwrap();
        assert_eq!(
            info.state(),
            ProviderState {
                label: "Groq (llama)".into(),
                problem: Some("no API key: enter it in the Web UI".into()),
            }
        );
        let none: ProviderInfo =
            serde_json::from_str(r#"{"configured":null,"needs":null}"#).unwrap();
        assert!(none.state().problem.unwrap().starts_with("choose one"));
    }
}
