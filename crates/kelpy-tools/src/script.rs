//! Commands the model writes, run as written in the system's shell. Kelpy cannot tell what such a command does, so
//! it is shown verbatim and approved before it runs, in every Approval mode.

use crate::executor::MAX_OUTPUT;
use crate::inspect::powershell;
use crate::{Command, Os, Work};

/// The most a command may hold. Encoded for PowerShell it stays within Windows' limit for a command line.
pub const MAX_SCRIPT: usize = 8 << 10;

/// `text` run by the shell of `os`: Windows PowerShell; else bash, or sh where bash is missing.
pub fn script(os: Os, text: &str) -> Work {
    Work::Script(match os {
        Os::Windows => vec![powershell(text)],
        Os::Linux | Os::MacOs => ["bash", "sh"]
            .into_iter()
            .map(|shell| Command {
                program: shell.to_owned(),
                args: vec!["-c".to_owned(), text.to_owned()],
                env: Vec::new(),
                display: text.to_owned(),
            })
            .collect(),
    })
}

/// What the model is told of `run_command` on `os`.
pub fn description(os: Os) -> String {
    let (shell, unattended) = match os {
        Os::Windows => (
            "Windows PowerShell 5.1",
            "-Confirm:$false, -Force where it is safe",
        ),
        Os::Linux | Os::MacOs => (
            "bash (sh where bash is missing)",
            "-y, --no-pager, DEBIAN_FRONTEND=noninteractive",
        ),
    };
    format!(
        "Runs a command you write in {shell}, as the account that started Kelpy, for what no other tool does. The \
         administrator sees it verbatim and always approves it before it runs. Nothing can be typed into it, so use \
         options that do not ask ({unattended}). Programs it starts end with it: start lasting ones as services. It \
         has no time limit, and the administrator can stop it; its output is kept up to {} KiB.",
        MAX_OUTPUT >> 10,
    )
}
