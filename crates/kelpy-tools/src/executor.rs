//! Where actions run. Today on the computer running Kelpy; the same actions will run over SSH on another one.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crate::output::Output;
use crate::tree::Tree;
use crate::{Account, Command, Os, Prepared, ToolError, Work, files};

/// How long one command of an inspection, or reading a file or a folder, may take. A command the model wrote has no
/// limit: a backup or an installation takes as long as it takes, and the administrator can stop it.
const RUN_TIME: Duration = Duration::from_secs(30);
/// The most of an action's output kept: the rest is read and dropped, so the program can finish.
pub const MAX_OUTPUT: usize = 256 << 10;

/// What an action gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ran {
    /// The first non-zero exit code of its commands, else 0; `None` for an action that is not a command.
    pub exit_code: Option<i32>,
    /// What the commands wrote, standard output then errors, or the file or folder read.
    pub output: String,
}

/// Runs actions on one system.
#[derive(Debug, Clone)]
pub enum Executor {
    /// This computer, as the account that started Kelpy.
    Local {
        /// Folders whose files are never read, such as Kelpy's own data.
        protected: Vec<PathBuf>,
        account: Account,
    },
}

impl Executor {
    /// This computer. Files in `protected` folders are never read.
    pub fn local(protected: Vec<PathBuf>) -> Self {
        // Compared with the real paths of the files asked for.
        let protected = protected
            .into_iter()
            .map(|folder| std::fs::canonicalize(&folder).unwrap_or(folder))
            .collect();
        Self::Local {
            protected,
            account: Account::current(),
        }
    }

    /// The account actions run as.
    pub fn account(&self) -> &Account {
        match self {
            Self::Local { account, .. } => account,
        }
    }

    pub fn os(&self) -> Os {
        match self {
            Self::Local { .. } => Os::current(),
        }
    }

    /// Chooses, for each step, the first command installed, so that what is shown is what runs. Fails when none of
    /// a step's commands is installed.
    pub fn resolve(&self, prepared: &mut Prepared) -> Result<(), ToolError> {
        match &mut prepared.work {
            Work::Commands(steps) => steps
                .iter_mut()
                .try_for_each(|alternatives| self.choose(alternatives)),
            Work::Script(shells) => self.choose(shells),
            Work::ReadFile(_) | Work::ListDirectory(_) => Ok(()),
        }
    }

    /// Keeps only the first of `alternatives` installed.
    fn choose(&self, alternatives: &mut Vec<Command>) -> Result<(), ToolError> {
        let chosen = alternatives
            .iter()
            .position(|command| self.installed(&command.program))
            .ok_or_else(|| {
                let names = alternatives.iter().map(|command| command.program.clone());
                ToolError::NotInstalled(names.collect())
            })?;
        let command = alternatives.swap_remove(chosen);
        *alternatives = vec![command];
        Ok(())
    }

    /// Runs a resolved action, writing what it gives to `output` as it comes: whoever holds a clone sees it while
    /// it runs, and keeps it if the run is stopped or fails. The error says why it could not run or end; a command
    /// that ran and failed is not an error, its exit code tells.
    pub async fn run(&self, prepared: &Prepared, output: &Output) -> Result<Ran, ToolError> {
        let Self::Local { protected, .. } = self;
        let exit_code = match &prepared.work {
            Work::Commands(steps) => {
                let commands: Vec<&Command> =
                    steps.iter().filter_map(|step| step.first()).collect();
                let mut exit_code = 0;
                for command in &commands {
                    if commands.len() > 1 {
                        output.line(&format!("$ {}", command.display));
                    }
                    let code = run_local(command, Some(RUN_TIME), true, output).await?;
                    if exit_code == 0 {
                        exit_code = code;
                    }
                }
                Some(exit_code)
            }
            Work::Script(shells) => {
                let shell = shells
                    .first()
                    .ok_or_else(|| crate::invalid("there is no shell to run it"))?;
                Some(run_local(shell, None, false, output).await?)
            }
            Work::ReadFile(path) => {
                output.text(&within(files::read(path, protected)).await?);
                None
            }
            Work::ListDirectory(path) => {
                output.text(&within(files::list(path)).await?);
                None
            }
        };
        Ok(Ran {
            exit_code,
            output: output.snapshot().1,
        })
    }

    /// Whether `program` is on the `PATH`.
    fn installed(&self, program: &str) -> bool {
        let Some(path) = env::var_os("PATH") else {
            return false;
        };
        env::split_paths(&path).any(|folder| {
            let candidate = folder.join(program);
            candidate.is_file()
                || (cfg!(windows) && Path::new(&candidate).with_extension("exe").is_file())
        })
    }
}

/// Runs one command on this computer, within `limit` if given, in English where the program allows it if `english`, else in
/// the administrator's own language, its lines going to `output` as they come. Returns its exit code. What the
/// command starts ends with it.
async fn run_local(
    command: &Command,
    limit: Option<Duration>,
    english: bool,
    output: &Output,
) -> Result<i32, ToolError> {
    let mut process = tokio::process::Command::new(&command.program);
    process
        .args(&command.args)
        .envs(command.env.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Stopping the answer drops the future, which ends the program.
        .kill_on_drop(true);
    if english {
        process.env("LC_ALL", "C");
    }
    Tree::prepare(&mut process);
    let mut child = process.spawn().map_err(|source| ToolError::Start {
        program: command.program.clone(),
        source,
    })?;
    // Dropped with the run, when stopped or out of time, it ends the processes the command started.
    let tree = Tree::of(&child);
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let finished = async {
        let reading = async { tokio::join!(output.pump(stdout), output.pump(stderr)) };
        let ending = async {
            let status = child.wait().await;
            // What it left running would keep its output open.
            tree.end();
            status
        };
        tokio::join!(reading, ending)
    };
    let (_, status) = match limit {
        Some(limit) => tokio::time::timeout(limit, finished)
            .await
            .map_err(|_| ToolError::TimedOut(limit))?,
        None => finished.await,
    };
    let status = status.map_err(|source| ToolError::Wait {
        program: command.program.clone(),
        source,
    })?;
    Ok(status.code().unwrap_or(-1))
}

/// What `work` gives, within [`RUN_TIME`].
async fn within(
    work: impl Future<Output = Result<String, ToolError>>,
) -> Result<String, ToolError> {
    tokio::time::timeout(RUN_TIME, work)
        .await
        .map_err(|_| ToolError::TimedOut(RUN_TIME))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared(work: Work) -> Prepared {
        Prepared {
            reason: String::new(),
            summary: String::new(),
            work,
        }
    }

    fn command(program: &str, args: &[&str]) -> Command {
        Command {
            program: program.to_owned(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            env: Vec::new(),
            display: program.to_owned(),
        }
    }

    #[test]
    fn the_first_installed_alternative_is_chosen() {
        let executor = Executor::local(Vec::new());
        let present = if cfg!(windows) { "cmd" } else { "sh" };
        let mut action = prepared(Work::Commands(vec![vec![
            command("kelpy-no-such-program", &[]),
            command(present, &[]),
        ]]));
        executor.resolve(&mut action).unwrap();
        assert_eq!(action.command_line().unwrap(), present);

        let mut missing = prepared(Work::Commands(vec![vec![command(
            "kelpy-no-such-program",
            &[],
        )]]));
        assert_eq!(
            executor.resolve(&mut missing).unwrap_err().to_string(),
            "kelpy-no-such-program is not installed"
        );
    }

    #[tokio::test]
    async fn commands_report_their_output_and_exit_code() {
        let executor = Executor::local(Vec::new());
        let (ok, failing) = if cfg!(windows) {
            (
                command("cmd", &["/C", "echo hello"]),
                command("cmd", &["/C", "echo oops 1>&2 & exit 3"]),
            )
        } else {
            (
                command("sh", &["-c", "echo hello"]),
                command("sh", &["-c", "echo oops >&2; exit 3"]),
            )
        };
        let ran = executor
            .run(
                &prepared(Work::Commands(vec![vec![ok], vec![failing]])),
                &Output::new(),
            )
            .await
            .unwrap();
        assert_eq!(ran.exit_code, Some(3));
        assert!(ran.output.contains("hello"), "{}", ran.output);
        assert!(ran.output.contains("oops"), "{}", ran.output);

        let missing = executor
            .run(
                &prepared(Work::Commands(vec![vec![command(
                    "kelpy-no-such-program",
                    &[],
                )]])),
                &Output::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&missing, ToolError::Start { program, .. } if program == "kelpy-no-such-program"),
            "{missing}"
        );
    }

    #[tokio::test]
    async fn a_command_the_model_wrote_runs_as_written() {
        let executor = Executor::local(Vec::new());
        let os = executor.os();
        let text = match os {
            Os::Windows => {
                "Write-Output 'it''s \"quoted\" \\ here'\nWrite-Output 'C:\\dir\\\" \\'\nWrite-Error 'oops'\nexit 4"
            }
            _ => {
                "echo \"it's \\\"quoted\\\" \\\\ here\"\necho 'C:\\dir\\\" \\'\necho oops >&2\nexit 4"
            }
        };
        let mut action = prepared(crate::script::script(os, text));
        executor.resolve(&mut action).unwrap();
        assert!(action.always_asks());
        assert_eq!(action.command_line().unwrap(), text);
        let ran = executor.run(&action, &Output::new()).await.unwrap();
        assert_eq!(ran.exit_code, Some(4));
        let lines: Vec<&str> = ran.output.lines().collect();
        assert_eq!(lines[..2], ["it's \"quoted\" \\ here", "C:\\dir\\\" \\"]);
        // Errors as text, not as PowerShell's XML, and without Kelpy's own lines.
        assert!(ran.output.contains("oops"), "{}", ran.output);
        assert!(!ran.output.contains("CLIXML"), "{}", ran.output);
        assert!(!ran.output.contains("OutputEncoding"), "{}", ran.output);
    }

    #[tokio::test]
    async fn what_a_command_leaves_running_ends_with_it() {
        let executor = Executor::local(Vec::new());
        // The program left in the background shares the command's output: Kelpy would wait for it.
        let leaving = if cfg!(windows) {
            command(
                "cmd",
                &["/C", "start /B ping -n 60 127.0.0.1 >NUL & echo started"],
            )
        } else {
            command("sh", &["-c", "sleep 60 & echo started"])
        };
        let started = std::time::Instant::now();
        let ran = executor
            .run(
                &prepared(Work::Commands(vec![vec![leaving]])),
                &Output::new(),
            )
            .await
            .unwrap();
        assert_eq!(ran.output.trim(), "started");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
    }
}
