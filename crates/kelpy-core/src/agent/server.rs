//! The server Kelpy changes, beyond the conversations: what it expects to find there after the changes it made and
//! checked (its records), the differences found since, the operations the administrator asks for (putting a file
//! back as it was), and the report and export of a conversation.
//!
//! The records are checked when Kelpy starts and before each answer, at most once a minute: a difference is noted
//! once, shown to the administrator and told to the model, never reverted by itself. The administrator accepts it,
//! and it is expected from then on, or puts Kelpy's version back.
//!
//! The records are also written for people to read, as the memory page `server.md`. The page is a record itself: an
//! edit made outside Kelpy is a difference like any other, and the page is not written again until it is settled.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kelpy_common::{lock, unix_now, utc};
use kelpy_storage::{
    Action, ActionState, Change, Difference, NewAction, Record, RecordKind, Settled, Storage,
};
use kelpy_tools::{
    Card, Edit, Executor, FileChange, Output, Prepared, Recorded, RecordedKind, Secrets, Tools,
    Work,
};
use serde::Serialize;
use serde_json::{Value, json};

use super::AgentError;
use crate::events::{Event, Events};

/// How often the records are checked at most, before answers.
const CHECK_EVERY: Duration = Duration::from_secs(60);
/// The most of the differences told to the model.
const MAX_NOTES: usize = 12 << 10;
/// The most of one difference's diff told to the model.
const MAX_NOTE: usize = 3 << 10;

/// The secrets of each conversation, shared by its answers and the administrator's operations.
pub(super) type SessionSecrets = Arc<Mutex<HashMap<i64, Secrets>>>;

pub(super) struct Server {
    store: Storage,
    executor: Executor,
    events: Events,
    secrets: SessionSecrets,
    /// Where the versions of files before a change are kept.
    backups: PathBuf,
    /// Kelpy's memory of the server, where it writes `server.md`.
    memory: PathBuf,
    /// When the records were last checked.
    checked: Mutex<Option<Instant>>,
    /// One check at a time.
    checking: tokio::sync::Mutex<()>,
    /// One writing of `server.md` at a time.
    writing: tokio::sync::Mutex<()>,
}

/// A difference as the administrator sees it.
#[derive(Debug, Clone, Serialize)]
pub struct DifferenceView {
    pub id: i64,
    pub kind: RecordKind,
    pub subject: String,
    pub expected: String,
    pub found: String,
    /// For a file whose content may be shown, what changed, as a unified diff.
    pub diff: Option<String>,
    pub found_at: i64,
    pub recorded_at: i64,
    /// Whether Kelpy can put its version back: a file whose content it kept.
    pub can_put_back: bool,
}

/// What an operation of the administrator would do, or did.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Operated {
    /// Shown before it runs.
    Preview { summary: String, change: Change },
    /// It ran, or failed: the action says.
    Done { action: Box<Action> },
}

impl Server {
    pub(super) fn new(
        store: Storage,
        executor: Executor,
        events: Events,
        backups: PathBuf,
        memory: PathBuf,
    ) -> Self {
        Self {
            store,
            executor,
            events,
            secrets: SessionSecrets::default(),
            backups,
            memory,
            checked: Mutex::new(None),
            checking: tokio::sync::Mutex::new(()),
            writing: tokio::sync::Mutex::new(()),
        }
    }

    /// The secrets of a conversation.
    pub(super) fn secrets(&self, session_id: i64) -> Secrets {
        lock(&self.secrets).entry(session_id).or_default().clone()
    }

    /// Forgets the secrets of a conversation deleted.
    pub(super) fn forget(&self, session_id: i64) {
        lock(&self.secrets).remove(&session_id);
    }

    /// Saves what a change of the action `action_id` left, as Kelpy expects to find it from now on.
    pub(super) async fn record(
        &self,
        action_id: i64,
        records: Vec<Recorded>,
    ) -> Result<(), AgentError> {
        if records.is_empty() {
            return Ok(());
        }
        for record in records.iter().filter(|record| record.secret) {
            self.executor.hold(PathBuf::from(&record.subject));
        }
        self.store
            .call(move |store| {
                for record in &records {
                    store.record(
                        kind(record.kind),
                        &record.subject,
                        &record.state,
                        record.content.as_deref(),
                        record.secret,
                        Some(action_id),
                    )?;
                }
                Ok(())
            })
            .await?;
        self.write_page().await;
        Ok(())
    }

    /// Writes the records as the memory page `server.md`, unless it was edited outside Kelpy: that is noted as a
    /// difference, to settle first. Best effort: the records stay in the database whatever happens to the page.
    pub(super) async fn write_page(&self) {
        let _writing = self.writing.lock().await;
        let path = self
            .memory
            .join(format!("{}.md", kelpy_tools::memory::SERVER_PAGE));
        let subject = path.to_string_lossy().into_owned();
        let Ok((records, differences)) = self
            .store
            .call(|store| Ok((store.records()?, store.differences()?)))
            .await
        else {
            return;
        };
        if differences
            .iter()
            .any(|difference| difference.record.subject == subject)
        {
            return;
        }
        let own = records.iter().find(|record| record.subject == subject);
        let now = self
            .executor
            .observe(RecordedKind::File, &subject)
            .await
            .ok();
        if let (Some(own), Some(now)) = (own, &now)
            && now.state != own.expected
            && now.state != "missing"
        {
            // Edited since Kelpy wrote it.
            let (id, state, content) = (own.id, now.state.clone(), now.content.clone());
            let _ = self
                .store
                .call(move |store| store.found_different(id, &state, content.as_deref()))
                .await;
            self.events.publish(Event::DifferencesChanged);
            return;
        }
        let others: Vec<&Record> = records
            .iter()
            .filter(|record| record.subject != subject)
            .collect();
        let page = page(&others, &differences);
        if now.as_ref().and_then(|now| now.content.as_deref()) == Some(page.as_str()) {
            return;
        }
        if kelpy_tools::memory::write_server_page(&self.memory, &page)
            .await
            .is_err()
        {
            return;
        }
        let state = kelpy_tools::change::digest(&page);
        let _ = self
            .store
            .call(move |store| {
                store.record(RecordKind::File, &subject, &state, Some(&page), false, None)
            })
            .await;
    }

    /// Checks every record against the server, unless it was checked less than a minute ago and not `now`. A
    /// difference is noted once; one gone is forgotten. The interfaces are told when they changed.
    pub(super) async fn check(&self, now: bool) -> Result<(), AgentError> {
        let _checking = self.checking.lock().await;
        if !now && lock(&self.checked).is_some_and(|checked| checked.elapsed() < CHECK_EVERY) {
            return Ok(());
        }
        let records = self.store.call(|store| store.records()).await?;
        let mut changed = false;
        // The files Kelpy wrote secrets into stay kept from the model, after a restart too.
        for record in records.iter().filter(|record| record.secret) {
            self.executor.hold(PathBuf::from(&record.subject));
        }
        for record in records {
            let observed = self
                .executor
                .observe(recorded_kind(record.kind), &record.subject)
                .await;
            let (state, content) = match observed {
                Ok(observed) => (observed.state, observed.content),
                // A service removed, a file grown past the limit: what is found is that it cannot be read.
                Err(error) => (format!("unreadable: {error}"), None),
            };
            let id = record.id;
            if state == record.expected {
                changed |= self
                    .store
                    .call(move |store| store.found_as_expected(id))
                    .await?;
            } else {
                // A content is kept only where the record's may be shown.
                let content = content.filter(|_| record.content.is_some());
                changed |= self
                    .store
                    .call(move |store| store.found_different(id, &state, content.as_deref()))
                    .await?;
            }
        }
        *lock(&self.checked) = Some(Instant::now());
        if changed {
            self.events.publish(Event::DifferencesChanged);
        }
        self.write_page().await;
        Ok(())
    }

    /// The differences not settled, as the administrator sees them.
    pub async fn differences(&self) -> Result<Vec<DifferenceView>, AgentError> {
        let differences = self.store.call(|store| store.differences()).await?;
        Ok(differences.iter().map(view).collect())
    }

    /// The differences for the model, if there are any.
    pub(super) async fn notes(&self) -> Option<String> {
        let differences = self.store.call(|store| store.differences()).await.ok()?;
        if differences.is_empty() {
            return None;
        }
        let mut notes = String::from(
            "## Changes made outside Kelpy\n\nKelpy found these differences from what it last changed and checked \
             on this server. Someone or something changed them outside Kelpy: tell the administrator when it \
             matters to the request, explain what changed and what it may mean, and do not undo them unless the \
             administrator asks. The administrator settles them in the Web UI (Accept or Put back).\n",
        );
        for difference in &differences {
            let view = view(difference);
            let mut note = format!(
                "\n- {} {}: expected {}, found {}.",
                kind_word(view.kind),
                view.subject,
                short(&view.expected),
                short(&view.found)
            );
            if let Some(diff) = &view.diff {
                note.push_str(&format!(
                    "\n```diff\n{}\n```",
                    cut(diff, MAX_NOTE).trim_end()
                ));
            }
            if notes.len() + note.len() > MAX_NOTES {
                notes.push_str("\n- …and more, listed in the Web UI.");
                break;
            }
            notes.push_str(&note);
        }
        Some(notes)
    }

    /// Accepts a difference: what was found is expected from now on.
    pub async fn accept(&self, id: i64) -> Result<(), AgentError> {
        self.store
            .call(move |store| store.settle_difference(id, Settled::Accepted))
            .await?;
        self.events.publish(Event::DifferencesChanged);
        self.write_page().await;
        Ok(())
    }

    /// Puts back a file as Kelpy last wrote it, in place of a difference found, as an operation of the
    /// administrator in the conversation `session_id`; only shown if `preview`.
    pub async fn put_back(
        &self,
        session_id: i64,
        id: i64,
        preview: bool,
    ) -> Result<Operated, AgentError> {
        let difference = self
            .store
            .call(move |store| {
                store
                    .differences()?
                    .into_iter()
                    .find(|difference| difference.id == id)
                    .ok_or(kelpy_storage::StoreError::DifferenceNotFound(id))
            })
            .await?;
        let (RecordKind::File, Some(content)) =
            (difference.record.kind, &difference.record.content)
        else {
            return Err(AgentError::Invalid(
                "only a file whose content Kelpy kept can be put back".into(),
            ));
        };
        let path = PathBuf::from(&difference.record.subject);
        let summary = format!("Put back {} as Kelpy last wrote it", path.display());
        let operated = self
            .operate(
                session_id,
                "put_back_file",
                summary,
                path,
                content.clone(),
                preview,
            )
            .await?;
        if let Operated::Done { action } = &operated
            && action.state == ActionState::Done
        {
            // The new record, saved with the action, settled it already: it is marked put back all the same.
            let _ = self
                .store
                .call(move |store| store.settle_difference(id, Settled::PutBack))
                .await;
            self.events.publish(Event::DifferencesChanged);
        }
        Ok(operated)
    }

    /// Puts a file back as it was before the change of the action `action_id`, from its backup, as an operation of
    /// the administrator; only shown if `preview`. The version replaced is backed up first.
    pub async fn restore(
        &self,
        session_id: i64,
        action_id: i64,
        preview: bool,
    ) -> Result<Operated, AgentError> {
        let action = self
            .store
            .call(move |store| store.action(session_id, action_id))
            .await?;
        let (Some(path), Some(backup)) = (
            action
                .change
                .as_ref()
                .and_then(|change| change.path.clone()),
            action
                .change
                .as_ref()
                .and_then(|change| change.backup.clone()),
        ) else {
            return Err(AgentError::Invalid(
                "this action has no backup to put back".into(),
            ));
        };
        let content = self.backup(Path::new(&backup)).await?;
        let summary = format!("Put back {path} as it was before this change");
        self.operate(
            session_id,
            "restore_file",
            summary,
            PathBuf::from(path),
            content,
            preview,
        )
        .await
    }

    /// The content of a backup, which must be in the backups folder.
    async fn backup(&self, path: &Path) -> Result<String, AgentError> {
        let folder = tokio::fs::canonicalize(&self.backups).await;
        let file = tokio::fs::canonicalize(path).await;
        match (folder, file) {
            (Ok(folder), Ok(file)) if file.starts_with(&folder) => {
                tokio::fs::read_to_string(&file).await.map_err(|error| {
                    AgentError::Invalid(format!("the backup cannot be read: {error}"))
                })
            }
            _ => Err(AgentError::Invalid(
                "the backup is gone from Kelpy's backups folder".into(),
            )),
        }
    }

    /// Writes `content` to the file at `path`, as the operation `tool` of the administrator in the conversation
    /// `session_id`: planned, and shown if `preview`; else recorded, run and recorded again with what it gave.
    async fn operate(
        &self,
        session_id: i64,
        tool: &'static str,
        summary: String,
        path: PathBuf,
        content: String,
        preview: bool,
    ) -> Result<Operated, AgentError> {
        let secrets = self.secrets(session_id);
        let mut prepared = Prepared {
            reason: "Asked by the administrator.".to_owned(),
            summary,
            work: Work::ChangeFile(Box::new(FileChange {
                path,
                edit: Edit::Write(content),
                planned: None,
            })),
            already: None,
        };
        self.executor
            .plan(&mut prepared, &secrets)
            .await
            .map_err(|error| AgentError::Invalid(error.to_string()))?;
        if let Some(already) = prepared.already {
            return Err(AgentError::Invalid(already));
        }
        let mut change = prepared.card().map(change_of);
        // The version put back may hold secrets: like the version replaced, it is not shown.
        if prepared.planned().is_some_and(|planned| planned.secret)
            && let Some(change) = change.as_mut()
        {
            change.diff = "The file may hold secrets: neither version is shown.".to_owned();
        }
        if preview {
            return Ok(Operated::Preview {
                summary: prepared.summary,
                change: change.unwrap_or_else(|| change_of(Card::default())),
            });
        }
        let (summary, reason) = (prepared.summary.clone(), prepared.reason.clone());
        let mut action = self
            .store
            .call(move |store| {
                let mut action = store.insert_action(&NewAction {
                    session_id,
                    message_id: None,
                    step: 0,
                    position: 0,
                    call_id: "",
                    tool,
                    input: "{}",
                    reason: &reason,
                    summary: &summary,
                    command: None,
                    change: change.as_ref(),
                    plan: None,
                    plan_step: None,
                    state: ActionState::Running,
                    error: None,
                })?;
                action.phase = Some("starting the change".to_owned());
                store.update_action(&action)?;
                Ok(action)
            })
            .await?;
        self.publish(&action);
        let started = Instant::now();
        let output = Output::new();
        let ran = self.executor.run(&prepared, &secrets, &output).await;
        action.output = secrets.redact(&output.snapshot().1);
        action.phase = output.phase();
        action.duration_ms = Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
        let records = match ran {
            Ok(ran) => {
                action.state = ActionState::Done;
                if let (Some(change), Some(backup)) = (action.change.as_mut(), ran.backup) {
                    change.backup = Some(backup.to_string_lossy().into_owned());
                }
                ran.records
            }
            Err(error) => {
                action.state = ActionState::Failed;
                action.error = Some(secrets.redact(&error.to_string()));
                Vec::new()
            }
        };
        let saved = action.clone();
        self.store
            .call(move |store| store.update_action(&saved))
            .await?;
        self.record(action.id, records).await?;
        self.publish(&action);
        Ok(Operated::Done {
            action: Box::new(action),
        })
    }

    /// Tells the interfaces that an operation of the administrator changed.
    fn publish(&self, action: &Action) {
        self.events.publish(Event::ActionChanged {
            session_id: action.session_id,
            action: Box::new(action.clone()),
        });
    }

    /// The handover report of a conversation, in Markdown: what Kelpy changed, what did not complete, the secrets
    /// and where they went, what stays on the server, and the differences found.
    pub async fn report(&self, session_id: i64) -> Result<String, AgentError> {
        let (session, actions, differences) = self
            .store
            .call(move |store| {
                Ok((
                    store.session(session_id)?,
                    store.actions(session_id)?,
                    store.differences()?,
                ))
            })
            .await?;
        let mut ports = None;
        let tools = Tools::new(self.executor.os());
        if let Ok(mut looking) = tools.prepare("listening_ports", "{}")
            && self.executor.resolve(&mut looking).is_ok()
            && let Ok(ran) = self
                .executor
                .run(&looking, &Secrets::new(), &Output::new())
                .await
        {
            ports = Some(ran.output);
        }
        Ok(report(
            &session.title,
            session.created_at,
            &actions,
            &differences,
            ports.as_deref(),
        ))
    }

    /// The operations of a conversation that succeeded, in order, with their arguments and checks, as JSON.
    pub async fn export(&self, session_id: i64) -> Result<Value, AgentError> {
        let (session, actions) = self
            .store
            .call(move |store| Ok((store.session(session_id)?, store.actions(session_id)?)))
            .await?;
        let operations: Vec<Value> = actions
            .iter()
            .filter(|action| succeeded(action) && is_operation(&action.tool))
            .map(|action| {
                let mut arguments: Value =
                    serde_json::from_str(&action.input).unwrap_or_else(|_| json!({}));
                if let Some(arguments) = arguments.as_object_mut() {
                    arguments.remove("reason");
                }
                json!({
                    "tool": action.tool,
                    "arguments": arguments,
                    "summary": action.summary,
                    "reason": action.reason,
                    "command": action.command,
                    "preconditions_and_checks": action.change.as_ref().map(|change| change.notes.clone()),
                    "result": action.output.lines().take(20).collect::<Vec<_>>().join("\n"),
                })
            })
            .collect();
        Ok(json!({
            "format": "kelpy-operations",
            "version": 1,
            "exported_at": utc(unix_now()),
            "server": { "host": host(), "os": std::env::consts::OS },
            "conversation": { "id": session.id, "title": session.title },
            "note": "The operations that succeeded, in order, with their arguments as given: secrets appear as \
                     their placeholders. Not repeatable until replayed on another server.",
            "operations": operations,
        }))
    }
}

/// The memory page `server.md`: what Kelpy changed and expects to find, by kind, and what was changed outside it.
fn page(records: &[&Record], differences: &[Difference]) -> String {
    let mut page = String::from(
        "# Server\n\nWhat Kelpy changed on this server, checked, and expects to find; and what was changed outside \
         Kelpy since. Kelpy writes this page from its records: write your own notes on other pages.\n",
    );
    for (kind, heading) in [
        (RecordKind::File, "Files"),
        (RecordKind::Service, "Services"),
        (RecordKind::Package, "Packages"),
    ] {
        let lines: Vec<String> = records
            .iter()
            .filter(|record| record.kind == kind)
            .map(|record| match kind {
                RecordKind::File => {
                    format!("- {} (since {})", record.subject, utc(record.recorded_at))
                }
                _ => format!("- {}: {}", record.subject, record.expected),
            })
            .collect();
        if !lines.is_empty() {
            page.push_str(&format!("\n## {heading}\n\n{}\n", lines.join("\n")));
        }
    }
    if records.is_empty() {
        page.push_str("\nKelpy has changed nothing on this server yet.\n");
    }
    if !differences.is_empty() {
        let lines: Vec<String> = differences
            .iter()
            .map(|difference| {
                format!(
                    "- {} {}: {}, found on {}",
                    kind_word(difference.record.kind),
                    difference.record.subject,
                    if difference.found.starts_with("sha256:") {
                        "changed".to_owned()
                    } else {
                        format!("now {}", difference.found)
                    },
                    utc(difference.found_at)
                )
            })
            .collect();
        page.push_str(&format!(
            "\n## Changed outside Kelpy, not settled\n\n{}\n",
            lines.join("\n")
        ));
    }
    page
}

/// A difference as the administrator sees it.
fn view(difference: &Difference) -> DifferenceView {
    let record = &difference.record;
    let diff =
        match (&record.content, &difference.content) {
            (Some(expected), Some(found)) => Some(kelpy_tools::change::diff(
                Path::new(&record.subject),
                expected,
                found,
            )),
            (Some(expected), None) if difference.found == "missing" => Some(
                kelpy_tools::change::diff(Path::new(&record.subject), expected, ""),
            ),
            _ => None,
        };
    DifferenceView {
        id: difference.id,
        kind: record.kind,
        subject: record.subject.clone(),
        expected: record.expected.clone(),
        found: difference.found.clone(),
        diff,
        found_at: difference.found_at,
        recorded_at: record.recorded_at,
        can_put_back: record.kind == RecordKind::File && record.content.is_some(),
    }
}

/// A card's content as an action stores it.
pub(super) fn change_of(card: Card) -> Change {
    Change {
        diff: card.diff,
        notes: card.notes,
        floor: card.floor,
        path: card.path.map(|path| path.to_string_lossy().into_owned()),
        backup: None,
    }
}

fn kind(kind: RecordedKind) -> RecordKind {
    match kind {
        RecordedKind::File => RecordKind::File,
        RecordedKind::Service => RecordKind::Service,
        RecordedKind::Package => RecordKind::Package,
    }
}

fn recorded_kind(kind: RecordKind) -> RecordedKind {
    match kind {
        RecordKind::File => RecordedKind::File,
        RecordKind::Service => RecordedKind::Service,
        RecordKind::Package => RecordedKind::Package,
    }
}

fn kind_word(kind: RecordKind) -> &'static str {
    match kind {
        RecordKind::File => "The file",
        RecordKind::Service => "The service",
        RecordKind::Package => "The package",
    }
}

/// A digest shortened for reading; other states as they are.
fn short(state: &str) -> String {
    match state.strip_prefix("sha256:") {
        Some(hex) => format!("content {}…", &hex[..hex.len().min(12)]),
        None => state.to_owned(),
    }
}

/// `text` cut to at most `limit` bytes, where a character ends.
fn cut(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let end = (0..=limit)
        .rev()
        .find(|&index| text.is_char_boundary(index))
        .unwrap_or(0);
    &text[..end]
}

/// Whether the action ran to its end and did what it was for.
fn succeeded(action: &Action) -> bool {
    action.state == ActionState::Done && action.exit_code.is_none_or(|code| code == 0)
}

/// Whether a change found the system already as asked, and did nothing: each such action says so.
fn did_nothing(action: &Action) -> bool {
    action.output.to_lowercase().contains("nothing to change")
}

/// Whether the tool changes the system, or prepares a change.
fn is_operation(tool: &str) -> bool {
    matches!(
        tool,
        "edit_file"
            | "write_file"
            | "manage_service"
            | "install_packages"
            | "remove_packages"
            | "create_database"
            | "generate_secret"
            | "run_command"
            | "restore_file"
            | "put_back_file"
    )
}

/// This computer's name.
pub(super) fn host() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/proc/sys/kernel/hostname").ok())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "this server".to_owned())
}

/// The report of the conversation `title`, from its `actions`, the open `differences` and the listening `ports`.
fn report(
    title: &str,
    created_at: i64,
    actions: &[Action],
    differences: &[Difference],
    ports: Option<&str>,
) -> String {
    let mut report = format!(
        "# Kelpy report: {title}\n\n{}, {}. Conversation started {}; report written {}.\n",
        host(),
        std::env::consts::OS,
        utc(created_at),
        utc(unix_now())
    );
    let done: Vec<&Action> = actions
        .iter()
        .filter(|action| succeeded(action) && !did_nothing(action))
        .collect();
    let section = |report: &mut String, heading: &str, lines: Vec<String>| {
        if !lines.is_empty() {
            report.push_str(&format!("\n### {heading}\n\n{}\n", lines.join("\n")));
        }
    };
    report.push_str("\n## What Kelpy changed\n");
    let mut changed = false;
    let mut lines_of = |tools: &[&str], line: &dyn Fn(&Action) -> Option<String>| -> Vec<String> {
        let lines: Vec<String> = done
            .iter()
            .filter(|action| tools.contains(&action.tool.as_str()))
            .filter_map(|action| line(action))
            .collect();
        changed |= !lines.is_empty();
        lines
    };
    let installed = |action: &Action| {
        // "nginx 1.24.0 is installed." lines; an action with nothing to do says "Already installed".
        let versions: Vec<String> = action
            .output
            .lines()
            .filter_map(|line| line.strip_suffix(" is installed."))
            .map(str::to_owned)
            .collect();
        (!versions.is_empty()).then(|| format!("- Installed {}", versions.join(", ")))
    };
    let packages = lines_of(&["install_packages"], &installed);
    let removed = lines_of(&["remove_packages"], &|action| {
        Some(format!("- {}", action.summary))
    });
    let services = lines_of(&["manage_service"], &|action| {
        let after = action
            .output
            .lines()
            .find_map(|line| line.strip_prefix("After: "))?;
        Some(format!(
            "- {}: {}",
            action.summary,
            after.trim_end_matches('.')
        ))
    });
    let files = lines_of(
        &["edit_file", "write_file", "restore_file", "put_back_file"],
        &|action| {
            let change = action.change.as_ref()?;
            let path = change.path.as_deref().unwrap_or(&action.summary);
            Some(match (&change.backup, action.message_id) {
                (_, None) => format!("- {} (by the administrator)", action.summary),
                (Some(backup), _) => format!("- Changed {path}; the version before is in {backup}"),
                (None, _) => format!("- Created {path}"),
            })
        },
    );
    let databases = lines_of(&["create_database"], &|action| {
        let arguments: Value = serde_json::from_str(&action.input).ok()?;
        Some(format!(
            "- Database {}, user {}@localhost, password {}",
            arguments["name"].as_str().unwrap_or_default(),
            arguments["user"].as_str().unwrap_or_default(),
            arguments["password"].as_str().unwrap_or_default()
        ))
    });
    let commands = lines_of(&["run_command"], &|action| {
        Some(format!(
            "- `{}`",
            action
                .command
                .as_deref()
                .unwrap_or_default()
                .replace('\n', "; ")
        ))
    });
    section(&mut report, "Packages", [packages, removed].concat());
    section(&mut report, "Services", services);
    section(&mut report, "Files", files);
    section(&mut report, "Databases", databases);
    section(&mut report, "Commands written by the AI model", commands);
    if !changed {
        report.push_str("\nNothing.\n");
    }

    let unfinished: Vec<String> = actions
        .iter()
        .filter(|action| is_operation(&action.tool) && !succeeded(action))
        .filter(|action| action.state != ActionState::Denied || action.error.is_some())
        .map(|action| {
            let state = match action.state {
                ActionState::Uncertain => "outcome uncertain",
                ActionState::Denied => "not allowed",
                ActionState::Done => "ran with errors",
                _ => "failed",
            };
            format!(
                "- {} ({state}){}",
                action.summary,
                action
                    .error
                    .as_deref()
                    .map(|error| format!(": {error}"))
                    .unwrap_or_default()
            )
        })
        .collect();
    if !unfinished.is_empty() {
        report.push_str(&format!(
            "\n## Not completed\n\n{}\n",
            unfinished.join("\n")
        ));
    }

    // Where each secret was written, by the placeholders in the changes that succeeded.
    let mut secrets: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for action in done.iter().filter(|action| action.plan.is_none()) {
        if action.tool == "generate_secret" {
            if let Ok(arguments) = serde_json::from_str::<Value>(&action.input)
                && let Some(name) = arguments["name"].as_str()
            {
                secrets.entry(name.to_owned()).or_default();
            }
            continue;
        }
        for (name, places) in secrets.iter_mut() {
            if action
                .input
                .contains(&kelpy_tools::secret::placeholder(name))
            {
                let place = match (&action.change, action.tool.as_str()) {
                    (
                        Some(Change {
                            path: Some(path), ..
                        }),
                        _,
                    ) => path.clone(),
                    (_, "create_database") => "the database user".to_owned(),
                    _ => action.summary.clone(),
                };
                if !places.contains(&place) {
                    places.push(place);
                }
            }
        }
    }
    if !secrets.is_empty() {
        let lines: Vec<String> = secrets
            .iter()
            .map(|(name, places)| {
                if places.is_empty() {
                    format!("- {name}: generated, written nowhere")
                } else {
                    format!("- {name}: in {}", places.join(", "))
                }
            })
            .collect();
        report.push_str(&format!(
            "\n## Generated secrets\n\nTheir values are not in this report: they are where they were written. \
             Kelpy keeps them only until it stops.\n\n{}\n",
            lines.join("\n")
        ));
    }

    let plans: Vec<String> = actions
        .iter()
        .filter(|action| action.state == ActionState::Done)
        .filter_map(|action| Some((action.id, action.plan.as_ref()?)))
        .map(|(id, plan)| {
            let carried = plan
                .steps
                .iter()
                .enumerate()
                .filter(|(index, _)| {
                    actions.iter().any(|action| {
                        succeeded(action)
                            && action.plan_step.is_some_and(|step| {
                                step.plan_id == id && step.step as usize == *index
                            })
                    })
                })
                .count();
            let steps = plan.steps.len();
            let noun = if steps == 1 { "step" } else { "steps" };
            format!("- {} ({carried} of {steps} {noun} done)", plan.goal)
        })
        .collect();
    if !plans.is_empty() {
        report.push_str(&format!("\n## Plans approved\n\n{}\n", plans.join("\n")));
    }

    if !differences.is_empty() {
        let lines: Vec<String> = differences
            .iter()
            .map(|difference| {
                let found = if difference.found.starts_with("sha256:") {
                    "changed (the diff is in the Web UI)".to_owned()
                } else {
                    format!("found {}", difference.found)
                };
                format!(
                    "- {} {}: {found}, on {}",
                    kind_word(difference.record.kind),
                    difference.record.subject,
                    utc(difference.found_at)
                )
            })
            .collect();
        report.push_str(&format!(
            "\n## Changed outside Kelpy, not settled\n\n{}\n",
            lines.join("\n")
        ));
    }
    if let Some(ports) = ports {
        report.push_str(&format!(
            "\n## Listening ports now\n\n```\n{}\n```\n",
            ports.trim_end()
        ));
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use kelpy_storage::{Plan, PlanStep, PlanStepRef, Record};

    fn action(id: i64, tool: &str, input: &str, output: &str) -> Action {
        Action {
            id,
            session_id: 1,
            message_id: Some(2),
            after_message: None,
            step: 0,
            position: 0,
            call_id: String::new(),
            tool: tool.into(),
            input: input.into(),
            reason: String::new(),
            summary: format!("Do {tool}"),
            command: None,
            state: ActionState::Done,
            exit_code: None,
            output: output.into(),
            error: None,
            created_at: 0,
            duration_ms: None,
            started_at: None,
            change: None,
            phase: None,
            plan: None,
            plan_step: None,
        }
    }

    #[test]
    fn the_report_says_what_changed_where_secrets_went_and_what_did_not_complete() {
        let secret = action(1, "generate_secret", r#"{"name":"wp_db"}"#, "Generated");
        let packages = action(
            2,
            "install_packages",
            r#"{"packages":["nginx"]}"#,
            "nginx 1.24.0 is installed.",
        );
        let mut config = action(
            3,
            "write_file",
            r#"{"path":"/var/www/wp-config.php","content":"'{{secret:wp_db}}'"}"#,
            "Wrote it.",
        );
        config.change = Some(Change {
            diff: String::new(),
            notes: Vec::new(),
            floor: None,
            path: Some("/var/www/wp-config.php".into()),
            backup: None,
        });
        let database = action(
            4,
            "create_database",
            r#"{"name":"wordpress","user":"wp","password":"{{secret:wp_db}}"}"#,
            "Created the database wordpress.",
        );
        let mut uncertain = action(5, "manage_service", "{}", "");
        uncertain.state = ActionState::Uncertain;
        uncertain.error = Some("Kelpy stopped while starting the service php8.3-fpm".into());
        let mut plan = action(6, "propose_plan", r#"{"steps":["{{secret:wp_db}}"]}"#, "");
        plan.plan = Some(Plan {
            goal: "Install WordPress".into(),
            steps: vec![
                PlanStep {
                    tool: "install_packages".into(),
                    input: String::new(),
                    summary: String::new(),
                    detail: None,
                    floor: None,
                };
                2
            ],
        });
        let mut step = packages.clone();
        step.plan_step = Some(PlanStepRef {
            plan_id: 6,
            step: 0,
        });
        let difference = Difference {
            id: 1,
            record: Record {
                id: 1,
                kind: RecordKind::Service,
                subject: "nginx".into(),
                expected: "active, enabled".into(),
                content: None,
                action_id: None,
                recorded_at: 0,
                secret: false,
            },
            found: "inactive, enabled".into(),
            content: None,
            found_at: 0,
        };
        // The same file again, already as asked: it did nothing.
        let again = action(
            8,
            "write_file",
            r#"{"path":"/var/www/wp-config.php","content":"'{{secret:wp_db}}'"}"#,
            "/var/www/wp-config.php already holds this content: nothing to change.",
        );
        let report = report(
            "WordPress",
            0,
            &[
                secret, packages, config, database, uncertain, plan, step, again,
            ],
            &[difference],
            Some("tcp LISTEN 0.0.0.0:80 nginx"),
        );
        for expected in [
            "# Kelpy report: WordPress",
            "- Installed nginx 1.24.0",
            "- Created /var/www/wp-config.php",
            "- Database wordpress, user wp@localhost, password {{secret:wp_db}}",
            "- Do manage_service (outcome uncertain): Kelpy stopped while",
            "- wp_db: in /var/www/wp-config.php, the database user\n",
            "- Install WordPress (1 of 2 steps done)",
            "- The service nginx: found inactive, enabled, on",
            "0.0.0.0:80",
        ] {
            assert!(report.contains(expected), "{expected}\n\n{report}");
        }
    }
}
