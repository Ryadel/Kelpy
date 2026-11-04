//! Turn execution: storing the user's message, writing the answer in the background, and the answers under way.
//!
//! Each answer is registered under its conversation with a stop signal and a done signal, so deleting a conversation
//! and stopping Kelpy can wait for exactly the answers they end. No new answer can start in a conversation being
//! deleted, nor anywhere once [`Turns::stop_all`] is called.
//!
//! An answer is written in steps: the model writes, may call tools to look at the system, reads what they gave and
//! goes on, until it answers without calling any (see [`actions`]). Before each step the conversation is fitted to
//! the model's context window (see [`context`]): when it is too long, its older part is summarised first.

mod actions;
mod fitting;
mod interface;

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kelpy_common::lock;
use kelpy_providers::{ChatRequest, Outcome, Provider, Settings, StopReason, ToolSpec};
use kelpy_storage::{
    Access, Action, Approval, Message, Modes, PartialSave, Session, Status, Storage, Summary,
};
use kelpy_tools::{Account, Executor, ToolKind, Tools};
use serde::Serialize;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::AgentError;
use super::context::{self, Budget};
use super::server::Server;
use crate::events::{Event, Events};
use actions::{Question, chat_messages};
use fitting::{WindowKey, window_key};

pub(super) const TITLE_LENGTH: usize = 60;
/// Partial answers are saved this often, so little is lost if the process dies.
const SAVE_EVERY: Duration = Duration::from_secs(2);
/// The most a message may hold.
pub(super) const MAX_MESSAGE: usize = 256 << 10;
/// The most an answer may hold: far more than models write, so only a runaway stream reaches it.
pub(super) const MAX_ANSWER: usize = 2 << 20;
/// How many times a request the provider refuses as too long for the model is sent again, shorter.
const OVERFLOW_RETRIES: usize = 3;
/// The most of `memory/MEMORY.md` given to the model.
pub(super) const MAX_MEMORY_INDEX: usize = 64 << 10;
/// How many steps with tool calls one answer may take; the next one answers with what was found.
pub(super) const MAX_STEPS: u32 = 20;

const INSTRUCTIONS: &str =
    "You are Kelpy, an AI assistant for system administrators. You run on the server you \
help manage ({os}, {arch}), started by the administrator for this session.

{access}

Answer in the user's language. Use Markdown, with code blocks for commands and files.";

/// What the model is told about acting on the system with System access Read-only.
pub(super) const LOOKING: &str = "You can look at this system with the tools provided, and the administrator may be \
asked to approve each one after reading its `reason`. Prefer the built-in tools, which only read. Use `run_command` \
only when none of them gives what you need, and only for commands that change nothing: System access is Read-only. \
Never use a command to read files that may hold secrets. Look before you answer a question about this system rather \
than guessing, and only as much as the question needs. You cannot change anything: when something must be done, give \
the exact commands or file changes for the administrator to run, in order, and point out anything risky or \
irreversible before it. Prefer the distribution's own packages and tools. If the administrator denies an action, do \
not ask for it again unless they say so: go on with what you know. If something is unclear, ask.";

/// What the model is told about acting on the system with System access Full.
pub(super) const CHANGING: &str = "You can look at this system and change it with the tools provided. Prefer the \
built-in tools to look, as they only read. Make changes with the typed tools: `edit_file` and `write_file` for files, \
`manage_service` for services, `install_packages` and `remove_packages` for packages, `create_database` for MariaDB \
or MySQL. Kelpy shows each change before it is approved, backs files up, checks the result and puts back what it can \
when the check fails, and does nothing when the system is already as asked: do not back files up yourself, nor \
repeat the checks it reports. Where a password or a key goes, create it with `generate_secret` and write \
{{secret:name}} there: you never see its value, so never ask for it nor write one yourself. `run_command` runs the \
commands you write, to look further or for changes no other tool makes; the administrator approves each one after \
reading it and its `reason`. Never use a command to read files that may hold secrets. Look before you answer or \
change anything rather than guessing. Before a change, say what will change, what could go wrong and how to undo \
it. Keep each command to one change, so that each is approved on its own, and check afterwards that it worked. Say \
plainly before asking for anything risky or irreversible: deleting data, firewall, SSH or network changes that \
could lock the administrator out, reboots and outages. An action whose outcome is uncertain may have been done in \
part: look before doing it again. Prefer the distribution's own packages and tools. If the administrator denies an \
action, do not ask for it again unless they say so: go on with what you know. If something is unclear, ask.";

/// How the model uses Kelpy's memory, after [`LOOKING`] or [`CHANGING`].
pub(super) const MEMORY_TOOLS: &str = "Kelpy's memory of this server is in memory/: read a page with `read_memory` \
(`server` lists what Kelpy changed here and what was changed outside it), and note lasting facts with `remember`, \
one a call, never a secret.";

/// What the model is told besides [`CHANGING`] in the Approval mode Plan.
pub(super) const PLANNING: &str = "The Approval mode is Plan: when a task needs changes, look first at what you \
need, then propose them all with `propose_plan`, each step as the exact call you will make. Once the administrator \
approves the plan, make the calls with the same arguments: they run without asking again. A change outside the plan \
asks on its own. Keep to the plan; when it must change, say why and propose a new one.";

/// What the model is told when it cannot look at the system, after saying why.
pub(super) const NOT_LOOKING: &str = "When something must be done or checked on the server, give the exact commands \
or file changes for the administrator to run, in order, and point out anything risky or irreversible before it. \
Prefer the distribution's own packages and tools. If something is unclear, ask.";

/// Whether the model may look at the system in a step, and if not, why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Looking {
    Yes,
    /// The conversation's System access is None.
    NotAllowed,
    /// The model cannot call tools.
    Unsupported,
    /// The answer took all the steps it may.
    Exhausted,
}

/// How answers are written when the conversation's explanations are off, the default.
pub(super) const CONCISE: &str =
    "Be precise and concise: say in one short line what each step does, nothing more.";

/// How answers are written when the conversation's explanations are on: the answer stays as concise as without them,
/// and each step's explanation goes in a block that the Web UI shows apart: a tab of the command's code block it
/// follows, or a callout.
pub(super) const EXPLAINED: &str = "Keep the answer itself precise and concise, as for a specialist. After each step, \
and after any concept a non-specialist may not know, add its explanation in a fenced block with the language \
`explanation`, shown to the administrator apart from the answer. For a step with a command, put it right after the \
command's code block, with nothing between them: it becomes a tab of that block.

```explanation
What the step does and why, in plain words; what to expect to see; how to check that it worked.
```

Write the explanations in Markdown, without code blocks inside them, and define each technical term the first time \
you use it. Keep each one short and about its step. Stay focused on the task.";

/// What an interface receives while an answer is written.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatEvent {
    /// The user's message, as stored.
    User { message: Message },
    /// The answer as written so far: empty when it has just started, partial when following an answer under way.
    Started { message: Message },
    /// More text of the answer.
    Delta { text: String },
    /// An action of the answer, new or changed: waiting for approval, running, ended.
    Action { action: Box<Action> },
    /// The answer is over, as stored, with the reason if it did not complete.
    Done {
        message: Message,
        error: Option<String>,
        /// The last message the conversation's summary covers, if its older part is summarised for the model.
        summarised_through: Option<i64>,
    },
}

/// The current state of an answer, shared by everyone following it.
#[derive(Debug, Clone)]
struct AnswerState {
    message: Message,
    /// Why the answer ended early, once it has.
    error: Option<String>,
    /// The last message the conversation's summary covers.
    summarised_through: Option<i64>,
}

/// The events of one answer for one interface, from where the answer is when following starts to its end.
///
/// Followers read the shared state of the answer instead of a queue of their own, so memory stays bounded by the
/// answer itself and a slow interface simply receives larger pieces of text.
pub struct AnswerEvents {
    user: Option<Message>,
    answer: watch::Receiver<AnswerState>,
    sent: usize,
    /// The actions as last sent.
    seen: Vec<Action>,
    started: bool,
    done: bool,
}

impl AnswerEvents {
    fn new(user: Option<Message>, answer: watch::Receiver<AnswerState>) -> Self {
        Self {
            user,
            answer,
            sent: 0,
            seen: Vec::new(),
            started: false,
            done: false,
        }
    }

    /// The next event, or `None` after [`ChatEvent::Done`].
    pub async fn next(&mut self) -> Option<ChatEvent> {
        if let Some(message) = self.user.take() {
            return Some(ChatEvent::User { message });
        }
        loop {
            if self.done {
                return None;
            }
            {
                let state = self.answer.borrow_and_update();
                let content = &state.message.content;
                if !self.started {
                    self.started = true;
                    self.sent = content.len();
                    self.seen.clone_from(&state.message.actions);
                    return Some(ChatEvent::Started {
                        message: state.message.clone(),
                    });
                }
                // While streaming the content only grows, by whole strings: `sent` is always a char boundary.
                if content.len() > self.sent {
                    let text = content[self.sent..].to_owned();
                    self.sent = content.len();
                    return Some(ChatEvent::Delta { text });
                }
                // Actions are only added, and change in place.
                let actions = &state.message.actions;
                if let Some(index) =
                    (0..actions.len()).find(|&index| self.seen.get(index) != Some(&actions[index]))
                {
                    let action = actions[index].clone();
                    if index < self.seen.len() {
                        self.seen[index] = action.clone();
                    } else {
                        self.seen.push(action.clone());
                    }
                    return Some(ChatEvent::Action {
                        action: Box::new(action),
                    });
                }
                if state.message.status != Status::Streaming {
                    self.done = true;
                    return Some(ChatEvent::Done {
                        message: state.message.clone(),
                        error: state.error.clone(),
                        summarised_through: state.summarised_through,
                    });
                }
            }
            if self.answer.changed().await.is_err() {
                // The writer is gone without a final state: only a crash of its task can do that.
                self.done = true;
                let state = self.answer.borrow().clone();
                let mut message = state.message;
                message.status = Status::Interrupted;
                return Some(ChatEvent::Done {
                    message,
                    error: Some("the answer stopped unexpectedly".to_owned()),
                    summarised_through: state.summarised_through,
                });
            }
        }
    }
}

/// The turns of every conversation: the answers being written, and where new ones may start.
pub(super) struct Turns {
    store: Storage,
    client: Provider,
    /// Where the answers run.
    tasks: TaskTracker,
    /// Where answers starting and ending are published.
    events: Events,
    pub(super) memory_index: PathBuf,
    /// The tools the model may call, and where they run.
    tools: Tools,
    executor: Executor,
    /// What Kelpy left on the server, and the secrets of each conversation.
    server: Arc<Server>,
    /// Under one lock: starting an answer never interleaves with deleting its conversation or stopping Kelpy.
    pub(super) answers: Mutex<Answers>,
    /// The context windows learned while Kelpy runs.
    windows: Mutex<HashMap<WindowKey, u64>>,
    /// Whether each model can call tools, as learned while Kelpy runs.
    tool_support: Mutex<HashMap<WindowKey, bool>>,
}

#[derive(Default)]
pub(super) struct Answers {
    stopping: bool,
    /// Conversations being deleted, from the start of the deletion to its end.
    pub(super) deleting: HashSet<i64>,
    pub(super) running: HashMap<i64, Running>,
}

/// An answer being written: `stop` asks it to end, `done` fires once it has ended and been saved, `answer` is its
/// current state, once the turn is stored, `questions` are its actions waiting for a decision, and `halts` stop its
/// actions running.
#[derive(Default)]
pub(super) struct Running {
    stop: CancellationToken,
    pub(super) done: CancellationToken,
    answer: Option<watch::Receiver<AnswerState>>,
    questions: HashMap<i64, Question>,
    halts: HashMap<i64, CancellationToken>,
}

impl Running {
    /// Asks the answer to stop, and returns the signal of its end.
    fn ask_to_stop(&self) -> CancellationToken {
        self.stop.cancel();
        self.done.clone()
    }
}

/// An answer registered in its conversation. Dropping it (once the answer has ended, or when its start failed or
/// its task panicked) unregisters the answer and signals its end, so the conversation is never left busy.
struct Registered {
    turns: Arc<Turns>,
    session_id: i64,
    done: CancellationToken,
}

impl Drop for Registered {
    fn drop(&mut self) {
        lock(&self.turns.answers).running.remove(&self.session_id);
        self.done.cancel();
    }
}

/// A conversation being deleted: no answer can start in it until this is dropped, when the deletion has ended or
/// was abandoned (its request cancelled, or the wait for the answer timed out).
pub(super) struct Deletion<'a> {
    turns: &'a Turns,
    session_id: i64,
}

impl Drop for Deletion<'_> {
    fn drop(&mut self) {
        lock(&self.turns.answers).deleting.remove(&self.session_id);
    }
}

impl Turns {
    /// The account actions run as.
    pub(super) fn account(&self) -> &Account {
        self.executor.account()
    }

    /// Turns stored in `store`, answered through `client`, run in `tasks` and published to `events`, with the memory
    /// index at `memory_index`. The model looks at the system through `executor`; what its changes leave is recorded
    /// by `server`.
    pub(super) fn new(
        store: Storage,
        client: Provider,
        tasks: TaskTracker,
        events: Events,
        memory_index: PathBuf,
        executor: Executor,
        server: Arc<Server>,
    ) -> Self {
        Self {
            store,
            client,
            tasks,
            events,
            memory_index,
            tools: Tools::new(executor.os()),
            executor,
            server,
            answers: Mutex::new(Answers::default()),
            windows: Mutex::new(HashMap::new()),
            tool_support: Mutex::new(HashMap::new()),
        }
    }

    pub(super) fn is_running(&self, session_id: i64) -> bool {
        lock(&self.answers).running.contains_key(&session_id)
    }

    pub(super) fn cancel(&self, session_id: i64) -> bool {
        lock(&self.answers)
            .running
            .get(&session_id)
            .map(Running::ask_to_stop)
            .is_some()
    }

    /// Refuses new answers from now on and stops every answer being written. This happens at once, when called; the
    /// returned future then waits, up to `limit`, until the answers are saved, and tells whether all were.
    pub(super) fn stop_all(&self, limit: Duration) -> impl Future<Output = bool> + use<> {
        let ends: Vec<CancellationToken> = {
            let mut answers = lock(&self.answers);
            answers.stopping = true;
            answers.running.values().map(Running::ask_to_stop).collect()
        };
        async move {
            let all_ended = async {
                for end in &ends {
                    end.cancelled().await;
                }
            };
            tokio::time::timeout(limit, all_ended).await.is_ok()
        }
    }

    pub(super) fn follow(&self, session_id: i64) -> Option<AnswerEvents> {
        let answers = lock(&self.answers);
        let answer = answers.running.get(&session_id)?.answer.clone()?;
        Some(AnswerEvents::new(None, answer))
    }

    /// Marks a conversation as being deleted and stops its answer, in one critical section. Returns the mark and the
    /// signal of the answer's end, if one was being written.
    pub(super) fn begin_deletion(
        &self,
        session_id: i64,
    ) -> Result<(Deletion<'_>, Option<CancellationToken>), AgentError> {
        let mut answers = lock(&self.answers);
        if !answers.deleting.insert(session_id) {
            return Err(AgentError::Deleting);
        }
        let answer_end = answers.running.get(&session_id).map(Running::ask_to_stop);
        let deletion = Deletion {
            turns: self,
            session_id,
        };
        Ok((deletion, answer_end))
    }

    /// Stores `text`, a [`message`], and starts its answer with `settings`. `guard` is kept alive until the answer
    /// ends.
    pub(super) async fn start(
        self: &Arc<Self>,
        session_id: i64,
        text: &str,
        settings: Settings,
        guard: impl Send + 'static,
    ) -> Result<AnswerEvents, AgentError> {
        let (stop, registered) = self.register(session_id)?;

        // From here the answer has one owner, a task of its own: a caller that stops waiting cancels nothing, the
        // turn is stored and answered all the same, and the conversation is freed however the task ends.
        let (started, start) = oneshot::channel();
        let turns = Arc::clone(self);
        let text = text.to_owned();
        self.tasks.spawn(async move {
            let _guard = guard;
            let (title, model) = (title_from(&text), settings.model.clone());
            let turn = match turns
                .store
                .call(move |store| store.start_turn(session_id, &text, &title, &model))
                .await
            {
                Ok(turn) => turn,
                Err(error) => {
                    let _ = started.send(Err(error.into()));
                    return;
                }
            };
            let (answer, follower) = watch::channel(AnswerState {
                message: turn.answer,
                error: None,
                summarised_through: turn.summary.as_ref().map(|summary| summary.through),
            });
            if let Some(running) = lock(&turns.answers).running.get_mut(&session_id) {
                running.answer = Some(follower.clone());
            }
            turns.events.publish(Event::SessionChanged {
                session: turn.session.clone(),
            });
            turns.events.publish(Event::AnswerStarted {
                session_id,
                user: turn.user.clone(),
                answer: follower.borrow().message.clone(),
            });
            // The caller may have stopped waiting: the answer can still be followed with `follow`.
            let _ = started.send(Ok(AnswerEvents::new(Some(turn.user), follower)));
            turns
                .write_answer(
                    settings,
                    turn.session,
                    turn.history,
                    turn.administrator,
                    turn.summary,
                    &answer,
                    stop,
                )
                .await;
            // Freed first: an interface told that the answer ended may send the next message at once.
            drop(registered);
            let ended = answer.borrow().clone();
            turns.events.publish(Event::AnswerEnded {
                session_id,
                message: ended.message,
                error: ended.error,
                summarised_through: ended.summarised_through,
            });
        });
        start.await.unwrap_or(Err(AgentError::NotStarted))
    }

    /// Registers a new answer for a conversation, unless Kelpy is stopping, the conversation is being deleted or an
    /// answer is already being written in it.
    fn register(
        self: &Arc<Self>,
        session_id: i64,
    ) -> Result<(CancellationToken, Registered), AgentError> {
        let mut answers = lock(&self.answers);
        if answers.stopping {
            return Err(AgentError::Stopping);
        }
        if answers.deleting.contains(&session_id) {
            return Err(AgentError::Deleting);
        }
        if answers.running.contains_key(&session_id) {
            return Err(AgentError::Busy);
        }
        let running = Running::default();
        let registered = Registered {
            turns: Arc::clone(self),
            session_id,
            done: running.done.clone(),
        };
        let stop = running.stop.clone();
        answers.running.insert(session_id, running);
        Ok((stop, registered))
    }

    /// Writes the answer to the conversation `history`, whose older part `summary` may already summarise, in steps
    /// (see the module's description). Each step follows the conversation's `session` modes as they are then: the
    /// administrator may change them meanwhile. The answer as it is so far joins `history` for each step, in place of
    /// the previous step's copy: the rest is never copied again. The operations the `administrator` asked for are
    /// told to the model with the messages that follow them.
    #[allow(clippy::too_many_arguments)]
    async fn write_answer(
        &self,
        settings: Settings,
        mut session: Session,
        mut history: Vec<Message>,
        administrator: Vec<Action>,
        mut summary: Option<Summary>,
        answer: &watch::Sender<AnswerState>,
        stop: CancellationToken,
    ) {
        let id = answer.borrow().message.id;
        let text = || answer.borrow().message.content.clone();

        let partial = PartialSave::new(&self.store, id);
        let mut saved_at = Instant::now();
        // Cancelled by the first piece that would take the answer past its limit; that piece is left out.
        let full = CancellationToken::new();
        // Stopped meanwhile, neither matters: the loop below ends at once.
        let (window, mut tools_supported) = tokio::select! {
            known = async { tokio::join!(self.window(&settings), self.supports_tools(&settings)) } => known,
            () = stop.cancelled() => (0, false),
        };
        let mut budget = Budget::new(window, context::correction(&history, &settings.model));
        let asked = history.len();
        let memory = self.memory().await;
        // What changed on the server outside Kelpy, checked again if it was not lately.
        if session.access != Access::None {
            let _ = self.server.check(false).await;
        }
        let differences = if session.access == Access::None {
            None
        } else {
            self.server.notes().await
        };
        // The tools that look with System access Read-only; with Full also those that change, and propose_plan in
        // the Approval mode Plan.
        let specs = |kind: ToolKind| -> Vec<ToolSpec> {
            self.tools
                .list()
                .into_iter()
                .filter(|tool| tool.kind == kind)
                .map(|tool| ToolSpec {
                    name: tool.name.to_owned(),
                    description: tool.description,
                    parameters: tool.parameters,
                })
                .collect()
        };
        let reading = specs(ToolKind::Looks);
        let every = [reading.clone(), specs(ToolKind::Changes)].concat();
        let planning = [every.clone(), specs(ToolKind::Plans)].concat();
        let size = |tools: &[ToolSpec]| {
            tools
                .iter()
                .map(|tool| {
                    context::estimate(&tool.description)
                        + context::estimate(&tool.parameters.to_string())
                })
                .sum::<u64>()
        };
        // What the model wrote besides text with the calls of each step, to give back with them.
        let mut replays: HashMap<u32, Vec<Value>> = HashMap::new();
        let mut replays_size = 0;
        let (mut step, mut retries) = (0, 0);
        // How the steps ended, and the estimated tokens of the last request.
        let (end, estimate) = 'steps: loop {
            // Its modes may have changed. Unread, they are not assumed: the answer ends.
            let id = session.id;
            session = match self.store.call(move |store| store.session(id)).await {
                Ok(session) => session,
                Err(error) => {
                    let problem = format!("the conversation could not be read: {error}");
                    break (End::Failed(problem), 0);
                }
            };
            let looking = if session.access == Access::None {
                Looking::NotAllowed
            } else if !tools_supported {
                Looking::Unsupported
            } else if step >= MAX_STEPS {
                Looking::Exhausted
            } else {
                Looking::Yes
            };
            let offered: &[ToolSpec] = match (looking, session.access, session.approval) {
                (Looking::Yes, Access::Full, Approval::Plan) => &planning,
                (Looking::Yes, Access::Full, _) => &every,
                (Looking::Yes, _, _) => &reading,
                _ => &[],
            };
            let modes = Modes {
                explanations: session.explanations,
                access: session.access,
                approval: session.approval,
            };
            let mut instructions =
                instructions(modes, looking, self.executor.account(), memory.as_deref());
            if let Some(differences) = differences.as_deref() {
                instructions.push_str("\n\n");
                instructions.push_str(differences);
            }
            history.truncate(asked);
            let current = answer.borrow().message.clone();
            if !current.content.is_empty() || !current.actions.is_empty() {
                history.push(current);
            }
            // The tools offered, and what goes back with the calls of this answer, beside its messages.
            let reserved = size(offered) + replays_size;
            let fitted = tokio::select! {
                biased;
                () = stop.cancelled() => break (End::Stopped, 0),
                fitted = self.fit(&settings, &instructions, reserved, &mut summary, &history, budget) => fitted,
            };
            let messages = chat_messages(fitted.messages, id, &replays, &administrator);
            let request = ChatRequest {
                system: &fitted.system,
                messages: &messages,
                tools: offered,
            };
            // Text after actions starts a paragraph of its own.
            let step_start = answer.borrow().message.content.len();
            let mut separated = step_start == 0;
            let on_text = |piece: &str| {
                let separator = if separated { "" } else { "\n\n" };
                if full.is_cancelled()
                    || answer.borrow().message.content.len() + separator.len() + piece.len()
                        > MAX_ANSWER
                {
                    full.cancel();
                    return;
                }
                answer.send_modify(|state| {
                    state.message.content.push_str(separator);
                    state.message.content.push_str(piece);
                });
                separated = true;
                if saved_at.elapsed() >= SAVE_EVERY {
                    // Best effort, against a crash: the final save below, sent after it, is the one that counts.
                    partial.save(text());
                    saved_at = Instant::now();
                }
            };
            let result = tokio::select! {
                result = self.client.stream(&settings, &request, on_text) => Some(result),
                () = stop.cancelled() => None,
                () = full.cancelled() => None,
            };
            match result {
                None => break (End::Stopped, fitted.estimate),
                // The window was smaller than Kelpy knew: it is remembered smaller, and the conversation fitted again.
                Some(Err(error))
                    if error.is_context_overflow()
                        && retries < OVERFLOW_RETRIES
                        && answer.borrow().message.content.len() == step_start =>
                {
                    retries += 1;
                    budget = budget.shrunk();
                    lock(&self.windows).insert(window_key(&settings), budget.window);
                }
                Some(Err(error)) => break (End::Failed(error.to_string()), fitted.estimate),
                Some(Ok(outcome)) => {
                    if outcome.tools_refused {
                        tools_supported = false;
                        lock(&self.tool_support).insert(window_key(&settings), false);
                    }
                    if outcome.calls.is_empty() || offered.is_empty() {
                        break (End::Answered(outcome), fitted.estimate);
                    }
                    let position = answer.borrow().message.content.len();
                    for call in &outcome.calls {
                        if let Err(end) = self
                            .act(call, step, position, session.id, answer, &stop)
                            .await
                        {
                            break 'steps (end, fitted.estimate);
                        }
                    }
                    replays_size += outcome
                        .replay
                        .iter()
                        .map(|block| context::estimate(&block.to_string()))
                        .sum::<u64>();
                    replays.insert(step, outcome.replay);
                    step += 1;
                    retries = 0;
                }
            }
        };

        let (status, error) = match &end {
            End::Stopped if full.is_cancelled() => (
                Status::Interrupted,
                Some(format!(
                    "the answer reached Kelpy's limit of {} MiB",
                    MAX_ANSWER >> 20
                )),
            ),
            End::Stopped => (Status::Interrupted, Some("stopped".to_owned())),
            End::Failed(error) => (Status::Failed, Some(error.clone())),
            End::Answered(Outcome { stop, .. }) => match stop {
                StopReason::Complete | StopReason::Other(_) => (Status::Complete, None),
                StopReason::TooLong => (
                    Status::Interrupted,
                    Some("the answer reached the length limit".to_owned()),
                ),
                StopReason::Incomplete(reason) => (
                    Status::Interrupted,
                    Some(format!("the provider ended the answer early ({reason})")),
                ),
                StopReason::Refused => (
                    Status::Refused,
                    Some("the model declined to answer".to_owned()),
                ),
            },
        };
        let input_tokens = match end {
            End::Answered(Outcome { input_tokens, .. }) => input_tokens,
            _ => None,
        };
        // What the interfaces are told is what was stored: a failed save is reported, never hidden.
        let content = text();
        let saved = self
            .store
            .call(move |store| {
                store.update_message(id, &content, status)?;
                if estimate > 0 {
                    store.set_usage(id, estimate, input_tokens)?;
                }
                Ok(())
            })
            .await;
        let usage = (estimate > 0).then_some((estimate, input_tokens));
        let (status, error, usage) = match saved {
            Ok(()) => (status, error, usage),
            Err(save) => (
                Status::Failed,
                Some(format!("the answer could not be saved: {save}")),
                None,
            ),
        };
        answer.send_modify(|state| {
            state.message.status = status;
            state.summarised_through = summary.as_ref().map(|summary| summary.through);
            if let Some((estimate, input_tokens)) = usage {
                state.message.context_estimate = Some(estimate);
                state.message.input_tokens = input_tokens;
            }
            state.error = error;
        });
    }

    /// The memory index, at most [`MAX_MEMORY_INDEX`] bytes of it, if there is one. Read once for each answer. An
    /// index other accounts could change is not read: the model is told why.
    pub(super) async fn memory(&self) -> Option<String> {
        if let Err(untrusted) = kelpy_tools::memory::trusted(&self.memory_index).await {
            return Some(format!("(Not read: {untrusted}. Tell the administrator.)"));
        }
        let file = tokio::fs::File::open(&self.memory_index).await.ok()?;
        let mut bytes = Vec::new();
        file.take(MAX_MEMORY_INDEX as u64)
            .read_to_end(&mut bytes)
            .await
            .ok()?;
        // A character cut by the limit is left out.
        let end = match std::str::from_utf8(&bytes) {
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            _ => bytes.len(),
        };
        let memory = String::from_utf8_lossy(&bytes[..end]).trim().to_owned();
        (!memory.is_empty()).then_some(memory)
    }
}

/// The system prompt: Kelpy's instructions with what it may do on the system and in the style of the conversation's
/// `modes`, the interface with those modes, then the user's `memory` index if there is one.
pub(super) fn instructions(
    modes: Modes,
    looking: Looking,
    account: &Account,
    memory: Option<&str>,
) -> String {
    let acting = match (modes.access, modes.approval) {
        (Access::Full, Approval::Plan) => format!("{CHANGING} {PLANNING} {MEMORY_TOOLS}"),
        (Access::Full, _) => format!("{CHANGING} {MEMORY_TOOLS}"),
        (Access::ReadOnly | Access::None, _) => format!("{LOOKING} {MEMORY_TOOLS}"),
    };
    let access = match looking {
        Looking::Yes => acting,
        Looking::Exhausted => format!(
            "{acting} You have used the tools as many times as one answer allows: answer now with what you found \
             and did, and say what is still to do."
        ),
        Looking::NotAllowed => format!(
            "In this conversation the administrator has not let you look at the system: System access is None, \
             so you cannot run commands or read files. {NOT_LOOKING}"
        ),
        Looking::Unsupported => format!(
            "This model cannot use tools, so you cannot look at the system. {NOT_LOOKING} If looking at the \
             system would help, say that another model could, chosen in the AI provider menu."
        ),
    };
    let mut instructions = INSTRUCTIONS
        .replace("{os}", std::env::consts::OS)
        .replace("{arch}", std::env::consts::ARCH)
        .replace("{access}", &access);
    instructions.push(' ');
    instructions.push_str(if modes.explanations {
        EXPLAINED
    } else {
        CONCISE
    });
    instructions.push_str("\n\n");
    instructions.push_str(&interface::interface(modes, account));
    if let Some(memory) = memory {
        instructions.push_str("\n\n## Memory\n\nThe user's memory index (memory/MEMORY.md):\n\n");
        instructions.push_str(memory);
    }
    instructions
}

/// A message as sent: trimmed, not empty, within its limit.
pub(super) fn message(text: &str) -> Result<&str, AgentError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(AgentError::EmptyMessage);
    }
    if text.len() > MAX_MESSAGE {
        return Err(AgentError::Invalid(format!(
            "the message is longer than {} KiB",
            MAX_MESSAGE >> 10
        )));
    }
    Ok(text)
}

/// How the steps of an answer ended.
enum End {
    /// The model answered without calling tools.
    Answered(Outcome),
    /// Stopped, or the answer reached its limit.
    Stopped,
    Failed(String),
}

/// A conversation title from its first message: the first line, cut at a word near `TITLE_LENGTH` characters.
pub(super) fn title_from(text: &str) -> String {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(text)
        .trim();
    if line.chars().count() <= TITLE_LENGTH {
        return line.to_owned();
    }
    let cut: String = line.chars().take(TITLE_LENGTH).collect();
    let cut = match cut.rfind(' ') {
        Some(space) if space > TITLE_LENGTH / 2 => &cut[..space],
        _ => &cut,
    };
    format!("{}…", cut.trim_end())
}
