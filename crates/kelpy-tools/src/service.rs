//! Starting, stopping, restarting, reloading, enabling and disabling a service: systemd on Linux, the service
//! manager on Windows.
//!
//! Before approval Kelpy reads the service's state ([`plan`]): a service already as asked is left alone. Once approved
//! ([`apply`]) it changes the state and waits for the state expected; when the service does not reach it, its latest
//! log lines are shown and the state before is put back where Kelpy can: started again after a failed restart,
//! stopped again after a failed start.

use std::time::Duration;

use crate::executor::{RUN_TIME, captured};
use crate::inspect::{powershell, quoted, run};
use crate::{Command, Os, Output, Recorded, RecordedKind, ToolError, invalid};

/// How long a service may take to reach the state asked for.
const SETTLE_TIME: Duration = Duration::from_secs(30);
const POLL_EVERY: Duration = Duration::from_millis(500);

/// What to do with a service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceAction {
    Start,
    Stop,
    Restart,
    Reload,
    Enable,
    Disable,
}

impl ServiceAction {
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "start" => Self::Start,
            "stop" => Self::Stop,
            "restart" => Self::Restart,
            "reload" => Self::Reload,
            "enable" => Self::Enable,
            "disable" => Self::Disable,
            _ => return None,
        })
    }

    pub fn verb(self) -> &'static str {
        match self {
            Self::Start => "Start",
            Self::Stop => "Stop",
            Self::Restart => "Restart",
            Self::Reload => "Reload",
            Self::Enable => "Enable",
            Self::Disable => "Disable",
        }
    }

    fn doing(self) -> &'static str {
        match self {
            Self::Start => "starting",
            Self::Stop => "stopping",
            Self::Restart => "restarting",
            Self::Reload => "reloading",
            Self::Enable => "enabling",
            Self::Disable => "disabling",
        }
    }

    fn word(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
            Self::Reload => "reload",
            Self::Enable => "enable",
            Self::Disable => "disable",
        }
    }
}

/// A change of a service: as asked, then with its state read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceChange {
    pub name: String,
    pub action: ServiceAction,
    pub planned: Option<ServicePlanned>,
}

/// A service's state before the change, and what the change runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServicePlanned {
    pub before: ServiceState,
    /// The command that makes the change.
    pub command: Command,
    /// Why it waits for approval in every Approval mode, if it does.
    pub floor: Option<String>,
}

/// Whether a service runs, and whether it starts with the system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceState {
    /// The unit's names, aliases included, as the system resolved the name asked for.
    pub names: Vec<String>,
    /// `active`, `inactive`, `failed`…; on Windows `Running`, `Stopped`…
    pub running: String,
    /// `enabled`, `disabled`, `static`…; on Windows `Automatic`, `Manual`, `Disabled`.
    pub starts: String,
}

impl ServiceState {
    pub fn shown(&self) -> String {
        format!("{}, {}", self.running, self.starts)
    }

    fn is_running(&self) -> bool {
        matches!(self.running.as_str(), "active" | "Running")
    }

    fn is_stopped(&self) -> bool {
        matches!(self.running.as_str(), "inactive" | "failed" | "Stopped")
    }

    fn starts_with_system(&self) -> bool {
        matches!(self.starts.as_str(), "enabled" | "Automatic")
    }

    fn never_starts(&self) -> bool {
        matches!(self.starts.as_str(), "disabled" | "Disabled")
    }
}

impl ServiceChange {
    /// The summary of the change, such as "Restart nginx".
    pub fn summary(&self) -> String {
        format!("{} the service {}", self.action.verb(), self.name)
    }
}

/// The services through which the administrator reaches the server: stopping, restarting or disabling them could
/// lock them out.
pub(crate) const ACCESS: &[&str] = &[
    "ssh",
    "sshd",
    "networking",
    "network",
    "networkmanager",
    "systemd-networkd",
    "systemd-resolved",
    "ufw",
    "firewalld",
    "nftables",
    "iptables",
    "winrm",
    "termservice",
    "mpssvc",
    "dnscache",
    "netman",
];

/// A service's name as the model gives it: one name, no pattern, so that it means one service.
pub fn name(value: &str) -> Result<String, ToolError> {
    if value.chars().any(|char| "*?[]\\".contains(char)) {
        return Err(invalid(
            "`name` must be one service's name, without * ? [ ] or \\",
        ));
    }
    Ok(value.to_owned())
}

/// Why the change waits for approval in every Approval mode, if it does.
pub fn floor(name: &str, action: ServiceAction) -> Option<String> {
    let base = name
        .trim_end_matches(".service")
        .trim_end_matches(".socket")
        .to_lowercase();
    if !ACCESS.contains(&base.as_str()) {
        return None;
    }
    let doing = match action {
        ServiceAction::Stop => "stops",
        ServiceAction::Restart => "restarts",
        ServiceAction::Disable => "disables",
        _ => return None,
    };
    Some(format!(
        "It {doing} {name}, which keeps your way into this server open: a mistake could lock you out, so it always \
         asks."
    ))
}

/// Reads the service's state and plans the change. `Ok(Err(text))` when the service is already as asked.
pub async fn plan(
    change: &ServiceChange,
    os: Os,
) -> Result<Result<ServicePlanned, String>, ToolError> {
    let before = state(os, &change.name).await?;
    let (name, action) = (&change.name, change.action);
    let already = match action {
        ServiceAction::Start if before.is_running() => Some("runs already"),
        ServiceAction::Stop if before.is_stopped() => Some("is stopped already"),
        ServiceAction::Enable if before.starts_with_system() => {
            Some("starts with the system already")
        }
        ServiceAction::Disable if before.never_starts() => Some("is disabled already"),
        _ => None,
    };
    if let Some(already) = already {
        return Ok(Err(format!(
            "The service {name} {already} ({}): nothing to change.",
            before.shown()
        )));
    }
    if action == ServiceAction::Reload && !before.is_running() {
        return Err(invalid(format!(
            "the service {name} is not running ({}), so it cannot reload: start it instead",
            before.shown()
        )));
    }
    // Whatever name was asked for, the unit's own names decide: an alias of sshd is sshd.
    let floor = std::iter::once(name.as_str())
        .chain(before.names.iter().map(String::as_str))
        .find_map(|known| floor(known, action));
    Ok(Ok(ServicePlanned {
        command: command(os, name, action),
        floor,
        before,
    }))
}

/// Makes the change and waits for the state expected; puts back the state before when it is not reached.
pub async fn apply(
    change: &ServiceChange,
    planned: &ServicePlanned,
    os: Os,
    output: &Output,
) -> Result<Recorded, ToolError> {
    let (name, action) = (&change.name, change.action);
    output.set_phase(&format!("{} the service {name}", action.doing()));
    output.line(&format!("Before: {}.", planned.before.shown()));
    let (code, said) = captured(&planned.command, None, SETTLE_TIME).await?;
    output.text(&said);
    let reached = if code == 0 {
        settle(os, name, |state| expected(action, state)).await?
    } else {
        state(os, name).await?
    };
    if code == 0 && expected(action, &reached) {
        output.line(&format!("After: {}.", reached.shown()));
        return Ok(Recorded {
            kind: RecordedKind::Service,
            subject: name.clone(),
            state: reached.shown(),
            content: None,
            secret: false,
        });
    }
    output.line(&format!(
        "After: {}, not what {} should give.",
        reached.shown(),
        action.word()
    ));
    if os == Os::Linux {
        let journal = run("journalctl", &["-u", name, "-n", "20", "--no-pager"]);
        if let Ok((_, lines)) = captured(&journal, None, RUN_TIME).await {
            output.line("Its latest log lines:");
            output.text(&lines);
        }
    }
    output.set_phase(&format!("putting the service {name} back"));
    let back = put_back(os, name, action, &planned.before).await;
    output.line(&back);
    Err(ToolError::Unverified(format!(
        "the service {name} did not {} as expected: {back}",
        action.word()
    )))
}

/// Puts back the state before a change that failed, where Kelpy can. Says what it did.
async fn put_back(os: Os, name: &str, action: ServiceAction, before: &ServiceState) -> String {
    let undo = match action {
        ServiceAction::Start if before.is_stopped() => Some(ServiceAction::Stop),
        ServiceAction::Restart | ServiceAction::Reload if before.is_running() => {
            Some(ServiceAction::Start)
        }
        ServiceAction::Enable => Some(ServiceAction::Disable),
        ServiceAction::Disable => Some(ServiceAction::Enable),
        _ => None,
    };
    let Some(undo) = undo else {
        return "Nothing was put back: it is as the change left it.".to_owned();
    };
    let _ = captured(&command(os, name, undo), None, SETTLE_TIME).await;
    match settle(os, name, |state| expected(undo, state)).await {
        Ok(state) if expected(undo, &state) => format!("Put back: it is {} again.", state.shown()),
        Ok(state) => format!("It could not be put back: it is {}.", state.shown()),
        Err(error) => format!("It could not be put back: {error}."),
    }
}

/// Whether `state` is what `action` should give.
fn expected(action: ServiceAction, state: &ServiceState) -> bool {
    match action {
        ServiceAction::Start | ServiceAction::Restart | ServiceAction::Reload => state.is_running(),
        ServiceAction::Stop => state.is_stopped(),
        ServiceAction::Enable => state.starts_with_system(),
        ServiceAction::Disable => state.never_starts(),
    }
}

/// The service's state once `done` holds, or once it stops changing towards it, within [`SETTLE_TIME`].
async fn settle(
    os: Os,
    name: &str,
    done: impl Fn(&ServiceState) -> bool,
) -> Result<ServiceState, ToolError> {
    let deadline = tokio::time::Instant::now() + SETTLE_TIME;
    loop {
        let now = state(os, name).await?;
        let moving = matches!(
            now.running.as_str(),
            "activating" | "deactivating" | "reloading" | "StartPending" | "StopPending"
        );
        if done(&now)
            || (!moving && now.running == "failed")
            || tokio::time::Instant::now() >= deadline
        {
            return Ok(now);
        }
        tokio::time::sleep(POLL_EVERY).await;
    }
}

/// The service's state, read now. Fails when there is no such service.
pub async fn state(os: Os, name: &str) -> Result<ServiceState, ToolError> {
    match os {
        Os::Linux => {
            let show = run(
                "systemctl",
                &[
                    "show",
                    name,
                    "--property=LoadState,ActiveState,UnitFileState,Names",
                ],
            );
            let (_, text) = captured(&show, None, RUN_TIME).await?;
            let value = |key: &str| {
                text.lines()
                    .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
                    .unwrap_or_default()
                    .to_owned()
            };
            if value("LoadState") != "loaded" {
                return Err(invalid(format!("there is no service named {name}")));
            }
            Ok(ServiceState {
                names: value("Names")
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect(),
                running: value("ActiveState"),
                starts: match value("UnitFileState") {
                    starts if starts.is_empty() => "static".to_owned(),
                    starts => starts,
                },
            })
        }
        Os::Windows => {
            let script = format!(
                "$s = Get-Service -Name {} -ErrorAction Stop; \"$($s.Status)`t$($s.StartType)\"",
                quoted(name)
            );
            let (code, text) = captured(&powershell(&script), None, RUN_TIME).await?;
            let line = text.lines().next().unwrap_or_default();
            match (code, line.split_once('\t')) {
                (0, Some((running, starts))) => Ok(ServiceState {
                    names: vec![name.to_owned()],
                    running: running.trim().to_owned(),
                    starts: starts.trim().to_owned(),
                }),
                _ => Err(invalid(format!("there is no service named {name}"))),
            }
        }
        Os::MacOs => Err(invalid(
            "Kelpy does not manage services on macOS: use run_command",
        )),
    }
}

/// The command that does `action` to the service `name`.
pub(crate) fn command(os: Os, name: &str, action: ServiceAction) -> Command {
    match os {
        Os::Windows => {
            let name = quoted(name);
            powershell(&match action {
                ServiceAction::Start => format!("Start-Service -Name {name} -ErrorAction Stop"),
                ServiceAction::Stop => {
                    format!("Stop-Service -Name {name} -Force -ErrorAction Stop")
                }
                ServiceAction::Restart | ServiceAction::Reload => {
                    format!("Restart-Service -Name {name} -Force -ErrorAction Stop")
                }
                ServiceAction::Enable => {
                    format!("Set-Service -Name {name} -StartupType Automatic -ErrorAction Stop")
                }
                ServiceAction::Disable => {
                    format!("Set-Service -Name {name} -StartupType Disabled -ErrorAction Stop")
                }
            })
        }
        _ => run("systemctl", &[action.word(), name]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopping_the_way_in_always_asks() {
        assert!(floor("sshd", ServiceAction::Stop).is_some());
        assert!(floor("ssh.service", ServiceAction::Restart).is_some());
        assert!(floor("WinRM", ServiceAction::Disable).is_some());
        assert!(floor("ssh.socket", ServiceAction::Stop).is_some());
        assert!(name("ssh*").is_err() && name("ssh?").is_err() && name("nginx").is_ok());
        assert_eq!(floor("sshd", ServiceAction::Start), None);
        assert_eq!(floor("nginx", ServiceAction::Stop), None);
    }

    #[test]
    fn each_action_has_its_command_and_expected_state() {
        assert_eq!(
            command(Os::Linux, "nginx", ServiceAction::Reload).display,
            "systemctl reload nginx"
        );
        assert!(
            command(Os::Windows, "W3SVC", ServiceAction::Enable)
                .display
                .contains("-StartupType Automatic")
        );
        let state = |running: &str, starts: &str| ServiceState {
            names: Vec::new(),
            running: running.into(),
            starts: starts.into(),
        };
        assert!(expected(ServiceAction::Start, &state("active", "disabled")));
        assert!(expected(ServiceAction::Stop, &state("failed", "enabled")));
        assert!(!expected(ServiceAction::Enable, &state("active", "static")));
        assert!(expected(
            ServiceAction::Disable,
            &state("Stopped", "Disabled")
        ));
    }

    /// Reads a service of this computer: `cargo test -p kelpy-tools -- --ignored`.
    #[tokio::test]
    #[ignore = "looks at the computer running the tests"]
    async fn a_service_is_read_and_one_already_as_asked_is_left_alone() {
        let os = Os::current();
        let name = if os == Os::Windows {
            "EventLog"
        } else {
            "cron"
        };
        let running = ServiceChange {
            name: name.into(),
            action: ServiceAction::Start,
            planned: None,
        };
        let planned = plan(&running, os).await.unwrap();
        assert!(planned.unwrap_err().contains("runs already"));
        let missing = ServiceChange {
            name: "kelpy-no-such-service".into(),
            ..running
        };
        assert!(plan(&missing, os).await.is_err());
    }
}
