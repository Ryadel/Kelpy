//! Kelpy storage: conversations, their messages, the actions their answers took on the system and the summaries of
//! their older parts, in one SQLite database inside the Kelpy home.
//!
//! The database runs in WAL mode, so stopping Kelpy at any moment leaves it consistent. An answer still being
//! written when the process died is marked `interrupted` the next time the store opens, and an action still waiting
//! or running `failed`.
//!
//! [`Store`] is the database itself, with blocking calls. [`Storage`] serves it from a thread of its own, so that
//! the async parts of Kelpy reach it without ever blocking their workers.

use std::collections::HashMap;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::{io, thread};

use kelpy_common::{lock, unix_now};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

/// Each entry upgrades the schema by one version, recorded in SQLite's `user_version`.
const MIGRATIONS: &[&str] = &[
    "
    CREATE TABLE sessions (
        id         INTEGER PRIMARY KEY,
        title      TEXT    NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE messages (
        id         INTEGER PRIMARY KEY,
        session_id INTEGER NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
        role       TEXT    NOT NULL,
        content    TEXT    NOT NULL,
        status     TEXT    NOT NULL,
        model      TEXT,
        created_at INTEGER NOT NULL
    );
    CREATE INDEX messages_by_session ON messages (session_id, id);
",
    "ALTER TABLE sessions ADD COLUMN explanations INTEGER NOT NULL DEFAULT 0;",
    // Ids are never reused: an answer still finishing in a deleted conversation must not reach a newer one that
    // got the same id. SQLite cannot add AUTOINCREMENT to a table, so both tables are rebuilt.
    "
    CREATE TABLE sessions_new (
        id           INTEGER PRIMARY KEY AUTOINCREMENT,
        title        TEXT    NOT NULL,
        explanations INTEGER NOT NULL DEFAULT 0,
        created_at   INTEGER NOT NULL,
        updated_at   INTEGER NOT NULL
    );
    INSERT INTO sessions_new SELECT id, title, explanations, created_at, updated_at FROM sessions;
    CREATE TABLE messages_new (
        id         INTEGER PRIMARY KEY AUTOINCREMENT,
        session_id INTEGER NOT NULL REFERENCES sessions_new (id) ON DELETE CASCADE,
        role       TEXT    NOT NULL,
        content    TEXT    NOT NULL,
        status     TEXT    NOT NULL,
        model      TEXT,
        created_at INTEGER NOT NULL
    );
    INSERT INTO messages_new SELECT id, session_id, role, content, status, model, created_at FROM messages;
    DROP TABLE messages;
    DROP TABLE sessions;
    ALTER TABLE sessions_new RENAME TO sessions;
    ALTER TABLE messages_new RENAME TO messages;
    CREATE INDEX messages_by_session ON messages (session_id, id);
",
    // What each answer's request took, and the summary the model reads in place of a conversation's older part.
    "
    ALTER TABLE messages ADD COLUMN context_estimate INTEGER;
    ALTER TABLE messages ADD COLUMN input_tokens INTEGER;
    CREATE TABLE summaries (
        session_id INTEGER PRIMARY KEY REFERENCES sessions (id) ON DELETE CASCADE,
        through_id INTEGER NOT NULL,
        content    TEXT    NOT NULL,
        model      TEXT    NOT NULL,
        created_at INTEGER NOT NULL
    );
",
    // What Kelpy may do on the system in each conversation, and the actions its answers took: the Command log.
    "
    ALTER TABLE sessions ADD COLUMN access TEXT NOT NULL DEFAULT 'read_only';
    ALTER TABLE sessions ADD COLUMN approval TEXT NOT NULL DEFAULT 'ask';
    CREATE TABLE actions (
        id          INTEGER PRIMARY KEY AUTOINCREMENT,
        message_id  INTEGER NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
        step        INTEGER NOT NULL,
        position    INTEGER NOT NULL,
        call_id     TEXT    NOT NULL,
        tool        TEXT    NOT NULL,
        input       TEXT    NOT NULL,
        reason      TEXT    NOT NULL,
        summary     TEXT    NOT NULL,
        command     TEXT,
        state       TEXT    NOT NULL,
        exit_code   INTEGER,
        output      TEXT    NOT NULL DEFAULT '',
        error       TEXT,
        created_at  INTEGER NOT NULL,
        duration_ms INTEGER
    );
    CREATE INDEX actions_by_message ON actions (message_id, id);
",
    // 6: when an action started running, for the time shown while it runs.
    "ALTER TABLE actions ADD COLUMN started_at INTEGER;",
    // 7: the change an action makes, as its card shows it (JSON).
    "ALTER TABLE actions ADD COLUMN change TEXT;",
    // 8: a number each change of a conversation raises, so that an older copy is never taken for a newer one.
    "ALTER TABLE sessions ADD COLUMN revision INTEGER NOT NULL DEFAULT 0;",
    // 9: actions belong to their conversation, and those the administrator asks for to no answer; the phase of a
    // change under way, the plan an action proposes and the plan step it carries out. What Kelpy changed and
    // verified on the server, and the differences found since: kept when a conversation is deleted.
    "
    CREATE TABLE actions_new (
        id            INTEGER PRIMARY KEY AUTOINCREMENT,
        session_id    INTEGER NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
        message_id    INTEGER REFERENCES messages (id) ON DELETE CASCADE,
        step          INTEGER NOT NULL,
        position      INTEGER NOT NULL,
        call_id       TEXT    NOT NULL,
        tool          TEXT    NOT NULL,
        input         TEXT    NOT NULL,
        reason        TEXT    NOT NULL,
        summary       TEXT    NOT NULL,
        command       TEXT,
        state         TEXT    NOT NULL,
        exit_code     INTEGER,
        output        TEXT    NOT NULL DEFAULT '',
        error         TEXT,
        created_at    INTEGER NOT NULL,
        duration_ms   INTEGER,
        started_at    INTEGER,
        change        TEXT,
        after_message INTEGER,
        phase         TEXT,
        plan          TEXT,
        plan_id       INTEGER,
        plan_step     INTEGER
    );
    INSERT INTO actions_new (id, session_id, message_id, step, position, call_id, tool, input, reason, summary,
                             command, state, exit_code, output, error, created_at, duration_ms, started_at, change)
        SELECT actions.id, messages.session_id, message_id, step, position, call_id, tool, input, reason, summary,
               command, state, exit_code, output, error, actions.created_at, duration_ms, started_at, change
        FROM actions JOIN messages ON messages.id = actions.message_id;
    DELETE FROM sqlite_sequence WHERE name = 'actions_new';
    INSERT INTO sqlite_sequence (name, seq) SELECT 'actions_new', seq FROM sqlite_sequence WHERE name = 'actions';
    DROP TABLE actions;
    ALTER TABLE actions_new RENAME TO actions;
    CREATE INDEX actions_by_message ON actions (message_id, id);
    CREATE INDEX actions_by_session ON actions (session_id, id);
    CREATE TABLE records (
        id          INTEGER PRIMARY KEY AUTOINCREMENT,
        kind        TEXT    NOT NULL,
        subject     TEXT    NOT NULL,
        expected    TEXT    NOT NULL,
        content     TEXT,
        secret      INTEGER NOT NULL DEFAULT 0,
        action_id   INTEGER REFERENCES actions (id) ON DELETE SET NULL,
        recorded_at INTEGER NOT NULL,
        UNIQUE (kind, subject)
    );
    CREATE TABLE differences (
        id         INTEGER PRIMARY KEY AUTOINCREMENT,
        record_id  INTEGER NOT NULL REFERENCES records (id) ON DELETE CASCADE,
        found      TEXT    NOT NULL,
        content    TEXT,
        found_at   INTEGER NOT NULL,
        settled    TEXT,
        settled_at INTEGER
    );
",
];

const SESSION_COLUMNS: &str =
    "id, title, explanations, created_at, updated_at, access, approval, revision";
const ACTION_COLUMNS: &str = "id, message_id, step, position, call_id, tool, input, reason, summary, command, state, \
     exit_code, output, error, created_at, duration_ms, started_at, change, session_id, after_message, phase, plan, \
     plan_id, plan_step";
const RECORD_COLUMNS: &str = "id, kind, subject, expected, content, action_id, recorded_at, secret";
const MESSAGE_COLUMNS: &str =
    "id, session_id, role, content, status, model, created_at, context_estimate, input_tokens";

/// The title of a conversation until its first message names it.
pub const UNTITLED: &str = "New conversation";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("conversation {0} not found")]
    SessionNotFound(i64),
    /// The message is gone, typically because its conversation was deleted.
    #[error("message {0} not found")]
    MessageNotFound(i64),
    #[error("action {0} not found")]
    ActionNotFound(i64),
    #[error("difference {0} not found")]
    DifferenceNotFound(i64),
    #[error("the database was created by a newer version of Kelpy (schema {0})")]
    TooNew(i64),
    #[error("cannot start the database thread: {0}")]
    Start(#[from] io::Error),
    /// The database thread is gone, or a call failed unexpectedly on it.
    #[error("the database is not available")]
    Unavailable,
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// A conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Session {
    pub id: i64,
    pub title: String,
    /// Answers explain each step in plain words, instead of staying concise.
    pub explanations: bool,
    pub access: Access,
    pub approval: Approval,
    /// Unix time, in seconds.
    pub created_at: i64,
    /// Unix time of the last message, in seconds.
    pub updated_at: i64,
    /// Raised by every change of the conversation, in the database's own order: of two copies, the one with the
    /// higher revision is the newer, however late it arrives.
    pub revision: i64,
}

/// How a conversation's answers are written and what they may do: chosen before the first message, changed at any
/// time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Modes {
    pub explanations: bool,
    pub access: Access,
    pub approval: Approval,
}

/// What Kelpy may do on the system.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    /// Kelpy only answers.
    None,
    /// Kelpy may look at the system, changing nothing.
    #[default]
    ReadOnly,
    /// Kelpy may also change the system.
    Full,
}

/// When Kelpy asks before acting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Approval {
    /// Every action waits for approval.
    #[default]
    Ask,
    /// A plan of changes is approved once.
    Plan,
    /// Only risky changes ask.
    Auto,
}

/// Where an action is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionState {
    /// Waiting for the administrator's approval.
    Waiting,
    Running,
    /// It ran to its end; its exit code tells how it went.
    Done,
    /// It could not run, or did not end: `error` says why.
    Failed,
    /// The administrator refused it, or it was not allowed.
    Denied,
    /// Kelpy stopped while it was changing the system: what it left is not known. Never run again by itself.
    Uncertain,
}

/// What Kelpy did on the system, or was asked to, as the Command log shows it: a call of a tool by the model in an
/// answer, or an operation the administrator asked for, such as putting a file back as it was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Action {
    pub id: i64,
    pub session_id: i64,
    /// The answer whose model called it; `None` for an operation the administrator asked for.
    pub message_id: Option<i64>,
    /// For the administrator's operations, the last message of the conversation when it was asked for: the model is
    /// told of it with the next message.
    #[serde(skip)]
    pub after_message: Option<i64>,
    /// The model's step in the answer, from 0: the calls of one step were asked for together.
    pub step: u32,
    /// Where in the answer's text the model asked for it, in bytes.
    pub position: usize,
    /// How the model names the call.
    #[serde(skip)]
    pub call_id: String,
    pub tool: String,
    /// The arguments, as the model wrote them.
    #[serde(skip)]
    pub input: String,
    /// Why the model asked for it.
    pub reason: String,
    /// What it does, in one line.
    pub summary: String,
    /// The commands it runs, one per line; `None` for an action that is not a command, such as reading a file.
    pub command: Option<String>,
    pub state: ActionState,
    pub exit_code: Option<i32>,
    pub output: String,
    pub error: Option<String>,
    /// Unix time, in seconds.
    pub created_at: i64,
    /// How long it ran, once it has.
    pub duration_ms: Option<u64>,
    /// When it started running, Unix time in seconds.
    pub started_at: Option<i64>,
    /// The change it makes to the system, for an action that changes something.
    pub change: Option<Change>,
    /// What a change is doing while it runs, such as "writing the file": if Kelpy stops then, its outcome is
    /// uncertain.
    pub phase: Option<String>,
    /// The plan it proposes, for a plan.
    pub plan: Option<Plan>,
    /// The plan, and the step of it, this action carries out.
    pub plan_step: Option<PlanStepRef>,
}

/// A change to the system an action makes, as its card shows it before it runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// What changes, such as a file's unified diff, or the packages a package manager would install.
    pub diff: String,
    /// What Kelpy checks and how the change is undone, one line each.
    pub notes: Vec<String>,
    /// Why it waits for approval in every Approval mode, if it does.
    pub floor: Option<String>,
    /// The file it changes, links resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Where the version of the file before the change was saved, once it ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
}

/// Changes the model proposes to make in order, approved together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// What the plan is for.
    pub goal: String,
    pub steps: Vec<PlanStep>,
}

/// A step of a plan: one call of a tool, with its arguments, as approved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    pub tool: String,
    /// The arguments without the reason, as canonical JSON: a call carries out the step only with these.
    pub input: String,
    pub summary: String,
    /// The commands it runs, or the file it changes.
    pub detail: Option<String>,
    /// Why it would ask outside a plan, if it would: approved with the plan, and marked.
    pub floor: Option<String>,
}

/// A step of a plan: the plan's action, and the step's index in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PlanStepRef {
    pub plan_id: i64,
    pub step: u32,
}

/// An action about to be recorded.
#[derive(Debug, Clone)]
pub struct NewAction<'a> {
    pub session_id: i64,
    /// `None` for an operation the administrator asked for.
    pub message_id: Option<i64>,
    pub step: u32,
    pub position: usize,
    pub call_id: &'a str,
    pub tool: &'a str,
    pub input: &'a str,
    pub reason: &'a str,
    pub summary: &'a str,
    pub command: Option<&'a str>,
    pub change: Option<&'a Change>,
    pub plan: Option<&'a Plan>,
    pub plan_step: Option<PlanStepRef>,
    pub state: ActionState,
    pub error: Option<&'a str>,
}

/// What Kelpy found on the server after changing it, and expects to find again: a file's content, a service's
/// state, a package's version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Record {
    pub id: i64,
    pub kind: RecordKind,
    /// The file's path, the service's or the package's name.
    pub subject: String,
    /// What is expected: a file's digest, a service's state such as `active, enabled`, a package's version or
    /// `absent`.
    pub expected: String,
    /// A file's content, when it may be shown: not for files that may hold secrets, nor for large ones.
    #[serde(skip)]
    pub content: Option<String>,
    /// The action that made the change, while its conversation exists.
    pub action_id: Option<i64>,
    pub recorded_at: i64,
    /// Whether the file holds a secret Kelpy wrote: never read for the model nor shown.
    pub secret: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordKind {
    File,
    Service,
    Package,
}

/// A difference between a record and what Kelpy found later: a change made outside Kelpy, shown and explained,
/// never reverted by itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Difference {
    pub id: i64,
    pub record: Record,
    /// What was found, in the record's terms; `missing` for a file gone.
    pub found: String,
    /// A file's content as found, when it may be shown.
    #[serde(skip)]
    pub content: Option<String>,
    pub found_at: i64,
}

/// How the administrator settled a difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Settled {
    /// What was found is kept, and expected from now on.
    Accepted,
    /// Kelpy's version was put back.
    PutBack,
    /// Kelpy changed it again, which replaced the record.
    Replaced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Complete,
    /// The answer is still being written.
    Streaming,
    /// Stopped by the user, or by the end of the process: the content is partial.
    Interrupted,
    /// The provider failed: the content holds what arrived before the error.
    Failed,
    /// The model declined to answer.
    Refused,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Message {
    pub id: i64,
    pub session_id: i64,
    pub role: Role,
    pub content: String,
    pub status: Status,
    /// The model that wrote an assistant message.
    pub model: Option<String>,
    /// Unix time, in seconds.
    pub created_at: i64,
    /// For an answer, the tokens Kelpy estimated its request would take.
    #[serde(skip)]
    pub context_estimate: Option<u64>,
    /// For an answer, the tokens its request took, as the provider counted them.
    #[serde(skip)]
    pub input_tokens: Option<u64>,
    /// For an answer, what it did on the system, in order.
    pub actions: Vec<Action>,
}

/// What the model reads in place of a conversation's older messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    /// The id of the last message it covers: the messages after it are given to the model as they are.
    pub through: i64,
    pub content: String,
}

pub struct Store {
    connection: Connection,
}

impl Store {
    /// Opens the database at `path`, creating or upgrading it as needed.
    pub fn open(path: &Path) -> Result<Self> {
        Self::with_connection(Connection::open(path)?)
    }

    /// An empty database in memory, for tests.
    pub fn in_memory() -> Result<Self> {
        Self::with_connection(Connection::open_in_memory()?)
    }

    fn with_connection(mut connection: Connection) -> Result<Self> {
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        migrate(&mut connection)?;
        // Answers that were being written when the previous runtime stopped will never be completed.
        connection.execute(
            "UPDATE messages SET status = 'interrupted' WHERE status = 'streaming'",
            [],
        )?;
        // A change stopped in a phase may have changed part of the system: never run again by itself.
        connection.execute(
            "UPDATE actions SET state = 'uncertain',
                 error = 'Kelpy stopped while ' || phase || ': what it left is not known'
             WHERE state = 'running' AND phase IS NOT NULL",
            [],
        )?;
        connection.execute(
            "UPDATE actions SET state = 'failed', error = 'Kelpy stopped before it ended'
             WHERE state IN ('waiting', 'running')",
            [],
        )?;
        Ok(Self { connection })
    }

    /// Creates a conversation with the `modes` chosen before its first message.
    pub fn create_session(&self, title: &str, modes: Modes) -> Result<Session> {
        let now = unix_now();
        self.connection.execute(
            "INSERT INTO sessions (title, explanations, access, approval, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![
                title,
                modes.explanations,
                modes.access.as_str(),
                modes.approval.as_str(),
                now
            ],
        )?;
        Ok(Session {
            id: self.connection.last_insert_rowid(),
            title: title.to_owned(),
            explanations: modes.explanations,
            access: modes.access,
            approval: modes.approval,
            created_at: now,
            updated_at: now,
            revision: 0,
        })
    }

    /// Every conversation, most recently active first.
    pub fn sessions(&self) -> Result<Vec<Session>> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT {SESSION_COLUMNS} FROM sessions ORDER BY updated_at DESC, id DESC"
        ))?;
        let sessions = statement
            .query_map([], session_from_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(sessions)
    }

    pub fn session(&self, id: i64) -> Result<Session> {
        query_session(&self.connection, id)
    }

    pub fn rename_session(&self, id: i64, title: &str) -> Result<()> {
        let changed = self.connection.execute(
            "UPDATE sessions SET title = ?2, revision = revision + 1 WHERE id = ?1",
            params![id, title],
        )?;
        if changed == 0 {
            return Err(StoreError::SessionNotFound(id));
        }
        Ok(())
    }

    pub fn set_explanations(&self, id: i64, explanations: bool) -> Result<()> {
        let changed = self.connection.execute(
            "UPDATE sessions SET explanations = ?2, revision = revision + 1 WHERE id = ?1",
            params![id, explanations],
        )?;
        if changed == 0 {
            return Err(StoreError::SessionNotFound(id));
        }
        Ok(())
    }

    pub fn set_access(&self, id: i64, access: Access) -> Result<()> {
        self.set_session_field(id, "access", access.as_str())
    }

    pub fn set_approval(&self, id: i64, approval: Approval) -> Result<()> {
        self.set_session_field(id, "approval", approval.as_str())
    }

    fn set_session_field(&self, id: i64, column: &'static str, value: &str) -> Result<()> {
        let changed = self.connection.execute(
            &format!("UPDATE sessions SET {column} = ?2, revision = revision + 1 WHERE id = ?1"),
            params![id, value],
        )?;
        if changed == 0 {
            return Err(StoreError::SessionNotFound(id));
        }
        Ok(())
    }

    /// Records an action of an answer, or of the administrator.
    pub fn insert_action(&self, action: &NewAction<'_>) -> Result<Action> {
        let now = unix_now();
        let started_at = (action.state == ActionState::Running).then_some(now);
        let change = action.change.map(to_json);
        let plan = action.plan.map(to_json);
        let after_message: Option<i64> = match action.message_id {
            Some(_) => None,
            None => self.connection.query_row(
                "SELECT MAX(id) FROM messages WHERE session_id = ?1",
                [action.session_id],
                |row| row.get(0),
            )?,
        };
        let inserted = self.connection.execute(
            "INSERT INTO actions (session_id, message_id, step, position, call_id, tool, input, reason, summary,
                                  command, state, error, created_at, started_at, change, after_message, plan, plan_id,
                                  plan_step)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
            params![
                action.session_id,
                action.message_id,
                action.step,
                count(action.position),
                action.call_id,
                action.tool,
                action.input,
                action.reason,
                action.summary,
                action.command,
                action.state.as_str(),
                action.error,
                now,
                started_at,
                change,
                after_message,
                plan,
                action.plan_step.map(|step| step.plan_id),
                action.plan_step.map(|step| step.step)
            ],
        );
        match inserted {
            // The answer is gone with its conversation: its foreign key fails.
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                return Err(match action.message_id {
                    Some(id) => StoreError::MessageNotFound(id),
                    None => StoreError::SessionNotFound(action.session_id),
                });
            }
            inserted => inserted?,
        };
        Ok(Action {
            id: self.connection.last_insert_rowid(),
            session_id: action.session_id,
            message_id: action.message_id,
            after_message,
            step: action.step,
            position: action.position,
            call_id: action.call_id.to_owned(),
            tool: action.tool.to_owned(),
            input: action.input.to_owned(),
            reason: action.reason.to_owned(),
            summary: action.summary.to_owned(),
            command: action.command.map(str::to_owned),
            state: action.state,
            exit_code: None,
            output: String::new(),
            error: action.error.map(str::to_owned),
            created_at: now,
            duration_ms: None,
            started_at,
            change: action.change.cloned(),
            phase: None,
            plan: action.plan.cloned(),
            plan_step: action.plan_step,
        })
    }

    /// Saves where an action is: its state, exit code, output, error, start, duration, phase and change.
    pub fn update_action(&self, action: &Action) -> Result<()> {
        let changed = self.connection.execute(
            "UPDATE actions SET state = ?2, exit_code = ?3, output = ?4, error = ?5, duration_ms = ?6, started_at = ?7,
                 phase = ?8, change = ?9
             WHERE id = ?1",
            params![
                action.id,
                action.state.as_str(),
                action.exit_code,
                action.output,
                action.error,
                action.duration_ms.map(count),
                action.started_at,
                action.phase,
                action.change.as_ref().map(to_json)
            ],
        )?;
        if changed == 0 {
            return Err(match action.message_id {
                Some(id) => StoreError::MessageNotFound(id),
                None => StoreError::SessionNotFound(action.session_id),
            });
        }
        Ok(())
    }

    /// An action of a conversation.
    pub fn action(&self, session_id: i64, id: i64) -> Result<Action> {
        self.connection
            .query_row(
                &format!("SELECT {ACTION_COLUMNS} FROM actions WHERE session_id = ?1 AND id = ?2"),
                [session_id, id],
                action_from_row,
            )
            .optional()?
            .ok_or(StoreError::ActionNotFound(id))
    }

    /// The operations the administrator asked for in a conversation, oldest first.
    pub fn administrator_actions(&self, session_id: i64) -> Result<Vec<Action>> {
        self.session(session_id)?;
        query_administrator_actions(&self.connection, session_id)
    }

    /// The latest plan approved in a conversation, with the actions that carried out its steps, oldest first.
    pub fn approved_plan(&self, session_id: i64) -> Result<Option<(Action, Vec<Action>)>> {
        let plan = self
            .connection
            .query_row(
                &format!(
                    "SELECT {ACTION_COLUMNS} FROM actions
                     WHERE session_id = ?1 AND plan IS NOT NULL ORDER BY id DESC LIMIT 1"
                ),
                [session_id],
                action_from_row,
            )
            .optional()?;
        let Some(plan) = plan.filter(|plan| plan.state == ActionState::Done) else {
            return Ok(None);
        };
        let mut statement = self.connection.prepare(&format!(
            "SELECT {ACTION_COLUMNS} FROM actions WHERE plan_id = ?1 ORDER BY id"
        ))?;
        let steps = statement
            .query_map([plan.id], action_from_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(Some((plan, steps)))
    }

    /// Every action of a conversation that ran, oldest first: for its report.
    pub fn actions(&self, session_id: i64) -> Result<Vec<Action>> {
        self.session(session_id)?;
        let mut statement = self.connection.prepare(&format!(
            "SELECT {ACTION_COLUMNS} FROM actions WHERE session_id = ?1 ORDER BY id"
        ))?;
        let actions = statement
            .query_map([session_id], action_from_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(actions)
    }

    /// Records what Kelpy expects to find from now on, after a change it made and verified, in place of what it
    /// expected before. The differences still open for it are settled: the new change replaced them.
    pub fn record(
        &mut self,
        kind: RecordKind,
        subject: &str,
        expected: &str,
        content: Option<&str>,
        secret: bool,
        action_id: Option<i64>,
    ) -> Result<Record> {
        let now = unix_now();
        let transaction = self.connection.transaction()?;
        // Once a file held a secret, it is kept from the model: a later change does not make it safe.
        transaction.execute(
            "INSERT INTO records (kind, subject, expected, content, secret, action_id, recorded_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (kind, subject) DO UPDATE SET
                 expected = excluded.expected, content = excluded.content, secret = secret OR excluded.secret,
                 action_id = excluded.action_id, recorded_at = excluded.recorded_at",
            params![kind.as_str(), subject, expected, content, secret, action_id, now],
        )?;
        let record = transaction.query_row(
            &format!("SELECT {RECORD_COLUMNS} FROM records WHERE kind = ?1 AND subject = ?2"),
            params![kind.as_str(), subject],
            record_from_row,
        )?;
        transaction.execute(
            "UPDATE differences SET settled = 'replaced', settled_at = ?2 WHERE record_id = ?1 AND settled IS NULL",
            params![record.id, now],
        )?;
        transaction.commit()?;
        Ok(record)
    }

    /// Everything Kelpy expects to find on the server, oldest first.
    pub fn records(&self) -> Result<Vec<Record>> {
        let mut statement = self
            .connection
            .prepare(&format!("SELECT {RECORD_COLUMNS} FROM records ORDER BY id"))?;
        let records = statement
            .query_map([], record_from_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(records)
    }

    /// Notes that a record's subject was `found` different, unless that difference is already open: the earlier
    /// note keeps its time. A difference found otherwise replaces the open one. Returns whether it is new.
    pub fn found_different(
        &mut self,
        record_id: i64,
        found: &str,
        content: Option<&str>,
    ) -> Result<bool> {
        let transaction = self.connection.transaction()?;
        let open: Option<String> = transaction
            .query_row(
                "SELECT found FROM differences WHERE record_id = ?1 AND settled IS NULL",
                [record_id],
                |row| row.get(0),
            )
            .optional()?;
        if open.as_deref() == Some(found) {
            return Ok(false);
        }
        transaction.execute(
            "DELETE FROM differences WHERE record_id = ?1 AND settled IS NULL",
            [record_id],
        )?;
        transaction.execute(
            "INSERT INTO differences (record_id, found, content, found_at) VALUES (?1, ?2, ?3, ?4)",
            params![record_id, found, content, unix_now()],
        )?;
        transaction.commit()?;
        Ok(true)
    }

    /// Notes that a record's subject is again as expected: its open difference is gone with it.
    pub fn found_as_expected(&self, record_id: i64) -> Result<bool> {
        let removed = self.connection.execute(
            "DELETE FROM differences WHERE record_id = ?1 AND settled IS NULL",
            [record_id],
        )?;
        Ok(removed > 0)
    }

    /// The differences not settled yet, oldest first.
    pub fn differences(&self) -> Result<Vec<Difference>> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT differences.id, found, differences.content, found_at, {} FROM differences
             JOIN records ON records.id = differences.record_id
             WHERE settled IS NULL ORDER BY differences.id",
            RECORD_COLUMNS
                .split(", ")
                .map(|column| format!("records.{column}"))
                .collect::<Vec<_>>()
                .join(", ")
        ))?;
        let differences = statement
            .query_map([], |row| {
                Ok(Difference {
                    id: row.get(0)?,
                    found: row.get(1)?,
                    content: row.get(2)?,
                    found_at: row.get(3)?,
                    record: record_at(row, 4)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(differences)
    }

    /// Settles an open difference. Accepted, the record expects what was found from then on.
    pub fn settle_difference(&mut self, id: i64, settled: Settled) -> Result<()> {
        let now = unix_now();
        let transaction = self.connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE differences SET settled = ?2, settled_at = ?3 WHERE id = ?1 AND settled IS NULL",
            params![id, settled.as_str(), now],
        )?;
        if changed == 0 {
            return Err(StoreError::DifferenceNotFound(id));
        }
        if settled == Settled::Accepted {
            transaction.execute(
                "UPDATE records SET (expected, content, recorded_at) =
                     (SELECT found, content, ?2 FROM differences WHERE id = ?1)
                 WHERE id = (SELECT record_id FROM differences WHERE id = ?1)",
                params![id, now],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Deletes a conversation with all its messages.
    pub fn delete_session(&self, id: i64) -> Result<()> {
        let changed = self
            .connection
            .execute("DELETE FROM sessions WHERE id = ?1", [id])?;
        if changed == 0 {
            return Err(StoreError::SessionNotFound(id));
        }
        Ok(())
    }

    /// The messages of a conversation, oldest first.
    pub fn messages(&self, session_id: i64) -> Result<Vec<Message>> {
        self.session(session_id)?;
        query_messages(&self.connection, session_id)
    }

    /// The summary of a conversation's older messages, if it has one.
    pub fn summary(&self, session_id: i64) -> Result<Option<Summary>> {
        query_summary(&self.connection, session_id)
    }

    /// Saves the summary of a conversation's older messages, written by `model`, in place of the previous one.
    pub fn save_summary(&self, session_id: i64, summary: &Summary, model: &str) -> Result<()> {
        let saved = self.connection.execute(
            "INSERT INTO summaries (session_id, through_id, content, model, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (session_id) DO UPDATE SET
                 through_id = excluded.through_id, content = excluded.content,
                 model = excluded.model, created_at = excluded.created_at",
            params![
                session_id,
                summary.through,
                summary.content,
                model,
                unix_now()
            ],
        );
        match saved {
            // The conversation is gone: its foreign key fails.
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Err(StoreError::SessionNotFound(session_id))
            }
            saved => saved.map(drop).map_err(StoreError::from),
        }
    }

    /// Starts a turn, in one transaction: names an untitled conversation after `title` when this is its first
    /// message, stores the user's `text`, adds an empty answer that `model` is about to write, and reads the
    /// conversation as it is now.
    pub fn start_turn(
        &mut self,
        session_id: i64,
        text: &str,
        title: &str,
        model: &str,
    ) -> Result<Turn> {
        let now = unix_now();
        let transaction = self.connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE sessions SET updated_at = ?2, revision = revision + 1 WHERE id = ?1",
            params![session_id, now],
        )?;
        if changed == 0 {
            return Err(StoreError::SessionNotFound(session_id));
        }
        transaction.execute(
            "UPDATE sessions SET title = ?2, revision = revision + 1
             WHERE id = ?1 AND title = ?3 AND NOT EXISTS (SELECT 1 FROM messages WHERE session_id = ?1)",
            params![session_id, title, UNTITLED],
        )?;
        let user = insert_message(
            &transaction,
            NewMessage {
                session_id,
                role: Role::User,
                content: text,
                status: Status::Complete,
                model: None,
                now,
            },
        )?;
        let history = query_messages(&transaction, session_id)?;
        let administrator = query_administrator_actions(&transaction, session_id)?;
        let summary = query_summary(&transaction, session_id)?;
        let session = query_session(&transaction, session_id)?;
        let answer = insert_message(
            &transaction,
            NewMessage {
                session_id,
                role: Role::Assistant,
                content: "",
                status: Status::Streaming,
                model: Some(model),
                now,
            },
        )?;
        transaction.commit()?;
        Ok(Turn {
            user,
            history,
            administrator,
            summary,
            answer,
            session,
        })
    }

    /// Replaces the content and status of a message, for example when an answer finishes. Fails if the message is
    /// gone, so that a write that changed nothing is never taken for a saved one.
    pub fn update_message(&self, id: i64, content: &str, status: Status) -> Result<()> {
        let changed = self.connection.execute(
            "UPDATE messages SET content = ?2, status = ?3 WHERE id = ?1",
            params![id, content, status.as_str()],
        )?;
        if changed == 0 {
            return Err(StoreError::MessageNotFound(id));
        }
        Ok(())
    }

    /// Records what an answer's request took: the tokens Kelpy estimated, and those the provider counted if it said.
    pub fn set_usage(
        &self,
        id: i64,
        context_estimate: u64,
        input_tokens: Option<u64>,
    ) -> Result<()> {
        let changed = self.connection.execute(
            "UPDATE messages SET context_estimate = ?2, input_tokens = ?3 WHERE id = ?1",
            params![id, tokens(context_estimate), input_tokens.map(tokens)],
        )?;
        if changed == 0 {
            return Err(StoreError::MessageNotFound(id));
        }
        Ok(())
    }
}

enum Job {
    Work(Box<dyn FnOnce(&mut Store) + Send>),
    /// Closes the database, then answers.
    Stop(oneshot::Sender<()>),
}

/// How many calls may wait for the database.
pub const QUEUE_CAPACITY: usize = 64;

/// The [`Store`], served by a thread of its own that holds its only connection: async callers send it work and await
/// the result, so a database call never blocks their workers. Calls run one at a time, in the order they arrive.
///
/// At most [`QUEUE_CAPACITY`] calls wait in the queue. When it is full, [`Storage::call`] waits for room, so a caller
/// that gives up meanwhile leaves nothing behind; [`Storage::submit`] does not wait, and drops its work.
#[derive(Clone)]
pub struct Storage {
    jobs: mpsc::Sender<Job>,
}

impl Storage {
    /// Starts the thread serving `store`. It ends with [`Storage::stop`], or once every handle is dropped, after the
    /// work already sent.
    pub fn start(mut store: Store) -> Result<Self> {
        let (jobs, mut queue) = mpsc::channel::<Job>(QUEUE_CAPACITY);
        thread::Builder::new()
            .name("kelpy-storage".into())
            .spawn(move || {
                while let Some(job) = queue.blocking_recv() {
                    match job {
                        // A call that panics must not take the database away from every later one; its caller is
                        // told the database is not available.
                        Job::Work(work) => {
                            let _ = panic::catch_unwind(AssertUnwindSafe(|| work(&mut store)));
                        }
                        Job::Stop(stopped) => {
                            // Work sent after the stop is dropped with the queue: its callers get `Unavailable`.
                            drop(queue);
                            drop(store);
                            let _ = stopped.send(());
                            return;
                        }
                    }
                }
            })?;
        Ok(Self { jobs })
    }

    /// Runs the work already sent, closes the database and ends the thread. Calls made after it fail with
    /// [`StoreError::Unavailable`].
    pub async fn stop(&self) {
        let (stopped, done) = oneshot::channel();
        if self.jobs.send(Job::Stop(stopped)).await.is_ok() {
            let _ = done.await;
        }
    }

    /// Runs `work` on the database and returns its result. Once queued, `work` runs even if the caller stops
    /// waiting.
    pub async fn call<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Store) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (reply, result) = oneshot::channel();
        let job = Job::Work(Box::new(move |store| {
            // The caller may have stopped waiting.
            let _ = reply.send(work(store));
        }));
        self.jobs
            .send(job)
            .await
            .map_err(|_| StoreError::Unavailable)?;
        result.await.map_err(|_| StoreError::Unavailable)?
    }

    /// Sends `work` to the database without waiting, for best-effort writes whose outcome a later call settles. The
    /// calls sent after it see its effect. Returns `false` when the queue is full, or the database is gone, and
    /// `work` was dropped.
    pub fn submit(&self, work: impl FnOnce(&mut Store) + Send + 'static) -> bool {
        self.jobs.try_send(Job::Work(Box::new(work))).is_ok()
    }
}

/// The text of a message being written, saved now and then without waiting, against a crash: at most one save of it
/// waits in the queue, and it writes the latest text. The final save, a [`Storage::call`], is the one that counts.
pub struct PartialSave {
    storage: Storage,
    id: i64,
    /// The text the waiting save will write; none when no save waits.
    latest: Arc<Mutex<Option<String>>>,
}

impl PartialSave {
    pub fn new(storage: &Storage, id: i64) -> Self {
        Self {
            storage: storage.clone(),
            id,
            latest: Arc::default(),
        }
    }

    /// Saves `content` as the message's text, still `streaming`; skipped while the queue is full.
    pub fn save(&self, content: String) {
        if lock(&self.latest).replace(content).is_some() {
            // The save already waiting takes the new text.
            return;
        }
        let (id, latest) = (self.id, Arc::clone(&self.latest));
        let queued = self.storage.submit(move |store| {
            // Taken before the write, which may wait for the database: the next save must not wait for it.
            let content = lock(&latest).take();
            if let Some(content) = content {
                let _ = store.update_message(id, &content, Status::Streaming);
            }
        });
        if !queued {
            // No save waits: the next one tries again.
            lock(&self.latest).take();
        }
    }
}

/// A turn just started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    /// The user's message, as stored.
    pub user: Message,
    /// The conversation up to and including the user's message.
    pub history: Vec<Message>,
    /// The operations the administrator asked for in the conversation, oldest first.
    pub administrator: Vec<Action>,
    /// The summary of the conversation's older messages, if it has one.
    pub summary: Option<Summary>,
    /// The empty answer, `streaming`.
    pub answer: Message,
    /// The conversation, titled and touched by the turn.
    pub session: Session,
}

struct NewMessage<'a> {
    session_id: i64,
    role: Role,
    content: &'a str,
    status: Status,
    model: Option<&'a str>,
    now: i64,
}

fn insert_message(connection: &Connection, message: NewMessage<'_>) -> Result<Message> {
    connection.execute(
        "INSERT INTO messages (session_id, role, content, status, model, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            message.session_id,
            message.role.as_str(),
            message.content,
            message.status.as_str(),
            message.model,
            message.now
        ],
    )?;
    Ok(Message {
        id: connection.last_insert_rowid(),
        session_id: message.session_id,
        role: message.role,
        content: message.content.to_owned(),
        status: message.status,
        model: message.model.map(str::to_owned),
        created_at: message.now,
        context_estimate: None,
        input_tokens: None,
        actions: Vec::new(),
    })
}

/// The messages of a conversation, oldest first, each answer with its actions.
fn query_messages(connection: &Connection, session_id: i64) -> Result<Vec<Message>> {
    let mut statement = connection.prepare(&format!(
        "SELECT {MESSAGE_COLUMNS} FROM messages WHERE session_id = ?1 ORDER BY id"
    ))?;
    let mut messages: Vec<Message> = statement
        .query_map([session_id], message_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    let mut statement = connection.prepare(&format!(
        "SELECT {ACTION_COLUMNS} FROM actions
         WHERE message_id IN (SELECT id FROM messages WHERE session_id = ?1) ORDER BY id"
    ))?;
    let mut actions: HashMap<i64, Vec<Action>> = HashMap::new();
    for action in statement.query_map([session_id], action_from_row)? {
        let action = action?;
        if let Some(message_id) = action.message_id {
            actions.entry(message_id).or_default().push(action);
        }
    }
    for message in &mut messages {
        message.actions = actions.remove(&message.id).unwrap_or_default();
    }
    Ok(messages)
}

fn query_administrator_actions(connection: &Connection, session_id: i64) -> Result<Vec<Action>> {
    let mut statement = connection.prepare(&format!(
        "SELECT {ACTION_COLUMNS} FROM actions WHERE session_id = ?1 AND message_id IS NULL ORDER BY id"
    ))?;
    let actions = statement
        .query_map([session_id], action_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(actions)
}

fn query_session(connection: &Connection, id: i64) -> Result<Session> {
    connection
        .query_row(
            &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1"),
            [id],
            session_from_row,
        )
        .optional()?
        .ok_or(StoreError::SessionNotFound(id))
}

fn query_summary(connection: &Connection, session_id: i64) -> Result<Option<Summary>> {
    let summary = connection
        .query_row(
            "SELECT through_id, content FROM summaries WHERE session_id = ?1",
            [session_id],
            |row| {
                Ok(Summary {
                    through: row.get(0)?,
                    content: row.get(1)?,
                })
            },
        )
        .optional()?;
    Ok(summary)
}

fn migrate(connection: &mut Connection) -> Result<()> {
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let latest = MIGRATIONS.len() as i64;
    if version > latest {
        return Err(StoreError::TooNew(version));
    }
    for (index, migration) in MIGRATIONS.iter().enumerate().skip(version as usize) {
        let transaction = connection.transaction()?;
        transaction.execute_batch(migration)?;
        transaction.pragma_update(None, "user_version", index as i64 + 1)?;
        transaction.commit()?;
    }
    Ok(())
}

fn session_from_row(row: &Row<'_>) -> rusqlite::Result<Session> {
    let access: String = row.get(5)?;
    let approval: String = row.get(6)?;
    Ok(Session {
        id: row.get(0)?,
        title: row.get(1)?,
        explanations: row.get(2)?,
        access: Access::parse(&access).ok_or_else(|| invalid(5, &access))?,
        approval: Approval::parse(&approval).ok_or_else(|| invalid(6, &approval))?,
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
        revision: row.get(7)?,
    })
}

fn action_from_row(row: &Row<'_>) -> rusqlite::Result<Action> {
    let state: String = row.get(10)?;
    Ok(Action {
        id: row.get(0)?,
        session_id: row.get(18)?,
        message_id: row.get(1)?,
        after_message: row.get(19)?,
        step: row.get(2)?,
        position: usize::try_from(row.get::<_, i64>(3)?).unwrap_or(0),
        call_id: row.get(4)?,
        tool: row.get(5)?,
        input: row.get(6)?,
        reason: row.get(7)?,
        summary: row.get(8)?,
        command: row.get(9)?,
        state: ActionState::parse(&state).ok_or_else(|| invalid(10, &state))?,
        exit_code: row.get(11)?,
        output: row.get(12)?,
        error: row.get(13)?,
        created_at: row.get(14)?,
        duration_ms: row
            .get::<_, Option<i64>>(15)?
            .map(|duration| duration.unsigned_abs()),
        started_at: row.get(16)?,
        change: from_json(row, 17)?,
        phase: row.get(20)?,
        plan: from_json(row, 21)?,
        plan_step: match (row.get(22)?, row.get(23)?) {
            (Some(plan_id), Some(step)) => Some(PlanStepRef { plan_id, step }),
            _ => None,
        },
    })
}

fn record_from_row(row: &Row<'_>) -> rusqlite::Result<Record> {
    record_at(row, 0)
}

/// The record whose columns, [`RECORD_COLUMNS`], start at `offset`.
fn record_at(row: &Row<'_>, offset: usize) -> rusqlite::Result<Record> {
    let kind: String = row.get(offset + 1)?;
    Ok(Record {
        id: row.get(offset)?,
        kind: RecordKind::parse(&kind).ok_or_else(|| invalid(offset + 1, &kind))?,
        subject: row.get(offset + 2)?,
        expected: row.get(offset + 3)?,
        content: row.get(offset + 4)?,
        action_id: row.get(offset + 5)?,
        recorded_at: row.get(offset + 6)?,
        secret: row.get(offset + 7)?,
    })
}

/// The JSON value in the column `index`, if any.
fn from_json<T: serde::de::DeserializeOwned>(
    row: &Row<'_>,
    index: usize,
) -> rusqlite::Result<Option<T>> {
    row.get::<_, Option<String>>(index)?
        .map(|text| serde_json::from_str(&text).map_err(|_| invalid(index, &text)))
        .transpose()
}

fn to_json(value: &impl Serialize) -> String {
    serde_json::to_string(value).expect("a stored value serializes")
}

fn message_from_row(row: &Row<'_>) -> rusqlite::Result<Message> {
    let role: String = row.get(2)?;
    let status: String = row.get(4)?;
    Ok(Message {
        id: row.get(0)?,
        session_id: row.get(1)?,
        role: Role::parse(&role).ok_or_else(|| invalid(2, &role))?,
        content: row.get(3)?,
        status: Status::parse(&status).ok_or_else(|| invalid(4, &status))?,
        model: row.get(5)?,
        created_at: row.get(6)?,
        context_estimate: row
            .get::<_, Option<i64>>(7)?
            .map(|count| count.unsigned_abs()),
        input_tokens: row
            .get::<_, Option<i64>>(8)?
            .map(|count| count.unsigned_abs()),
        actions: Vec::new(),
    })
}

/// A count of tokens as SQLite stores it: no real count comes near the limit.
fn tokens(count: u64) -> i64 {
    i64::try_from(count).unwrap_or(i64::MAX)
}

/// A size or a duration as SQLite stores it: none comes near the limit.
fn count(value: impl TryInto<i64>) -> i64 {
    value.try_into().unwrap_or(i64::MAX)
}

fn invalid(column: usize, value: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        column,
        rusqlite::types::Type::Text,
        format!("unknown value {value:?}").into(),
    )
}

/// The text a value is stored as, and back.
macro_rules! stored_as {
    ($type:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        impl $type {
            fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }

            fn parse(text: &str) -> Option<Self> {
                [$(Self::$variant),+].into_iter().find(|value| value.as_str() == text)
            }
        }
    };
}

stored_as!(Access { None => "none", ReadOnly => "read_only", Full => "full" });
stored_as!(Approval { Ask => "ask", Plan => "plan", Auto => "auto" });
stored_as!(ActionState {
    Waiting => "waiting",
    Running => "running",
    Done => "done",
    Failed => "failed",
    Denied => "denied",
    Uncertain => "uncertain",
});
stored_as!(RecordKind { File => "file", Service => "service", Package => "package" });

impl Settled {
    fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::PutBack => "put_back",
            Self::Replaced => "replaced",
        }
    }
}

stored_as!(Role { User => "user", Assistant => "assistant" });
stored_as!(Status {
    Complete => "complete",
    Streaming => "streaming",
    Interrupted => "interrupted",
    Failed => "failed",
    Refused => "refused",
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_names_the_conversation_and_keeps_messages_in_order() {
        let mut store = Store::in_memory().unwrap();
        let session = store.create_session(UNTITLED, Modes::default()).unwrap();
        let turn = store
            .start_turn(session.id, "Hello", "Greeting", "m")
            .unwrap();
        assert_eq!(turn.history, vec![turn.user.clone()]);
        assert_eq!(turn.answer.status, Status::Streaming);
        store
            .update_message(turn.answer.id, "Hi!", Status::Complete)
            .unwrap();
        store
            .start_turn(session.id, "Again", "Not a title", "m")
            .unwrap();

        let messages = store.messages(session.id).unwrap();
        let contents: Vec<&str> = messages.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(contents, ["Hello", "Hi!", "Again", ""]);
        assert_eq!(messages[1].model.as_deref(), Some("m"));
        // Only the first message names an untitled conversation.
        assert_eq!(store.session(session.id).unwrap().title, "Greeting");
    }

    #[test]
    fn a_turn_keeps_a_title_chosen_by_the_user() {
        let mut store = Store::in_memory().unwrap();
        let session = store
            .create_session("Nginx setup", Modes::default())
            .unwrap();
        store
            .start_turn(session.id, "Hello", "Greeting", "m")
            .unwrap();
        assert_eq!(store.session(session.id).unwrap().title, "Nginx setup");
        assert!(matches!(
            store.start_turn(999, "Hello", "x", "m"),
            Err(StoreError::SessionNotFound(999))
        ));
    }

    #[test]
    fn deleting_a_conversation_deletes_its_messages() {
        let mut store = Store::in_memory().unwrap();
        let session = store.create_session("Temporary", Modes::default()).unwrap();
        let turn = store.start_turn(session.id, "x", "x", "m").unwrap();
        store.delete_session(session.id).unwrap();
        assert!(matches!(
            store.messages(session.id),
            Err(StoreError::SessionNotFound(_))
        ));
        assert!(store.sessions().unwrap().is_empty());
        // A late write to a deleted answer is an error, never a silent success.
        assert!(matches!(
            store.update_message(turn.answer.id, "late", Status::Complete),
            Err(StoreError::MessageNotFound(_))
        ));
    }

    #[test]
    fn modes_and_actions_survive_a_restart_and_unfinished_actions_fail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kelpy.db");
        let change = Change {
            diff: "-a
+b
"
            .into(),
            notes: vec!["Backed up first.".into()],
            floor: None,
            path: None,
            backup: None,
        };
        let new = |session_id, message_id, call_id, state| NewAction {
            session_id,
            message_id: Some(message_id),
            step: 0,
            position: 4,
            call_id,
            tool: "disk_usage",
            input: "{}",
            reason: "To see the free space",
            summary: "Show the disk usage",
            command: Some("df -h"),
            change: Some(&change),
            plan: None,
            plan_step: None,
            state,
            error: None,
        };
        let (session_id, answer_id, done) = {
            let mut store = Store::open(&path).unwrap();
            let session = store.create_session("Disks", Modes::default()).unwrap();
            assert_eq!(
                (session.access, session.approval),
                (Access::ReadOnly, Approval::Ask)
            );
            store.set_access(session.id, Access::None).unwrap();
            store.set_approval(session.id, Approval::Auto).unwrap();
            let turn = store
                .start_turn(session.id, "Disks?", "Disks?", "m")
                .unwrap();
            let mut done = store
                .insert_action(&new(
                    session.id,
                    turn.answer.id,
                    "call_1",
                    ActionState::Running,
                ))
                .unwrap();
            // Recorded running: it started then.
            assert_eq!(done.started_at, Some(done.created_at));
            done.state = ActionState::Done;
            done.exit_code = Some(0);
            done.output = "/ 40% used".into();
            done.duration_ms = Some(120);
            done.change.as_mut().unwrap().backup = Some("/home/kelpy/data/backups/1-a".into());
            store.update_action(&done).unwrap();
            store
                .insert_action(&new(
                    session.id,
                    turn.answer.id,
                    "call_2",
                    ActionState::Waiting,
                ))
                .unwrap();
            // Stopped while writing: what it left is not known.
            let mut writing = store
                .insert_action(&new(
                    session.id,
                    turn.answer.id,
                    "call_3",
                    ActionState::Running,
                ))
                .unwrap();
            writing.phase = Some("writing the file".into());
            store.update_action(&writing).unwrap();
            assert_eq!(store.messages(session.id).unwrap()[1].actions[0], done);
            (session.id, turn.answer.id, done)
        };

        let store = Store::open(&path).unwrap();
        let session = store.session(session_id).unwrap();
        assert_eq!(
            (session.access, session.approval),
            (Access::None, Approval::Auto)
        );
        // Raised by each change: access, approval, then the turn.
        assert_eq!(session.revision, 3);
        let messages = store.messages(session_id).unwrap();
        assert!(messages[0].actions.is_empty());
        let actions = &messages[1].actions;
        assert_eq!(actions[0], done);
        assert_eq!(actions[1].state, ActionState::Failed);
        assert_eq!(
            actions[1].error.as_deref(),
            Some("Kelpy stopped before it ended")
        );
        assert_eq!(actions[2].state, ActionState::Uncertain);
        assert_eq!(
            actions[2].error.as_deref(),
            Some("Kelpy stopped while writing the file: what it left is not known")
        );

        // Gone with their conversation; none can be added to it afterwards.
        store.delete_session(session_id).unwrap();
        assert!(matches!(
            store.insert_action(&new(session_id, answer_id, "call_4", ActionState::Running)),
            Err(StoreError::MessageNotFound(id)) if id == answer_id
        ));
    }

    fn new_action<'a>(
        session_id: i64,
        message_id: Option<i64>,
        tool: &'a str,
        input: &'a str,
    ) -> NewAction<'a> {
        NewAction {
            session_id,
            message_id,
            step: 0,
            position: 0,
            call_id: "",
            tool,
            input,
            reason: "",
            summary: tool,
            command: None,
            change: None,
            plan: None,
            plan_step: None,
            state: ActionState::Done,
            error: None,
        }
    }

    #[test]
    fn the_administrators_operations_belong_to_the_conversation_after_its_last_message() {
        let mut store = Store::in_memory().unwrap();
        let session = store.create_session("Site", Modes::default()).unwrap();
        let turn = store.start_turn(session.id, "Fix it", "x", "m").unwrap();
        let restore = store
            .insert_action(&new_action(session.id, None, "restore_file", "{}"))
            .unwrap();
        assert_eq!(restore.after_message, Some(turn.answer.id));
        // Not an answer's: listed apart.
        assert!(store.messages(session.id).unwrap()[1].actions.is_empty());
        assert_eq!(
            store.administrator_actions(session.id).unwrap(),
            std::slice::from_ref(&restore)
        );
        assert_eq!(store.action(session.id, restore.id).unwrap(), restore);
        assert!(matches!(
            store.action(session.id + 1, restore.id),
            Err(StoreError::ActionNotFound(_))
        ));
    }

    #[test]
    fn the_latest_plan_approved_comes_with_the_steps_carried_out() {
        let mut store = Store::in_memory().unwrap();
        let session = store.create_session("Blog", Modes::default()).unwrap();
        let turn = store.start_turn(session.id, "Install", "x", "m").unwrap();
        let plan = Plan {
            goal: "Install nginx".into(),
            steps: vec![PlanStep {
                tool: "install_packages".into(),
                input: r#"{"packages":["nginx"]}"#.into(),
                summary: "Install nginx".into(),
                detail: None,
                floor: None,
            }],
        };
        assert_eq!(store.approved_plan(session.id).unwrap(), None);
        let mut proposed = new_action(session.id, Some(turn.answer.id), "propose_plan", "{}");
        proposed.plan = Some(&plan);
        proposed.state = ActionState::Waiting;
        let mut proposed = store.insert_action(&proposed).unwrap();
        // Not approved yet.
        assert_eq!(store.approved_plan(session.id).unwrap(), None);
        proposed.state = ActionState::Done;
        store.update_action(&proposed).unwrap();
        let mut step = new_action(session.id, Some(turn.answer.id), "install_packages", "{}");
        step.plan_step = Some(PlanStepRef {
            plan_id: proposed.id,
            step: 0,
        });
        let step = store.insert_action(&step).unwrap();
        let (approved, steps) = store.approved_plan(session.id).unwrap().unwrap();
        assert_eq!(approved.plan.as_ref(), Some(&plan));
        assert_eq!(steps, [step]);
        // A later plan ends it, even before it is approved or when it is denied.
        let mut newer = new_action(session.id, Some(turn.answer.id), "propose_plan", "{}");
        newer.plan = Some(&plan);
        newer.state = ActionState::Denied;
        store.insert_action(&newer).unwrap();
        assert_eq!(store.approved_plan(session.id).unwrap(), None);
    }

    #[test]
    fn differences_are_noted_once_settled_by_the_administrator_and_replaced_by_a_new_change() {
        let mut store = Store::in_memory().unwrap();
        let path = "/etc/nginx/nginx.conf";
        let record = store
            .record(
                RecordKind::File,
                path,
                "a1",
                Some("worker 2;\n"),
                false,
                None,
            )
            .unwrap();
        assert!(
            store
                .found_different(record.id, "b2", Some("worker 4;\n"))
                .unwrap()
        );
        // The same difference again: still the first note.
        assert!(
            !store
                .found_different(record.id, "b2", Some("worker 4;\n"))
                .unwrap()
        );
        let open = store.differences().unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(
            (open[0].found.as_str(), open[0].record.expected.as_str()),
            ("b2", "a1")
        );
        assert_eq!(open[0].content.as_deref(), Some("worker 4;\n"));

        store
            .settle_difference(open[0].id, Settled::Accepted)
            .unwrap();
        assert!(store.differences().unwrap().is_empty());
        let records = store.records().unwrap();
        assert_eq!(
            (records[0].expected.as_str(), records[0].content.as_deref()),
            ("b2", Some("worker 4;\n"))
        );
        assert!(matches!(
            store.settle_difference(open[0].id, Settled::PutBack),
            Err(StoreError::DifferenceNotFound(_))
        ));

        // Changed by hand, then back as expected: nothing to settle.
        store.found_different(record.id, "c3", None).unwrap();
        assert!(store.found_as_expected(record.id).unwrap());
        assert!(store.differences().unwrap().is_empty());
        // Changed by hand, then changed by Kelpy: the new record replaces the difference.
        store.found_different(record.id, "c3", None).unwrap();
        store
            .record(RecordKind::File, path, "d4", None, false, None)
            .unwrap();
        assert!(store.differences().unwrap().is_empty());
        assert_eq!(store.records().unwrap().len(), 1);
    }

    #[test]
    fn actions_of_an_older_database_join_their_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kelpy.db");
        {
            let mut connection = Connection::open(&path).unwrap();
            let transaction = connection.transaction().unwrap();
            for migration in &MIGRATIONS[..8] {
                transaction.execute_batch(migration).unwrap();
            }
            transaction.pragma_update(None, "user_version", 8).unwrap();
            transaction
                .execute_batch(
                    "INSERT INTO sessions (title, created_at, updated_at) VALUES ('Old', 0, 0);
                     INSERT INTO messages (session_id, role, content, status, created_at)
                         VALUES (1, 'assistant', 'x', 'complete', 0);
                     INSERT INTO actions (message_id, step, position, call_id, tool, input, reason, summary, state,
                                          created_at)
                         VALUES (1, 0, 0, 'c', 'disk_usage', '{}', '', 'Show the disk usage', 'done', 0),
                                (1, 0, 0, 'd', 'disk_usage', '{}', '', 'Show the disk usage', 'done', 0);
                     DELETE FROM actions WHERE call_id = 'd';",
                )
                .unwrap();
            transaction.commit().unwrap();
        }
        let mut store = Store::open(&path).unwrap();
        let action = store.messages(1).unwrap()[0].actions[0].clone();
        assert_eq!((action.session_id, action.message_id), (1, Some(1)));
        // New ids follow the old ones, those of deleted actions included: an id is never given twice.
        let turn = store.start_turn(1, "Again", "x", "m").unwrap();
        let next = store
            .insert_action(&new_action(1, Some(turn.answer.id), "disk_usage", "{}"))
            .unwrap();
        assert_eq!(next.id, action.id + 2);
    }

    #[test]
    fn data_survives_a_restart_and_unfinished_answers_are_interrupted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kelpy.db");
        let session_id = {
            let mut store = Store::open(&path).unwrap();
            let session = store
                .create_session("Before restart", Modes::default())
                .unwrap();
            let turn = store.start_turn(session.id, "Hi", "x", "m").unwrap();
            store
                .update_message(turn.answer.id, "partial", Status::Streaming)
                .unwrap();
            session.id
        };
        let store = Store::open(&path).unwrap();
        assert_eq!(store.sessions().unwrap()[0].title, "Before restart");
        let messages = store.messages(session_id).unwrap();
        assert_eq!(messages[1].status, Status::Interrupted);
        assert_eq!(messages[1].content, "partial");
    }

    #[test]
    fn a_summary_replaces_the_previous_one_and_goes_with_its_conversation() {
        let mut store = Store::in_memory().unwrap();
        let session = store.create_session("Long", Modes::default()).unwrap();
        let first = store.start_turn(session.id, "One", "x", "m").unwrap();
        assert_eq!(first.summary, None);
        let summary = |through, content: &str| Summary {
            through,
            content: content.to_owned(),
        };
        store
            .save_summary(session.id, &summary(first.user.id, "Asked one."), "m")
            .unwrap();
        store
            .save_summary(
                session.id,
                &summary(first.answer.id, "Asked one, answered."),
                "m",
            )
            .unwrap();
        let second = store.start_turn(session.id, "Two", "x", "m").unwrap();
        assert_eq!(
            second.summary,
            Some(summary(first.answer.id, "Asked one, answered."))
        );

        store.delete_session(session.id).unwrap();
        assert_eq!(store.summary(session.id).unwrap(), None);
        assert!(matches!(
            store.save_summary(session.id, &summary(1, "late"), "m"),
            Err(StoreError::SessionNotFound(_))
        ));
    }

    #[test]
    fn an_answer_keeps_what_its_request_took() {
        let mut store = Store::in_memory().unwrap();
        let session = store.create_session("Usage", Modes::default()).unwrap();
        let turn = store.start_turn(session.id, "Hi", "x", "m").unwrap();
        assert_eq!(
            (turn.answer.context_estimate, turn.answer.input_tokens),
            (None, None)
        );
        store.set_usage(turn.answer.id, 1_200, Some(1_500)).unwrap();
        let answer = &store.messages(session.id).unwrap()[1];
        assert_eq!(
            (answer.context_estimate, answer.input_tokens),
            (Some(1_200), Some(1_500))
        );
        // Interfaces are not sent it.
        assert!(!serde_json::to_string(answer).unwrap().contains("1500"));
        assert!(matches!(
            store.set_usage(999, 1, None),
            Err(StoreError::MessageNotFound(999))
        ));
    }

    #[test]
    fn explanations_are_off_until_turned_on() {
        let store = Store::in_memory().unwrap();
        let session = store.create_session("Explained", Modes::default()).unwrap();
        assert!(!session.explanations);
        store.set_explanations(session.id, true).unwrap();
        assert!(store.session(session.id).unwrap().explanations);
        assert!(matches!(
            store.set_explanations(999, true),
            Err(StoreError::SessionNotFound(999))
        ));
    }

    #[test]
    fn conversations_can_start_with_explanations() {
        let store = Store::in_memory().unwrap();
        let session = store
            .create_session(
                "Explained from the start",
                Modes {
                    explanations: true,
                    ..Modes::default()
                },
            )
            .unwrap();
        assert!(session.explanations);
        assert!(store.session(session.id).unwrap().explanations);
    }

    #[test]
    fn older_databases_are_upgraded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kelpy.db");
        {
            let connection = Connection::open(&path).unwrap();
            connection.execute_batch(MIGRATIONS[0]).unwrap();
            connection.pragma_update(None, "user_version", 1).unwrap();
            connection
                .execute_batch(
                    "INSERT INTO sessions (title, created_at, updated_at) VALUES ('Old', 1, 1);
                     INSERT INTO messages (session_id, role, content, status, created_at)
                     VALUES (1, 'user', 'Hello', 'complete', 1);",
                )
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let sessions = store.sessions().unwrap();
        assert_eq!(sessions[0].title, "Old");
        assert!(!sessions[0].explanations);
        assert_eq!(store.messages(sessions[0].id).unwrap()[0].content, "Hello");

        // The rebuilt tables keep the cascade and never reuse an id.
        store.delete_session(sessions[0].id).unwrap();
        let count: i64 = store
            .connection
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
        assert!(store.create_session("New", Modes::default()).unwrap().id > sessions[0].id);
    }

    #[test]
    fn ids_are_never_reused() {
        let mut store = Store::in_memory().unwrap();
        let first = store
            .create_session("Deleted while answering", Modes::default())
            .unwrap();
        let old = store.start_turn(first.id, "A", "A", "m").unwrap();
        store.delete_session(first.id).unwrap();

        let second = store
            .create_session("Created right after", Modes::default())
            .unwrap();
        let new = store.start_turn(second.id, "B", "B", "m").unwrap();
        assert_ne!(second.id, first.id);
        assert_ne!(new.answer.id, old.answer.id);
        assert_ne!(new.user.id, old.answer.id);
    }

    #[test]
    fn most_recently_active_conversations_come_first() {
        let store = Store::in_memory().unwrap();
        let first = store.create_session("First", Modes::default()).unwrap();
        let second = store.create_session("Second", Modes::default()).unwrap();
        store.rename_session(first.id, "First, renamed").unwrap();
        let sessions = store.sessions().unwrap();
        assert_eq!(sessions[0].id, second.id);
        assert_eq!(sessions[1].title, "First, renamed");
        assert!(matches!(
            store.rename_session(999, "x"),
            Err(StoreError::SessionNotFound(999))
        ));
    }

    /// Keeps the storage thread busy, with nothing queued, until the returned sender sends.
    fn hold(storage: &Storage) -> std::sync::mpsc::Sender<()> {
        let (started, running) = std::sync::mpsc::channel::<()>();
        let (release, gate) = std::sync::mpsc::channel::<()>();
        assert!(storage.submit(move |_| {
            let _ = started.send(());
            let _ = gate.recv();
        }));
        running.recv().unwrap();
        release
    }

    #[tokio::test]
    async fn the_queue_is_bounded_and_callers_giving_up_while_it_is_full_leave_nothing() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        let storage = Storage::start(Store::in_memory().unwrap()).unwrap();
        let release = hold(&storage);
        let ran = Arc::new(AtomicUsize::new(0));
        for _ in 0..QUEUE_CAPACITY * 2 {
            let ran = Arc::clone(&ran);
            let call = storage.call(move |_| {
                ran.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });
            let given_up = tokio::time::timeout(Duration::from_millis(10), call).await;
            assert!(given_up.is_err(), "the database is held");
        }
        release.send(()).unwrap();
        storage.call(|_| Ok(())).await.unwrap();
        assert_eq!(ran.load(Ordering::SeqCst), QUEUE_CAPACITY);
    }

    #[tokio::test]
    async fn partial_saves_of_a_message_wait_as_one_with_the_latest_text() {
        let storage = Storage::start(Store::in_memory().unwrap()).unwrap();
        let session = storage
            .call(|store| store.create_session("Saves", Modes::default()))
            .await
            .unwrap();
        let turn = storage
            .call(move |store| store.start_turn(session.id, "Hi", "Saves", "model"))
            .await
            .unwrap();
        let release = hold(&storage);
        let saves = PartialSave::new(&storage, turn.answer.id);
        let last = QUEUE_CAPACITY * 2;
        for words in 1..=last {
            saves.save(format!("{words} words"));
        }
        // One save waits for all of them: the queue still has room.
        assert!(storage.submit(|_| {}));

        release.send(()).unwrap();
        let messages = storage
            .call(move |store| store.messages(session.id))
            .await
            .unwrap();
        assert_eq!(messages[1].content, format!("{last} words"));
        assert_eq!(messages[1].status, Status::Streaming);
    }

    #[tokio::test]
    async fn a_partial_save_skipped_while_the_queue_is_full_does_not_stop_the_next() {
        let storage = Storage::start(Store::in_memory().unwrap()).unwrap();
        let session = storage
            .call(|store| store.create_session("Saves", Modes::default()))
            .await
            .unwrap();
        let turn = storage
            .call(move |store| store.start_turn(session.id, "Hi", "Saves", "model"))
            .await
            .unwrap();
        let saves = PartialSave::new(&storage, turn.answer.id);
        let release = hold(&storage);
        while storage.submit(|_| {}) {}
        saves.save("skipped".into());
        release.send(()).unwrap();
        storage.call(|_| Ok(())).await.unwrap();

        saves.save("saved".into());
        let messages = storage
            .call(move |store| store.messages(session.id))
            .await
            .unwrap();
        assert_eq!(messages[1].content, "saved");
    }

    #[tokio::test]
    async fn a_partial_save_does_not_wait_for_the_one_being_written() {
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kelpy.db");
        let storage = Storage::start(Store::open(&path).unwrap()).unwrap();
        let session = storage
            .call(|store| store.create_session("Saves", Modes::default()))
            .await
            .unwrap();
        let turn = storage
            .call(move |store| store.start_turn(session.id, "Hi", "Saves", "model"))
            .await
            .unwrap();
        // Another connection holds the database, so the first save stays in its write.
        let holder = Connection::open(&path).unwrap();
        holder.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let saves = Arc::new(PartialSave::new(&storage, turn.answer.id));
        saves.save("first".into());
        std::thread::sleep(Duration::from_millis(200));

        let (saved, returned) = std::sync::mpsc::channel();
        std::thread::spawn({
            let saves = Arc::clone(&saves);
            move || {
                saves.save("second".into());
                let _ = saved.send(());
            }
        });
        let at_once = returned.recv_timeout(Duration::from_secs(1));
        holder.execute_batch("COMMIT").unwrap();
        assert!(at_once.is_ok(), "the next save waited for the write");

        let messages = storage
            .call(move |store| store.messages(session.id))
            .await
            .unwrap();
        assert_eq!(messages[1].content, "second");
    }

    #[tokio::test]
    async fn stopping_runs_the_work_already_sent_then_refuses_more() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let storage = Storage::start(Store::in_memory().unwrap()).unwrap();
        let release = hold(&storage);
        let ran = Arc::new(AtomicBool::new(false));
        let sent = Arc::clone(&ran);
        assert!(storage.submit(move |_| sent.store(true, Ordering::SeqCst)));
        let stopping = tokio::spawn({
            let storage = storage.clone();
            async move { storage.stop().await }
        });
        release.send(()).unwrap();
        stopping.await.unwrap();

        assert!(ran.load(Ordering::SeqCst));
        assert!(matches!(
            storage.call(|store| store.sessions()).await,
            Err(StoreError::Unavailable)
        ));
        assert!(!storage.submit(|_| {}));
    }

    #[tokio::test]
    async fn the_storage_thread_runs_calls_in_order_and_survives_a_failing_one() {
        let storage = Storage::start(Store::in_memory().unwrap()).unwrap();
        let session = storage
            .call(|store| store.create_session("Ordered", Modes::default()))
            .await
            .unwrap();
        assert!(storage.submit(move |store| {
            let _ = store.rename_session(session.id, "Renamed without waiting");
        }));
        let renamed = storage
            .call(move |store| store.session(session.id))
            .await
            .unwrap();
        assert_eq!(renamed.title, "Renamed without waiting");

        let failed = storage
            .call(|_| -> Result<()> { panic!("a call failing unexpectedly") })
            .await;
        assert!(matches!(failed, Err(StoreError::Unavailable)));
        assert_eq!(
            storage.call(|store| store.sessions()).await.unwrap().len(),
            1
        );
    }
}
