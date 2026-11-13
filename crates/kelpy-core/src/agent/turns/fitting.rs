//! Fitting a conversation to the model's context window: learning the window and whether the model can call tools,
//! and summarising the older part of a conversation that no longer fits (see [`context`] for the estimates).

use std::time::Duration;

use kelpy_common::lock;
use kelpy_providers::{ChatMessage, ChatRequest, Kind, Outcome, Settings, StopReason};
use kelpy_storage::{Message, Summary};

use super::Turns;
use crate::agent::context::{self, Budget};

/// How long a service may take to tell a model's context window before one is assumed.
const WINDOW_TIME: Duration = Duration::from_secs(5);
/// The smallest context window taken as true: a smaller one is a mistake of the service.
const MIN_WINDOW: u64 = 1_024;
/// How long writing each part of a summary may take.
const SUMMARY_TIME: Duration = Duration::from_secs(180);

impl Turns {
    /// Whether the model `settings` name can call tools: as its service says, else as learned from a refusal. A model
    /// whose service does not say is offered tools.
    pub(super) async fn supports_tools(&self, settings: &Settings) -> bool {
        let key = window_key(settings);
        if let Some(&known) = lock(&self.tool_support).get(&key) {
            return known;
        }
        match tokio::time::timeout(WINDOW_TIME, self.client.supports_tools(settings)).await {
            Ok(Ok(said)) => {
                let supported = said.unwrap_or(true);
                lock(&self.tool_support).insert(key, supported);
                supported
            }
            // Asked again with the next answer.
            _ => true,
        }
    }

    /// Whether the model `settings` name can see images: as its service says, else assumed, until a service
    /// refuses them. Remembered while Kelpy runs once the service says.
    pub(super) async fn sees_images(&self, settings: &Settings) -> bool {
        let key = window_key(settings);
        if let Some(&known) = lock(&self.vision).get(&key) {
            return known;
        }
        match tokio::time::timeout(WINDOW_TIME, self.client.sees_images(settings)).await {
            Ok(Ok(Some(said))) => {
                lock(&self.vision).insert(key, said);
                said
            }
            // Unknown: shown, and remembered as not seen if the service refuses them.
            Ok(Ok(None)) => true,
            // Asked again with the next answer; meanwhile the text read from the images stands in for them.
            _ => false,
        }
    }

    /// The context window of the model `settings` name: as its service declares it, else assumed. A declared window
    /// is remembered while Kelpy runs; Ollama is asked again until it tells, which it does once the model is loaded.
    pub(in crate::agent) async fn window(&self, settings: &Settings) -> u64 {
        let key = window_key(settings);
        if let Some(&window) = lock(&self.windows).get(&key) {
            return window;
        }
        let assumed = context::assumed_window(settings.kind, &settings.model);
        let declared =
            tokio::time::timeout(WINDOW_TIME, self.client.context_window(settings)).await;
        let window = match declared {
            Ok(Ok(Some(window))) if window >= MIN_WINDOW => window,
            // The service answered without a window, and will not tell later either.
            Ok(Ok(_)) if settings.kind != Kind::Ollama => assumed,
            _ => return assumed,
        };
        lock(&self.windows).insert(key, window);
        window
    }

    /// What the model reads for this answer: the whole conversation if it fits `budget`, else the summary of its
    /// older part, written now if needed, and the recent turns. `summary`, the conversation's summary, is replaced
    /// by the one written, which is also saved. If no summary can be written, the oldest whole turns are left out.
    /// `reserved` tokens of the request go to something else, such as the tools offered.
    pub(super) async fn fit<'h>(
        &self,
        settings: &Settings,
        instructions: &str,
        reserved: u64,
        summary: &mut Option<Summary>,
        history: &'h [Message],
        budget: Budget,
    ) -> Fitted<'h> {
        let start = summary.as_ref().map_or(0, |summary| {
            history.partition_point(|message| message.id <= summary.through)
        });
        let unsummarised = &history[start..];
        let whole = Fitted::new(
            instructions,
            reserved,
            summary.as_ref(),
            unsummarised,
            false,
        );
        if whole.estimate <= budget.limit() {
            return whole;
        }
        let recent = context::recent_start(unsummarised, budget.recent());
        if recent > 0 {
            let older = &unsummarised[..recent];
            let previous = summary.as_ref().map(|summary| summary.content.as_str());
            if let Some(content) = self.summarise(settings, previous, older, budget).await {
                let written = Summary {
                    through: older[older.len() - 1].id,
                    content,
                };
                let (session_id, saved, model) =
                    (older[0].session_id, written.clone(), settings.model.clone());
                // A summary that cannot be saved still serves this answer; the next one writes it again.
                let _ = self
                    .store
                    .call(move |store| store.save_summary(session_id, &saved, &model))
                    .await;
                *summary = Some(written);
                return Fitted::new(
                    instructions,
                    reserved,
                    summary.as_ref(),
                    &unsummarised[recent..],
                    false,
                );
            }
        }
        let note = Fitted::new(instructions, reserved, summary.as_ref(), &[], true);
        let start =
            context::recent_start(unsummarised, budget.limit().saturating_sub(note.estimate));
        Fitted::new(
            instructions,
            reserved,
            summary.as_ref(),
            &unsummarised[start..],
            start > 0,
        )
    }

    /// Folds `older` into the `previous` summary with the model, a part at a time so that each request fits
    /// `budget`. `None` if the model does not write it.
    async fn summarise(
        &self,
        settings: &Settings,
        previous: Option<&str>,
        older: &[Message],
        budget: Budget,
    ) -> Option<String> {
        let length = budget.summary();
        // About three words for every four tokens.
        let instructions =
            context::SUMMARY_INSTRUCTIONS.replace("{words}", &(length * 3 / 4).to_string());
        // Each request holds the instructions, the summary so far and a part, and leaves room for the new summary.
        let room = budget
            .limit()
            .saturating_sub(context::estimate(&instructions) + 2 * length)
            .max(length);
        // Bytes: twice the length asked, for a model that writes on; beyond it the summary is cut.
        let most = usize::try_from(length * 8).unwrap_or(usize::MAX);
        let mut summary = previous.map(str::to_owned);
        for part in context::transcript_parts(older, room) {
            let messages = [ChatMessage::user(context::summary_request(
                summary.as_deref(),
                &part,
            ))];
            let request = ChatRequest {
                system: &instructions,
                messages: &messages,
                tools: &[],
            };
            let mut written = String::new();
            let streamed = tokio::time::timeout(
                SUMMARY_TIME,
                self.client.stream(settings, &request, |piece| {
                    if written.len() < most {
                        written.push_str(piece);
                    }
                }),
            )
            .await;
            match streamed {
                Ok(Ok(Outcome {
                    stop: StopReason::Complete | StopReason::TooLong,
                    ..
                })) if !written.trim().is_empty() => summary = Some(written.trim().to_owned()),
                _ => return None,
            }
        }
        summary
    }
}

/// What the model reads for one answer.
pub(super) struct Fitted<'h> {
    /// The instructions, then the summary of the older messages and a note if some were left out.
    pub(super) system: String,
    pub(super) messages: &'h [Message],
    /// Its tokens, as estimated: uncorrected, so that the provider's count can correct the next estimates.
    pub(super) estimate: u64,
}

impl<'h> Fitted<'h> {
    /// `reserved` tokens go to something besides the system prompt and the messages.
    fn new(
        instructions: &str,
        reserved: u64,
        summary: Option<&Summary>,
        messages: &'h [Message],
        left_out: bool,
    ) -> Self {
        let mut system = instructions.to_owned();
        if let Some(summary) = summary {
            system.push_str(
                "\n\n## Earlier in this conversation\n\nThe earlier messages are summarised here, and no longer \
                 shown:\n\n",
            );
            system.push_str(&summary.content);
        }
        if left_out {
            system.push_str(
                "\n\nSome earlier messages of this conversation were left out to fit what you can read: ask if \
                 something you need is missing.",
            );
        }
        let estimate = reserved + context::estimate(&system) + context::estimate_messages(messages);
        Self {
            system,
            messages,
            estimate,
        }
    }
}

/// Where a model's context window is remembered: by kind of service, address and model, as the same model may run
/// with another window elsewhere.
pub(super) type WindowKey = (&'static str, Option<String>, String);

pub(super) fn window_key(settings: &Settings) -> WindowKey {
    (
        settings.kind.id(),
        settings.base_url.clone(),
        settings.model.clone(),
    )
}
