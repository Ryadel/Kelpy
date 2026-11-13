//! Kelpy tools: what the model may use to look at and change the system, and the executor that runs it.
//!
//! [`Tools`] is the catalog for one kind of system: each tool's name, description and arguments for the model,
//! and [`Tools::prepare`], which turns a call into a [`Prepared`] action: what it runs, a one-line summary and the
//! reason the model gave. The inspections only read, by construction: Kelpy fixes their commands, and the model only
//! fills in arguments, which are checked and passed as arguments, never through a shell. `run_command` runs a command
//! the model wrote, in the system's shell: Kelpy cannot tell what it does, so it is always approved before it runs
//! ([`Prepared::always_asks`]). The typed operations change the system in ways Kelpy understands: files ([`change`]),
//! services ([`service`]), packages ([`package`]) and databases ([`database`]). Kelpy plans each before it is shown
//! (the diff, the packages apt would install, the service's state), and once approved checks what it did and puts
//! back what it can when the check fails. A change already made is not made again: the action says so and ends.
//! `propose_plan` groups changes for one approval, and `generate_secret` makes secrets the model only names
//! ([`secret`]). [`Executor`] runs the actions: on this computer now, over SSH later.

mod account;
pub mod change;
pub mod database;
mod executor;
mod files;
mod inspect;
pub mod memory;
mod output;
pub mod package;
mod script;
pub mod secret;
pub mod service;
mod tree;
pub mod windows;

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Map, Value, json};

pub use account::Account;
pub use change::{Edit, FileChange, Planned};
pub use database::DatabaseChange;
pub use executor::{Executor, Observed, Ran};
pub use output::Output;
pub use package::PackageChange;
pub use script::MAX_SCRIPT;
pub use secret::{Registry, Secrets};
pub use service::{ServiceAction, ServiceChange};
pub use windows::{FeatureChange, SiteChange};

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
    /// The change did not give what was expected: what it is, and what was put back.
    #[error("{0}")]
    Unverified(String),
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
    pub kind: ToolKind,
    pub description: String,
    /// The JSON Schema of its arguments.
    pub parameters: Value,
}

/// What a tool is for, which decides when it is offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    /// It looks at the system: offered with System access Read-only and Full.
    Looks,
    /// It changes the system, or prepares a change: offered only with Full.
    Changes,
    /// It proposes a plan: offered with Full in the Approval mode Plan.
    Plans,
}

/// What Kelpy expects to find after a change it made and checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub kind: RecordedKind,
    /// The file's path, the service's or the package's name.
    pub subject: String,
    /// A file's digest, a service's state, `installed` or `absent` for a package.
    pub state: String,
    /// A file's content, when it may be shown later: not when it holds secrets, nor when it is large.
    pub content: Option<String>,
    /// Whether the file holds a secret: it is then never read for the model nor shown.
    pub secret: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordedKind {
    File,
    Service,
    Package,
}

/// What a change's card shows before it runs, besides its command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Card {
    /// A file's diff, or the packages installed (`+`) and removed (`-`), or a service's state.
    pub diff: String,
    /// How it is checked and undone, one line each.
    pub notes: Vec<String>,
    /// Why it waits for approval in every Approval mode, if it does.
    pub floor: Option<String>,
    /// The file it changes, links resolved.
    pub path: Option<PathBuf>,
}

/// Changes the model proposes to make, approved together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    pub goal: String,
    pub steps: Vec<Step>,
}

/// A step of a plan: one call, checked like any other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub tool: String,
    /// The arguments without the reason, as [`canonical`] JSON.
    pub input: String,
    pub summary: String,
    /// The commands it runs or the change of the file, as shown in the plan.
    pub detail: Option<String>,
    pub floor: Option<String>,
}

/// The most steps a plan may have.
pub const MAX_STEPS: usize = 40;

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
    Service(Box<ServiceChange>),
    Packages(Box<PackageChange>),
    Database(Box<DatabaseChange>),
    /// Windows features to install, such as IIS.
    Features(Box<FeatureChange>),
    /// An IIS site to create.
    Site(Box<SiteChange>),
    /// A secret to generate, by its name: nothing changes on the system.
    Secret(String),
    /// A page of Kelpy's memory to read.
    ReadMemory(String),
    /// A fact to add to a page of Kelpy's memory.
    Remember {
        page: String,
        note: String,
    },
    /// Changes proposed together: approving it runs nothing.
    Plan(Box<Proposal>),
}

/// A call of a tool, checked and ready to show and run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    /// Why the model wants it, in its words.
    pub reason: String,
    /// One line saying what it does, such as "List the services matching nginx".
    pub summary: String,
    pub work: Work,
    /// Set by [`Executor::plan`] when the system is already as the change asks: why there is nothing to do.
    pub already: Option<String>,
}

impl Prepared {
    /// Whether it waits for the administrator's approval in every Approval mode, outside a plan: a command the
    /// model wrote may do anything, and a floor change could lock the administrator out. With System access Full
    /// and Approval mode Auto, a command the model wrote runs without asking unless it has a [`Prepared::risk`].
    pub fn always_asks(&self) -> bool {
        matches!(self.work, Work::Script { .. }) || self.floor().is_some()
    }

    /// For a command the model wrote, why it asks even in Auto, if it looks like it could lose data, lock the
    /// administrator out or stop the system: found by patterns on its text, no guarantee.
    pub fn risk(&self) -> Option<&'static str> {
        match &self.work {
            Work::Script { shells, .. } => script::risk(&shells.first()?.display),
            _ => None,
        }
    }

    /// Whether it runs without approval in every Approval mode: it neither looks at nor changes the system.
    pub fn never_asks(&self) -> bool {
        matches!(self.work, Work::Secret(_))
    }

    /// Whether it changes the system through a typed operation.
    pub fn changes(&self) -> bool {
        matches!(
            self.work,
            Work::ChangeFile(_)
                | Work::Service(_)
                | Work::Packages(_)
                | Work::Database(_)
                | Work::Features(_)
                | Work::Site(_)
        )
    }

    /// Why it waits for approval in every Approval mode, for a change where a mistake could lock the administrator
    /// out, keep the system from starting or lose data.
    pub fn floor(&self) -> Option<&str> {
        match &self.work {
            Work::ChangeFile(change) => change.planned.as_ref()?.floor.as_deref(),
            Work::Service(change) => change.planned.as_ref()?.floor.as_deref(),
            Work::Packages(change) => change.planned.as_ref()?.floor.as_deref(),
            _ => None,
        }
    }

    /// The change of a file it makes, once planned.
    pub fn planned(&self) -> Option<&Planned> {
        match &self.work {
            Work::ChangeFile(change) => change.planned.as_ref(),
            _ => None,
        }
    }

    /// The plan it proposes.
    pub fn proposal(&self) -> Option<&Proposal> {
        match &self.work {
            Work::Plan(proposal) => Some(proposal),
            _ => None,
        }
    }

    /// What its card shows, for a change once planned.
    pub fn card(&self) -> Option<Card> {
        match &self.work {
            Work::ChangeFile(change) => {
                let planned = change.planned.as_ref()?;
                Some(Card {
                    diff: planned.diff.clone(),
                    notes: planned.notes(),
                    floor: planned.floor.clone(),
                    path: Some(planned.real.clone()),
                })
            }
            Work::Service(change) => {
                let planned = change.planned.as_ref()?;
                Some(Card {
                    diff: format!("Now: {}", planned.before.shown()),
                    notes: vec![
                        "Kelpy waits up to 30 seconds for the state asked for; if it is not reached, the service's \
                         latest log lines are shown and its state before is put back where it can be."
                            .to_owned(),
                    ],
                    floor: planned.floor.clone(),
                    path: None,
                })
            }
            Work::Packages(change) => {
                let planned = change.planned.as_ref()?;
                Some(Card {
                    diff: planned.preview.clone(),
                    notes: change.notes(planned),
                    floor: planned.floor.clone(),
                    path: None,
                })
            }
            Work::Database(change) => {
                let planned = change.planned.as_ref()?;
                Some(Card {
                    diff: String::new(),
                    notes: change.notes(planned),
                    floor: None,
                    path: None,
                })
            }
            Work::Features(change) => Some(Card {
                diff: change.planned.as_ref()?.preview.clone(),
                notes: vec![
                    "Kelpy installs the features marked + with their management tools and checks each; a restart \
                     Windows asks for is reported, never made."
                        .to_owned(),
                ],
                floor: None,
                path: None,
            }),
            Work::Site(change) => Some(Card {
                diff: change.planned.as_ref()?.preview.clone(),
                notes: vec![
                    "Kelpy then checks that the site is started and that IIS answers on its binding; if a step \
                     fails, it removes what it created."
                        .to_owned(),
                ],
                floor: None,
                path: None,
            }),
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
            Work::Service(change) => Some(match &change.planned {
                Some(planned) => planned.command.display.clone(),
                None => service::command(Os::current(), &change.name, change.action).display,
            }),
            Work::Packages(change) => change.planned.as_ref().map(|planned| {
                if planned.refresh {
                    format!("apt-get update\n{}", planned.command.display)
                } else {
                    planned.command.display.clone()
                }
            }),
            Work::Database(change) => Some(change.statements(change.planned.as_ref())),
            Work::Features(change) => change
                .planned
                .as_ref()
                .map(|planned| planned.command.display.clone()),
            Work::Site(change) => change
                .planned
                .as_ref()
                .map(|planned| planned.command.display.clone()),
            Work::ReadFile(_)
            | Work::ListDirectory(_)
            | Work::ChangeFile(_)
            | Work::Secret(_)
            | Work::ReadMemory(_)
            | Work::Remember { .. }
            | Work::Plan(_) => None,
        }
    }
}

/// The JSON `arguments` of a call without its reason, keys in order: two calls with the same arguments give the same
/// text. `None` when they are not a JSON object.
pub fn canonical(arguments: &str) -> Option<String> {
    let mut arguments: Map<String, Value> = if arguments.trim().is_empty() {
        Map::new()
    } else {
        serde_json::from_str(arguments).ok()?
    };
    arguments.remove("reason");
    // `Map` keeps its keys in order.
    Some(Value::Object(arguments).to_string())
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
        tools.push(tool(
            "read_memory",
            "A page of Kelpy's memory of this server, listed in MEMORY.md. server.md is Kelpy's own page: what it \
             changed on this server and expects to find, and what was changed outside it."
                .into(),
            &[("page", "The page's name, such as server or nginx.")],
            &[],
        ));
        tools.push(tool(
            "remember",
            "Adds a lasting fact about this server to a page of Kelpy's memory, dated, and lists the page in \
             MEMORY.md: what is installed where and why, decisions, things to know next time. One fact a call, in \
             the administrator's language; never a secret. Not for what Kelpy changed: it records that itself."
                .into(),
            &[
                (
                    "page",
                    "The topic's page: lower-case letters, digits and -, such as nginx or backups.",
                ),
                ("note", "The fact, in one or two sentences."),
            ],
            &[],
        ));
        let checked = "Kelpy shows the change as a diff, backs the file up first, and checks it where it can (such as \
                       nginx -t): if the change breaks the check, the previous version is put back at once. Files over \
                       20 MiB cannot be changed. Where a password or key goes, write {{secret:name}} after creating \
                       the secret with generate_secret: Kelpy puts its value in, and you never see it.";
        tools.push(Tool {
            kind: ToolKind::Changes,
            ..tool(
                "edit_file",
                format!(
                    "Changes a text file by replacing one exact piece of it, which must occur exactly once: copy it \
                     from read_file, with enough lines around it to be unique. Files that may hold secrets (such as \
                     wp-config.php, .env) cannot be edited, only written whole with write_file. {checked}"
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
            kind: ToolKind::Changes,
            ..tool(
                "write_file",
                format!(
                    "Creates a text file, or replaces all its content; its folder must exist. To change part of an \
                     existing file, use edit_file. An existing file keeps its owner and permissions. On Linux and \
                     macOS a new file that may hold secrets, or holds one, is readable by Kelpy's account only (mode \
                     600): give the service that reads it access with run_command, such as chown or chmod. {checked}"
                ),
                &[
                    ("path", "The file's absolute path."),
                    ("content", "The whole content of the file."),
                ],
                &[],
            )
        });
        let mut manage_service = Tool {
            kind: ToolKind::Changes,
            ..tool(
                "manage_service",
                "Starts, stops, restarts, reloads, enables (starts with the system) or disables a service. Kelpy reads \
                 its state first, does nothing if it is already as asked, waits up to 30 seconds for the new state, \
                 and puts the state before back if it is not reached, showing the service's latest log lines. \
                 Stopping, restarting or disabling SSH, remote management or the network always asks."
                    .into(),
                &[
                    ("name", "The service's name, as in list_services."),
                    ("action", "start, stop, restart, reload, enable or disable."),
                ],
                &[],
            )
        };
        manage_service.parameters["properties"]["action"]["enum"] =
            json!(["start", "stop", "restart", "reload", "enable", "disable"]);
        tools.push(manage_service);
        for (name, description) in [
            (
                "install_packages",
                "Installs packages with apt, with what they need. Kelpy shows what apt would install, refreshes old \
                 package lists first, checks each package afterwards, and removes what the installation added if it \
                 fails. Packages already installed are left alone.",
            ),
            (
                "remove_packages",
                "Removes packages with apt. Kelpy shows what apt would remove with them; it always asks, and cannot \
                 undo it.",
            ),
        ] {
            let mut packages = Tool {
                kind: ToolKind::Changes,
                ..tool(name, description.into(), &[], &[])
            };
            packages.parameters["properties"]["packages"] = json!({
                "type": "array",
                "items": { "type": "string" },
                "minItems": 1,
                "maxItems": 50,
                "description": "The packages' names, such as nginx or php8.3-fpm.",
            });
            packages.parameters["required"] = json!(["reason", "packages"]);
            tools.push(packages);
        }
        if self.os == Os::Windows {
            let mut features = Tool {
                kind: ToolKind::Changes,
                ..tool(
                    "install_windows_features",
                    "Installs Windows features on Windows Server with Install-WindowsFeature, with their management \
                     tools, such as Web-Server for IIS. Kelpy shows which are missing, installs them and checks each; a \
                     restart Windows asks for is reported, never made. Features already installed are left alone."
                        .into(),
                    &[],
                    &[],
                )
            };
            features.parameters["properties"]["features"] = json!({
                "type": "array",
                "items": { "type": "string" },
                "minItems": 1,
                "maxItems": 20,
                "description": "The features' names, as Get-WindowsFeature lists them, such as Web-Server.",
            });
            features.parameters["required"] = json!(["reason", "features"]);
            tools.push(features);
            let mut site = Tool {
                kind: ToolKind::Changes,
                ..tool(
                    "create_iis_site",
                    "Creates an IIS site: its folder if missing, an application pool of its name (no .NET CLR, \
                     integrated pipeline), the site with an HTTP binding, started. Kelpy checks first that IIS is \
                     installed and that no site, pool or binding takes the name, port and host name, and afterwards \
                     that IIS answers on the binding; if a step fails, it removes what it created. A site already \
                     there as asked is left alone. For HTTPS, certificates or other settings, write commands."
                        .into(),
                    &[
                        ("name", "The site's name, such as Contoso: also the application pool's."),
                        ("path", "The site's folder, absolute, such as C:\\inetpub\\contoso."),
                    ],
                    &[("host", "The host name it answers to, such as www.contoso.com; none for every name.")],
                )
            };
            site.parameters["properties"]["port"] = json!({
                "type": "integer",
                "minimum": 1,
                "maximum": 65535,
                "description": "The HTTP port; 80 if not given.",
            });
            tools.push(site);
        }
        tools.push(Tool {
            kind: ToolKind::Changes,
            ..tool(
                "generate_secret",
                "Generates a secret, such as a password or a key, that only Kelpy knows: 32 letters and digits. \
                 Write {{secret:name}} where it goes, in write_file, edit_file or create_database, and Kelpy puts the \
                 value in; it is hidden from what you read. Secrets last until Kelpy stops, unless the administrator keeps \
                 them in the vault: write them where they are needed in the same conversation. The administrator's \
                 own secrets, listed in your instructions, are used the same way and need no generating. Nothing \
                 changes on the system, so it never asks."
                    .into(),
                &[(
                    "name",
                    "The secret's name: letters, digits and _, such as wordpress_db.",
                )],
                &[],
            )
        });
        tools.push(Tool {
            kind: ToolKind::Changes,
            ..tool(
                "create_database",
                "Creates a MariaDB or MySQL database and a user for it on localhost, with all rights on it, as the \
                 account that started Kelpy. The password must be a secret: create it with generate_secret first, and \
                 write it in the application's configuration with the same placeholder. Kelpy signs in as the user to \
                 check it, and drops what it created if that fails. Existing ones are left alone."
                    .into(),
                &[
                    ("name", "The database's name: letters, digits and _."),
                    ("user", "The user's name: letters, digits and _."),
                    ("password", "The secret's placeholder, such as {{secret:wordpress_db}}."),
                ],
                &[],
            )
        });
        let mut propose_plan = Tool {
            kind: ToolKind::Plans,
            ..tool(
                "propose_plan",
                "Proposes the changes of a task as one plan, for the administrator to approve once, before making \
                 them. Look first, then list every change in order, each as the exact call you will make, with the \
                 same arguments: the calls that match a step then run without asking again, in order or not. Calls \
                 that are not in the plan ask on their own. A new plan replaces the previous one."
                    .into(),
                &[("goal", "What the plan achieves, in one sentence in the user's language.")],
                &[],
            )
        };
        propose_plan.parameters["properties"]["steps"] = json!({
            "type": "array",
            "minItems": 1,
            "maxItems": MAX_STEPS,
            "description": "The changes, in order.",
            "items": {
                "type": "object",
                "properties": {
                    "tool": {
                        "type": "string",
                        "description": "A tool that changes the system, or run_command.",
                    },
                    "arguments": {
                        "type": "object",
                        "description": "Its arguments as you will give them, without reason.",
                    },
                },
                "required": ["tool", "arguments"],
                "additionalProperties": false,
            },
        });
        propose_plan.parameters["required"] = json!(["reason", "goal", "steps"]);
        tools.push(propose_plan);
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
                if secret::has_placeholder(command) {
                    return Err(invalid(
                        "secrets can be used only in write_file, edit_file and create_database, never in a command",
                    ));
                }
                let directory = absolute(&required("directory")?)?;
                (
                    format!("Run a command in {}", directory.display()),
                    script::script(os, command, directory),
                )
            }
            "manage_service" => {
                let name = service::name(&argument_like(&required("name")?, "name")?)?;
                let action = ServiceAction::parse(&required("action")?).ok_or_else(|| {
                    invalid("`action` must be start, stop, restart, reload, enable or disable")
                })?;
                let change = ServiceChange {
                    name,
                    action,
                    planned: None,
                };
                (change.summary(), Work::Service(Box::new(change)))
            }
            "install_packages" | "remove_packages" => {
                let packages = match arguments.get("packages") {
                    Some(Value::Array(packages)) if (1..=50).contains(&packages.len()) => packages
                        .iter()
                        .map(|package| {
                            package::name(package.as_str().ok_or_else(|| {
                                invalid("`packages` must hold the packages' names, as strings")
                            })?)
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                    _ => return Err(invalid("`packages` must list from 1 to 50 packages")),
                };
                let change = PackageChange {
                    packages,
                    install: name == "install_packages",
                    planned: None,
                };
                (change.summary(), Work::Packages(Box::new(change)))
            }
            "read_memory" => {
                let page = memory::page(&required("page")?)?;
                (format!("Read memory/{page}.md"), Work::ReadMemory(page))
            }
            "remember" => {
                let page = memory::page(&required("page")?)?;
                let note = memory::note(&required("note")?)?;
                (
                    format!("Remember on memory/{page}.md: {note}"),
                    Work::Remember { page, note },
                )
            }
            "generate_secret" => {
                let name = secret::name(&required("name")?)?;
                (format!("Generate the secret {name}"), Work::Secret(name))
            }
            "create_database" => {
                let change = DatabaseChange {
                    name: database::identifier(&required("name")?, "name")?,
                    user: database::identifier(&required("user")?, "user")?,
                    password: database::password(&required("password")?)?,
                    planned: None,
                };
                (change.summary(), Work::Database(Box::new(change)))
            }
            "install_windows_features" => {
                let features = match arguments.get("features") {
                    Some(Value::Array(features)) if (1..=20).contains(&features.len()) => features
                        .iter()
                        .map(|feature| {
                            windows::feature(feature.as_str().ok_or_else(|| {
                                invalid("`features` must hold the features' names, as strings")
                            })?)
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                    _ => return Err(invalid("`features` must list from 1 to 20 features")),
                };
                let change = FeatureChange {
                    features,
                    planned: None,
                };
                (change.summary(), Work::Features(Box::new(change)))
            }
            "create_iis_site" => {
                let port = match arguments.get("port") {
                    None => 80,
                    Some(port) => port
                        .as_u64()
                        .and_then(|port| u16::try_from(port).ok())
                        .filter(|&port| port != 0)
                        .ok_or_else(|| invalid("`port` must be a port number, from 1 to 65535"))?,
                };
                let host = match arguments.get("host").and_then(Value::as_str) {
                    Some(host) => windows::host_name(host)?,
                    None => String::new(),
                };
                let change = SiteChange {
                    name: windows::site_name(&required("name")?)?,
                    path: absolute(&required("path")?)?,
                    port,
                    host,
                    planned: None,
                };
                (change.summary(), Work::Site(Box::new(change)))
            }
            "propose_plan" => {
                let goal = required("goal")?;
                let steps = match arguments.get("steps") {
                    Some(Value::Array(steps)) if (1..=MAX_STEPS).contains(&steps.len()) => steps,
                    _ => {
                        return Err(invalid(format!(
                            "`steps` must list from 1 to {MAX_STEPS} changes"
                        )));
                    }
                };
                let steps = steps
                    .iter()
                    .enumerate()
                    .map(|(index, step)| {
                        self.step(step)
                            .map_err(|problem| invalid(format!("step {}: {problem}", index + 1)))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                (
                    format!("Plan: {goal}"),
                    Work::Plan(Box::new(Proposal { goal, steps })),
                )
            }
            other => return Err(invalid(format!("there is no tool named `{other}`"))),
        };
        Ok(Prepared {
            reason,
            summary,
            work,
            already: None,
        })
    }

    /// A step of a proposed plan: a change, checked like its call will be.
    fn step(&self, step: &Value) -> Result<Step, ToolError> {
        let tool = step
            .get("tool")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("`tool` is required, as a string"))?;
        let arguments = match step.get("arguments") {
            Some(Value::Object(arguments)) => Value::Object(arguments.clone()).to_string(),
            _ => return Err(invalid("`arguments` must be an object")),
        };
        let prepared = self.prepare(tool, &arguments)?;
        if !prepared.changes() && !matches!(prepared.work, Work::Script { .. }) {
            return Err(invalid(format!(
                "{tool} does not change the system: a plan lists only changes, and looking needs no plan"
            )));
        }
        let detail = match &prepared.work {
            Work::ChangeFile(change) => Some(change::preview(&change.edit)),
            Work::Service(change) => {
                Some(service::command(self.os, &change.name, change.action).display)
            }
            _ => prepared.command_line(),
        };
        let floor = match &prepared.work {
            Work::ChangeFile(change) => change::floor(self.os, &change.path),
            Work::Service(change) => service::floor(&change.name, change.action),
            Work::Packages(change) if !change.install => Some(
                "It removes packages: the programs stop working and their data may go with them."
                    .to_owned(),
            ),
            _ => None,
        };
        Ok(Step {
            tool: tool.to_owned(),
            input: canonical(&arguments).unwrap_or_default(),
            summary: prepared.summary,
            detail,
            floor,
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
        kind: ToolKind::Looks,
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
            // Windows has two more: its features and IIS sites.
            assert_eq!(tools.len(), if os == Os::Windows { 23 } else { 21 });
            for tool in tools {
                assert_eq!(tool.parameters["required"][0], "reason", "{}", tool.name);
                assert_eq!(tool.parameters["additionalProperties"], false);
                assert!(!tool.description.is_empty());
                // No run of spaces from a line cut badly in the source.
                assert!(!tool.description.contains("  "), "{}", tool.name);
                for required in tool.parameters["required"].as_array().unwrap() {
                    let field = required.as_str().unwrap();
                    assert!(
                        tool.parameters["properties"].get(field).is_some(),
                        "{} {field}",
                        tool.name
                    );
                }
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
            let ran = executor
                .run(&prepared, &Secrets::new(), &Output::new())
                .await
                .unwrap();
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

        assert!(
            linux
                .prepare(
                    "run_command",
                    &serde_json::json!({ "command": "echo {{secret:x}}", "directory": folder })
                        .to_string()
                )
                .unwrap_err()
                .to_string()
                .contains("never in a command")
        );

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

    #[test]
    fn a_plan_lists_changes_checked_like_their_calls() {
        let linux = Tools::new(Os::Linux);
        // A Linux path is not absolute on Windows, where the tests may run.
        let (config, floored) = if cfg!(windows) {
            (r"C:\srv\sshd_config", false)
        } else {
            ("/etc/ssh/sshd_config", true)
        };
        let plan = serde_json::json!({
            "reason": "To serve the blog",
            "goal": "Serve the blog with nginx",
            "steps": [
                { "tool": "install_packages", "arguments": { "packages": ["nginx"] } },
                { "tool": "edit_file", "arguments": {
                    "path": config, "old": "Port 22", "new": "Port 2222" } },
                { "tool": "manage_service", "arguments": { "name": "nginx", "action": "restart" } },
                { "tool": "remove_packages", "arguments": { "packages": ["apache2"] } },
            ],
        });
        let prepared = linux.prepare("propose_plan", &plan.to_string()).unwrap();
        assert_eq!(prepared.summary, "Plan: Serve the blog with nginx");
        let proposal = prepared.proposal().unwrap();
        let steps = &proposal.steps;
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[0].summary, "Install nginx");
        assert_eq!(steps[0].input, r#"{"packages":["nginx"]}"#);
        assert_eq!(steps[1].detail.as_deref(), Some("-Port 22\n+Port 2222"));
        assert_eq!(steps[1].floor.is_some(), floored);
        assert_eq!(steps[2].detail.as_deref(), Some("systemctl restart nginx"));
        assert!(steps[3].floor.is_some());

        // A call carries out a step only with the same arguments, whatever their order and reason.
        assert_eq!(
            canonical(r#"{"reason":"now","action":"restart","name":"nginx"}"#).unwrap(),
            canonical(r#"{"name":"nginx","action":"restart"}"#).unwrap()
        );

        for (steps, problem) in [
            (serde_json::json!([]), "from 1 to"),
            (
                serde_json::json!([{ "tool": "read_file", "arguments": { "path": config } }]),
                "step 1: read_file does not change the system",
            ),
            (
                serde_json::json!([{ "tool": "manage_service", "arguments": { "name": "nginx", "action": "explode" } }]),
                "step 1: `action` must be",
            ),
            (
                serde_json::json!([{ "tool": "propose_plan", "arguments": {} }]),
                "step 1: `goal` is required",
            ),
        ] {
            let plan = serde_json::json!({ "goal": "x", "steps": steps });
            let error = linux
                .prepare("propose_plan", &plan.to_string())
                .unwrap_err()
                .to_string();
            assert!(error.contains(problem), "{error}");
        }
    }

    #[test]
    fn typed_changes_check_their_arguments() {
        let linux = Tools::new(Os::Linux);
        let service = linux
            .prepare("manage_service", r#"{"name":"nginx","action":"reload"}"#)
            .unwrap();
        assert!(service.changes());
        assert_eq!(service.summary, "Reload the service nginx");
        let packages = linux
            .prepare("install_packages", r#"{"packages":["nginx","php8.3-fpm"]}"#)
            .unwrap();
        assert_eq!(packages.summary, "Install nginx, php8.3-fpm");
        let database = linux
            .prepare(
                "create_database",
                r#"{"name":"wordpress","user":"wp","password":"{{secret:wp_db}}"}"#,
            )
            .unwrap();
        assert!(
            database
                .command_line()
                .unwrap()
                .contains("IDENTIFIED BY '{{secret:wp_db}}'")
        );
        let secret = linux
            .prepare("generate_secret", r#"{"name":"wp_db"}"#)
            .unwrap();
        assert!(secret.never_asks() && !secret.changes());
        for (name, arguments) in [
            ("manage_service", r#"{"name":"-x","action":"start"}"#),
            ("install_packages", r#"{"packages":[]}"#),
            ("install_packages", r#"{"packages":["nginx; reboot"]}"#),
            ("remove_packages", r#"{"packages":"nginx"}"#),
            (
                "create_database",
                r#"{"name":"wp","user":"wp","password":"hunter2"}"#,
            ),
            ("generate_secret", r#"{"name":"a b"}"#),
        ] {
            assert!(
                linux.prepare(name, arguments).is_err(),
                "{name} {arguments}"
            );
        }
    }
}
