//! The actions of an answer. Each tool the model calls becomes an action: recorded, shown, approved when the
//! conversation asks for it, run, and given back to the model as the call's result.
//!
//! The administrator may change the conversation's System access and Approval mode meanwhile. What an action may do
//! is decided by the database's own thread, which serves one call at a time: recording an action, and starting it
//! once decided, each read the conversation's modes and write the action in one call, so a change of the modes
//! comes before or after them, never between. Setting access to None also denies the actions waiting at once.
//!
//! With Approval mode Plan, a change that matches a step of the plan approved last, with the same tool and the same
//! arguments, runs without asking; each step runs once. A change already made ends at once, saying so. A change
//! saves its phase as it goes: should Kelpy stop meanwhile, its outcome is uncertain, and it is never run again by
//! itself.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use kelpy_common::{lock, unix_now};
use kelpy_providers::{ChatMessage, Images, ToolCall, ToolResult};
use kelpy_storage::{
    Access, Action, ActionState, Approval, Message, NewAction, Plan, PlanStep, PlanStepRef, Role,
    Session,
};
use kelpy_tools::{Output, Prepared, ToolError, Work};
use serde_json::Value;
use tokio::sync::{oneshot, watch};
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

use super::{AnswerState, End, Running, Turns};
use crate::agent::{AgentError, context};

/// How long an action waits for its approval. Meanwhile the answer keeps Kelpy from stopping for inactivity.
const APPROVAL_TIME: Duration = Duration::from_secs(30 * 60);

/// How often the output of an action running is shown, when it changed.
const LIVE_EVERY: Duration = Duration::from_millis(500);

/// Why an action is denied when the conversation's access does not allow it.
const NO_ACCESS: &str = "System access is None in this conversation";
/// Why a change is denied when the conversation's access only allows looking.
const READ_ONLY: &str =
    "System access is Read-only in this conversation: changing the system needs Full";
/// What a change outside the plan approved says on its card.
const NOT_IN_PLAN: &str = "Not in the plan approved: it asks on its own.";
/// What a floor change inside the plan approved says on its card.
const WITH_PLAN: &str = "Approved with the plan.";

/// An action waiting for a decision, in its answer's [`Running`](super::Running).
pub(super) enum Question {
    /// The decision goes to this sender.
    Open(oneshot::Sender<bool>),
    /// A decision is being recorded: no other can be given.
    Taken,
    /// The wait ended while a decision was being recorded: the wait takes it if it comes.
    Closing,
}

/// How an action that started running ended.
enum Ended {
    Ran(Result<kelpy_tools::Ran, kelpy_tools::ToolError>),
    /// The administrator stopped this action: the answer goes on.
    Halted,
    /// The answer was stopped.
    Stopped,
}

/// What became of an approval asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decided {
    Approved,
    Denied,
    /// Nobody answered in time.
    Unanswered,
    Stopped,
}

impl Turns {
    /// Carries out `call`, asked for at `position` in the answer's text in its `step`, in the conversation
    /// `session_id`. Its result is in the answer's actions when this returns; an error ends the answer.
    pub(super) async fn act(
        &self,
        call: &ToolCall,
        step: u32,
        position: usize,
        session_id: i64,
        answer: &watch::Sender<AnswerState>,
        stop: &CancellationToken,
    ) -> Result<(), End> {
        // A change is planned before it is shown: what is approved is what will be written.
        let secrets = self.server.secrets(session_id);
        let prepared = async {
            let mut prepared = self.tools.prepare(&call.name, &call.arguments)?;
            self.executor.resolve(&mut prepared)?;
            self.executor.plan(&mut prepared, &secrets).await?;
            Ok::<_, ToolError>(prepared)
        }
        .await;
        let (summary, reason, command) = match &prepared {
            Ok(prepared) => (
                prepared.summary.clone(),
                prepared.reason.clone(),
                prepared.command_line(),
            ),
            Err(_) => (format!("Use {}", call.name), String::new(), None),
        };
        let problem = prepared
            .as_ref()
            .err()
            .map(|problem| secrets.redact(&problem.to_string()));
        let ready = prepared.as_ref().ok();
        let always_asks = ready.is_some_and(Prepared::always_asks);
        let never_asks = ready.is_some_and(Prepared::never_asks);
        let changes = ready.is_some_and(Prepared::changes);
        let script = ready.is_some_and(|prepared| matches!(prepared.work, Work::Script { .. }));
        let risk = ready.and_then(Prepared::risk);
        let proposal = ready.and_then(Prepared::proposal).map(plan_of);
        // A change, a plan or a secret: only with System access Full.
        let needs_full = changes || proposal.is_some() || never_asks;
        let already = ready.and_then(|prepared| prepared.already.clone());
        let mut change = ready
            .and_then(Prepared::card)
            .map(crate::agent::server::change_of);
        let input = (changes || script)
            .then(|| kelpy_tools::canonical(&call.arguments))
            .flatten();
        let floor = ready.and_then(Prepared::floor).map(str::to_owned);
        let (message_id, call) = (answer.borrow().message.id, call.clone());
        let recorded = self
            .store
            .call(move |store| {
                let session = store.session(session_id)?;
                let (plan_step, planned) = match (&input, session.access, session.approval) {
                    (Some(input), Access::Full, Approval::Plan) => {
                        let approved = store.approved_plan(session_id)?;
                        let step = approved.as_ref().and_then(|(plan, steps)| {
                            step_of(plan, steps, &call.name, input, floor.as_deref())
                        });
                        (step, approved.is_some())
                    }
                    _ => (None, false),
                };
                let (state, error) = match (problem, session.access, session.approval) {
                    (Some(problem), _, _) => (ActionState::Failed, Some(problem)),
                    // Tools are offered only with access: the model called one anyway, or the access changed
                    // meanwhile.
                    (None, Access::None, _) => (ActionState::Denied, Some(NO_ACCESS.to_owned())),
                    (None, Access::ReadOnly, _) if needs_full => {
                        (ActionState::Denied, Some(READ_ONLY.to_owned()))
                    }
                    // The system is already as asked: nothing runs.
                    (None, _, _) if already.is_some() => (ActionState::Done, None),
                    (None, _, _) if never_asks => (ActionState::Running, None),
                    (None, _, _) if proposal.is_some() => (ActionState::Waiting, None),
                    // A step of the plan approved: approved with it, floor or not.
                    (None, _, Approval::Plan) if plan_step.is_some() => {
                        (ActionState::Running, None)
                    }
                    (None, _, Approval::Ask) => (ActionState::Waiting, None),
                    // A command the model wrote, in Auto with Full: it runs, unless it looks like it could lose
                    // data, lock the administrator out or stop the system.
                    (None, Access::Full, Approval::Auto) if script && risk.is_none() => {
                        (ActionState::Running, None)
                    }
                    (None, _, _) if always_asks => (ActionState::Waiting, None),
                    // In Plan, a change outside the plan asks on its own.
                    (None, _, Approval::Plan) if changes => (ActionState::Waiting, None),
                    // What is left only reads, or makes a change Kelpy checks and can put back.
                    (None, _, Approval::Plan | Approval::Auto) => (ActionState::Running, None),
                };
                if let Some(change) = change.as_mut() {
                    if plan_step.is_some() && change.floor.is_some() {
                        change.notes.push(WITH_PLAN.to_owned());
                    } else if planned && plan_step.is_none() && state == ActionState::Waiting {
                        change.notes.push(NOT_IN_PLAN.to_owned());
                    }
                }
                let mut action = store.insert_action(&NewAction {
                    session_id,
                    message_id: Some(message_id),
                    step,
                    position,
                    call_id: &call.id,
                    tool: &call.name,
                    input: &call.arguments,
                    reason: &reason,
                    summary: &summary,
                    command: command.as_deref(),
                    change: change.as_ref(),
                    plan: proposal.as_ref(),
                    plan_step,
                    risk,
                    state,
                    error: error.as_deref(),
                })?;
                if state == ActionState::Done {
                    action.output = already.unwrap_or_default();
                    store.update_action(&action)?;
                }
                Ok(action)
            })
            .await;
        let mut action = recorded.map_err(not_saved)?;
        show(answer, &action);
        let Ok(prepared) = prepared else {
            return Ok(());
        };
        match action.state {
            ActionState::Waiting => match self.approval(session_id, action.id, stop).await {
                Decided::Stopped => {
                    action.state = ActionState::Failed;
                    action.error = Some("stopped before it ran".to_owned());
                    self.save(&action, answer).await?;
                    return Err(End::Stopped);
                }
                decided => {
                    action = self.settle(session_id, action, decided, needs_full).await?;
                    show(answer, &action);
                }
            },
            ActionState::Running => {}
            _ => return Ok(()),
        }
        if action.state != ActionState::Running {
            return Ok(());
        }
        if changes || script {
            // Saved before anything runs: should Kelpy stop from now on, what the change left is not known.
            action.phase = Some(if script {
                "running the command".to_owned()
            } else {
                "starting the change".to_owned()
            });
            self.save(&action, answer).await?;
        }

        let halt = CancellationToken::new();
        self.running(session_id, |running| {
            running.halts.insert(action.id, halt.clone())
        });
        let started = Instant::now();
        let output = Output::new();
        let mut run = Box::pin(self.executor.run(&prepared, &secrets, &output));
        let mut live = tokio::time::interval(LIVE_EVERY);
        live.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut shown = 0;
        let ended = loop {
            tokio::select! {
                ran = &mut run => break Ended::Ran(ran),
                () = halt.cancelled() => break Ended::Halted,
                () = stop.cancelled() => break Ended::Stopped,
                _ = live.tick() => {
                    if let Some((changes, text)) = output.snapshot_since(shown) {
                        shown = changes;
                        action.output = secrets.redact(&text);
                        let phase = output.phase();
                        if phase.is_some() && phase != action.phase {
                            // Saved as it changes: the phase tells, after a restart, where the change stopped.
                            action.phase = phase;
                            self.save(&action, answer).await?;
                        } else {
                            show(answer, &action);
                        }
                    }
                }
            }
        };
        // Dropping the run ends its program, and what the program started.
        drop(run);
        self.running(session_id, |running| running.halts.remove(&action.id));
        // What it wrote before it was stopped or failed stays with it, secrets hidden.
        action.output = secrets.redact(&output.snapshot().1);
        action.phase = output.phase().or(action.phase);
        action.duration_ms = Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
        let stopped = matches!(ended, Ended::Stopped);
        let mut records = Vec::new();
        match ended {
            Ended::Ran(Ok(ran)) => {
                action.state = ActionState::Done;
                action.exit_code = ran.exit_code;
                if let (Some(change), Some(backup)) = (action.change.as_mut(), ran.backup) {
                    change.backup = Some(backup.to_string_lossy().into_owned());
                }
                records = ran.records;
            }
            Ended::Ran(Err(problem)) => {
                action.state = ActionState::Failed;
                action.error = Some(secrets.redact(&problem.to_string()));
            }
            Ended::Halted => {
                action.state = ActionState::Failed;
                action.error = Some("stopped by the administrator".to_owned());
            }
            Ended::Stopped => {
                action.state = ActionState::Failed;
                action.error = Some("stopped".to_owned());
            }
        }
        if let Err(end) = self.save(&action, answer).await {
            // It ran all the same: the action is shown as what is known of it, and the model is not given its result.
            action.state = ActionState::Failed;
            action.output.clear();
            action.error = Some("it ran, but its result could not be saved".to_owned());
            show(answer, &action);
            return Err(end);
        }
        // What the change left, as Kelpy expects to find it: a record missed only loses a later difference.
        let _ = self.server.record(action.id, records).await;
        if stopped { Err(End::Stopped) } else { Ok(()) }
    }

    /// Settles the waiting `action` with what was `decided`, against the conversation's access as it is now, and
    /// saves it: it runs only if approved while access allows it, Full if it `changes` the system.
    async fn settle(
        &self,
        session_id: i64,
        mut action: Action,
        decided: Decided,
        changes: bool,
    ) -> Result<Action, End> {
        self.store
            .call(move |store| {
                let access = store.session(session_id)?.access;
                (action.state, action.error) = match (decided, access) {
                    (_, Access::None) => (ActionState::Denied, Some(NO_ACCESS.to_owned())),
                    (_, Access::ReadOnly) if changes => {
                        (ActionState::Denied, Some(READ_ONLY.to_owned()))
                    }
                    (Decided::Approved, _) => (ActionState::Running, None),
                    (Decided::Unanswered, _) => (
                        ActionState::Denied,
                        Some(format!(
                            "not approved within {} minutes",
                            APPROVAL_TIME.as_secs() / 60
                        )),
                    ),
                    (Decided::Denied | Decided::Stopped, _) => (ActionState::Denied, None),
                };
                if action.state == ActionState::Running {
                    action.started_at = Some(unix_now());
                }
                store.update_action(&action)?;
                Ok(action)
            })
            .await
            .map_err(not_saved)
    }

    /// Waits for the administrator's decision on the action `action_id` of the answer in `session_id`.
    async fn approval(&self, session_id: i64, action_id: i64, stop: &CancellationToken) -> Decided {
        let deadline = tokio::time::Instant::now() + APPROVAL_TIME;
        loop {
            let (decide, mut decision) = oneshot::channel();
            let asked = self.questions(session_id, |questions| {
                questions.insert(action_id, Question::Open(decide));
            });
            if asked.is_none() {
                return Decided::Stopped;
            }
            let ended = tokio::select! {
                decision = &mut decision => match decision {
                    Ok(approved) => {
                        self.questions(session_id, |questions| questions.remove(&action_id));
                        return if approved { Decided::Approved } else { Decided::Denied };
                    }
                    // A decision given up before it was recorded: the question stands.
                    Err(_) => continue,
                },
                () = stop.cancelled() => Decided::Stopped,
                () = tokio::time::sleep_until(deadline) => Decided::Unanswered,
            };
            let taken =
                self.questions(session_id, |questions| match questions.remove(&action_id) {
                    Some(Question::Taken) => {
                        questions.insert(action_id, Question::Closing);
                        true
                    }
                    _ => false,
                });
            if taken != Some(true) {
                return ended;
            }
            // A decision is being recorded, which takes one write: it is awaited. Stopping still wins.
            let decided = decision.await;
            self.questions(session_id, |questions| questions.remove(&action_id));
            return match (ended, decided) {
                (Decided::Unanswered, Ok(true)) => Decided::Approved,
                (Decided::Unanswered, Ok(false)) => Decided::Denied,
                _ => ended,
            };
        }
    }

    /// Approves or denies the action `action_id` waiting in the answer being written in `session_id`. Approving
    /// can also switch the conversation to the Approval mode `switch_to`; the conversation is then returned.
    ///
    /// The question is taken first, so no other decision can be given meanwhile, and the mode changes only for a
    /// decision taken. If the mode cannot be saved, the question stands.
    pub(in crate::agent) async fn decide(
        &self,
        session_id: i64,
        action_id: i64,
        approve: bool,
        switch_to: Option<Approval>,
    ) -> Result<Option<Session>, AgentError> {
        let decide = self
            .questions(session_id, |questions| {
                match questions.remove(&action_id)? {
                    Question::Open(decide) => {
                        questions.insert(action_id, Question::Taken);
                        Some(decide)
                    }
                    other => {
                        questions.insert(action_id, other);
                        None
                    }
                }
            })
            .flatten()
            .ok_or(AgentError::NotWaiting)?;
        let switched = match switch_to.filter(|_| approve) {
            Some(approval) => {
                let saved = self
                    .store
                    .call(move |store| {
                        store.set_approval(session_id, approval)?;
                        store.session(session_id)
                    })
                    .await;
                match saved {
                    Ok(session) => Some(session),
                    Err(error) => {
                        // Put back, unless the wait ended meanwhile: it then ends with `decide` dropped.
                        self.questions(session_id, |questions| {
                            if matches!(questions.get(&action_id), Some(Question::Taken)) {
                                questions.insert(action_id, Question::Open(decide));
                            }
                        });
                        return Err(error.into());
                    }
                }
            }
            None => None,
        };
        decide.send(approve).map_err(|_| AgentError::NotWaiting)?;
        Ok(switched)
    }

    /// Stops the action `action_id`, running in the answer being written in `session_id`. The answer goes on, and
    /// the model is told that the administrator stopped it.
    pub(in crate::agent) fn halt(&self, session_id: i64, action_id: i64) -> Result<(), AgentError> {
        self.running(session_id, |running| {
            running.halts.get(&action_id).map(CancellationToken::cancel)
        })
        .flatten()
        .ok_or(AgentError::NotRunning)
    }

    /// Denies the actions waiting in the answer being written in `session_id`: its access was set to None. A
    /// decision already being recorded is settled against the access as well.
    pub(in crate::agent) fn revoke(&self, session_id: i64) {
        self.questions(session_id, |questions| {
            let open = questions.extract_if(|_, question| matches!(question, Question::Open(_)));
            for (_, question) in open {
                if let Question::Open(decide) = question {
                    let _ = decide.send(false);
                }
            }
        });
    }

    /// Runs `work` on the questions of the answer being written in `session_id`, if there is one.
    fn questions<T>(
        &self,
        session_id: i64,
        work: impl FnOnce(&mut HashMap<i64, Question>) -> T,
    ) -> Option<T> {
        self.running(session_id, |running| work(&mut running.questions))
    }

    /// Runs `work` on the answer being written in `session_id`, if there is one.
    fn running<T>(&self, session_id: i64, work: impl FnOnce(&mut Running) -> T) -> Option<T> {
        lock(&self.answers).running.get_mut(&session_id).map(work)
    }

    /// Saves `action` as it is now, then shows it.
    async fn save(&self, action: &Action, answer: &watch::Sender<AnswerState>) -> Result<(), End> {
        let saved = action.clone();
        self.store
            .call(move |store| store.update_action(&saved))
            .await
            .map_err(not_saved)?;
        show(answer, action);
        Ok(())
    }
}

/// The end of an answer whose action could not be saved.
fn not_saved(error: kelpy_storage::StoreError) -> End {
    End::Failed(format!("the action could not be saved: {error}"))
}

/// A proposal as the plan's action stores it.
fn plan_of(proposal: &kelpy_tools::Proposal) -> Plan {
    Plan {
        goal: proposal.goal.clone(),
        steps: proposal
            .steps
            .iter()
            .map(|step| PlanStep {
                tool: step.tool.clone(),
                input: step.input.clone(),
                summary: step.summary.clone(),
                detail: step.detail.clone(),
                floor: step.floor.clone(),
            })
            .collect(),
    }
}

/// The first step of the `plan` approved that a call of `tool` with the canonical `input` carries out, among those
/// no action has taken in `steps`: carried out, carrying out, started (stopped or failed once running) or left
/// uncertain. A call whose change is on the `floor` carries out only a step shown with a floor: the plan approved
/// would otherwise hide it.
fn step_of(
    plan: &Action,
    steps: &[Action],
    tool: &str,
    input: &str,
    floor: Option<&str>,
) -> Option<PlanStepRef> {
    let proposed = plan.plan.as_ref()?;
    let taken = |index: usize| {
        steps.iter().any(|action| {
            action
                .plan_step
                .is_some_and(|step| step.step as usize == index)
                && (matches!(
                    action.state,
                    ActionState::Done
                        | ActionState::Running
                        | ActionState::Waiting
                        | ActionState::Uncertain
                ) || (action.state == ActionState::Failed && action.started_at.is_some()))
        })
    };
    let index = proposed
        .steps
        .iter()
        .enumerate()
        .position(|(index, step)| {
            step.tool == tool
                && step.input == input
                && (floor.is_none() || step.floor.is_some())
                && !taken(index)
        })?;
    Some(PlanStepRef {
        plan_id: plan.id,
        step: u32::try_from(index).ok()?,
    })
}

/// Puts `action` in the answer as it is now, in its place or at the end.
fn show(answer: &watch::Sender<AnswerState>, action: &Action) {
    answer.send_modify(|state| {
        let actions = &mut state.message.actions;
        match actions.iter_mut().find(|shown| shown.id == action.id) {
            Some(shown) => shown.clone_from(action),
            None => actions.push(action.clone()),
        }
    });
}

/// The conversation as the model reads it. An answer with actions becomes its steps: the text before each step's
/// calls, the calls, their results, and the text after the last step. The answer being written, `current`, carries
/// back what the model wrote besides text in each step, from `replays`. The operations the `administrator` asked
/// for come before the user's message that follows them. A user's message with images in `shown` shows them, with
/// the text that goes with them.
pub(super) fn chat_messages(
    messages: &[Message],
    current: i64,
    replays: &HashMap<u32, Vec<Value>>,
    administrator: &[Action],
    shown: &HashMap<i64, Images>,
) -> Vec<ChatMessage> {
    let mut chat = Vec::with_capacity(messages.len());
    // Those before the messages given are in their summary, or older.
    let first = messages.first().map_or(i64::MAX, |message| message.id);
    let mut operations = administrator
        .iter()
        .filter(|action| action.after_message.unwrap_or(0) >= first)
        .peekable();
    for message in messages {
        match message.role {
            Role::User if !message.content.is_empty() => {
                let mut notes = Vec::new();
                while let Some(action) =
                    operations.next_if(|action| action.after_message.unwrap_or(0) < message.id)
                {
                    notes.push(format!("- {}", context::administrator_note(action)));
                }
                let with_notes = |text: &str| {
                    if notes.is_empty() {
                        text.to_owned()
                    } else {
                        format!(
                            "[Since your last answer, the administrator did this through Kelpy:\n{}]\n\n{text}",
                            notes.join("\n"),
                        )
                    }
                };
                chat.push(ChatMessage::User {
                    text: with_notes(&message.content),
                    images: shown.get(&message.id).map(|images| Images {
                        text: with_notes(&images.text),
                        images: images.images.clone(),
                    }),
                });
            }
            Role::User => {}
            Role::Assistant => {
                let content = &message.content;
                let mut from = 0;
                for step in message
                    .actions
                    .chunk_by(|one, other| one.step == other.step)
                {
                    let to = step[0].position.clamp(from, content.len());
                    let replay = (message.id == current)
                        .then(|| replays.get(&step[0].step).cloned())
                        .flatten()
                        .unwrap_or_default();
                    chat.push(ChatMessage::Assistant {
                        text: content.get(from..to).unwrap_or_default().trim().to_owned(),
                        calls: step
                            .iter()
                            .map(|action| ToolCall {
                                id: action.call_id.clone(),
                                name: action.tool.clone(),
                                arguments: action.input.clone(),
                            })
                            .collect(),
                        replay,
                    });
                    chat.extend(step.iter().map(|action| {
                        ChatMessage::Tool(ToolResult {
                            call_id: action.call_id.clone(),
                            output: context::action_result(action),
                            is_error: action.state != ActionState::Done,
                        })
                    }));
                    from = to;
                }
                let rest = content.get(from..).unwrap_or_default().trim();
                if !rest.is_empty() {
                    chat.push(ChatMessage::assistant(rest));
                }
            }
        }
    }
    chat
}

#[cfg(test)]
mod tests {
    use super::*;
    use kelpy_storage::Status;

    fn action(id: i64, step: u32, position: usize, state: ActionState) -> Action {
        Action {
            id,
            session_id: 1,
            message_id: Some(2),
            after_message: None,
            phase: None,
            plan: None,
            plan_step: None,
            risk: None,
            step,
            position,
            call_id: format!("call_{id}"),
            tool: "disk_usage".into(),
            input: "{}".into(),
            reason: String::new(),
            summary: "Show the disk usage".into(),
            command: Some("df -h".into()),
            state,
            exit_code: Some(0),
            output: "/ 40% used".into(),
            error: None,
            created_at: 0,
            duration_ms: Some(5),
            started_at: Some(0),
            change: None,
        }
    }

    #[test]
    fn an_answer_with_actions_goes_back_as_its_steps() {
        let content = "Let me look.\n\nThe disk is 40% full.";
        let message = |id, role, content: &str, actions| Message {
            id,
            session_id: 1,
            role,
            content: content.to_owned(),
            status: Status::Complete,
            model: None,
            created_at: 0,
            context_estimate: None,
            input_tokens: None,
            actions,
        };
        let messages = [
            message(1, Role::User, "Disks?", Vec::new()),
            message(
                2,
                Role::Assistant,
                content,
                vec![
                    action(1, 0, 12, ActionState::Done),
                    action(2, 0, 12, ActionState::Denied),
                    action(3, 1, 12, ActionState::Done),
                ],
            ),
        ];
        let thinking = serde_json::json!({ "type": "thinking" });
        let replays = HashMap::from([(1, vec![thinking.clone()])]);
        let chat = chat_messages(&messages, 2, &replays, &[], &HashMap::new());
        assert_eq!(chat.len(), 7);
        assert_eq!(chat[0], ChatMessage::user("Disks?"));
        assert!(
            matches!(&chat[1], ChatMessage::Assistant { text, calls, replay }
            if text == "Let me look." && calls.len() == 2 && replay.is_empty())
        );
        assert!(matches!(&chat[2], ChatMessage::Tool(result)
            if result.call_id == "call_1" && result.output.starts_with("Exit code 0.") && !result.is_error));
        assert!(matches!(&chat[3], ChatMessage::Tool(result)
            if result.output == "The administrator denied this action." && result.is_error));
        // A step without text of its own, then the text after the last one.
        assert!(
            matches!(&chat[4], ChatMessage::Assistant { text, calls, replay }
            if text.is_empty() && calls[0].id == "call_3" && replay == std::slice::from_ref(&thinking))
        );
        assert_eq!(chat[6], ChatMessage::assistant("The disk is 40% full."));
        // Only the answer being written carries back its blocks.
        assert!(
            matches!(&chat_messages(&messages, 9, &replays, &[], &HashMap::new())[4],
            ChatMessage::Assistant { replay, .. } if replay.is_empty())
        );

        // An operation of the administrator after the answer comes with the next message.
        let mut later = messages.to_vec();
        later.push(message(3, Role::User, "Is it fixed?", Vec::new()));
        let mut restore = action(9, 0, 0, ActionState::Done);
        (restore.message_id, restore.after_message) = (None, Some(2));
        restore.summary = "Put back /etc/hosts as it was before this change".into();
        let chat = chat_messages(&later, 2, &replays, &[restore], &HashMap::new());
        let ChatMessage::User { text, .. } = chat.last().unwrap() else {
            panic!("{chat:?}");
        };
        assert!(
            text.starts_with("[Since your last answer, the administrator did this through Kelpy:\n- Put back /etc/hosts")
                && text.ends_with("Is it fixed?"),
            "{text}"
        );
    }

    #[test]
    fn a_call_carries_out_the_first_step_of_the_plan_not_taken_with_the_same_arguments() {
        let step = |input: &str, floor: Option<&str>| PlanStep {
            tool: "manage_service".into(),
            input: input.into(),
            summary: String::new(),
            detail: None,
            floor: floor.map(str::to_owned),
        };
        let mut plan = action(5, 0, 0, ActionState::Done);
        plan.plan = Some(Plan {
            goal: "Restart twice".into(),
            steps: vec![
                step("{\"a\":1}", None),
                step("{\"a\":1}", None),
                step("{\"b\":2}", Some("It stops sshd.")),
            ],
        });
        let next = |taken: &[Action], input: &str, floor: Option<&str>| {
            step_of(&plan, taken, "manage_service", input, floor).map(|step| step.step)
        };
        let first = step_of(&plan, &[], "manage_service", "{\"a\":1}", None).unwrap();
        assert_eq!((first.plan_id, first.step), (5, 0));
        let mut taken = action(6, 0, 0, ActionState::Done);
        taken.plan_step = Some(first);
        assert_eq!(
            next(std::slice::from_ref(&taken), "{\"a\":1}", None),
            Some(1)
        );
        // A failed attempt that never started leaves its step to the next call; one that started, or left the
        // outcome uncertain, does not: it asks again.
        taken.state = ActionState::Failed;
        taken.started_at = None;
        assert_eq!(
            next(std::slice::from_ref(&taken), "{\"a\":1}", None),
            Some(0)
        );
        taken.started_at = Some(0);
        assert_eq!(
            next(std::slice::from_ref(&taken), "{\"a\":1}", None),
            Some(1)
        );
        taken.state = ActionState::Uncertain;
        assert_eq!(
            next(std::slice::from_ref(&taken), "{\"a\":1}", None),
            Some(1)
        );
        assert_eq!(next(&[], "{\"a\":2}", None), None);
        assert_eq!(step_of(&plan, &[], "write_file", "{\"b\":2}", None), None);
        // A call on the floor matches only a step shown with a floor.
        assert_eq!(
            next(&[], "{\"a\":1}", Some("It changes the SSH configuration.")),
            None
        );
        assert_eq!(next(&[], "{\"b\":2}", Some("It stops sshd.")), Some(2));
    }
}
