//! The administrator's likely next message: once an answer ends, the model reads the end of the conversation and
//! writes what the administrator would probably ask next, shown in grey in the message box. It is only text: sending
//! it stays with the administrator, and what it asks is approved like any other message.

use std::sync::atomic::Ordering;
use std::time::Duration;

use kelpy_common::lock;
use kelpy_providers::{ChatMessage, ChatRequest, Outcome, Settings, StopReason};
use kelpy_storage::Message;
use tokio_util::sync::CancellationToken;

use super::Turns;
use crate::agent::context;
use crate::events::Event;

/// How long the model may take to suggest: past it, nothing is shown.
const SUGGESTION_TIME: Duration = Duration::from_secs(30);
/// How much of the end of the conversation the model reads, in tokens.
const SUGGESTION_ROOM: u64 = 3_000;
/// The longest suggestion shown, in characters; a longer one is dropped.
const MAX_SUGGESTION: usize = 200;
/// What the model writes when there is no obvious next message.
const NOTHING: &str = "NONE";

const SUGGESTION_INSTRUCTIONS: &str = "You predict the next message a system administrator will write to Kelpy, \
an AI assistant that manages their server. Read the end of the conversation below and write the message the \
administrator would most likely send next, as they would type it: one short sentence, in the language the \
administrator writes in, from their side (a request or a question to Kelpy, never Kelpy's answer). Prefer the \
obvious next step: checking a result, the cause of a problem just fixed, the step Kelpy offered. Never suggest \
deleting, removing, stopping, restarting or overwriting something unless the administrator was already heading \
there. Never include passwords, keys or other secrets. If there is no clear next message, write NONE. Answer with \
the message only, without quotes.";

impl Turns {
    /// Whether the next message is suggested after each answer.
    pub(in crate::agent) fn suggests(&self) -> bool {
        self.suggestions.load(Ordering::Relaxed)
    }

    /// Turns suggesting the next message on or off; turned off, the suggestions being written stop.
    pub(in crate::agent) fn set_suggests(&self, enabled: bool) {
        self.suggestions.store(enabled, Ordering::Relaxed);
        if !enabled {
            for (_, (_, stop)) in lock(&self.suggesting).drain() {
                stop.cancel();
            }
        }
    }

    /// Stops the suggestion being written for a conversation, if any: a new answer or its deletion makes it moot.
    pub(super) fn stop_suggesting(&self, session_id: i64) {
        if let Some((_, stop)) = lock(&self.suggesting).remove(&session_id) {
            stop.cancel();
        }
    }

    /// Stops every suggestion being written, as Kelpy stops.
    pub(super) fn stop_all_suggestions(&self) {
        for (_, (_, stop)) in lock(&self.suggesting).drain() {
            stop.cancel();
        }
    }

    /// Asks the model, with `settings`, for the administrator's next message after the answer `after` of the
    /// conversation, and publishes it. Nothing is published if suggestions are off, the model has nothing to suggest,
    /// takes too long, or a new answer starts meanwhile.
    pub(super) async fn suggest(&self, settings: Settings, session_id: i64, after: i64) {
        if !self.suggests() {
            return;
        }
        let number = self.last_suggestion.fetch_add(1, Ordering::Relaxed) + 1;
        let stop = CancellationToken::new();
        if let Some((_, older)) = lock(&self.suggesting).insert(session_id, (number, stop.clone()))
        {
            older.cancel();
        }
        let suggested = tokio::select! {
            () = stop.cancelled() => None,
            suggested = tokio::time::timeout(SUGGESTION_TIME, self.write_suggestion(&settings, session_id)) =>
                suggested.ok().flatten(),
        };
        {
            let mut suggesting = lock(&self.suggesting);
            // Still this suggestion's turn: no new answer, deletion or newer suggestion took its place.
            if suggesting.get(&session_id).map(|(current, _)| *current) != Some(number) {
                return;
            }
            suggesting.remove(&session_id);
        }
        if let Some(text) = suggested {
            self.events.publish(Event::Suggestion {
                session_id,
                after,
                text,
            });
        }
    }

    async fn write_suggestion(&self, settings: &Settings, session_id: i64) -> Option<String> {
        let messages = self
            .store
            .call(move |store| store.messages(session_id))
            .await
            .ok()?;
        let messages = [ChatMessage::user(suggestion_request(&messages)?)];
        let request = ChatRequest {
            system: SUGGESTION_INSTRUCTIONS,
            messages: &messages,
            tools: &[],
        };
        let mut written = String::new();
        let outcome = self
            .client
            .stream(settings, &request, |piece| {
                // A model that writes on is cut: what it wrote is dropped as too long anyway.
                if written.len() <= MAX_SUGGESTION * 4 {
                    written.push_str(piece);
                }
            })
            .await;
        match outcome {
            Ok(Outcome {
                stop: StopReason::Complete,
                ..
            }) => suggestion(&written),
            _ => None,
        }
    }
}

/// The end of the conversation `messages`, as the model reads it for the suggestion; `None` for an empty one.
fn suggestion_request(messages: &[Message]) -> Option<String> {
    let start = context::recent_start(messages, SUGGESTION_ROOM);
    let parts = context::transcript_parts(&messages[start..], SUGGESTION_ROOM);
    let last = parts.last()?;
    Some(format!("<conversation>\n{last}</conversation>"))
}

/// The suggestion in what the model `written`: one line, without the quotes a model may add, unless it is empty,
/// [`NOTHING`] or too long to be a message typed in a box.
fn suggestion(written: &str) -> Option<String> {
    let text = written.trim();
    let text = text
        .strip_prefix(['"', '“', '\''])
        .and_then(|inner| inner.strip_suffix(['"', '”', '\'']))
        .unwrap_or(text)
        .trim();
    let useful = !text.is_empty()
        && !text.contains('\n')
        && text.chars().count() <= MAX_SUGGESTION
        && !text.trim_end_matches('.').eq_ignore_ascii_case(NOTHING);
    useful.then(|| text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::context::tests::message;
    use kelpy_storage::Role;

    #[test]
    fn a_suggestion_is_one_short_line_without_quotes() {
        assert_eq!(
            suggestion("  \"Find out what stopped php8.3-fpm\"\n").as_deref(),
            Some("Find out what stopped php8.3-fpm")
        );
        assert_eq!(
            suggestion("“Controlla i log di nginx”").as_deref(),
            Some("Controlla i log di nginx")
        );
        assert_eq!(suggestion("NONE"), None);
        assert_eq!(suggestion("none."), None);
        assert_eq!(suggestion("   "), None);
        assert_eq!(suggestion("First line\nSecond line"), None);
        assert_eq!(suggestion(&"a".repeat(MAX_SUGGESTION + 1)), None);
    }

    #[test]
    fn the_request_holds_the_end_of_the_conversation() {
        assert_eq!(suggestion_request(&[]), None);
        let mut messages = vec![message(1, Role::User, &"old ".repeat(20_000))];
        messages.push(message(2, Role::Assistant, "Long gone"));
        messages.push(message(3, Role::User, "The shop returns 502"));
        messages.push(message(
            4,
            Role::Assistant,
            "php8.3-fpm was down: started again.",
        ));
        let request = suggestion_request(&messages).unwrap();
        assert!(request.starts_with("<conversation>\n") && request.ends_with("</conversation>"));
        assert!(request.contains("User:\nThe shop returns 502"));
        assert!(request.contains("Kelpy:\nphp8.3-fpm was down"));
        assert!(context::estimate(&request) <= SUGGESTION_ROOM + 100);
    }
}
