//! The terminal UI's state and what it does with keys and with what the runtime sends: pure, so that it is tested
//! without a terminal or a runtime. What needs the runtime goes out as an [`Effect`]; its result comes back as an
//! [`Update`].

use std::collections::{HashMap, HashSet};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use kelpy_core::agent::{Access, Approval};
use serde_json::json;

use crate::Banner;
use crate::history::{self, Recall};
use crate::model::{
    Action, ActionState, ChatEvent, Connection, ContextUse, Conversation, Message, ProviderState,
    Role, RuntimeEvent, Session, Status,
};
use crate::ui::WIDE;

/// What the terminal UI asks of the runtime.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    LoadProvider,
    LoadConnections,
    DecideConnection {
        id: u64,
        allow: bool,
    },
    /// Reads the conversations, as listed at the [`App::listing`] given.
    LoadSessions(u64),
    /// Reads a conversation, and follows its answer if one is being written. `read` numbers the read (only the
    /// last one begun is taken); `since` is the [`App::changes`] when it began: what changed after wins over what
    /// it reads.
    Open {
        id: i64,
        read: u64,
        since: u64,
    },
    /// Follows the answer being written in a conversation, unless it is followed already.
    Follow(i64),
    /// Sends a message; without a conversation, creates one with these modes first.
    Send {
        session: Option<i64>,
        content: String,
        modes: serde_json::Value,
    },
    Decide {
        session: i64,
        action: i64,
        approve: bool,
    },
    Cancel(i64),
    /// Opens a link in this computer's browser.
    OpenLink(String),
    Change {
        session: i64,
        change: serde_json::Value,
    },
    /// Ends the terminal UI.
    Exit(Exit),
}

/// Why the terminal UI ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// The user quit.
    Quit,
    /// Kelpy is stopping.
    Stopped,
}

/// What comes back from the runtime.
#[derive(Debug, Clone)]
pub enum Update {
    Provider(ProviderState),
    /// The remote connections waiting for a decision.
    Connections(Vec<Connection>),
    Sessions {
        listing: u64,
        sessions: Vec<Session>,
    },
    Conversation {
        id: i64,
        read: u64,
        since: u64,
        conversation: Conversation,
    },
    /// A conversation this interface created for its first message.
    Created(Session),
    /// Kelpy refused a message, `problem` saying why: no answer is being written for it.
    NotSent {
        session: i64,
        content: String,
        problem: String,
    },
    Session(Session),
    Chat {
        session: i64,
        event: ChatEvent,
    },
    Runtime(RuntimeEvent),
    Failed(String),
}

/// Where the keys go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// The list of the conversations, opened over the screen.
    Conversations,
    Message,
}

pub struct App {
    pub banner: Banner,
    pub provider: ProviderState,
    /// Remote connections waiting for a decision: Ctrl+Y and Ctrl+N decide the first.
    pub connections: Vec<Connection>,
    pub sessions: Vec<Session>,
    /// The open conversation; none is a new one, created with its first message.
    pub selected: Option<i64>,
    /// The highlighted line of the list of conversations: 0 is *New conversation*, then the conversations.
    pub cursor: usize,
    pub messages: Vec<Message>,
    /// Whether the open conversation's messages are being read: until then, none shows.
    pub loading: bool,
    /// What the open conversation's latest answer took of the model's context window, once known.
    pub context: Option<ContextUse>,
    /// Conversations with an answer being written.
    pub answering: HashSet<i64>,
    /// Conversations this interface created for a message, among `answering` only on its word until Kelpy says
    /// whether an answer began: a refusal of that message takes back this mark, and no other.
    presumed: HashSet<i64>,
    /// The modes of a new conversation, until its first message.
    pub draft: Modes,
    pub input: String,
    /// Where Up and Down are in the open conversation's messages sent.
    recall: Recall,
    pub focus: Focus,
    pub notice: Option<String>,
    /// How many lines the conversation is scrolled up from its end.
    pub scroll: usize,
    /// Quitting asks first: it stops Kelpy while an answer is being written.
    pub confirming_quit: bool,
    /// Whether quitting stops Kelpy: this interface started it.
    pub stops_runtime: bool,
    /// Whether the terminal is wide enough for the side panel beside the conversation.
    pub wide: bool,
    /// The side panel hidden with Ctrl+B, where it fits.
    pub panel_hidden: bool,
    /// The side panel opened with Ctrl+B over the conversation, where it does not fit.
    pub overlay: bool,
    /// Drawn inline, in the terminal's own history, with only a footer: no side panel.
    pub inline: bool,
    /// Whether Ctrl+O opens the Web UI in this computer's browser: not over SSH, where it would open on the server.
    pub opens_links: bool,
    /// Whether the conversations were read once: the start banner lists them.
    pub loaded: bool,
    /// Raised by each conversation added or deleted: a list read before is out of date, and read again.
    listing: u64,
    /// Raised by each change to the open conversation that its answer's stream or Kelpy's events bring.
    changes: u64,
    /// Raised by each read of a conversation begun: only the answer to the last one is taken, so a read from an
    /// earlier opening of the same conversation (A, then B, then A again) cannot replace a newer one.
    reads: u64,
    /// For each message of the open conversation, the change that last set it.
    touched: HashMap<i64, u64>,
    /// The change that last set whether an answer is written in the open conversation.
    running_set: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Modes {
    pub explanations: bool,
    pub access: Access,
    pub approval: Approval,
}

impl Default for Modes {
    fn default() -> Self {
        Self {
            explanations: false,
            access: Access::ReadOnly,
            approval: Approval::Ask,
        }
    }
}

impl App {
    pub fn new(banner: Banner, stops_runtime: bool) -> Self {
        Self {
            banner,
            provider: ProviderState::default(),
            connections: Vec::new(),
            sessions: Vec::new(),
            selected: None,
            cursor: 0,
            messages: Vec::new(),
            loading: false,
            context: None,
            answering: HashSet::new(),
            presumed: HashSet::new(),
            draft: Modes::default(),
            input: String::new(),
            recall: Recall::default(),
            focus: Focus::Message,
            notice: None,
            scroll: 0,
            confirming_quit: false,
            stops_runtime,
            wide: true,
            panel_hidden: false,
            overlay: false,
            inline: false,
            opens_links: false,
            loaded: false,
            listing: 0,
            changes: 0,
            reads: 0,
            touched: HashMap::new(),
            running_set: 0,
        }
    }

    /// What starts the terminal UI.
    pub fn start(&self) -> Vec<Effect> {
        vec![
            Effect::LoadProvider,
            Effect::LoadSessions(self.listing),
            Effect::LoadConnections,
        ]
    }

    /// Reads conversation `id` from now on.
    fn read(&mut self, id: i64) -> Effect {
        self.reads += 1;
        self.loading = true;
        Effect::Open {
            id,
            read: self.reads,
            since: self.changes,
        }
    }

    /// Makes `id` the open conversation, nothing of it changed yet.
    fn open(&mut self, id: Option<i64>) {
        self.selected = id;
        self.messages.clear();
        self.loading = false;
        self.recall.reset();
        self.context = None;
        self.touched.clear();
        self.running_set = 0;
    }

    /// Records what Kelpy says: whether an answer is being written in conversation `id`.
    fn set_answering(&mut self, id: i64, answering: bool) {
        self.presumed.remove(&id);
        if answering {
            self.answering.insert(id);
        } else {
            self.answering.remove(&id);
        }
    }

    /// Counts a change to the open conversation, and returns it.
    fn change(&mut self) -> u64 {
        self.changes += 1;
        self.changes
    }

    /// The open conversation's modes, or those of the new one.
    pub fn modes(&self) -> Modes {
        self.selected
            .and_then(|id| self.session(id))
            .map_or(self.draft, |session| Modes {
                explanations: session.explanations,
                access: session.access,
                approval: session.approval,
            })
    }

    pub fn session(&self, id: i64) -> Option<&Session> {
        self.sessions.iter().find(|session| session.id == id)
    }

    /// Whether the start screen shows: no conversation open yet.
    pub fn starting(&self) -> bool {
        self.selected.is_none() && self.messages.is_empty()
    }

    /// The terminal is now `columns` wide: the side panel goes beside the conversation or over it.
    pub fn resize(&mut self, columns: u16) {
        self.wide = columns >= WIDE;
        if self.wide {
            self.overlay = false;
        }
    }

    /// Whether an answer is being written in the open conversation.
    pub fn running(&self) -> bool {
        self.selected.is_some_and(|id| self.answering.contains(&id))
    }

    /// The first action of the open conversation that waits for a decision.
    pub fn waiting(&self) -> Option<&Action> {
        self.messages
            .iter()
            .flat_map(|message| &message.actions)
            .find(|action| action.state == ActionState::Waiting)
    }

    pub fn key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if key.kind == KeyEventKind::Release {
            return Vec::new();
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.confirming_quit {
            self.confirming_quit = false;
            return match key.code {
                KeyCode::Char('y' | 'Y') => vec![Effect::Exit(Exit::Quit)],
                _ => Vec::new(),
            };
        }
        self.notice = None;
        match key.code {
            KeyCode::Char('c' | 'q') if control => {
                if self.stops_runtime && !self.answering.is_empty() {
                    self.confirming_quit = true;
                    Vec::new()
                } else {
                    vec![Effect::Exit(Exit::Quit)]
                }
            }
            KeyCode::Char('y') if control => self.decide(true),
            KeyCode::Char('n') if control => self.decide(false),
            KeyCode::Char('o') if control && self.opens_links => {
                vec![Effect::OpenLink(self.banner.web_ui.clone())]
            }
            KeyCode::Char('r') if control => {
                self.focus = Focus::Conversations;
                self.overlay = false;
                Vec::new()
            }
            KeyCode::Char('b') if control => {
                if self.inline {
                    // No panel: its state is in the status line.
                } else if self.wide {
                    self.panel_hidden = !self.panel_hidden;
                } else {
                    self.overlay = !self.overlay;
                }
                Vec::new()
            }
            KeyCode::Esc if self.overlay => {
                self.overlay = false;
                Vec::new()
            }
            KeyCode::F(2) => self.change_modes(|modes| {
                modes.access = match modes.access {
                    Access::None => Access::ReadOnly,
                    Access::ReadOnly => Access::Full,
                    Access::Full => Access::None,
                }
            }),
            KeyCode::BackTab | KeyCode::F(3) => self.change_modes(|modes| {
                modes.approval = match modes.approval {
                    Approval::Ask => Approval::Plan,
                    Approval::Plan => Approval::Auto,
                    Approval::Auto => Approval::Ask,
                }
            }),
            KeyCode::F(4) => self.change_modes(|modes| modes.explanations = !modes.explanations),
            KeyCode::PageUp => {
                self.scroll += 10;
                Vec::new()
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_sub(10);
                Vec::new()
            }
            _ => match self.focus {
                Focus::Conversations => self.list_key(key.code),
                Focus::Message => self.message_key(key),
            },
        }
    }

    /// Text pasted in the terminal goes to the message.
    pub fn paste(&mut self, text: &str) {
        self.focus = Focus::Message;
        self.input.push_str(&text.replace("\r\n", "\n"));
    }

    fn list_key(&mut self, code: KeyCode) -> Vec<Effect> {
        match code {
            KeyCode::Up => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down => self.cursor = (self.cursor + 1).min(self.sessions.len()),
            KeyCode::Enter => {
                self.focus = Focus::Message;
                return self.select(
                    self.cursor
                        .checked_sub(1)
                        .map(|index| self.sessions[index].id),
                );
            }
            KeyCode::Char('n') => {
                self.cursor = 0;
                self.focus = Focus::Message;
                return self.select(None);
            }
            KeyCode::Esc => self.focus = Focus::Message,
            _ => {}
        }
        Vec::new()
    }

    fn message_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if !matches!(key.code, KeyCode::Up | KeyCode::Down) {
            self.recall.reset();
        }
        match key.code {
            // The messages sent in this conversation, as a shell's history.
            KeyCode::Up => {
                let sent = history::sent(&self.messages);
                if let Some(text) = self.recall.back(&sent, &self.input) {
                    self.input = text;
                }
            }
            KeyCode::Down => {
                let sent = history::sent(&self.messages);
                if let Some(text) = self.recall.forward(&sent) {
                    self.input = text;
                }
            }
            KeyCode::Char('u') if control => self.input.clear(),
            // On the start screen, a number opens that recent conversation.
            KeyCode::Char(digit @ '1'..='3')
                if self.starting()
                    && self.input.is_empty()
                    && self.sessions.len() >= digit as usize - '0' as usize =>
            {
                let index = digit as usize - '1' as usize;
                self.cursor = index + 1;
                return self.select(Some(self.sessions[index].id));
            }
            KeyCode::Char(char) if !control => self.input.push(char),
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                self.input.push('\n');
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Enter if self.running() => {
                self.notice = Some(
                    "An answer is being written: wait for it, or press Esc to stop it.".into(),
                );
            }
            // Kelpy would refuse it: say why now, the message kept, rather than open a conversation for nothing.
            KeyCode::Enter if !self.input.trim().is_empty() && self.provider.problem.is_some() => {
                self.notice = self.provider.refusal();
            }
            KeyCode::Enter if !self.input.trim().is_empty() => {
                let content = std::mem::take(&mut self.input);
                self.scroll = 0;
                let modes = self.draft;
                return vec![Effect::Send {
                    session: self.selected,
                    content,
                    modes: json!({
                        "explanations": modes.explanations,
                        "access": modes.access,
                        "approval": modes.approval,
                    }),
                }];
            }
            KeyCode::Esc => {
                if let Some(id) = self.selected.filter(|_| self.running()) {
                    return vec![Effect::Cancel(id)];
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn select(&mut self, id: Option<i64>) -> Vec<Effect> {
        if id == self.selected && id.is_some() {
            return Vec::new();
        }
        self.open(id);
        self.scroll = 0;
        id.map(|id| self.read(id)).into_iter().collect()
    }

    fn decide(&mut self, approve: bool) -> Vec<Effect> {
        // A connection waiting is decided first: whoever connects is waiting at their terminal.
        if let Some(connection) = self.connections.first() {
            return vec![Effect::DecideConnection {
                id: connection.id,
                allow: approve,
            }];
        }
        match (self.selected, self.waiting()) {
            (Some(session), Some(action)) => vec![Effect::Decide {
                session,
                action: action.id,
                approve,
            }],
            _ => Vec::new(),
        }
    }

    fn change_modes(&mut self, change: impl FnOnce(&mut Modes)) -> Vec<Effect> {
        let mut modes = self.modes();
        change(&mut modes);
        match self.selected {
            None => {
                self.draft = modes;
                Vec::new()
            }
            Some(session) => vec![Effect::Change {
                session,
                change: json!({
                    "explanations": modes.explanations,
                    "access": modes.access,
                    "approval": modes.approval,
                }),
            }],
        }
    }

    pub fn apply(&mut self, update: Update) -> Vec<Effect> {
        match update {
            Update::Provider(provider) => self.provider = provider,
            Update::Connections(connections) => self.connections = connections,
            Update::Sessions { listing, sessions } => {
                // A conversation added or deleted since the read began: the list read is out of date.
                if listing != self.listing {
                    return vec![Effect::LoadSessions(self.listing)];
                }
                // Each as already listed when that copy is newer: an event may have crossed this answer.
                let merged = sessions
                    .into_iter()
                    .map(|session| match self.session(session.id) {
                        Some(known) if !session.supersedes(known) => known.clone(),
                        _ => session,
                    })
                    .collect();
                self.sessions = merged;
                self.loaded = true;
                self.sort();
            }
            Update::Conversation {
                id,
                read,
                since,
                conversation,
            } => {
                if read != self.reads {
                    return Vec::new();
                }
                return self.merge(id, since, conversation);
            }
            Update::Created(session) => {
                self.listing += 1;
                self.open(Some(session.id));
                // Until Kelpy says whether an answer began, or refuses the message.
                if self.answering.insert(session.id) {
                    self.presumed.insert(session.id);
                }
                self.running_set = self.change();
                self.upsert(session);
                self.cursor = 1;
            }
            Update::NotSent {
                session,
                content,
                problem,
            } => {
                self.notice = Some(format!("Not sent: {problem}."));
                // Only this interface's mark is taken back: an answer another one began since stays.
                if self.presumed.remove(&session) {
                    self.answering.remove(&session);
                }
                if self.selected == Some(session) {
                    if self.input.is_empty() {
                        self.input = content;
                    }
                    // And the conversation read again, for what Kelpy says of it.
                    return vec![self.read(session)];
                }
            }
            Update::Session(session) => self.upsert(session),
            Update::Chat { session, event } => self.chat(session, event),
            Update::Runtime(event) => return self.runtime(event),
            Update::Failed(problem) => self.notice = Some(problem),
        }
        Vec::new()
    }

    /// Puts what a read of conversation `id` found together with what changed since the read began, at change
    /// `since`: the messages and the answer's state changed after it stay as they are.
    fn merge(&mut self, id: i64, since: u64, conversation: Conversation) -> Vec<Effect> {
        if self.selected != Some(id) {
            return Vec::new();
        }
        self.loading = false;
        if self.running_set <= since {
            // An answer that ended meanwhile brought a newer one.
            self.context = conversation.context.or(self.context);
            self.set_answering(id, conversation.running);
        }
        let newer = |message: &Message| self.touched.get(&message.id).is_some_and(|&at| at > since);
        let mut local: HashMap<i64, Message> = std::mem::take(&mut self.messages)
            .into_iter()
            .filter(|message| newer(message))
            .map(|message| (message.id, message))
            .collect();
        let mut messages: Vec<Message> = conversation
            .messages
            .into_iter()
            .map(|read| local.remove(&read.id).unwrap_or(read))
            .collect();
        // Messages the stream brought after the read began.
        messages.extend(local.into_values());
        messages.sort_by_key(|message| message.id);
        self.messages = messages;
        if self.answering.contains(&id) {
            vec![Effect::Follow(id)]
        } else {
            Vec::new()
        }
    }

    fn chat(&mut self, session: i64, event: ChatEvent) {
        if self.selected == Some(session)
            && matches!(event, ChatEvent::Started { .. } | ChatEvent::Done { .. })
        {
            self.running_set = self.change();
        }
        if matches!(event, ChatEvent::Started { .. }) {
            self.set_answering(session, true);
        }
        if let ChatEvent::Done {
            error: Some(error), ..
        } = &event
            && self.selected == Some(session)
        {
            self.notice = Some(error.clone());
        }
        if let ChatEvent::Done { context, .. } = &event {
            self.set_answering(session, false);
            if self.selected == Some(session) && context.is_some() {
                self.context = *context;
            }
        }
        if self.selected != Some(session) {
            return;
        }
        let change = self.change();
        match event {
            ChatEvent::User { message }
            | ChatEvent::Started { message }
            | ChatEvent::Done { message, .. } => {
                self.touched.insert(message.id, change);
                match self
                    .messages
                    .iter_mut()
                    .find(|known| known.id == message.id)
                {
                    Some(known) => *known = message,
                    None => self.messages.push(message),
                }
            }
            ChatEvent::Delta { text } => {
                if let Some(answer) = self
                    .messages
                    .last_mut()
                    .filter(|last| last.role == Role::Assistant)
                {
                    answer.content.push_str(&text);
                    self.touched.insert(answer.id, change);
                }
            }
            ChatEvent::Action { action } => {
                if let Some(answer) = self
                    .messages
                    .last_mut()
                    .filter(|last| last.role == Role::Assistant)
                {
                    self.touched.insert(answer.id, change);
                    match answer
                        .actions
                        .iter_mut()
                        .find(|known| known.id == action.id)
                    {
                        Some(known) => *known = *action,
                        None => answer.actions.push(*action),
                    }
                }
            }
        }
    }

    fn runtime(&mut self, event: RuntimeEvent) -> Vec<Effect> {
        match event {
            RuntimeEvent::SessionCreated { session } => {
                self.listing += 1;
                self.upsert(session);
            }
            RuntimeEvent::SessionChanged { session } => self.upsert(session),
            RuntimeEvent::SessionDeleted { id } => {
                self.listing += 1;
                self.sessions.retain(|session| session.id != id);
                self.cursor = self.cursor.min(self.sessions.len());
                if self.selected == Some(id) {
                    self.open(None);
                }
            }
            RuntimeEvent::AnswerStarted { session_id } => {
                self.set_answering(session_id, true);
                if self.selected == Some(session_id) {
                    self.running_set = self.change();
                    return vec![Effect::Follow(session_id)];
                }
            }
            RuntimeEvent::AnswerEnded {
                session_id,
                context,
            } => {
                self.set_answering(session_id, false);
                if self.selected == Some(session_id) {
                    self.running_set = self.change();
                    self.context = context.or(self.context);
                }
            }
            RuntimeEvent::ProviderChanged => return vec![Effect::LoadProvider],
            RuntimeEvent::ConnectionsChanged => return vec![Effect::LoadConnections],
            RuntimeEvent::Resync => {
                let read = self.selected.map(|id| self.read(id));
                return [
                    Effect::LoadProvider,
                    Effect::LoadSessions(self.listing),
                    Effect::LoadConnections,
                ]
                .into_iter()
                .chain(read)
                .collect();
            }
            RuntimeEvent::Stopping => return vec![Effect::Exit(Exit::Stopped)],
            RuntimeEvent::Other => {}
        }
        Vec::new()
    }

    /// Lists `session`, unless the copy listed is newer.
    fn upsert(&mut self, session: Session) {
        match self
            .sessions
            .iter_mut()
            .find(|known| known.id == session.id)
        {
            Some(known) if session.supersedes(known) => *known = session,
            Some(_) => return,
            None => self.sessions.push(session),
        }
        self.sort();
    }

    /// Most recent first, as in the Web UI, keeping the cursor on the open conversation.
    fn sort(&mut self) {
        self.sessions.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        if let Some(index) = self
            .selected
            .and_then(|id| self.sessions.iter().position(|session| session.id == id))
        {
            self.cursor = index + 1;
        }
        self.cursor = self.cursor.min(self.sessions.len());
    }
}

/// A message's state, when it is not simply complete.
pub fn status_note(message: &Message) -> Option<&'static str> {
    match message.status {
        Status::Complete | Status::Streaming => None,
        Status::Interrupted => Some("interrupted"),
        Status::Failed => Some("failed"),
        Status::Refused => Some("refused by the service"),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn control(char: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(char), KeyModifiers::CONTROL)
    }

    pub(crate) fn session(id: i64, updated_at: i64) -> Session {
        serde_json::from_value(json!({
            "id": id, "title": format!("Conversation {id}"), "explanations": false, "access": "read_only",
            "approval": "ask", "created_at": 1, "updated_at": updated_at, "revision": 0,
        }))
        .unwrap()
    }

    pub(crate) fn message(id: i64, role: Role, content: &str) -> Message {
        serde_json::from_value(json!({
            "id": id, "session_id": 1, "role": role, "content": content, "status": "complete", "model": null,
            "created_at": 1, "actions": [],
        }))
        .unwrap()
    }

    /// An action waiting for a decision, as the runtime sends it, with `extra` fields.
    pub(crate) fn waiting(id: i64, extra: serde_json::Value) -> Action {
        let mut action = json!({
            "id": id, "session_id": 1, "message_id": null, "step": 0, "position": 0, "tool": "run_command",
            "reason": "To see", "summary": "Run a command in /", "command": "uptime", "state": "waiting",
            "exit_code": null, "output": "", "error": null, "created_at": 1, "duration_ms": null,
            "started_at": null, "change": null, "phase": null, "plan": null, "plan_step": null,
        });
        action
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(action).unwrap()
    }

    #[test]
    fn a_first_message_creates_a_conversation_with_the_chosen_modes() {
        let mut app = App::new(Banner::default(), true);
        assert_eq!(
            app.start()[..2],
            [Effect::LoadProvider, Effect::LoadSessions(0)]
        );
        app.key(key(KeyCode::F(2)));
        assert_eq!(app.modes().access, Access::Full);
        for char in "uptime?".chars() {
            app.key(key(KeyCode::Char(char)));
        }
        let effects = app.key(key(KeyCode::Enter));
        assert_eq!(
            effects,
            [Effect::Send {
                session: None,
                content: "uptime?".into(),
                modes: json!({ "explanations": false, "access": "full", "approval": "ask" }),
            }]
        );
        assert!(app.input.is_empty());
        app.apply(Update::Created(session(7, 1)));
        assert_eq!(app.selected, Some(7));
        assert!(app.running());
    }

    #[test]
    fn up_and_down_bring_back_the_messages_sent_in_the_open_conversation() {
        let mut app = App::new(Banner::default(), true);
        app.selected = Some(1);
        app.messages = vec![
            message(1, Role::User, "uptime?"),
            message(2, Role::Assistant, "Up 3 days."),
            message(3, Role::User, "df -h?"),
        ];
        for char in "draft".chars() {
            app.key(key(KeyCode::Char(char)));
        }
        app.key(key(KeyCode::Up));
        assert_eq!(app.input, "df -h?");
        app.key(key(KeyCode::Up));
        assert_eq!(app.input, "uptime?");
        app.key(key(KeyCode::Down));
        app.key(key(KeyCode::Down));
        assert_eq!(app.input, "draft");
        // Another conversation has its own.
        app.key(key(KeyCode::Up));
        app.apply(Update::Created(session(7, 1)));
        assert!(app.key(key(KeyCode::Down)).is_empty());
        assert_eq!(app.input, "df -h?");
    }

    #[test]
    fn a_message_kelpy_would_refuse_is_kept_and_why_is_said() {
        let mut app = App::new(Banner::default(), true);
        app.apply(Update::Provider(ProviderState {
            label: "No AI provider".into(),
            problem: Some("choose one in the Web UI or run `kelpy provider`".into()),
            configured: false,
        }));
        for char in "uptime?".chars() {
            app.key(key(KeyCode::Char(char)));
        }
        assert!(app.key(key(KeyCode::Enter)).is_empty());
        assert_eq!(app.input, "uptime?");
        assert_eq!(app.selected, None);
        assert_eq!(
            app.notice.as_deref(),
            Some(
                "No AI provider configured yet: choose one in the Web UI or run 'kelpy provider'."
            )
        );
    }

    #[test]
    fn a_message_refused_leaves_no_answer_to_wait_for() {
        let mut app = App::new(Banner::default(), true);
        for char in "uptime?".chars() {
            app.key(key(KeyCode::Char(char)));
        }
        app.key(key(KeyCode::Enter));
        app.apply(Update::Created(session(7, 1)));
        assert!(app.running());
        let effects = app.apply(Update::NotSent {
            session: 7,
            content: "uptime?".into(),
            problem: "no AI provider is configured yet".into(),
        });
        assert!(!app.running());
        // Read again, for what Kelpy says of it.
        assert!(matches!(effects[..], [Effect::Open { id: 7, .. }]));
        assert_eq!(app.input, "uptime?");
        assert_eq!(
            app.notice.as_deref(),
            Some("Not sent: no AI provider is configured yet.")
        );
        // Sent again, not held back by an answer that never began.
        assert!(matches!(
            app.key(key(KeyCode::Enter))[..],
            [Effect::Send {
                session: Some(7),
                ..
            }]
        ));
    }

    #[test]
    fn a_message_refused_leaves_the_answer_another_interface_began() {
        let mut app = App::new(Banner::default(), true);
        app.apply(Update::Sessions {
            listing: 0,
            sessions: vec![session(3, 5)],
        });
        app.key(control('r'));
        app.key(key(KeyCode::Down));
        let [Effect::Open { read, since, .. }] = app.key(key(KeyCode::Enter))[..] else {
            panic!("not opened");
        };
        let idle = Conversation {
            messages: Vec::new(),
            running: false,
            context: None,
        };
        app.apply(Update::Conversation {
            id: 3,
            read,
            since,
            conversation: idle,
        });
        for char in "uptime?".chars() {
            app.key(key(KeyCode::Char(char)));
        }
        assert!(matches!(
            app.key(key(KeyCode::Enter))[..],
            [Effect::Send {
                session: Some(3),
                ..
            }]
        ));

        // Another interface began an answer meanwhile, and this message is refused for it.
        app.apply(Update::Runtime(RuntimeEvent::AnswerStarted {
            session_id: 3,
        }));
        let effects = app.apply(Update::NotSent {
            session: 3,
            content: "uptime?".into(),
            problem: "an answer is being written".into(),
        });
        assert!(app.running());
        assert_eq!(app.input, "uptime?");
        // Read again: what Kelpy says of it is taken.
        let [Effect::Open { id: 3, read, since }] = effects[..] else {
            panic!("not read again: {effects:?}");
        };
        let effects = app.apply(Update::Conversation {
            id: 3,
            read,
            since,
            conversation: Conversation {
                messages: Vec::new(),
                running: true,
                context: None,
            },
        });
        assert!(app.running());
        assert_eq!(effects, [Effect::Follow(3)]);
    }

    #[test]
    fn an_answer_streams_into_the_open_conversation_and_its_action_is_decided() {
        let mut app = App::new(Banner::default(), true);
        app.apply(Update::Sessions {
            listing: 0,
            sessions: vec![session(1, 5), session(2, 9)],
        });
        assert_eq!(app.sessions[0].id, 2);
        app.key(control('r'));
        app.key(key(KeyCode::Down));
        assert_eq!(
            app.key(key(KeyCode::Enter)),
            [Effect::Open {
                id: 2,
                read: 1,
                since: 0
            }]
        );
        assert_eq!(app.focus, Focus::Message);

        app.apply(Update::Chat {
            session: 2,
            event: ChatEvent::User {
                message: message(10, Role::User, "Disk?"),
            },
        });
        app.apply(Update::Chat {
            session: 2,
            event: ChatEvent::Started {
                message: message(11, Role::Assistant, ""),
            },
        });
        app.apply(Update::Chat {
            session: 2,
            event: ChatEvent::Delta {
                text: "Look".into(),
            },
        });
        app.apply(Update::Chat {
            session: 2,
            event: ChatEvent::Action {
                action: Box::new(waiting(3, json!({}))),
            },
        });
        assert_eq!(app.messages[1].content, "Look");
        assert!(app.running());
        // Sending waits for the answer; the action is decided with a key.
        app.key(key(KeyCode::Char('x')));
        assert!(app.key(key(KeyCode::Enter)).is_empty());
        assert!(app.notice.is_some());
        assert_eq!(
            app.key(control('y')),
            [Effect::Decide {
                session: 2,
                action: 3,
                approve: true
            }]
        );
        assert_eq!(app.key(key(KeyCode::Esc)), [Effect::Cancel(2)]);
        // Another conversation's answer does not show here.
        app.apply(Update::Chat {
            session: 1,
            event: ChatEvent::Delta {
                text: "elsewhere".into(),
            },
        });
        assert_eq!(app.messages[1].content, "Look");
    }

    #[test]
    fn quitting_asks_only_while_an_answer_would_be_cut_short() {
        let mut app = App::new(Banner::default(), true);
        assert_eq!(app.key(control('c')), [Effect::Exit(Exit::Quit)]);
        app.apply(Update::Runtime(RuntimeEvent::AnswerStarted {
            session_id: 4,
        }));
        assert!(app.key(control('c')).is_empty());
        assert!(app.confirming_quit);
        assert!(app.key(key(KeyCode::Char('n'))).is_empty());
        app.key(control('q'));
        assert_eq!(app.key(key(KeyCode::Char('y'))), [Effect::Exit(Exit::Quit)]);

        // A terminal UI attached later quits without asking: Kelpy goes on.
        let mut attached = App::new(Banner::default(), false);
        attached.apply(Update::Runtime(RuntimeEvent::AnswerStarted {
            session_id: 4,
        }));
        assert_eq!(attached.key(control('c')), [Effect::Exit(Exit::Quit)]);
        assert_eq!(
            attached.apply(Update::Runtime(RuntimeEvent::Stopping)),
            [Effect::Exit(Exit::Stopped)]
        );
    }

    #[test]
    fn an_answer_started_elsewhere_in_the_open_conversation_is_followed() {
        let mut app = App::new(Banner::default(), false);
        app.apply(Update::Sessions {
            listing: 0,
            sessions: vec![session(5, 1)],
        });
        app.key(control('r'));
        app.key(key(KeyCode::Down));
        app.key(key(KeyCode::Enter));
        assert_eq!(
            app.apply(Update::Runtime(RuntimeEvent::AnswerStarted {
                session_id: 5
            })),
            [Effect::Follow(5)]
        );
        assert!(
            app.apply(Update::Runtime(RuntimeEvent::AnswerStarted {
                session_id: 6
            }))
            .is_empty()
        );
        // Changing a mode of an open conversation goes to Kelpy.
        assert!(matches!(
            &app.key(key(KeyCode::F(3)))[..],
            [Effect::Change { session: 5, .. }]
        ));
    }

    #[test]
    fn a_connection_waiting_is_decided_before_an_action() {
        let mut app = App::new(Banner::default(), true);
        assert_eq!(
            app.apply(Update::Runtime(RuntimeEvent::ConnectionsChanged)),
            [Effect::LoadConnections]
        );
        app.apply(Update::Connections(vec![Connection {
            id: 3,
            address: "203.0.113.5:51234".into(),
            user: "root".into(),
            key: "ssh-ed25519 SHA256:abc".into(),
        }]));
        assert_eq!(
            app.key(control('n')),
            [Effect::DecideConnection {
                id: 3,
                allow: false
            }]
        );
        app.apply(Update::Connections(Vec::new()));
        assert!(app.key(control('y')).is_empty());
    }

    #[test]
    fn an_older_copy_of_a_conversation_never_replaces_a_newer_one() {
        let mut app = App::new(Banner::default(), false);
        let mut full = session(5, 1);
        full.access = Access::Full;
        full.revision = 2;
        app.apply(Update::Runtime(RuntimeEvent::SessionChanged {
            session: full.clone(),
        }));
        // The answer to an earlier change arrives after the event of a later one.
        let mut earlier = session(5, 1);
        earlier.revision = 1;
        app.apply(Update::Session(earlier.clone()));
        app.apply(Update::Sessions {
            listing: 0,
            sessions: vec![earlier],
        });
        assert_eq!(app.session(5), Some(&full));
        // A newer copy replaces it.
        full.revision = 3;
        full.access = Access::None;
        app.apply(Update::Session(full.clone()));
        assert_eq!(app.session(5).unwrap().access, Access::None);
    }

    #[test]
    fn a_list_read_before_a_conversation_was_added_or_deleted_is_read_again() {
        let mut app = App::new(Banner::default(), false);
        let [_, read, _] = &app.start()[..] else {
            panic!()
        };
        assert_eq!(read, &Effect::LoadSessions(0));
        // Created while the list was read: the list read lacks it.
        app.apply(Update::Runtime(RuntimeEvent::SessionCreated {
            session: session(9, 5),
        }));
        assert_eq!(
            app.apply(Update::Sessions {
                listing: 0,
                sessions: vec![session(1, 1)],
            }),
            [Effect::LoadSessions(1)]
        );
        assert_eq!(app.session(9).map(|known| known.id), Some(9));
        assert!(app.session(1).is_none());

        // Deleted while the list was read: it does not come back.
        app.apply(Update::Sessions {
            listing: 1,
            sessions: vec![session(1, 1), session(9, 5)],
        });
        app.apply(Update::Runtime(RuntimeEvent::SessionDeleted { id: 1 }));
        app.apply(Update::Sessions {
            listing: 1,
            sessions: vec![session(1, 1), session(9, 5)],
        });
        assert!(app.session(1).is_none());
    }

    #[test]
    fn a_conversation_read_before_its_answer_streamed_keeps_what_the_stream_brought() {
        let mut app = App::new(Banner::default(), false);
        app.apply(Update::Sessions {
            listing: 0,
            sessions: vec![session(2, 1)],
        });
        app.key(control('r'));
        app.key(key(KeyCode::Down));
        let [Effect::Open { id: 2, read, since }] = app.key(key(KeyCode::Enter))[..] else {
            panic!()
        };
        // The answer starts and streams while the read is on its way.
        app.apply(Update::Runtime(RuntimeEvent::AnswerStarted {
            session_id: 2,
        }));
        app.apply(Update::Chat {
            session: 2,
            event: ChatEvent::Started {
                message: message(11, Role::Assistant, ""),
            },
        });
        app.apply(Update::Chat {
            session: 2,
            event: ChatEvent::Delta {
                text: "Look".into(),
            },
        });
        // The read, from before: no answer yet, nothing running.
        let effects = app.apply(Update::Conversation {
            id: 2,
            read,
            since,
            conversation: Conversation {
                messages: vec![message(10, Role::User, "Disk?")],
                running: false,
                context: None,
            },
        });
        assert_eq!(effects, [Effect::Follow(2)]);
        assert!(app.running());
        let shown: Vec<(i64, &str)> = app
            .messages
            .iter()
            .map(|message| (message.id, message.content.as_str()))
            .collect();
        assert_eq!(shown, [(10, "Disk?"), (11, "Look")]);

        // A read from after the answer ended is taken as it is.
        let ended = ContextUse {
            used: 900,
            window: 4096,
        };
        app.apply(Update::Runtime(RuntimeEvent::AnswerEnded {
            session_id: 2,
            context: Some(ended),
        }));
        assert_eq!(app.context, Some(ended));
        let since = app.changes;
        app.apply(Update::Conversation {
            id: 2,
            read,
            since,
            conversation: Conversation {
                messages: vec![
                    message(10, Role::User, "Disk?"),
                    message(11, Role::Assistant, "Look at /var."),
                ],
                running: false,
                context: None,
            },
        });
        assert!(!app.running());
        assert_eq!(app.messages[1].content, "Look at /var.");
    }

    #[test]
    fn a_read_from_an_earlier_opening_of_the_conversation_is_left_out() {
        let mut app = App::new(Banner::default(), false);
        app.apply(Update::Sessions {
            listing: 0,
            sessions: vec![session(2, 9), session(3, 5)],
        });
        app.key(control('r'));
        app.key(key(KeyCode::Down));
        let [
            Effect::Open {
                id: 2,
                read: first,
                since: first_since,
            },
        ] = app.key(key(KeyCode::Enter))[..]
        else {
            panic!()
        };
        // Conversation 3, then 2 again, before the first read of 2 answers.
        app.key(control('r'));
        app.key(key(KeyCode::Down));
        app.key(key(KeyCode::Enter));
        app.key(control('r'));
        app.key(key(KeyCode::Up));
        let [Effect::Open { id: 2, read, since }] = app.key(key(KeyCode::Enter))[..] else {
            panic!()
        };
        assert_ne!(read, first);
        let conversation = |content: &str| Conversation {
            messages: vec![
                message(10, Role::User, "Disk?"),
                message(11, Role::Assistant, content),
            ],
            running: false,
            context: None,
        };
        app.apply(Update::Conversation {
            id: 2,
            read,
            since,
            conversation: conversation("Look at /var."),
        });
        // The first read answers last: it is out of date.
        assert!(
            app.apply(Update::Conversation {
                id: 2,
                read: first,
                since: first_since,
                conversation: conversation("Loo"),
            })
            .is_empty()
        );
        assert_eq!(app.messages[1].content, "Look at /var.");
    }

    #[test]
    fn ctrl_o_opens_the_web_ui_only_where_this_computer_has_a_browser() {
        let banner = Banner {
            web_ui: "http://127.0.0.1:4321/#code=K7QM-4ZP2".into(),
            ..Banner::default()
        };
        let mut app = App::new(banner, false);
        // Over SSH it would open on the server: the key does nothing, and types nothing.
        assert!(app.key(control('o')).is_empty());
        assert_eq!(app.input, "");
        app.opens_links = true;
        assert!(matches!(
            &app.key(control('o'))[..],
            [Effect::OpenLink(url)] if url == "http://127.0.0.1:4321/#code=K7QM-4ZP2"
        ));
    }

    #[test]
    fn the_keys_of_the_screens_open_the_panel_the_list_and_a_recent_conversation() {
        let mut app = App::new(Banner::default(), false);
        app.apply(Update::Sessions {
            listing: 0,
            sessions: vec![session(2, 9), session(3, 5)],
        });
        // Ctrl+B hides the panel where it fits beside the conversation, and opens it over it where it does not.
        app.key(control('b'));
        assert!(app.panel_hidden && !app.overlay);
        app.resize(100);
        app.key(control('b'));
        assert!(app.overlay);
        assert!(app.key(key(KeyCode::Esc)).is_empty());
        assert!(!app.overlay);
        // Shift+Tab changes the approval.
        app.key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(app.modes().approval, Approval::Plan);
        // On the start screen a number opens that recent conversation; one with none is typed.
        assert!(app.key(key(KeyCode::Char('3'))).is_empty());
        assert_eq!(app.input, "3");
        app.input.clear();
        assert!(matches!(
            app.key(key(KeyCode::Char('2')))[..],
            [Effect::Open { id: 3, .. }]
        ));
        // Alt+Enter starts a new line; Enter sends.
        app.key(key(KeyCode::Char('a')));
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        app.key(key(KeyCode::Char('b')));
        assert_eq!(app.input, "a\nb");
        // Ctrl+R lists the conversations; Esc goes back.
        app.key(control('r'));
        assert_eq!(app.focus, Focus::Conversations);
        app.key(key(KeyCode::Esc));
        assert_eq!(app.focus, Focus::Message);
    }
}
