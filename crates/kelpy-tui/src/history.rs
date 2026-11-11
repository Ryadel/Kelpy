//! The messages already sent in the open conversation, brought back into the message box with Up and Down, as a
//! shell brings back its commands: back from the latest, and forward again to the draft being written.

use crate::model::{Message, Role};

/// Where the message box is in the history: at the draft, or some messages back.
#[derive(Debug, Default)]
pub struct Recall {
    /// 0 is the draft, 1 the latest message sent.
    position: usize,
    draft: String,
}

impl Recall {
    /// The message one further back in `sent` (oldest first), keeping `current` as the draft when leaving it; none
    /// at the oldest.
    pub fn back(&mut self, sent: &[&str], current: &str) -> Option<String> {
        if self.position >= sent.len() {
            return None;
        }
        if self.position == 0 {
            self.draft = current.to_owned();
        }
        self.position += 1;
        Some(sent[sent.len() - self.position].to_owned())
    }

    /// The message one further forward, the draft after the latest; none when not browsing.
    pub fn forward(&mut self, sent: &[&str]) -> Option<String> {
        if self.position == 0 {
            return None;
        }
        self.position -= 1;
        Some(match self.position {
            0 => std::mem::take(&mut self.draft),
            position => sent[sent.len() - position].to_owned(),
        })
    }

    /// Typing, sending or another conversation ends the browsing: what the box holds is the draft.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// The messages `messages` sent, oldest first; a message repeated at once is offered once.
pub fn sent(messages: &[Message]) -> Vec<&str> {
    let mut sent: Vec<&str> = Vec::new();
    for message in messages.iter().filter(|message| message.role == Role::User) {
        let content = message.content.as_str();
        if !content.trim().is_empty() && sent.last() != Some(&content) {
            sent.push(content);
        }
    }
    sent
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::message;

    #[test]
    fn up_goes_back_from_the_latest_and_down_comes_forward_to_the_draft() {
        let (sent, mut recall) = (["uptime?", "df -h?"], Recall::default());
        assert_eq!(recall.back(&sent, "half wri").as_deref(), Some("df -h?"));
        assert_eq!(recall.back(&sent, "df -h?").as_deref(), Some("uptime?"));
        assert_eq!(recall.back(&sent, "uptime?"), None);
        assert_eq!(recall.forward(&sent).as_deref(), Some("df -h?"));
        assert_eq!(recall.forward(&sent).as_deref(), Some("half wri"));
        assert_eq!(recall.forward(&sent), None);
    }

    #[test]
    fn typing_ends_the_browsing() {
        let (sent, mut recall) = (["one", "two"], Recall::default());
        recall.back(&sent, "");
        recall.back(&sent, "two");
        recall.reset();
        assert_eq!(recall.back(&sent, "one, edited").as_deref(), Some("two"));
        assert_eq!(recall.forward(&sent).as_deref(), Some("one, edited"));
    }

    #[test]
    fn only_the_messages_sent_are_offered_a_repeat_once() {
        let messages = [
            message(1, Role::User, "uptime?"),
            message(2, Role::Assistant, "Up 3 days."),
            message(3, Role::User, "uptime?"),
            message(4, Role::User, " "),
            message(5, Role::User, "df -h?"),
        ];
        assert_eq!(sent(&messages), ["uptime?", "df -h?"]);
    }
}
