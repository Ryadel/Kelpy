//! Where actions run. Today on the computer running Kelpy; the same actions will run over SSH on another one.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kelpy_common::lock;
use tokio::io::AsyncWriteExt;

use crate::output::{Output, Stream};
use crate::tree::Tree;
use crate::{
    Account, Command, Os, Prepared, Recorded, RecordedKind, Secrets, ToolError, Work, change,
    database, files, memory, package, service, windows,
};

/// How long one command of an inspection, or reading a file or a folder, may take. A command the model wrote has no
/// limit: a backup or an installation takes as long as it takes, and the administrator can stop it.
pub(crate) const RUN_TIME: Duration = Duration::from_secs(30);
/// The most of an action's output kept: the rest is read and dropped, so the program can finish.
pub const MAX_OUTPUT: usize = 256 << 10;

/// What an action gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ran {
    /// The first non-zero exit code of its commands, else 0; `None` for an action that is not a command.
    pub exit_code: Option<i32>,
    /// What the commands wrote, standard output then errors, or the file or folder read.
    pub output: String,
    /// What a change left, as Kelpy expects to find it from now on.
    pub records: Vec<Recorded>,
    /// Where a file's version before the change was saved.
    pub backup: Option<PathBuf>,
}

/// A file, a service or a package as found now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    /// In the terms of [`Recorded::state`]; `missing` for a file gone.
    pub state: String,
    /// A file's content.
    pub content: Option<String>,
}

/// Runs actions on one system.
#[derive(Debug, Clone)]
pub enum Executor {
    /// This computer, as the account that started Kelpy.
    Local {
        /// Folders whose files are never read or changed, such as Kelpy's own data.
        protected: Vec<PathBuf>,
        /// Where the versions of files before a change are kept.
        backups: PathBuf,
        /// Kelpy's memory of the server, if it has one.
        memory: Option<PathBuf>,
        /// Whether that memory is shared with the other administrators: each note then says whose Kelpy made it.
        memory_shared: bool,
        /// Files Kelpy wrote a generated secret into: like secret files, never read for the model nor shown.
        held: Arc<Mutex<Vec<PathBuf>>>,
        account: Account,
    },
}

impl Executor {
    /// This computer. Files in `protected` folders are never read or changed; files keep their version before a
    /// change in `backups`.
    pub fn local(protected: Vec<PathBuf>, backups: PathBuf) -> Self {
        // Compared with the real paths of the files asked for.
        let protected = protected
            .into_iter()
            .map(|folder| std::fs::canonicalize(&folder).unwrap_or(folder))
            .collect();
        Self::Local {
            protected,
            backups,
            memory: None,
            memory_shared: false,
            held: Arc::default(),
            account: Account::current(),
        }
    }

    /// Keeps the file at `path`, which holds a secret, from the model from now on.
    pub fn hold(&self, path: PathBuf) {
        let Self::Local { held, .. } = self;
        let mut held = lock(held);
        if !held.contains(&path) {
            held.push(path);
        }
    }

    /// Whether the file at the real `path` holds a secret Kelpy wrote.
    async fn holds_secret(&self, path: &Path) -> bool {
        let Self::Local { held, .. } = self;
        let real = match tokio::fs::canonicalize(path).await {
            Ok(real) => change::plain(real),
            Err(_) => return false,
        };
        lock(held).contains(&real)
    }

    /// The same, with Kelpy's memory in `folder`.
    pub fn with_memory(self, folder: PathBuf) -> Self {
        match self {
            Self::Local {
                protected,
                backups,
                held,
                account,
                memory_shared,
                ..
            } => Self::Local {
                protected,
                backups,
                memory: Some(folder),
                memory_shared,
                held,
                account,
            },
        }
    }

    /// The same, its memory shared with the other administrators: each note says whose Kelpy made it, by the name of
    /// the account it runs as.
    pub fn sharing_memory(mut self) -> Self {
        let Self::Local { memory_shared, .. } = &mut self;
        *memory_shared = true;
        self
    }

    /// Who the notes are by, with the memory shared.
    fn author(&self) -> Option<&str> {
        match self {
            Self::Local {
                memory_shared: true,
                account,
                ..
            } => Some(&account.name),
            Self::Local { .. } => None,
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
            Work::Script { shells, directory } => {
                if !directory.is_dir() {
                    return Err(crate::invalid(format!(
                        "the folder {} does not exist",
                        directory.display()
                    )));
                }
                self.choose(shells)
            }
            _ => Ok(()),
        }
    }

    /// Plans a change before it is shown: reads the file and works out the content after it, reads the service's
    /// state, asks apt what it would install. Nothing is changed. When the system is already as asked, says so in
    /// [`Prepared::already`]. The error says, for the model, why it cannot be made.
    pub async fn plan(&self, prepared: &mut Prepared, secrets: &Secrets) -> Result<(), ToolError> {
        let Self::Local {
            protected, held, ..
        } = self;
        let os = self.os();
        let installed = |program: &str| self.installed(program);
        let held = lock(held).clone();
        let already = match &mut prepared.work {
            Work::ChangeFile(file) => {
                let planned =
                    within(change::plan(file, os, protected, secrets, &held, installed)).await?;
                planned.map(|planned| file.planned = Some(planned)).err()
            }
            Work::Service(change) => {
                let planned = within(service::plan(change, os)).await?;
                planned.map(|planned| change.planned = Some(planned)).err()
            }
            Work::Packages(change) => {
                let planned = within(package::plan(change, os, installed)).await?;
                planned.map(|planned| change.planned = Some(planned)).err()
            }
            Work::Database(change) => {
                let planned = within(database::plan(change, secrets, installed)).await?;
                planned.map(|planned| change.planned = Some(planned)).err()
            }
            Work::Features(change) => {
                let planned = within(windows::plan_features(change)).await?;
                planned.map(|planned| change.planned = Some(planned)).err()
            }
            Work::Site(change) => {
                let planned = within(windows::plan_site(change)).await?;
                planned.map(|planned| change.planned = Some(planned)).err()
            }
            Work::Secret(name) if secrets.has(name) => Some(format!(
                "The secret {name} exists already: write {} where it goes.",
                crate::secret::placeholder(name)
            )),
            _ => None,
        };
        prepared.already = already;
        Ok(())
    }

    /// What the file at `subject`, the service or the package of that name is now.
    pub async fn observe(&self, kind: RecordedKind, subject: &str) -> Result<Observed, ToolError> {
        match kind {
            RecordedKind::File => within(change::observe(Path::new(subject))).await,
            RecordedKind::Service => Ok(Observed {
                state: within(service::state(self.os(), subject)).await?.shown(),
                content: None,
            }),
            RecordedKind::Package => Ok(Observed {
                state: within(package::state(subject)).await?,
                content: None,
            }),
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
    pub async fn run(
        &self,
        prepared: &Prepared,
        secrets: &Secrets,
        output: &Output,
    ) -> Result<Ran, ToolError> {
        let Self::Local {
            protected,
            backups,
            memory,
            ..
        } = self;
        let (mut records, mut backup) = (Vec::new(), None);
        let memory = || {
            memory
                .as_deref()
                .ok_or_else(|| crate::invalid("Kelpy has no memory folder here"))
        };
        let exit_code = match &prepared.work {
            Work::Commands(steps) => {
                let commands: Vec<&Command> =
                    steps.iter().filter_map(|step| step.first()).collect();
                let mut exit_code = 0;
                for command in &commands {
                    if commands.len() > 1 {
                        output.line(&format!("$ {}", command.display));
                    }
                    let code = run_local(command, None, Some(RUN_TIME), true, None, output).await?;
                    if exit_code == 0 {
                        exit_code = code;
                    }
                }
                Some(exit_code)
            }
            Work::Script { shells, directory } => {
                let shell = shells
                    .first()
                    .ok_or_else(|| crate::invalid("there is no shell to run it"))?;
                // It may change anything: stopped by a restart, what it did is not known.
                output.set_phase("running the command");
                Some(run_local(shell, Some(directory), None, false, None, output).await?)
            }
            Work::ReadFile(path) => {
                if self.holds_secret(path).await {
                    return Err(ToolError::Protected(path.clone()));
                }
                output.text(&within(files::read(path, protected)).await?);
                None
            }
            Work::ListDirectory(path) => {
                output.text(&within(files::list(path)).await?);
                None
            }
            Work::ChangeFile(file) => {
                let planned = file.planned.as_ref().ok_or_else(not_planned)?;
                backup = change::apply(planned, backups, output).await?;
                records.push(planned.recorded());
                None
            }
            Work::Service(change) => {
                let planned = change.planned.as_ref().ok_or_else(not_planned)?;
                records.push(service::apply(change, planned, self.os(), output).await?);
                None
            }
            Work::Packages(change) => {
                let planned = change.planned.as_ref().ok_or_else(not_planned)?;
                records = package::apply(change, planned, output).await?;
                None
            }
            Work::Database(change) => {
                let planned = change.planned.as_ref().ok_or_else(not_planned)?;
                database::apply(change, planned, secrets, output).await?;
                None
            }
            Work::Features(change) => {
                let planned = change.planned.as_ref().ok_or_else(not_planned)?;
                windows::apply_features(planned, output).await?;
                None
            }
            Work::Site(change) => {
                let planned = change.planned.as_ref().ok_or_else(not_planned)?;
                windows::apply_site(planned, output).await?;
                None
            }
            Work::Secret(name) => {
                secrets.generate(name)?;
                output.line(&format!(
                    "Generated the secret {name}: write {} where it goes. Its value is never shown to you.",
                    crate::secret::placeholder(name)
                ));
                None
            }
            Work::ReadMemory(page) => {
                output.text(&within(memory::read(memory()?, page)).await?);
                None
            }
            Work::Remember { page, note } => {
                let today = kelpy_common::utc(kelpy_common::unix_now());
                let today = today.split(' ').next().unwrap_or_default();
                // A secret the model saw by mistake is not kept.
                let note = secrets.redact(note);
                let by = self.author();
                output.line(&within(memory::remember(memory()?, page, &note, today, by)).await?);
                None
            }
            Work::Plan(_) => {
                output.line(
                    "The administrator approved the plan: its steps run without asking again.",
                );
                None
            }
        };
        Ok(Ran {
            exit_code,
            output: output.snapshot().1,
            records,
            backup,
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

/// Why an approved change cannot run.
fn not_planned() -> ToolError {
    crate::invalid("the change was not planned")
}

/// Runs `command` within `limit`, in English, with `input` on its standard input if given: its exit code and what
/// it wrote.
pub(crate) async fn captured(
    command: &Command,
    input: Option<&str>,
    limit: Duration,
) -> Result<(i32, String), ToolError> {
    let said = Output::new();
    let code = run_local(command, None, Some(limit), true, input, &said).await?;
    Ok((code, said.snapshot().1))
}

/// Runs one command on this computer, within `limit` if given, in English where the program allows it if `english`,
/// else in the administrator's own language, with `input` on its standard input if given, its lines going to
/// `output` as they come. Returns its exit code. What the command starts ends with it.
pub(crate) async fn run_local(
    command: &Command,
    directory: Option<&Path>,
    limit: Option<Duration>,
    english: bool,
    input: Option<&str>,
    output: &Output,
) -> Result<i32, ToolError> {
    let mut process = tokio::process::Command::new(&command.program);
    process
        .args(&command.args)
        .envs(command.env.iter().map(|(name, value)| (name, value)))
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Stopping the answer drops the future, which ends the program.
        .kill_on_drop(true);
    if let Some(directory) = directory {
        process.current_dir(directory);
    }
    if english {
        process.env("LC_ALL", "C");
    }
    Tree::prepare(&mut process);
    let mut child = process.spawn().map_err(|source| ToolError::Start {
        program: command.program.clone(),
        source,
    })?;
    // Dropped with the run, when stopped or out of time, it ends the processes the command started.
    // Failing, the command never ran: it is ended with `child`, dropped.
    let tree = Tree::of(&child).map_err(|source| ToolError::Start {
        program: command.program.clone(),
        source,
    })?;
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let stdin = child.stdin.take();
    let finished = async {
        let reading = async {
            let feeding = async {
                // Closed once written: the program reads its end.
                if let (Some(mut stdin), Some(input)) = (stdin, input) {
                    let _ = stdin.write_all(input.as_bytes()).await;
                }
            };
            tokio::join!(
                feeding,
                output.pump(stdout, Stream::Out),
                output.pump(stderr, Stream::Err)
            )
        };
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
async fn within<T>(work: impl Future<Output = Result<T, ToolError>>) -> Result<T, ToolError> {
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
            already: None,
        }
    }

    async fn run(executor: &Executor, action: &Prepared) -> Result<Ran, ToolError> {
        executor.run(action, &Secrets::new(), &Output::new()).await
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
        let executor = Executor::local(Vec::new(), PathBuf::from("backups"));
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
        let executor = Executor::local(Vec::new(), PathBuf::from("backups"));
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
        let ran = run(
            &executor,
            &prepared(Work::Commands(vec![vec![ok], vec![failing]])),
        )
        .await
        .unwrap();
        assert_eq!(ran.exit_code, Some(3));
        assert!(ran.output.contains("hello"), "{}", ran.output);
        assert!(ran.output.contains("oops"), "{}", ran.output);

        let missing = run(
            &executor,
            &prepared(Work::Commands(vec![vec![command(
                "kelpy-no-such-program",
                &[],
            )]])),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&missing, ToolError::Start { program, .. } if program == "kelpy-no-such-program"),
            "{missing}"
        );
    }

    #[tokio::test]
    async fn a_command_the_model_wrote_starts_in_its_folder() {
        let executor = Executor::local(Vec::new(), PathBuf::from("backups"));
        let folder = tempfile::tempdir().unwrap();
        let text = match executor.os() {
            Os::Windows => "(Get-Location).Path",
            _ => "pwd",
        };
        let mut action = prepared(crate::script::script(
            executor.os(),
            text,
            folder.path().to_owned(),
        ));
        executor.resolve(&mut action).unwrap();
        let ran = run(&executor, &action).await.unwrap();
        let name = folder.path().file_name().unwrap().to_string_lossy();
        assert!(
            ran.output.trim_end().ends_with(name.as_ref()),
            "{}",
            ran.output
        );
    }

    #[tokio::test]
    async fn a_command_the_model_wrote_runs_as_written() {
        let executor = Executor::local(Vec::new(), PathBuf::from("backups"));
        let os = executor.os();
        let text = match os {
            Os::Windows => {
                "Write-Output 'it''s \"quoted\" \\ here'\nWrite-Output 'C:\\dir\\\" \\'\nWrite-Error 'oops'\nexit 4"
            }
            _ => {
                "echo \"it's \\\"quoted\\\" \\\\ here\"\necho 'C:\\dir\\\" \\'\necho oops >&2\nexit 4"
            }
        };
        let nowhere = prepared(crate::script::script(
            os,
            text,
            PathBuf::from("/no/such/folder"),
        ));
        assert!(executor.resolve(&mut nowhere.clone()).is_err());
        let mut action = prepared(crate::script::script(os, text, env::temp_dir()));
        executor.resolve(&mut action).unwrap();
        assert!(action.always_asks());
        assert_eq!(action.command_line().unwrap(), text);
        let output = Output::new();
        let ran = executor
            .run(&action, &Secrets::new(), &output)
            .await
            .unwrap();
        assert_eq!(output.phase().as_deref(), Some("running the command"));
        assert_eq!(ran.exit_code, Some(4));
        // Output and errors come through two pipes, so the error may land anywhere between the output lines.
        let lines: Vec<&str> = ran
            .output
            .lines()
            .filter(|line| !line.contains("oops"))
            .collect();
        assert_eq!(lines[..2], ["it's \"quoted\" \\ here", "C:\\dir\\\" \\"]);
        // Errors as text, not as PowerShell's XML, and without Kelpy's own lines.
        assert!(ran.output.contains("oops"), "{}", ran.output);
        assert!(!ran.output.contains("CLIXML"), "{}", ran.output);
        assert!(!ran.output.contains("OutputEncoding"), "{}", ran.output);
    }

    #[tokio::test]
    async fn what_a_command_leaves_running_ends_with_it() {
        let executor = Executor::local(Vec::new(), PathBuf::from("backups"));
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
        let ran = run(&executor, &prepared(Work::Commands(vec![vec![leaving]])))
            .await
            .unwrap();
        assert_eq!(ran.output.trim(), "started");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_command_reads_what_kelpy_gives_it() {
        let reading = if cfg!(windows) {
            command("findstr", &["x"])
        } else {
            command("cat", &[])
        };
        let (code, said) = captured(&reading, Some("x marks the spot\n"), RUN_TIME)
            .await
            .unwrap();
        assert_eq!((code, said.trim()), (0, "x marks the spot"));
    }

    #[tokio::test]
    async fn a_secret_is_generated_once_and_never_shown() {
        let executor = Executor::local(Vec::new(), PathBuf::from("backups"));
        let secrets = Secrets::new();
        let mut action = prepared(Work::Secret("db".into()));
        executor.plan(&mut action, &secrets).await.unwrap();
        assert_eq!(action.already, None);
        let ran = executor
            .run(&action, &secrets, &Output::new())
            .await
            .unwrap();
        assert!(ran.output.contains("{{secret:db}}"), "{}", ran.output);
        let (value, _) = secrets.fill("{{secret:db}}").unwrap();
        assert!(!ran.output.contains(&value));
        executor.plan(&mut action, &secrets).await.unwrap();
        assert!(action.already.unwrap().contains("exists already"));
    }
}
