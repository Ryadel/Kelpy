//! The operations the administrator asks for, outside the answers: putting a file back as it was before a change
//! (Restore), or as Kelpy last wrote it in place of a difference found (Put back). Each is shown first and runs only
//! while the file is as shown, then is kept in the conversation as an action.

use std::path::{Path, PathBuf};
use std::time::Instant;

use kelpy_storage::{Action, ActionState, Change, NewAction, RecordKind, Settled};
use kelpy_tools::{Card, Edit, FileChange, Output, Prepared, Work};
use serde::Serialize;

use super::{Server, change_of};
use crate::agent::AgentError;
use crate::events::Event;

/// What an operation of the administrator would do, or did.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Operated {
    /// Shown before it runs. Running it takes `fingerprint` back: it runs only while the file and the version
    /// written are still those shown.
    Preview {
        summary: String,
        change: Change,
        fingerprint: String,
    },
    /// It ran, or failed: the action says.
    Done { action: Box<Action> },
}

impl Server {
    /// Puts back a file as Kelpy last wrote it, in place of a difference found, as an operation of the
    /// administrator in the conversation `session_id`: shown without `confirmed`, run with the fingerprint shown.
    pub async fn put_back(
        &self,
        session_id: i64,
        id: i64,
        confirmed: Option<String>,
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
                confirmed,
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
    /// the administrator: shown without `confirmed`, run with the fingerprint shown. The version replaced is backed
    /// up first.
    pub async fn restore(
        &self,
        session_id: i64,
        action_id: i64,
        confirmed: Option<String>,
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
            confirmed,
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
    /// `session_id`: planned, and shown without `confirmed`; with it, planned again and run only if it is the change
    /// shown, then recorded with what it gave. The plan is made again because the file may change meanwhile: the
    /// fingerprint makes sure that what runs is what the administrator saw.
    async fn operate(
        &self,
        session_id: i64,
        tool: &'static str,
        summary: String,
        path: PathBuf,
        content: String,
        confirmed: Option<String>,
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
        let shown = fingerprint(prepared.planned());
        let Some(confirmed) = confirmed else {
            return Ok(Operated::Preview {
                summary: prepared.summary,
                change: change.unwrap_or_else(|| change_of(Card::default())),
                fingerprint: shown,
            });
        };
        if confirmed != shown {
            return Err(AgentError::Invalid(
                "the file, or the version to put back, changed after it was shown: nothing was written. Look at \
                 the change again"
                    .into(),
            ));
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
}

/// What a preview of a file change showed: the file as it was, or its absence, and the version written.
fn fingerprint(planned: Option<&kelpy_tools::Planned>) -> String {
    let Some(planned) = planned else {
        return String::new();
    };
    let before = planned
        .before
        .as_deref()
        .map_or_else(|| "absent".to_owned(), kelpy_tools::change::digest);
    kelpy_tools::change::digest(&format!(
        "{before}\n{}",
        kelpy_tools::change::digest(&planned.after)
    ))
}
