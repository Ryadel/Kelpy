//! What Kelpy expects to find on the server after the changes it made and checked (its records), and the
//! differences found since.
//!
//! The records are checked when Kelpy starts and before each answer, at most once a minute: a difference is noted
//! once, shown to the administrator and told to the model, never reverted by itself. The administrator accepts it,
//! and it is expected from then on, or puts Kelpy's version back.
//!
//! The records are also written for people to read, as the memory page `server.md`. The page is a record itself: an
//! edit made outside Kelpy is a difference like any other, and the page is not written again until it is settled.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kelpy_common::{lock, utc};
use kelpy_storage::{Difference, Record, RecordKind, Settled};
use kelpy_tools::{Recorded, RecordedKind};
use serde::Serialize;

use super::{Server, kind_word};
use crate::agent::AgentError;
use crate::events::Event;

/// How often the records are checked at most, before answers.
const CHECK_EVERY: Duration = Duration::from_secs(60);
/// The most of the differences told to the model.
const MAX_NOTES: usize = 12 << 10;
/// The most of one difference's diff told to the model.
const MAX_NOTE: usize = 3 << 10;

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

impl Server {
    /// Saves what a change of the action `action_id` left, as Kelpy expects to find it from now on.
    pub(in crate::agent) async fn record(
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
    async fn write_page(&self) {
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
    pub(in crate::agent) async fn check(&self, now: bool) -> Result<(), AgentError> {
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
    pub(in crate::agent) async fn notes(&self) -> Option<String> {
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
