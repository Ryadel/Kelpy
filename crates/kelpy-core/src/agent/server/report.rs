//! The handover report of a conversation, and the export of its operations.

use std::collections::BTreeMap;

use kelpy_common::{unix_now, utc};
use kelpy_storage::{Action, ActionState, Change, Difference};
use kelpy_tools::{Output, Secrets, Tools};
use serde_json::{Value, json};

use super::{Server, kind_word};
use crate::agent::AgentError;

impl Server {
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
fn host() -> String {
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
            "\n## Unexpected changes, not settled\n\n{}\n",
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
    use kelpy_storage::{Plan, PlanStep, PlanStepRef, Record, RecordKind};

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
