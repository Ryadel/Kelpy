//! What of a conversation the model reads: all of it while it fits the model's context window, less a reserve for
//! the answer; past that, a summary of the older part and the recent turns as they are.
//!
//! Tokens are estimated without a tokenizer, and the estimate is corrected by what the provider counted for an
//! earlier answer of the same model. The summary is a checkpoint for an administrator, written by the model and
//! updated, not rewritten, as the conversation grows. The conversation itself always stays stored whole.

use kelpy_providers::Kind;
use kelpy_storage::{Action, ActionState, Message, Role};
use serde::Serialize;

/// The most kept for the answer.
const RESERVE: u64 = 20_000;
/// The most of the conversation kept as it is when the older part is summarised.
const RECENT: u64 = 20_000;
/// The most a summary may take.
const SUMMARY: u64 = 4_000;
/// What a message adds to its text: its role and separators.
const PER_MESSAGE: u64 = 4;
/// The most the provider's count may exceed the estimate by: past it, the count is taken for a mistake.
const MAX_CORRECTION: f64 = 4.0;

/// How much of the model's context window the latest request of a conversation took, in tokens: as the provider
/// counted it, else as estimated and corrected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ContextUse {
    pub used: u64,
    pub window: u64,
}

/// How much of a conversation fits one request to a model. Its sizes are estimated tokens, scaled by the
/// correction: they compare directly with [`estimate`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Budget {
    /// The model's context window, in tokens.
    pub(super) window: u64,
    /// How many tokens the provider counts for each estimated one, at least 1.
    correction: f64,
}

impl Budget {
    pub(super) fn new(window: u64, correction: f64) -> Self {
        Self { window, correction }
    }

    /// The most a request may take: the window, less the reserve for the answer (at most a quarter of it).
    pub(super) fn limit(self) -> u64 {
        self.scaled(self.window - RESERVE.min(self.window / 4))
    }

    /// The recent part kept as it is when the older part is summarised.
    pub(super) fn recent(self) -> u64 {
        self.scaled(RECENT).min(self.limit() / 2)
    }

    /// The most a summary may take.
    pub(super) fn summary(self) -> u64 {
        self.scaled(SUMMARY).min(self.limit() / 4)
    }

    /// The budget once the provider has refused a request as too long for the model: two thirds of the window.
    pub(super) fn shrunk(self) -> Self {
        Self::new(self.window * 2 / 3, self.correction)
    }

    /// What an `estimate` comes to as the provider counts.
    pub(super) fn counted(self, estimate: u64) -> u64 {
        (estimate as f64 * self.correction) as u64
    }

    fn scaled(self, tokens: u64) -> u64 {
        (tokens as f64 / self.correction) as u64
    }
}

/// How many tokens the provider counts for each estimated one, from the latest answer of `model` in `history` that
/// has both counts. Only an estimate found too low is corrected: a cautious one is kept.
pub(super) fn correction(history: &[Message], model: &str) -> f64 {
    history
        .iter()
        .rev()
        .filter(|message| message.model.as_deref() == Some(model))
        .find_map(|message| Some((message.context_estimate?, message.input_tokens?)))
        .filter(|&(estimated, _)| estimated > 0)
        .map_or(1.0, |(estimated, counted)| {
            (counted as f64 / estimated as f64).clamp(1.0, MAX_CORRECTION)
        })
}

/// What the latest answer of `model` in `history` took of a `window`, if Kelpy kept its estimate.
pub(super) fn latest_use(history: &[Message], model: &str, window: u64) -> Option<ContextUse> {
    let answer = history
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant && message.context_estimate.is_some())
        .filter(|answer| answer.model.as_deref() == Some(model))?;
    let estimate = answer.context_estimate?;
    let used = answer
        .input_tokens
        .unwrap_or_else(|| Budget::new(window, correction(history, model)).counted(estimate));
    Some(ContextUse { used, window })
}

/// The context window assumed when the service does not tell: a known one for the model, else a cautious one for
/// the kind of service. Ollama runs models with a small window unless told otherwise.
pub(super) fn assumed_window(kind: Kind, model: &str) -> u64 {
    const KNOWN: &[(&str, u64)] = &[
        ("claude-", 200_000),
        ("gpt-4.1", 1_000_000),
        ("gpt-4o", 128_000),
        ("gpt-5", 272_000),
        ("o3", 200_000),
        ("o4", 200_000),
    ];
    // Services that host many companies' models name them `company/model`.
    let name = model.rsplit('/').next().unwrap_or(model);
    KNOWN
        .iter()
        .find(|(prefix, _)| name.starts_with(prefix))
        .map_or(
            match kind {
                Kind::Anthropic => 200_000,
                Kind::OpenAi | Kind::ChatGpt => 128_000,
                Kind::OpenAiCompatible => 32_000,
                Kind::Ollama => 4_096,
            },
            |&(_, window)| window,
        )
}

/// The tokens `text` takes, estimated cautiously: a token for every four ASCII characters, and one for each other
/// character, as accented letters, other scripts and symbols take one or more.
pub(super) fn estimate(text: &str) -> u64 {
    quarters(text).div_ceil(4)
}

/// The tokens `messages` take.
pub(super) fn estimate_messages(messages: &[Message]) -> u64 {
    messages.iter().map(message_tokens).sum()
}

/// A message with its actions: each call and its result, as the model reads them.
fn message_tokens(message: &Message) -> u64 {
    let actions: u64 = message
        .actions
        .iter()
        .map(|action| estimate(&action.input) + estimate(&action_result(action)) + 2 * PER_MESSAGE)
        .sum();
    estimate(&message.content) + PER_MESSAGE + actions
}

/// What the model reads of an action's output, at most: the Command log keeps it all.
const ACTION_OUTPUT: u64 = 4_000;

/// What the model is told an action gave: the exit code and output of commands, the content read, or why it did not
/// run. A long output keeps its beginning and its end.
pub(super) fn action_result(action: &Action) -> String {
    match action.state {
        ActionState::Done => {
            let output = if action.output.trim().is_empty() {
                "(no output)"
            } else {
                action.output.as_str()
            };
            let output = shortened(output, ACTION_OUTPUT);
            match action.exit_code {
                Some(code) => format!("Exit code {code}.\n{output}"),
                None => output,
            }
        }
        ActionState::Denied => match &action.error {
            Some(reason) => format!("It did not run: {reason}."),
            None => "The administrator denied this action.".to_owned(),
        },
        ActionState::Failed => {
            let problem = action.error.as_deref().unwrap_or("unknown error");
            if action.output.trim().is_empty() {
                format!("It did not complete: {problem}.")
            } else {
                let output = shortened(&action.output, ACTION_OUTPUT);
                format!("It did not complete: {problem}. Its output until then:\n{output}")
            }
        }
        ActionState::Uncertain => format!(
            "Its outcome is uncertain: {}. Look at the system to learn what it did before you do it again.",
            action
                .error
                .as_deref()
                .unwrap_or("Kelpy stopped while it was changing the system")
        ),
        ActionState::Waiting | ActionState::Running => {
            "It did not run: Kelpy stopped first.".to_owned()
        }
    }
}

/// An operation the administrator asked for, as the model is told of it.
pub(super) fn administrator_note(action: &Action) -> String {
    let outcome = match action.state {
        ActionState::Done => "done".to_owned(),
        ActionState::Uncertain => "outcome uncertain".to_owned(),
        _ => format!(
            "it did not complete: {}",
            action.error.as_deref().unwrap_or("unknown error")
        ),
    };
    format!("{} ({outcome})", action.summary)
}

/// The cost of `text` in quarters of a token.
fn quarters(text: &str) -> u64 {
    text.chars().map(char_quarters).sum()
}

fn char_quarters(c: char) -> u64 {
    if c.is_ascii() { 1 } else { 4 }
}

/// Where the recent part of `messages` starts: the latest whole turns that fit in `limit` tokens, and at least the
/// last turn. A turn starts with the user's message.
pub(super) fn recent_start(messages: &[Message], limit: u64) -> usize {
    let mut size = 0;
    let mut start = messages.len();
    while start > 0 && size + message_tokens(&messages[start - 1]) <= limit {
        start -= 1;
        size += message_tokens(&messages[start]);
    }
    let turn = messages[start..]
        .iter()
        .position(|message| message.role == Role::User)
        .map(|offset| start + offset);
    turn.or_else(|| {
        messages
            .iter()
            .rposition(|message| message.role == Role::User)
    })
    .unwrap_or(0)
}

/// How a summary is written. `{words}` is its length.
pub(super) const SUMMARY_INSTRUCTIONS: &str = "You keep the record of a long conversation between a system \
administrator and Kelpy, an assistant that helps manage a server. The older part of the conversation no longer fits \
what the model can read: your summary takes its place, and the recent messages follow it as they are.

Write a checkpoint that lets the work go on without losing anything that matters, in the conversation's language, \
under these headings:

## Server
The server and its context: system, services, versions, configuration.
## Asked
What the administrator asked for, and why.
## Done
What was done, and what was checked and how.
## Decisions
What was decided, with the reasons.
## Open
What is still pending or unresolved, and the next steps.

Keep exact identifiers as they are: paths, commands, package and service names, versions, ports, host names, error \
messages. Leave out greetings and anything settled that no longer matters. Never write passwords, keys, tokens or \
other secrets: write [secret] in their place. When a previous summary is given, update it with the new messages: \
keep what still holds, correct what changed, and do not drop a fact only because it is old. Write at most {words} \
words. Answer with the summary only.";

/// The request to fold `transcript`, a part of the conversation, into the `previous` summary.
pub(super) fn summary_request(previous: Option<&str>, transcript: &str) -> String {
    let mut request = String::new();
    if let Some(previous) = previous {
        request.push_str("<previous-summary>\n");
        request.push_str(previous);
        request.push_str("\n</previous-summary>\n\n");
    }
    request.push_str("<conversation>\n");
    request.push_str(transcript);
    request.push_str("</conversation>");
    request
}

/// `messages` written out for the summary, in parts of at most `room` tokens each. A message too long for a part
/// keeps its beginning and its end; an answer's actions follow it, each with the start of what it gave.
pub(super) fn transcript_parts(messages: &[Message], room: u64) -> Vec<String> {
    let mut parts = Vec::new();
    let mut part = String::new();
    let mut size = 0;
    for message in messages
        .iter()
        .filter(|message| !message.content.is_empty() || !message.actions.is_empty())
    {
        let speaker = match message.role {
            Role::User => "User",
            Role::Assistant => "Kelpy",
        };
        let content = shortened(&message.content, room.saturating_sub(PER_MESSAGE));
        let mut entry = format!("{speaker}:\n{content}\n");
        for action in &message.actions {
            let result = shortened(&action_result(action), ACTION_SUMMARY);
            let what = action.command.as_deref().unwrap_or(&action.summary);
            entry.push_str(&format!("[Kelpy's action: {what}]\n{result}\n"));
        }
        entry.push('\n');
        let tokens = estimate(&entry);
        if size + tokens > room && !part.is_empty() {
            parts.push(std::mem::take(&mut part));
            size = 0;
        }
        part.push_str(&entry);
        size += tokens;
    }
    if !part.is_empty() {
        parts.push(part);
    }
    parts
}

/// What a summary is given of each action's result.
const ACTION_SUMMARY: u64 = 150;

/// `text` within `tokens`: whole if it fits, else its beginning and its end with a note of what was left out.
fn shortened(text: &str, tokens: u64) -> String {
    if estimate(text) <= tokens {
        return text.to_owned();
    }
    const NOTE: &str = "\n[… {} characters left out …]\n";
    // Half of what is left after the note for each end, in quarters of a token. Together they take less than the
    // whole text, which does not fit.
    let keep = tokens.saturating_sub(estimate(NOTE) + 4) * 4 / 2;
    let (head, tail) = (
        take_quarters(text.chars(), keep),
        take_quarters(text.chars().rev(), keep),
    );
    let total = text.chars().count();
    let note = NOTE.replace("{}", &(total - head - tail).to_string());
    let head: String = text.chars().take(head).collect();
    let tail: String = text.chars().skip(total - tail).collect();
    format!("{head}{note}{tail}")
}

/// How many of `chars` fit in `quarters` quarters of a token.
fn take_quarters(chars: impl Iterator<Item = char>, quarters: u64) -> usize {
    let mut used = 0;
    chars
        .take_while(|&c| {
            used += char_quarters(c);
            used <= quarters
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kelpy_storage::Status;

    pub(crate) fn message(id: i64, role: Role, content: &str) -> Message {
        Message {
            id,
            session_id: 1,
            role,
            content: content.to_owned(),
            status: Status::Complete,
            model: (role == Role::Assistant).then(|| "m".to_owned()),
            created_at: 0,
            context_estimate: None,
            input_tokens: None,
            actions: Vec::new(),
        }
    }

    #[test]
    fn the_estimate_is_cautious_beyond_plain_ascii() {
        assert_eq!(estimate(""), 0);
        assert_eq!(estimate("abcd"), 1);
        assert_eq!(estimate("abcde"), 2);
        assert_eq!(estimate("perché"), 3);
        assert_eq!(estimate("日本語"), 3);
    }

    #[test]
    fn the_budget_keeps_a_reserve_and_room_for_the_recent_turns_and_the_summary() {
        let large = Budget::new(200_000, 1.0);
        assert_eq!(large.limit(), 180_000);
        assert_eq!((large.recent(), large.summary()), (20_000, 4_000));
        let small = Budget::new(4_096, 1.0);
        assert_eq!(small.limit(), 3_072);
        assert_eq!((small.recent(), small.summary()), (1_536, 768));
        // A provider that counts twice the estimate halves what fits.
        assert_eq!(Budget::new(200_000, 2.0).limit(), 90_000);
        assert_eq!(large.shrunk().window, 133_333);
    }

    #[test]
    fn the_use_shown_is_the_latest_answers_count_or_its_estimate_corrected() {
        let answer = |estimated, counted| Message {
            context_estimate: Some(estimated),
            input_tokens: counted,
            ..message(0, Role::Assistant, "")
        };
        let history = [answer(1_000, Some(1_500)), message(0, Role::User, "")];
        assert_eq!(
            latest_use(&history, "m", 8_000),
            Some(ContextUse {
                used: 1_500,
                window: 8_000
            })
        );
        // Not counted: the estimate, corrected by the earlier count.
        let history = [answer(1_000, Some(1_500)), answer(2_000, None)];
        assert_eq!(latest_use(&history, "m", 8_000).unwrap().used, 3_000);
        // Written by another model, or before Kelpy kept estimates: nothing to show.
        assert_eq!(latest_use(&history, "other", 8_000), None);
        assert_eq!(
            latest_use(&[message(0, Role::Assistant, "")], "m", 8_000),
            None
        );
    }

    #[test]
    fn the_estimate_is_corrected_by_the_last_count_of_the_same_model() {
        let counted = |model: &str, estimated, counted| Message {
            model: Some(model.to_owned()),
            context_estimate: Some(estimated),
            input_tokens: Some(counted),
            ..message(0, Role::Assistant, "")
        };
        let history = [
            counted("m", 1_000, 1_500),
            counted("other", 1_000, 3_000),
            message(0, Role::User, "x"),
        ];
        assert_eq!(correction(&history, "m"), 1.5);
        assert_eq!(correction(&history, "other"), 3.0);
        assert_eq!(correction(&history, "new"), 1.0);
        // A cautious estimate stays, and an absurd count is capped.
        assert_eq!(correction(&[counted("m", 1_000, 500)], "m"), 1.0);
        assert_eq!(correction(&[counted("m", 10, 1_000)], "m"), MAX_CORRECTION);
    }

    #[test]
    fn windows_are_assumed_by_model_then_by_kind() {
        assert_eq!(assumed_window(Kind::Anthropic, "claude-opus-5-5"), 200_000);
        assert_eq!(
            assumed_window(Kind::OpenAiCompatible, "openai/gpt-5.2"),
            272_000
        );
        assert_eq!(assumed_window(Kind::OpenAiCompatible, "llama-4"), 32_000);
        assert_eq!(assumed_window(Kind::Ollama, "qwen3:8b"), 4_096);
    }

    #[test]
    fn the_recent_part_is_made_of_whole_turns_and_holds_at_least_the_last() {
        let user = |id, text: &str| message(id, Role::User, text);
        let kelpy = |id, text: &str| message(id, Role::Assistant, text);
        let history = [
            user(1, &"a".repeat(400)),
            kelpy(2, &"b".repeat(400)),
            user(3, &"c".repeat(40)),
            kelpy(4, &"d".repeat(40)),
            user(5, "next"),
        ];
        // 104 + 104 + 14 + 14 + 5: the last three messages fit in 40 tokens, starting with a user's.
        assert_eq!(recent_start(&history, 40), 2);
        // Room for the second answer but not its question: the turn is left out whole.
        assert_eq!(recent_start(&history, 30), 4);
        assert_eq!(recent_start(&history, 1_000), 0);
        assert_eq!(recent_start(&history, 0), 4);
    }

    #[test]
    fn transcripts_come_in_parts_that_fit_and_long_messages_keep_both_ends() {
        let history = [
            message(1, Role::User, "Install nginx"),
            message(2, Role::Assistant, ""),
            message(
                3,
                Role::Assistant,
                &format!("start{}end", "x".repeat(4_000)),
            ),
            message(4, Role::User, "Thanks"),
        ];
        let parts = transcript_parts(&history, 200);
        assert!(parts.iter().all(|part| estimate(part) <= 200), "{parts:?}");
        assert!(parts[0].starts_with("User:\nInstall nginx\n\n"));
        let long = parts.iter().find(|part| part.contains("left out")).unwrap();
        assert!(long.contains("Kelpy:\nstart") && long.contains("end\n\n"));
        assert!(parts.last().unwrap().ends_with("User:\nThanks\n\n"));
        assert!(!parts.concat().contains("Kelpy:\n\n"));
    }

    #[test]
    fn a_summary_request_carries_the_previous_summary() {
        let request = summary_request(Some("## Server\nDebian 13"), "User:\nHi\n\n");
        assert!(
            request.starts_with("<previous-summary>\n## Server\nDebian 13\n</previous-summary>")
        );
        assert!(request.ends_with("<conversation>\nUser:\nHi\n\n</conversation>"));
        assert!(!summary_request(None, "").contains("previous"));
    }
}
