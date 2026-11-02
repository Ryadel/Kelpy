//! Kelpy tools: what the model may use to look at and change the system, and the executor that runs it.
//!
//! [`Tools`] is the catalog for one kind of system: each tool's name, description and arguments for the model,
//! and [`Tools::prepare`], which turns a call into a [`Prepared`] action: what it runs, a one-line summary and the
//! reason the model gave. The inspections only read, by construction: Kelpy fixes their commands, and the model only
//! fills in arguments, which are checked and passed as arguments, never through a shell. `run_command` runs a command
//! the model wrote, in the system's shell: Kelpy cannot tell what it does, so it is always approved before it runs
//! ([`Prepared::always_asks`]). `edit_file` and `write_file` change files ([`change`]): Kelpy plans the change before
//! it is shown, backs the file up and checks it. [`Executor`] runs the actions: on this computer now, over SSH later.

mod account;
pub mod change;
mod executor;
mod files;
mod inspect;
mod output;
mod script;
mod tree;

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Map, Value, json};

pub use account::Account;
pub use change::{Edit, FileChange, Planned};
pub use executor::{Executor, Ran};
pub use output::Output;
pub use script::MAX_SCRIPT;

/// Why an action could not be prepared or run. Its text is the call's result for the model, and is shown in the
/// Command log.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// The call does not fit the tool: what is wrong, for the model to correct.
    #[error("{0}")]
    Invalid(String),
    /// None of the programs that could run a step is installed.
    #[error("{} is not installed", .0.join(", "))]
    NotInstalled(Vec<String>),
    #[error("{} may hold secrets: Kelpy never sends it to the AI service", .0.display())]
    Protected(PathBuf),
    #[error("{} may hold secrets: Kelpy does not change such files with this tool", .0.display())]
    Secret(PathBuf),
    #[error("{} is Kelpy's own data: Kelpy does not change it", .0.display())]
    OwnData(PathBuf),
    #[error("{} is over {} MiB: too large to change and back up", .0.display(), change::MAX_FILE >> 20)]
    TooLarge(PathBuf),
    #[error("{} changed after the change was shown: nothing was written, plan it again", .0.display())]
    ChangedMeanwhile(PathBuf),
    #[error("{} cannot be written: {source}", path.display())]
    Write { path: PathBuf, source: io::Error },
    /// The change broke the file's check, and the previous version was put back.
    #[error(
        "`{check}` failed after the change, so the previous version was put back:
{output}"
    )]
    CheckFailed { check: String, output: String },
    /// A folder, a named pipe or a device: reading it could wait without end.
    #[error("{} is not a regular file", .0.display())]
    NotRegularFile(PathBuf),
    #[error("{} is not a text file", .0.display())]
    NotText(PathBuf),
    #[error("{} cannot be read: {source}", path.display())]
    Read { path: PathBuf, source: io::Error },
    #[error("{} cannot be listed: {source}", path.display())]
    List { path: PathBuf, source: io::Error },
    #[error("{program} could not start: {source}")]
    Start { program: String, source: io::Error },
    #[error("{program} could not be followed: {source}")]
    Wait { program: String, source: io::Error },
    #[error("it did not end within {}", span(.0))]
    TimedOut(Duration),
}

/// `duration` in minutes when whole, else in seconds.
fn span(duration: &Duration) -> String {
    match duration.as_secs() {
        seconds if seconds >= 60 && seconds % 60 == 0 => format!("{} minutes", seconds / 60),
        seconds => format!("{seconds} seconds"),
    }
}

/// An [`ToolError::Invalid`] call.
fn invalid(problem: impl Into<String>) -> ToolError {
    ToolError::Invalid(problem.into())
}

/// The kind of system the tools look at: each runs its own commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    Windows,
    MacOs,
}

impl Os {
    /// The system Kelpy runs on.
    pub fn current() -> Self {
        match std::env::consts::OS {
            "windows" => Self::Windows,
            "macos" => Self::MacOs,
            _ => Self::Linux,
        }
    }
}

/// A tool as the model sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    pub name: &'static str,
    /// Whether it changes the system: offered only with System access Full.
    pub changes: bool,
    pub description: String,
    /// The JSON Schema of its arguments.
    pub parameters: Value,
}

/// A program and its arguments, run without a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub program: String,
    pub args: Vec<String>,
    /// Environment variables set for it, besides those of Kelpy.
    pub env: Vec<(String, String)>,
    /// The command as the administrator would type it, shown before it runs and in the Command log.
    pub display: String,
}

/// What an action does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Work {
    /// Commands run in order. Each comes with alternatives for systems without it: the executor runs the first
    /// one installed, and [`Executor::resolve`] chooses it before the action is shown.
    Commands(Vec<Vec<Command>>),
    /// A command the model wrote, run in `directory` by the first of these shells installed, chosen like a step's
    /// command.
    Script {
        shells: Vec<Command>,
        directory: PathBuf,
    },
    ReadFile(PathBuf),
    ListDirectory(PathBuf),
    /// A change of a file, planned by [`Executor::plan`] before it is shown.
    ChangeFile(Box<FileChange>),
}

/// A call of a tool, checked and ready to show and run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    /// Why the model wants it, in its words.
    pub reason: String,
    /// One line saying what it does, such as "List the services matching nginx".
    pub summary: String,
    pub work: Work,
}

impl Prepared {
    /// Whether it waits for the administrator's approval in every Approval mode: a command the model wrote may do
    /// anything.
    pub fn always_asks(&self) -> bool {
        matches!(self.work, Work::Script { .. }) || self.floor().is_some()
    }

    /// Whether it changes the system.
    pub fn changes(&self) -> bool {
        matches!(self.work, Work::ChangeFile(_))
    }

    /// Why it waits for approval in every Approval mode, for a change where a mistake could lock the administrator
    /// out or keep the system from starting.
    pub fn floor(&self) -> Option<&str> {
        self.planned()?.floor.as_deref()
    }

    /// The change it makes, once planned.
    pub fn planned(&self) -> Option<&Planned> {
        match &self.work {
            Work::ChangeFile(change) => change.planned.as_ref(),
            _ => None,
        }
    }

    /// The commands it runs, one per line, once resolved; `None` for an action that is not a command.
    pub fn command_line(&self) -> Option<String> {
        match &self.work {
            Work::Script { shells, .. } => shells.first().map(|shell| shell.display.clone()),
            Work::Commands(steps) => Some(
                steps
                    .iter()
                    .filter_map(|alternatives| alternatives.first())
                    .map(|command| command.display.as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            Work::ReadFile(_) | Work::ListDirectory(_) | Work::ChangeFile(_) => None,
        }
    }
}

/// The most log lines one call may ask for.
const MAX_LOG_LINES: u64 = 500;
const DEFAULT_LOG_LINES: u64 = 100;

/// The catalog of tools for one kind of system.
#[derive(Debug, Clone)]
pub struct Tools {
    os: Os,
}

impl Tools {
    pub fn new(os: Os) -> Self {
        Self { os }
    }

    /// The tools, for the model.
    pub fn list(&self) -> Vec<Tool> {
        let system = match self.os {
            Os::Linux => "Linux",
            Os::Windows => "Windows",
            Os::MacOs => "macOS",
        };
        let filter = (
            "filter",
            "Only those whose name contains this text. Letters, digits and . _ + : @ - only.",
        );
        let mut tools = vec![
            tool(
                "system_info",
                format!("The {system} version, host name, uptime, processors and memory."),
                &[],
                &[],
            ),
            tool("list_services", "The services and their state.".into(), &[], &[filter]),
            tool(
                "service_status",
                "The state of one service in detail: running or not, start mode, process, recent messages.".into(),
                &[("name", "The service's name, as in list_services.")],
                &[],
            ),
            tool(
                "installed_packages",
                match self.os {
                    Os::Windows => "The installed programs and the Windows features turned on (such as IIS).",
                    _ => "The installed packages and their versions.",
                }
                .into(),
                &[],
                &[filter],
            ),
            tool(
                "listening_ports",
                "The network ports that programs listen on, with the programs.".into(),
                &[],
                &[],
            ),
            tool("disk_usage", "The disks and file systems: size, used, free.".into(), &[], &[]),
            tool(
                "processes",
                "The running processes, the busiest first: name, CPU, memory. Command lines are not shown, as they \
                 may hold passwords."
                    .into(),
                &[],
                &[],
            ),
            tool(
                "read_file",
                "The content of a text file, up to 256 KiB. Files that may hold secrets (private keys, .env, \
                 wp-config.php, password databases) are refused."
                    .into(),
                &[("path", "The file's absolute path.")],
                &[],
            ),
            tool(
                "list_directory",
                "The files and folders in a folder, with sizes and dates.".into(),
                &[("path", "The folder's absolute path.")],
                &[],
            ),
        ];
        let lines = (
            "lines",
            "How many of the latest entries, 100 if not given, at most 500.",
        );
        tools.push(match self.os {
            Os::Linux => tool(
                "recent_logs",
                "The latest entries of the system journal, of one service if given.".into(),
                &[],
                &[
                    ("unit", "Only this systemd unit, such as nginx.service."),
                    lines,
                ],
            ),
            Os::Windows => tool(
                "recent_logs",
                "The latest entries of the System or Application event log.".into(),
                &[("log", "System or Application.")],
                &[
                    ("source", "Only entries from this source (provider)."),
                    lines,
                ],
            ),
            Os::MacOs => tool(
                "recent_logs",
                "The system log of the last minutes.".into(),
                &[],
                &[lines],
            ),
        });
        let checked = "Kelpy shows the change as a diff, backs the file up first, and checks it where it can (such as                        nginx -t): if the change breaks the check, the previous version is put back at once. Files that                        may hold secrets and files over 20 MiB cannot be changed.";
        tools.push(Tool {
            changes: true,
            ..tool(
                "edit_file",
                format!(
                    "Changes a text file by replacing one exact piece of it, which must occur exactly once: copy it                      from read_file, with enough lines around it to be unique. {checked}"
                ),
                &[
                    ("path", "The file's absolute path."),
                    ("old", "The exact text to replace, as it is in the file."),
                    ("new", "The text that replaces it; empty to remove it."),
                ],
                &[],
            )
        });
        tools.push(Tool {
            changes: true,
            ..tool(
                "write_file",
                format!(
                    "Creates a text file, or replaces all its content; its folder must exist. To change part of an                      existing file, use edit_file. {checked}"
                ),
                &[
                    ("path", "The file's absolute path."),
                    ("content", "The whole content of the file."),
                ],
                &[],
            )
        });
        tools.push(tool(
            "run_command",
            script::description(self.os),
            &[
                (
                    "command",
                    "The command, as the administrator would type it; several lines if needed.",
                ),
                (
                    "directory",
                    "The absolute path of the folder it starts in, which must exist.",
                ),
            ],
            &[],
        ));
        tools
    }

    /// Checks a call of the tool `name` with its JSON `arguments`. The error, for the model, says what is wrong.
    pub fn prepare(&self, name: &str, arguments: &str) -> Result<Prepared, ToolError> {
        let arguments: Map<String, Value> = if arguments.trim().is_empty() {
            Map::new()
        } else {
            serde_json::from_str(arguments)
                .map_err(|error| invalid(format!("the arguments are not a JSON object: {error}")))?
        };
        let text = |field: &str| -> Result<Option<String>, ToolError> {
            match arguments.get(field) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(value)) if value.trim().is_empty() => Ok(None),
                Some(Value::String(value)) => Ok(Some(value.trim().to_owned())),
                Some(_) => Err(invalid(format!("`{field}` must be a string"))),
            }
        };
        let required =
            |field: &str| text(field)?.ok_or_else(|| invalid(format!("`{field}` is required")));
        let reason = text("reason")?.unwrap_or_default();
        let filter = text("filter")?
            .map(|filter| name_like(&filter, "filter"))
            .transpose()?;
        let matching = |summary: &str| match &filter {
            Some(filter) => format!("{summary} matching {filter}"),
            None => summary.to_owned(),
        };
        let os = self.os;
        let (summary, work) = match name {
            "system_info" => (
                "Show the system information".to_owned(),
                inspect::system_info(os),
            ),
            "list_services" => (
                matching("List the services"),
                inspect::services(os, filter.as_deref()),
            ),
            "service_status" => {
                let service = argument_like(&required("name")?, "name")?;
                (
                    format!("Show the service {service}"),
                    inspect::service(os, &service),
                )
            }
            "installed_packages" => (
                matching("List the installed packages"),
                inspect::packages(os, filter.as_deref()),
            ),
            "listening_ports" => ("List the listening ports".to_owned(), inspect::ports(os)),
            "disk_usage" => ("Show the disk usage".to_owned(), inspect::disks(os)),
            "processes" => ("List the processes".to_owned(), inspect::processes(os)),
            "read_file" => {
                let path = absolute(&required("path")?)?;
                (format!("Read {}", path.display()), Work::ReadFile(path))
            }
            "list_directory" => {
                let path = absolute(&required("path")?)?;
                (
                    format!("List {}", path.display()),
                    Work::ListDirectory(path),
                )
            }
            "recent_logs" => {
                let lines = match arguments.get("lines") {
                    None | Some(Value::Null) => DEFAULT_LOG_LINES,
                    Some(value) => value
                        .as_u64()
                        .filter(|lines| (1..=MAX_LOG_LINES).contains(lines))
                        .ok_or_else(|| {
                            invalid(format!(
                                "`lines` must be a number from 1 to {MAX_LOG_LINES}"
                            ))
                        })?,
                };
                let unit = text("unit")?
                    .map(|unit| argument_like(&unit, "unit"))
                    .transpose()?;
                let source = text("source")?
                    .map(|source| argument_like(&source, "source"))
                    .transpose()?;
                let log = match (os, text("log")?) {
                    (Os::Windows, Some(log)) if log.eq_ignore_ascii_case("application") => {
                        "Application"
                    }
                    (Os::Windows, Some(log)) if log.eq_ignore_ascii_case("system") => "System",
                    (Os::Windows, _) => {
                        return Err(invalid("`log` must be System or Application"));
                    }
                    _ => "",
                };
                let of = unit.as_deref().or(source.as_deref());
                let summary = match of {
                    Some(of) => format!("Show the latest {lines} log entries of {of}"),
                    None => format!("Show the latest {lines} log entries"),
                };
                (
                    summary,
                    inspect::logs(os, lines, unit.as_deref(), log, source.as_deref()),
                )
            }
            "edit_file" | "write_file" => {
                let path = absolute(&required("path")?)?;
                // Taken as written: spaces and line ends matter in a file.
                let raw = |field: &str| -> Result<String, ToolError> {
                    let value = arguments
                        .get(field)
                        .and_then(Value::as_str)
                        .ok_or_else(|| invalid(format!("`{field}` is required, as a string")))?;
                    if value.len() > change::MAX_TEXT {
                        return Err(invalid(format!(
                            "`{field}` may hold at most {} KiB",
                            change::MAX_TEXT >> 10
                        )));
                    }
                    Ok(value.to_owned())
                };
                let (summary, edit) = if name == "edit_file" {
                    let old = raw("old")?;
                    if old.is_empty() {
                        return Err(invalid("`old` cannot be empty"));
                    }
                    let edit = Edit::Replace {
                        old,
                        new: raw("new")?,
                    };
                    (format!("Change {}", path.display()), edit)
                } else {
                    (
                        format!("Write {}", path.display()),
                        Edit::Write(raw("content")?),
                    )
                };
                let change = FileChange {
                    path,
                    edit,
                    planned: None,
                };
                (summary, Work::ChangeFile(Box::new(change)))
            }
            "run_command" => {
                let command = arguments
                    .get("command")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|command| !command.is_empty())
                    .ok_or_else(|| invalid("`command` is required, as a string"))?;
                if command.len() > MAX_SCRIPT {
                    return Err(invalid(format!(
                        "`command` may hold at most {} KiB",
                        MAX_SCRIPT >> 10
                    )));
                }
                if command.contains('\0') {
                    return Err(invalid("`command` cannot hold a NUL character"));
                }
                let directory = absolute(&required("directory")?)?;
                (
                    format!("Run a command in {}", directory.display()),
                    script::script(os, command, directory),
                )
            }
            other => return Err(invalid(format!("there is no tool named `{other}`"))),
        };
        Ok(Prepared {
            reason,
            summary,
            work,
        })
    }
}

/// A tool whose arguments are `required` and `optional` strings, plus the `reason` every tool asks for.
fn tool(
    name: &'static str,
    description: String,
    required: &[(&str, &str)],
    optional: &[(&str, &str)],
) -> Tool {
    let mut properties = Map::new();
    properties.insert(
        "reason".into(),
        json!({
            "type": "string",
            "description": "Why you need this, in one short sentence in the user's language: the administrator \
                            reads it before approving.",
        }),
    );
    for (field, description) in required.iter().chain(optional) {
        let schema = if *field == "lines" {
            json!({ "type": "integer", "minimum": 1, "maximum": MAX_LOG_LINES, "description": description })
        } else {
            json!({ "type": "string", "description": description })
        };
        properties.insert((*field).into(), schema);
    }
    let required: Vec<&str> = std::iter::once("reason")
        .chain(required.iter().map(|(field, _)| *field))
        .collect();
    Tool {
        name,
        changes: false,
        description,
        parameters: json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        }),
    }
}

/// A name for a pattern: letters, digits and a few signs, so that it matches as written.
fn name_like(value: &str, field: &str) -> Result<String, ToolError> {
    let fits = value.len() <= 100
        && value
            .chars()
            .all(|char| char.is_alphanumeric() || "._+:@-".contains(char));
    if fits {
        Ok(value.to_owned())
    } else {
        Err(invalid(format!(
            "`{field}` may hold only letters, digits and . _ + : @ -"
        )))
    }
}

/// A free argument such as a service name: one line, not taken for an option.
fn argument_like(value: &str, field: &str) -> Result<String, ToolError> {
    if value.len() > 200 || value.chars().any(char::is_control) {
        return Err(invalid(format!("`{field}` must be one short line")));
    }
    if value.starts_with('-') {
        return Err(invalid(format!("`{field}` cannot start with -")));
    }
    Ok(value.to_owned())
}

fn absolute(path: &str) -> Result<PathBuf, ToolError> {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(invalid("the path must be absolute"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_asks_for_a_reason_and_describes_its_arguments() {
        for os in [Os::Linux, Os::Windows, Os::MacOs] {
            let tools = Tools::new(os).list();
            assert_eq!(tools.len(), 13);
            for tool in tools {
                assert_eq!(tool.parameters["required"][0], "reason", "{}", tool.name);
                assert_eq!(tool.parameters["additionalProperties"], false);
                assert!(!tool.description.is_empty());
            }
        }
    }

    /// Runs every inspection on this computer: `cargo test -p kelpy-tools -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "looks at the computer running the tests"]
    async fn every_inspection_runs_here() {
        let executor = Executor::local(Vec::new(), PathBuf::from("backups"));
        let tools = Tools::new(executor.os());
        let (service, log) = match executor.os() {
            Os::Windows => ("EventLog", r#""log":"System","#),
            _ => ("cron", ""),
        };
        let home = std::env::temp_dir();
        for (name, arguments) in [
            ("system_info", "{}".to_owned()),
            ("list_services", r#"{"filter":"e"}"#.to_owned()),
            ("service_status", format!(r#"{{"name":"{service}"}}"#)),
            ("installed_packages", r#"{"filter":"a"}"#.to_owned()),
            ("listening_ports", "{}".to_owned()),
            ("disk_usage", "{}".to_owned()),
            ("processes", "{}".to_owned()),
            ("recent_logs", format!(r#"{{{log}"lines":5}}"#)),
            (
                "list_directory",
                serde_json::json!({ "path": home }).to_string(),
            ),
        ] {
            let mut prepared = tools.prepare(name, &arguments).unwrap();
            executor.resolve(&mut prepared).unwrap();
            let started = std::time::Instant::now();
            let ran = executor.run(&prepared, &Output::new()).await.unwrap();
            println!(
                "== {name} ({:?}, exit {:?}, {} bytes)
{}",
                started.elapsed(),
                ran.exit_code,
                ran.output.len(),
                ran.output.chars().take(400).collect::<String>()
            );
            assert_eq!(ran.exit_code.unwrap_or(0), 0, "{name}: {}", ran.output);
        }
    }

    #[test]
    fn calls_become_fixed_commands_with_checked_arguments() {
        let linux = Tools::new(Os::Linux);
        let prepared = linux
            .prepare("service_status", r#"{"reason":"Is it up?","name":"nginx"}"#)
            .unwrap();
        assert_eq!(prepared.reason, "Is it up?");
        assert_eq!(prepared.summary, "Show the service nginx");
        let Work::Commands(steps) = &prepared.work else {
            panic!("{prepared:?}");
        };
        assert_eq!(steps[0][0].program, "systemctl");
        assert_eq!(
            steps[0][0].args,
            ["status", "nginx", "--no-pager", "--lines=20"]
        );

        // Arguments are arguments: never options, never more than one line, never a shell.
        for (name, arguments) in [
            ("service_status", r#"{"name":"--help"}"#),
            ("service_status", "{\"name\":\"a\\nb\"}"),
            ("service_status", r#"{"reason":"x"}"#),
            ("list_services", r#"{"filter":"x; rm -rf /"}"#),
            ("read_file", r#"{"path":"relative/file"}"#),
            ("recent_logs", r#"{"lines":5000}"#),
            ("format_disk", "{}"),
            ("disk_usage", "[1]"),
        ] {
            assert!(
                linux.prepare(name, arguments).is_err(),
                "{name} {arguments}"
            );
        }
        assert!(!prepared.always_asks());

        // A command the model wrote: as written, by a shell, always asked.
        // An absolute path where the tests run.
        let folder = std::env::temp_dir();
        let call = serde_json::json!({
            "reason": "Free space",
            "command": " du -sh /var/* | sort -h \n",
            "directory": folder,
        });
        let prepared = linux.prepare("run_command", &call.to_string()).unwrap();
        assert_eq!(
            prepared.summary,
            format!("Run a command in {}", folder.display())
        );
        assert!(prepared.always_asks());
        assert_eq!(prepared.command_line().unwrap(), "du -sh /var/* | sort -h");
        let Work::Script { shells, directory } = &prepared.work else {
            panic!("{prepared:?}");
        };
        assert_eq!(directory, &folder);
        assert_eq!(
            (shells[0].program.as_str(), shells[1].program.as_str()),
            ("bash", "sh")
        );
        assert_eq!(shells[0].args, ["-c", "du -sh /var/* | sort -h"]);
        let long = serde_json::json!({ "command": "x".repeat(MAX_SCRIPT + 1) }).to_string();
        for arguments in [
            r#"{"command":"  "}"#,
            r#"{"command":1}"#,
            "{\"command\":\"a\\u0000\"}",
            &long,
        ] {
            assert!(
                linux.prepare("run_command", arguments).is_err(),
                "{arguments}"
            );
        }

        let windows = Tools::new(Os::Windows);
        let prepared = windows
            .prepare("service_status", r#"{"name":"O'Brien"}"#)
            .unwrap();
        assert!(prepared.command_line().unwrap().contains("'O''Brien'"));
        assert!(
            windows
                .prepare("recent_logs", r#"{"log":"Security"}"#)
                .is_err()
        );
        assert_eq!(
            windows
                .prepare("recent_logs", r#"{"log":"system","lines":20}"#)
                .unwrap()
                .summary,
            "Show the latest 20 log entries"
        );
    }
}
