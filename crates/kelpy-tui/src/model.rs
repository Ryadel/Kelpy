//! What the terminal UI reads from the runtime's API. Conversations, messages and actions are the runtime's own
//! types, as it sends them, so that nothing it sends is lost on the way: a conversation's revision, every detail of
//! an action to approve. The envelopes of the streams are the terminal UI's, as it follows only some events.

pub use kelpy_client::setup::ProviderInfo;
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
    /// The administrator's likely next message, written after the answer `after`.
    Suggestion {
        session_id: i64,
        after: i64,
        text: String,
    },
    SuggestionsChanged {
        enabled: bool,
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

/// The provider as the terminal UI shows it: its name and model, and what keeps it from answering, if anything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderState {
    /// `Groq (llama-3.3-70b)`; empty until read.
    pub label: String,
    pub problem: Option<String>,
    /// Whether a provider is chosen at all.
    pub configured: bool,
}

impl ProviderState {
    /// What to say instead of sending a message, when Kelpy would refuse it.
    pub fn refusal(&self) -> Option<String> {
        let problem = self.problem.as_deref()?;
        Some(if self.configured {
            format!("{}: {problem}.", self.label)
        } else {
            format!(
                "No AI provider configured yet: {}.",
                problem.replace('`', "'")
            )
        })
    }
}

/// The provider as the terminal UI shows it, from what the runtime says of it.
pub fn provider_state(info: &ProviderInfo) -> ProviderState {
    let Some(configured) = &info.configured else {
        return ProviderState {
            label: "No AI provider".into(),
            problem: Some("press Ctrl+P to set one up".into()),
            configured: false,
        };
    };
    let service = info
        .configured_service()
        .map_or(configured.kind.as_str(), |service| service.name.as_str());
    let problem = match info.needs.as_deref() {
        Some("sign_in") => Some("not signed in: Ctrl+P signs in"),
        Some("key") => Some("no API key: Ctrl+P enters it"),
        _ => None,
    };
    ProviderState {
        label: format!("{service} ({})", configured.model),
        problem: problem.map(str::to_owned),
        configured: true,
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
    fn a_message_kelpy_would_refuse_is_held_back_with_what_is_missing() {
        let ready = ProviderState {
            label: "Groq (llama)".into(),
            problem: None,
            configured: true,
        };
        assert_eq!(ready.refusal(), None);
        let keyless = ProviderState {
            problem: Some("no API key: Ctrl+P enters it".into()),
            ..ready
        };
        assert_eq!(
            keyless.refusal().as_deref(),
            Some("Groq (llama): no API key: Ctrl+P enters it.")
        );
    }

    #[test]
    fn the_provider_is_named_with_its_state() {
        let info: ProviderInfo = serde_json::from_str(
            r#"{"configured":{"kind":"openai_compatible","service":"groq","model":"llama"},"needs":"key",
                "services":[{"id":"groq","name":"Groq","way":"free","kind":"openai_compatible","base_url":null,
                "description":"","key_page":null,"steps":[],"models":[]}]}"#,
        )
        .unwrap();
        assert_eq!(
            provider_state(&info),
            ProviderState {
                label: "Groq (llama)".into(),
                problem: Some("no API key: Ctrl+P enters it".into()),
                configured: true,
            }
        );
        let none: ProviderInfo =
            serde_json::from_str(r#"{"configured":null,"needs":null}"#).unwrap();
        assert_eq!(
            provider_state(&none).refusal().as_deref(),
            Some("No AI provider configured yet: press Ctrl+P to set one up.")
        );
    }
}
