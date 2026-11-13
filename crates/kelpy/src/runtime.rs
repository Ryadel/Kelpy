//! The local runtime: at most one per home, guarded by a lock file and announced through `runtime.json`.
//!
//! `runtime.json` also holds the runtime's credentials (the browser access code and the token for local clients),
//! so only the user who started Kelpy can open it.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use kelpy_common::unix_now;
use kelpy_core::VERSION;
use kelpy_core::agent::{Agent, KeySource, Needed, ProviderInfo};
use kelpy_core::config::{Config, MemoryUse};
use kelpy_core::home::{self, Home};
use kelpy_core::private_file;
use kelpy_core::providers::chatgpt::Endpoints;
use kelpy_core::server_folder::ServerFolder;
use kelpy_core::storage::Store;
use kelpy_core::vault::VaultState;
use kelpy_gateway::{
    Activity, Auth, Elevation, ElevationStep, How, MemoryView, RuntimeStatus, StopRequest,
};
use serde::{Deserialize, Serialize};

use crate::handover::{self, Channel, Handover, Message};
use crate::{duration, elevate, platform, prompt, wizard};
use kelpy_client::Client;

const LOCK_FILE: &str = "runtime.lock";
const INFO_FILE: &str = "runtime.json";
const DATABASE_FILE: &str = "kelpy.db";
/// The port of the last start, tried first when `config.toml` leaves the choice to the operating system.
const LAST_PORT_FILE: &str = "last-port";
/// After a stop is triggered, how long open connections and answers being written may take to finish.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
/// How long the SSH sessions get to close once told to, last before Kelpy says it stopped.
const SSH_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Serialize, Deserialize)]
struct RuntimeInfo {
    pid: u32,
    port: u16,
    url: String,
    version: String,
    started_at: i64,
    /// Unix time of the automatic shutdown requested with `--shutdown-after`, if any.
    #[serde(default)]
    stops_at: Option<i64>,
    /// `--shutdown-idle`, in seconds, if any.
    #[serde(default)]
    idle_limit: Option<u64>,
    /// Signs a browser in, through the one-click link or typed in.
    access_code: String,
    /// `Authorization: Bearer` token for local clients.
    token: String,
}

impl RuntimeInfo {
    /// The one-click link: the code travels in the fragment, which the browser never sends to the server, and the
    /// Web UI removes it from the address bar after signing in.
    fn sign_in_url(&self) -> String {
        format!("{}/#code={}", self.url, self.access_code)
    }
}

/// Held for the runtime's whole life. The operating system releases the lock if the process dies, so a stale
/// `runtime.json` is never mistaken for a running runtime.
struct RuntimeLock {
    _file: File,
    info_path: PathBuf,
}

impl RuntimeLock {
    /// Takes the lock, or returns `None` if a runtime already holds it.
    fn try_acquire(home: &Home) -> Result<Option<Self>> {
        let data_dir = home.data_dir();
        fs::create_dir_all(&data_dir)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path(home))
            .context("cannot open the runtime lock file")?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self {
                _file: file,
                info_path: data_dir.join(INFO_FILE),
            })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => {
                Err(error).context("cannot lock the runtime lock file")
            }
        }
    }

    fn announce(&self, info: &RuntimeInfo) -> Result<()> {
        private_file::write(&self.info_path, &serde_json::to_vec_pretty(info)?)
            .context("cannot write runtime.json")
    }
}

impl Drop for RuntimeLock {
    fn drop(&mut self) {
        // Best effort: the lock, not this file, says whether a runtime is running, so a leftover is harmless.
        let _ = fs::remove_file(&self.info_path);
    }
}

pub fn is_running(home: &Home) -> Result<bool> {
    Ok(RuntimeLock::try_acquire(home)?.is_none())
}

/// The runtime's lock file. Deleting it while held would let a second runtime lock a new file of the same name, so
/// only `cleanup` deletes it, once the folder is no longer a home.
pub fn lock_path(home: &Home) -> PathBuf {
    home.data_dir().join(LOCK_FILE)
}

/// While held, no runtime can start in this home: the executable can be replaced with nothing running it.
pub struct Hold(#[allow(dead_code)] RuntimeLock);

/// Holds the runtime's lock, or returns `None` if a runtime is running.
pub fn hold(home: &Home) -> Result<Option<Hold>> {
    Ok(RuntimeLock::try_acquire(home)?.map(Hold))
}

/// Automatic shutdown, from `--shutdown-after` and `--shutdown-idle`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Limits {
    pub after: Option<Duration>,
    pub idle: Option<Duration>,
}

/// How the runtime shows itself in the terminal that starts it, and whether it can be reached from outside.
#[derive(Debug, Clone, Default)]
pub struct Launch {
    /// Opens the Web UI in the browser once started.
    pub open_browser: bool,
    /// Shows the terminal UI in place of the banner; quitting it stops Kelpy.
    pub terminal_ui: bool,
    /// How the terminal UI draws.
    pub terminal: TerminalUi,
    /// Starts Kelpy's own SSH server.
    pub ssh: Option<Ssh>,
}

/// How the terminal UI draws, as asked on the command line.
#[derive(Debug, Clone, Copy, Default)]
pub struct TerminalUi {
    pub screen: kelpy_tui::Screen,
    pub background: kelpy_tui::Background,
}

/// Kelpy's SSH server, as asked with `--ssh`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Ssh {
    /// The port; 0 lets the system choose.
    pub port: u16,
    /// The fingerprints of keys that connect without asking (`--allow-key`).
    pub allowed_keys: Vec<String>,
}

/// While Kelpy can be reached from outside, it stops by itself: after this long without activity, unless another
/// limit was given.
pub const EXPOSED_IDLE: Duration = Duration::from_secs(60 * 60);

/// How long the new Kelpy has to connect and say hello once the system's prompt was answered, and to say it is
/// ready once handed over.
const HANDOVER_WAIT: Duration = Duration::from_secs(30);
/// How long the new Kelpy takes to start once handed over, at most: less than [`HANDOVER_WAIT`], so that the one
/// before still listens when told, and both agree on whether the restart happened.
const TAKING_OVER_WAIT: Duration = Duration::from_secs(25);
/// How long the new Kelpy waits for the state once it said hello: the Kelpy before stops first, its answers and
/// connections given `SHUTDOWN_GRACE`, its SSH sessions `SSH_GRACE`.
const STOPPING_WAIT: Duration =
    Duration::from_secs(SHUTDOWN_GRACE.as_secs() + SSH_GRACE.as_secs() + 15);
/// How long a Kelpy taking over waits for the one before to let go of the lock and the port.
const RELEASE_WAIT: Duration = Duration::from_secs(10);

/// Runs the runtime and the Web server in the foreground until Ctrl+C, the end of the terminal session, the terminal
/// UI closed, or one of the `limits`. In every case new work is refused at once, and work in progress gets
/// `SHUTDOWN_GRACE` to finish.
///
/// Asked from an interface, Kelpy restarts as administrator: a new process takes over on the same port, and this
/// one stops.
pub async fn run_foreground(home: &Home, launch: Launch, limits: Limits) -> Result<ExitCode> {
    platform::remove_leftovers(&home.root().join(home::executable_name()), &home.data_dir());
    // Taken again only on Windows, after the prompt to start as administrator was refused.
    #[cfg_attr(not(windows), allow(unused_mut))]
    let Some(mut lock) = RuntimeLock::try_acquire(home)? else {
        return already_running(home, &launch).await;
    };
    // The shared memory chosen needs administrator rights: started without them, Kelpy offers to start again with
    // them; otherwise it goes on with the home's own memory, and says so.
    if let Ok(config) = Config::load(&home.config_path())
        && config.memory.chosen() == MemoryUse::Shared
        && let Some(shared) = config.memory.shared_folder()
        && !kelpy_core::Account::current().elevated
    {
        println!("{}.", needs_administrator(&shared));
        #[cfg(windows)]
        if prompt::interactive()
            && prompt::confirm(
                "Start Kelpy as administrator now? Windows asks you to confirm. If not, Kelpy uses your private memory.",
                true,
            )?
        {
            drop(lock);
            if start_as_administrator().await? {
                println!("Kelpy runs as administrator in a new window.");
                return Ok(ExitCode::SUCCESS);
            }
            println!("Windows did not allow it: the prompt was refused or closed.");
            lock = match RuntimeLock::try_acquire(home)? {
                Some(lock) => lock,
                None => return already_running(home, &launch).await,
            };
        }
        if cfg!(windows) {
            println!(
                "Kelpy uses your private memory meanwhile: choose Shared in the Memory menu, under the message box, to \
                 restart it as administrator."
            );
        } else {
            println!("Kelpy uses your private memory meanwhile.");
        }
    }
    let start = Start {
        lock,
        handover: None,
        ready: None,
    };
    serve(home, &launch, limits, start).await
}

/// The Kelpy restarted as administrator, started by the one before with the name of its channel and its PID: takes
/// over what it hands over, then runs in its place, without a window, until stopped. Each step has a deadline: past
/// it, this Kelpy gives up and exits.
pub async fn take_over(home: &Home, channel: &str, from: u32) -> Result<ExitCode> {
    use tokio::time::Instant;
    let mut channel = handover::connect(channel, from, Instant::now() + HANDOVER_WAIT)
        .await
        .context("cannot reach the Kelpy to take over from")?;
    channel
        .send(&Message::Hello {
            version: VERSION.to_owned(),
            pid: std::process::id(),
        })
        .await?;
    channel.until(Instant::now() + STOPPING_WAIT);
    let handover = match channel.receive().await? {
        Message::State(handover) => handover,
        // Cancelled meanwhile: the Kelpy before goes on.
        Message::Refused(_) => return Ok(ExitCode::SUCCESS),
        _ => anyhow::bail!("the Kelpy before sent something unexpected"),
    };
    let starting = Instant::now() + TAKING_OVER_WAIT;
    // Told within the time the Kelpy before still listens.
    channel.until(Instant::now() + HANDOVER_WAIT);
    kelpy_core::environment::take_over(handover.environment.clone());
    let lock = match wait_for_lock(home).await {
        Ok(lock) => lock,
        Err(error) => {
            let _ = channel.send(&Message::Failed(format!("{error:#}"))).await;
            return Err(error);
        }
    };
    let launch = Launch {
        ssh: handover.ssh.clone(),
        ..Launch::default()
    };
    let (ready, told) = tokio::sync::oneshot::channel();
    let start = Start {
        lock,
        handover: Some(handover),
        ready: Some(ready),
    };
    let serving = serve(home, &launch, Limits::default(), start);
    tokio::pin!(serving);
    tokio::select! {
        told = told => {
            let message = match told {
                Ok(Ok(())) => Message::Ready,
                Ok(Err(reason)) => Message::Failed(reason),
                Err(_) => Message::Failed("it stopped while starting".into()),
            };
            channel.send(&message).await?;
        }
        served = &mut serving => {
            let reason = match &served {
                Ok(_) => "it stopped while starting".to_owned(),
                Err(error) => format!("{error:#}"),
            };
            let _ = channel.send(&Message::Failed(reason)).await;
            served?;
            return Ok(ExitCode::FAILURE);
        }
        () = tokio::time::sleep_until(starting) => {
            let reason = format!("it did not start within {}", duration::format(TAKING_OVER_WAIT));
            let _ = channel.send(&Message::Failed(reason)).await;
            return Ok(ExitCode::FAILURE);
        }
    }
    serving.await?;
    Ok(ExitCode::SUCCESS)
}

/// How a runtime starts.
struct Start {
    lock: RuntimeLock,
    /// What the Kelpy before handed over: this one goes on with it.
    handover: Option<Box<Handover>>,
    /// Told once the runtime serves, or why it could not start.
    ready: Option<tokio::sync::oneshot::Sender<std::result::Result<(), String>>>,
}

/// Why the runtime's loop ended.
enum Ending {
    Stop(Option<String>),
    HandOver(Prepared),
}

/// A Kelpy restarted as administrator, connected, waiting for the state.
struct Prepared {
    channel: Channel<handover::Stream>,
    pid: u32,
}

/// What `result`'s error says, to `ready` too: the Kelpy before learns why this one could not start.
fn told<T>(
    ready: &mut Option<tokio::sync::oneshot::Sender<std::result::Result<(), String>>>,
    result: Result<T>,
) -> Result<T> {
    if let Err(error) = &result
        && let Some(ready) = ready.take()
    {
        let _ = ready.send(Err(format!("{error:#}")));
    }
    result
}

/// `home`, with the server's memory in the server folder `shared`, which only administrators can open: its pages,
/// and the records `store` keeps from now on, each saying which administrator's Kelpy made the change.
fn share_memory(home: &Home, shared: &Path, store: &mut Store) -> Result<Home> {
    apart(home, shared)?;
    let account = kelpy_core::Account::current();
    if !account.elevated {
        anyhow::bail!("{}", needs_administrator(shared));
    }
    let folder = ServerFolder::open_or_create(shared).with_context(|| {
        format!(
            "cannot open the server's shared memory in {}",
            shared.display()
        )
    })?;
    store
        .share_records(&folder.database_path())
        .context("cannot open the server's shared records")?;
    store.set_recorder(&account.name);
    Ok(home.clone().with_memory(folder.memory_dir()))
}

/// Refuses a server folder `shared` that overlaps the home: the home is one administrator's, and `kelpy cleanup`
/// deletes it.
fn apart(home: &Home, shared: &Path) -> Result<()> {
    if kelpy_core::server_folder::overlaps(shared, home.root()) {
        anyhow::bail!(
            "the server's shared memory, {}, and Kelpy's home, {}, overlap: the home is yours alone, and \
             `kelpy cleanup` deletes it. Set a folder outside the home in the [memory] section of {}",
            shared.display(),
            home.root().display(),
            home.config_path().display()
        );
    }
    Ok(())
}

/// Starts Kelpy again as administrator, as it was started, in a window of its own, once Windows' prompt allows it.
/// Returns whether it did.
#[cfg(windows)]
async fn start_as_administrator() -> Result<bool> {
    let program = std::env::current_exe().context("cannot locate the Kelpy executable")?;
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let started =
        tokio::task::spawn_blocking(move || elevate::through_prompt(&program, &arguments, true))
            .await??;
    Ok(started.is_some())
}

/// Why Kelpy, not running as administrator, cannot use the shared memory in `shared`, and what to do.
pub fn needs_administrator(shared: &Path) -> String {
    let how = if cfg!(windows) {
        "start Kelpy as administrator"
    } else {
        "start Kelpy with `sudo kelpy`"
    };
    format!(
        "Only administrators can open the server's shared memory, in {}: to use it, {how}",
        shared.display()
    )
}

/// The memory of the server this Kelpy uses: `chosen`, unless it is the shared one and cannot be opened, when the
/// home's own is used and the interfaces say why. With the shared one, `home` is returned with its folder, and
/// `store` keeps its records there.
fn memory_of(
    home: &Home,
    config: &kelpy_core::config::MemoryConfig,
    chosen: MemoryUse,
    store: &mut Store,
) -> (Home, MemoryView) {
    let shared = config.shared_folder();
    let mut view = MemoryView {
        using: MemoryUse::Private,
        chosen,
        shared: shared.as_ref().map(|folder| folder.display().to_string()),
        problem: None,
    };
    let (MemoryUse::Shared, Some(shared)) = (chosen, shared) else {
        return (home.clone(), view);
    };
    match share_memory(home, &shared, store) {
        Ok(home) => {
            view.using = MemoryUse::Shared;
            (home, view)
        }
        Err(error) => {
            view.problem = Some(format!("{error:#}"));
            (home.clone(), view)
        }
    }
}

/// The memory to use: `asked` from an interface, once recorded in the configuration at `config`, or the one
/// `recorded` there, with why `asked` could not be.
fn record_memory(
    config: &Path,
    recorded: MemoryUse,
    asked: Option<MemoryUse>,
) -> (MemoryUse, Option<String>) {
    let Some(asked) = asked else {
        return (recorded, None);
    };
    match kelpy_core::config::save_memory(config, asked, None) {
        Ok(()) => (asked, None),
        Err(error) => {
            tracing::warn!("cannot record the memory chosen: {error}");
            let problem = format!(
                "the {} memory could not be recorded as the one chosen, so Kelpy goes on with the one recorded \
                 before: {error}",
                memory_name(asked)
            );
            (recorded, Some(problem))
        }
    }
}

/// The memory `using`, as the interfaces name it.
fn memory_name(using: MemoryUse) -> &'static str {
    match using {
        MemoryUse::Private => "private",
        MemoryUse::Shared => "shared",
    }
}

/// The memory line of the start screen and the start-up lines.
fn memory_line(home: &Home, memory: &MemoryView) -> String {
    match (memory.using, &memory.problem) {
        (MemoryUse::Shared, _) => format!(
            "shared with the other administrators, in {}",
            home.memory_dir().display()
        ),
        (MemoryUse::Private, Some(problem)) => {
            format!("private, as the shared one is not available: {problem}")
        }
        (MemoryUse::Private, None) => "private, only for you".to_owned(),
    }
}

/// Runs one runtime in this process: fresh, or going on with what the Kelpy before handed over.
async fn serve(home: &Home, launch: &Launch, limits: Limits, start: Start) -> Result<ExitCode> {
    let Start {
        lock,
        handover,
        mut ready,
    } = start;
    let config = told(
        &mut ready,
        Config::load(&home.config_path()).map_err(Into::into),
    )?;
    let handover = handover.map(|handover| *handover);
    let first = handover.is_none();

    // The data folder holds the conversations and the runtime's credentials.
    if let Err(error) = private_file::restrict_dir(&home.data_dir()) {
        println!(
            "Warning: cannot restrict {} to the current user ({error}). Other users may read the conversations.",
            home.data_dir().display()
        );
    }
    let mut store = told(
        &mut ready,
        Store::open(&home.data_dir().join(DATABASE_FILE))
            .context("cannot open the conversation database"),
    )?;
    // The server's memory: the home's own, or shared with the other administrators, its pages and records in the
    // server folder. Chosen from an interface, the Kelpy before hands the choice over: recorded first, then used, so
    // that the next start uses it too. Not recorded, Kelpy goes on with the one recorded, and says why.
    let (chosen, unrecorded) = record_memory(
        &home.config_path(),
        config.memory.chosen(),
        handover.as_ref().and_then(|handover| handover.memory),
    );
    let (home, mut memory) = memory_of(home, &config.memory, chosen, &mut store);
    if let Some(unrecorded) = unrecorded {
        memory.problem = Some(match memory.problem.take() {
            Some(problem) => format!("{unrecorded}; {problem}"),
            None => unrecorded,
        });
    }
    let home = &home;
    let agent = Arc::new(told(
        &mut ready,
        Agent::new(home, store, config.provider.clone(), Endpoints::default()).map_err(Into::into),
    )?);
    let (auth, port, started_at, limits, ssh) = match handover {
        Some(handover) => {
            for problem in agent.take_over(handover.agent).await {
                tracing::warn!("restarted as administrator: {problem}");
            }
            let now = unix_now();
            let limits = Limits {
                after: handover.stops_at.map(|at| {
                    Duration::from_secs(u64::try_from(at.saturating_sub(now)).unwrap_or(0))
                }),
                idle: handover.idle_limit.map(Duration::from_secs),
            };
            (
                Auth::take_over(handover.auth),
                Some(handover.port),
                handover.started_at,
                limits,
                handover.ssh,
            )
        }
        None => (
            told(
                &mut ready,
                Auth::new().context("cannot create the access code"),
            )?,
            None,
            unix_now(),
            limits,
            launch.ssh.clone(),
        ),
    };
    let auth = Arc::new(auth);
    // What Kelpy left on the server, checked against what is there: the differences wait in the Web UI.
    tokio::spawn({
        let agent = Arc::clone(&agent);
        async move {
            let _ = agent.check_server().await;
        }
    });

    let last_port_path = home.data_dir().join(LAST_PORT_FILE);
    let last_port = fs::read_to_string(&last_port_path)
        .ok()
        .and_then(|text| text.trim().parse::<u16>().ok())
        .filter(|&port| port != 0 && config.server.port == 0);
    let listener = match port {
        // The same port as before: the pages open there reconnect by themselves.
        Some(port) => told(&mut ready, bind_released(config.server.host, port).await)?,
        None => told(
            &mut ready,
            kelpy_gateway::bind(config.server.host, config.server.port, last_port)
                .await
                .context("cannot start the local server"),
        )?,
    };
    let address = listener.local_addr()?;
    // Best effort: without it, the next start only gets a port of the operating system's choice.
    let _ = fs::write(&last_port_path, address.port().to_string());
    let info = RuntimeInfo {
        pid: std::process::id(),
        port: address.port(),
        url: format!("http://{address}"),
        version: VERSION.to_owned(),
        started_at,
        stops_at: limits.after.map(|limit| {
            unix_now().saturating_add(i64::try_from(limit.as_secs()).unwrap_or(i64::MAX))
        }),
        idle_limit: limits.idle.map(|limit| limit.as_secs()),
        access_code: auth.access_code(),
        token: auth.token().to_owned(),
    };
    told(&mut ready, lock.announce(&info))?;

    // Kelpy's own SSH server, until Kelpy stops.
    let remote = match &ssh {
        None => None,
        Some(ssh) => {
            agent.connections().allow_keys(ssh.allowed_keys.clone());
            let options = kelpy_remote::RemoteOptions {
                port: ssh.port,
                host_key: home.data_dir().join("ssh_host_ed25519_key"),
                files: Arc::clone(agent.files()),
                web_port: address.port(),
                api_url: info.url.clone(),
                token: info.token.clone(),
                banner: banner(
                    VERSION,
                    info.sign_in_url(),
                    vec![format!(
                        "Connected through Kelpy's SSH: to open the Web UI on your computer, forward port {0} with \
                         -L {0}:127.0.0.1:{0}. Closing this terminal UI leaves Kelpy running.",
                        address.port()
                    )],
                ),
                connections: Arc::clone(agent.connections()),
            };
            Some(told(
                &mut ready,
                kelpy_remote::start(options)
                    .await
                    .context("cannot start Kelpy's SSH server"),
            )?)
        }
    };
    if let Some(ready) = ready.take() {
        let _ = ready.send(Ok(()));
    }
    let mut banner = None;
    if first {
        banner = introduce(
            home,
            launch,
            limits,
            &agent,
            &info,
            remote.as_ref(),
            last_port,
            &memory_line(home, &memory),
        )
        .await?;
        if config.provider.is_none() && !launch.terminal_ui && prompt::interactive() {
            let client = Client::new(info.url.clone(), info.token.clone())?;
            // A thread of its own, not a task: the questions block on the terminal, and an unanswered one must not
            // keep Kelpy from exiting.
            thread::spawn(move || {
                if let Err(error) = wizard::offer(client) {
                    println!("Setup not completed: {error:#}");
                }
            });
        }
    }

    let elevated = agent.account().elevated;
    let elevation = Arc::new(
        Elevation::new(elevated, how_to_elevate(), Arc::clone(&agent)).with_memory(
            memory.clone(),
            {
                let config = home.config_path();
                move |using| {
                    kelpy_core::config::save_memory(&config, using, None)
                        .map_err(|error| format!("cannot record the memory chosen: {error}"))
                }
            },
        ),
    );
    let status = RuntimeStatus {
        version: VERSION.to_owned(),
        pid: std::process::id(),
        home: home.root().to_path_buf(),
        started_at,
        account: agent.account().label(),
        account_limits: agent.account().limits().map(str::to_owned),
    };
    let activity = Arc::new(Activity::new());
    let stop_request = Arc::new(StopRequest::new());
    let router = kelpy_gateway::router(
        status,
        address.port(),
        Arc::clone(&activity),
        Arc::clone(&auth),
        Arc::clone(&agent),
        Arc::clone(&stop_request),
        Arc::clone(&elevation),
    );
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = kelpy_gateway::serve(listener, router, async {
        // Either a stop was sent or the sender is gone with the runtime: both mean stop.
        let _ = stopped.await;
    });
    tokio::pin!(server);
    let shutdown = shutdown_signal(limits, activity, stop_request);
    tokio::pin!(shutdown);
    // The terminal UI, when shown: quitting it stops Kelpy. Dropped when Kelpy stops otherwise, it gives the
    // terminal back first, so that what follows prints on it.
    let terminal_ui = async {
        let Some(banner) = banner else {
            return std::future::pending().await;
        };
        let api = kelpy_tui::Api::new(info.url.clone(), info.token.clone())?;
        let options = kelpy_tui::Options {
            banner,
            stops_runtime: true,
            screen: launch.terminal.screen,
            background: launch.terminal.background,
            open: browser(),
        };
        kelpy_tui::run_local(api, options)
            .await
            .context("the terminal UI failed")
    };
    let mut terminal_ui = Box::pin(terminal_ui);
    let mut preparing: Option<tokio::task::JoinHandle<std::result::Result<Prepared, String>>> =
        None;
    // The restart under way: whether as administrator, and with which memory.
    let mut restarting = (true, None);
    let ending = loop {
        tokio::select! {
            result = &mut server => {
                result.context("the local server failed")?;
                break Ending::Stop(None);
            }
            reason = &mut shutdown => break Ending::Stop(Some(reason)),
            exit = &mut terminal_ui => break Ending::Stop(Some(match exit {
                Ok(kelpy_tui::Exit::Quit) => "the terminal UI was closed".to_owned(),
                Ok(kelpy_tui::Exit::Stopped) => "requested from another interface".to_owned(),
                Err(error) => format!("{error:#}"),
            })),
            restart = elevation.requested(), if preparing.is_none() => {
                restarting = (restart.elevate, restart.memory);
                preparing = Some(tokio::spawn(prepare(
                    home.clone(),
                    launch.terminal_ui,
                    restart.cancel,
                    restart.elevate,
                )));
            }
            prepared = async { preparing.as_mut().expect("guarded").await }, if preparing.is_some() => {
                preparing = None;
                match prepared {
                    Ok(Ok(prepared)) => break Ending::HandOver(prepared),
                    Ok(Err(reason)) => elevation.set(ElevationStep::Failed { reason }),
                    Err(error) => elevation.set(ElevationStep::Failed { reason: error.to_string() }),
                }
            }
        }
    };

    let prepared = match ending {
        Ending::Stop(reason) => {
            if let Some(reason) = reason {
                stop_now(&agent, &reason, stop, &mut server).await?;
            }
            // The SSH sessions end before the lock goes: nothing reaches Kelpy once it says it stopped.
            if let Some(remote) = remote
                && !remote.stop(SSH_GRACE).await
            {
                println!(
                    "Some SSH sessions did not close within {}; closing them.",
                    duration::format(SSH_GRACE)
                );
            }
            drop(lock);
            #[cfg(unix)]
            if elevated {
                elevate::give_back(home.root());
            }
            println!("Kelpy stopped.");
            return Ok(ExitCode::SUCCESS);
        }
        Ending::HandOver(prepared) => prepared,
    };

    // Restarting, as administrator or with the other memory: this runtime stops without telling the interfaces,
    // which reconnect to the new one, and without ending the ChatGPT session, which the new one goes on with. The
    // terminal UI closes first.
    let (as_administrator, with_memory) = restarting;
    elevation.set(ElevationStep::Restarting);
    drop(terminal_ui);
    println!();
    match with_memory {
        Some(MemoryUse::Shared) => println!("Restarting Kelpy with the shared memory."),
        Some(MemoryUse::Private) => println!("Restarting Kelpy with your private memory."),
        None => println!("Restarting Kelpy as administrator."),
    }
    let Prepared { mut channel, pid } = prepared;
    let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
    let answers_saved = agent.stop_for_restart(SHUTDOWN_GRACE);
    let _ = stop.send(());
    answers_saved.await;
    let _ = tokio::time::timeout_at(deadline, &mut server).await;
    if let Some(remote) = remote {
        remote.stop(SSH_GRACE).await;
    }
    agent.close(deadline).await;
    let handover = Handover {
        auth: auth.handed_over(),
        agent: agent.handed_over(),
        environment: kelpy_core::environment::handed_over(),
        port: address.port(),
        started_at,
        stops_at: info.stops_at,
        idle_limit: info.idle_limit,
        ssh,
        memory: with_memory,
    };
    let sent = serde_json::to_vec(&handover)?;
    drop(handover);
    drop(lock);
    channel.until(tokio::time::Instant::now() + HANDOVER_WAIT);
    let outcome = match channel.send_state(&sent).await {
        Err(error) if error.kind() == io::ErrorKind::TimedOut => {
            Err("it did not take the state in time".to_owned())
        }
        Err(error) => Err(format!("it stopped answering ({error})")),
        Ok(()) => match channel.receive().await {
            Ok(Message::Ready) => Ok(()),
            Ok(Message::Failed(reason)) => Err(format!("it could not start: {reason}")),
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                Err("it did not start in time".to_owned())
            }
            _ => Err("it stopped".to_owned()),
        },
    };
    match outcome {
        Ok(()) => {
            let how = if as_administrator {
                " as administrator"
            } else {
                ""
            };
            println!(
                "Kelpy runs{how} (PID {pid}), without a window, at the same address: the Web UI reloads by itself. \
                 `kelpy tui` opens the terminal UI, `kelpy stop` stops it."
            );
            Ok(ExitCode::SUCCESS)
        }
        Err(reason) => {
            println!("The Kelpy restarted did not take over: {reason}. Start Kelpy again.");
            Ok(ExitCode::FAILURE)
        }
    }
}

/// Stops this runtime for `reason`, as Kelpy always stops: answers stopped and kept as interrupted, connections
/// given `SHUTDOWN_GRACE`, the ChatGPT session ended, then the work in the background and the database.
async fn stop_now(
    agent: &Agent,
    reason: &str,
    stop: tokio::sync::oneshot::Sender<()>,
    server: &mut std::pin::Pin<&mut impl Future<Output = io::Result<()>>>,
) -> Result<()> {
    println!();
    println!("Stopping Kelpy: {reason}.");
    // Access ends at once: no new answers, then no new connections or requests. Answers being written are stopped
    // and saved as interrupted; open connections, such as answer streams, get a bounded time to close.
    let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
    let answers_saved = agent.stop_all(SHUTDOWN_GRACE);
    // Fails only if the server already ended on its own, which is just as stopped.
    let _ = stop.send(());
    if !answers_saved.await {
        println!("Some answers could not be saved in time.");
    }
    // Access ends with the session: the ChatGPT tokens are revoked within the same grace, unless the user chose to
    // save the sign-in on this computer.
    if !agent.chatgpt().close(deadline).await {
        println!(
            "OpenAI did not confirm that the ChatGPT session was revoked; it ends on its own within 30 days."
        );
    }
    match tokio::time::timeout_at(deadline, server.as_mut()).await {
        Ok(result) => result.context("the local server failed")?,
        Err(_) => println!(
            "Some connections did not close within {}; closing them.",
            duration::format(SHUTDOWN_GRACE)
        ),
    }
    // Last, the work in the background and the database, which the steps above may still have used.
    if !agent.close(deadline).await {
        println!(
            "Some work in the background did not end within {}; stopping it.",
            duration::format(SHUTDOWN_GRACE)
        );
    }
    Ok(())
}

/// Says where Kelpy runs, at its first start in this process: on the terminal UI's start screen, or as lines in the
/// console; then opens the browser if asked.
#[allow(clippy::too_many_arguments)]
async fn introduce(
    home: &Home,
    launch: &Launch,
    limits: Limits,
    agent: &Agent,
    info: &RuntimeInfo,
    remote: Option<&kelpy_remote::Remote>,
    last_port: Option<u16>,
    memory: &str,
) -> Result<Option<kelpy_tui::Banner>> {
    let address: SocketAddr = info
        .url
        .trim_start_matches("http://")
        .parse()
        .context("unexpected local address")?;
    let reachable = remote.map(|remote| {
        format!(
            "port {}, host key {}: `ssh -p {} {}@<this server>`; each connection waits for your approval here or in the \
             Web UI",
            remote.port,
            remote.fingerprint,
            remote.port,
            login_name().as_deref().unwrap_or("you")
        )
    });
    let vault = describe_vault(agent).await;
    let moved = last_port.filter(|&last| last != address.port()).map(|last| {
        format!("Port {last} of the last start was not available: open the new link, and set up an SSH tunnel again.")
    });
    let tunnel = platform::ssh_connection()
        .and_then(|connection| ssh_tunnel_command(&connection, login_name().as_deref(), address));
    let stops = [
        limits
            .after
            .map(|limit| format!("Stops automatically in {}.", duration::format(limit))),
        limits.idle.map(|limit| {
            format!(
                "Stops automatically after {} without activity.",
                duration::format(limit)
            )
        }),
    ];
    let banner = if launch.terminal_ui {
        // The banner's content, on the terminal UI's start screen.
        let mut header = vec![format!(
            "Code: {} (to sign in from another browser) · Home: {}",
            info.access_code,
            home.root().display()
        )];
        let mut account = format!("Account: {}", agent.account().label());
        if let Some(vault) = &vault {
            account.push_str(&format!(" · Vault: {vault}"));
        }
        header.push(account);
        header.push(format!("Memory: {memory}"));
        header.extend(agent.account().limits().map(str::to_owned));
        header.extend(reachable.iter().map(|line| format!("SSH: {line}")));
        header.extend(moved);
        if let Some(command) = &tunnel {
            header.push(format!(
                "Over SSH: to use the Web UI from your own computer, run there `{command}`"
            ));
        }
        header.extend(stops.into_iter().flatten());
        Some(banner(VERSION, info.sign_in_url(), header))
    } else {
        println!("Kelpy v{VERSION}");
        println!();
        println!("Agent runtime started.");
        println!("Web UI:   {}", info.sign_in_url());
        println!(
            "Code:     {}  (to sign in from another browser)",
            info.access_code
        );
        println!("Home:     {}", home.root().display());
        println!("Memory:   {memory}");
        println!("Provider: {}", describe_provider(agent));
        println!("Account:  {}", agent.account().label());
        if let Some(limits) = agent.account().limits() {
            println!("          {limits}");
        }
        if let Some(vault) = &vault {
            println!("Vault:    {vault}");
        }
        if let Some(reachable) = &reachable {
            println!("SSH:      {reachable}");
        }
        if let Some(moved) = &moved {
            println!("{moved}");
        }
        println!();
        if let Some(command) = &tunnel {
            println!(
                "You are connected over SSH. To use the Web UI from your own computer, run there:"
            );
            println!("  {command}");
            println!("then open the Web UI link above.");
            println!();
        }
        for stop in stops.iter().flatten() {
            println!("{stop}");
        }
        println!("Press Ctrl+C to stop.");
        None
    };
    if launch.open_browser {
        open_url(&info.sign_in_url());
    }
    Ok(banner)
}

/// How this Kelpy can restart with administrator rights: User Account Control on Windows, where its prompt shows on
/// this computer's screen (not over SSH); `sudo` in Kelpy's terminal elsewhere, or by hand without one.
fn how_to_elevate() -> Option<How> {
    if cfg!(windows) {
        platform::ssh_connection().is_none().then_some(How::Prompt)
    } else if prompt::interactive() {
        Some(How::Terminal)
    } else {
        Some(How::Command)
    }
}

/// Starts `program` with `arguments` again, with this Kelpy's rights, without a window or a terminal. Returns its PID.
fn start_again(program: &Path, arguments: &[String]) -> io::Result<u32> {
    use std::process::Stdio;
    let mut command = std::process::Command::new(program);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own process group: a Ctrl+C in this terminal does not reach it.
        command.process_group(0);
    }
    Ok(command.spawn()?.id())
}

/// Starts `program` with `arguments` as administrator, through the system's prompt: User Account Control, or `sudo`
/// in Kelpy's terminal. Returns who the new Kelpy will be.
async fn as_administrator(
    program: std::path::PathBuf,
    arguments: Vec<String>,
    terminal_ui: bool,
) -> std::result::Result<handover::Peer, String> {
    #[cfg(windows)]
    {
        let _ = terminal_ui;
        let started = tokio::task::spawn_blocking(move || {
            elevate::through_prompt(&program, &arguments, false)
        })
        .await
        .map_err(|error| error.to_string())?;
        match started {
            Ok(Some(pid)) => Ok(handover::Peer::Process(pid)),
            Ok(None) => Err("Windows did not allow it: the prompt was refused or closed".into()),
            Err(error) => Err(format!("Windows could not start it ({error})")),
        }
    }
    #[cfg(unix)]
    {
        // The terminal UI asked `sudo` for the password already, in its terminal; otherwise `sudo` asks there now.
        match elevate::with_sudo(&program, &arguments, !terminal_ui).await {
            Ok(true) => Ok(handover::Peer::Root),
            Ok(false) if terminal_ui => {
                Err("sudo needs your password: press Ctrl+E in Kelpy's terminal".into())
            }
            Ok(false) => Err("sudo did not allow it".into()),
            Err(error) => Err(format!("sudo could not start it ({error})")),
        }
    }
}

/// Restarts Kelpy, as administrator if `elevate`, up to the point where the new one is connected: the system's
/// prompt, the channel. Until then nothing changes here; refused, cancelled or failed, the reason is returned and
/// Kelpy goes on as it was.
async fn prepare(
    home: Home,
    terminal_ui: bool,
    cancel: kelpy_gateway::CancellationToken,
    elevate: bool,
) -> std::result::Result<Prepared, String> {
    let listener =
        handover::Listener::open().map_err(|error| format!("cannot open a channel ({error})"))?;
    let program = std::env::current_exe().map_err(|error| error.to_string())?;
    let arguments = vec![
        "--home".to_owned(),
        home.root().display().to_string(),
        "handover".to_owned(),
        listener.name().to_owned(),
        std::process::id().to_string(),
    ];
    // Development builds can restart without the prompt, which no automated check can answer.
    let without_prompt = !elevate
        || (cfg!(debug_assertions)
            && std::env::var_os("KELPY_TEST_RESTART_WITHOUT_PROMPT").is_some());
    let peer = if without_prompt {
        let _ = terminal_ui;
        handover::Peer::Process(
            start_again(&program, &arguments)
                .map_err(|error| format!("cannot start it ({error})"))?,
        )
    } else {
        as_administrator(program, arguments, terminal_ui).await?
    };
    // Connected and said hello by then, or given up.
    let deadline = tokio::time::Instant::now() + HANDOVER_WAIT;
    let accepted = tokio::select! {
        accepted = listener.accept(peer, deadline) => Some(accepted),
        () = cancel.cancelled() => None,
    };
    let Some(accepted) = accepted else {
        return Err("Cancelled.".into());
    };
    let mut channel = match accepted {
        Ok(channel) => channel,
        Err(error) if error.kind() == io::ErrorKind::TimedOut => {
            return Err("the Kelpy restarted did not connect in time".into());
        }
        Err(error) => return Err(format!("the channel was refused ({error})")),
    };
    let refuse = async |channel: &mut Channel<handover::Stream>, reason: &str| {
        let _ = channel.send(&Message::Refused(reason.to_owned())).await;
        reason.to_owned()
    };
    let pid = match channel.receive().await {
        Ok(Message::Hello { version, pid }) if version == VERSION => pid,
        Ok(Message::Hello { version, .. }) => {
            return Err(refuse(
                &mut channel,
                &format!("the new Kelpy is version {version}, not {VERSION}"),
            )
            .await);
        }
        _ => return Err("the Kelpy restarted as administrator did not say hello".into()),
    };
    if cancel.is_cancelled() {
        return Err(refuse(&mut channel, "Cancelled.").await);
    }
    Ok(Prepared { channel, pid })
}

/// Waits for the runtime's lock, which the Kelpy before lets go of when it hands over.
async fn wait_for_lock(home: &Home) -> Result<RuntimeLock> {
    let deadline = tokio::time::Instant::now() + RELEASE_WAIT;
    loop {
        if let Some(lock) = RuntimeLock::try_acquire(home)? {
            return Ok(lock);
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "the Kelpy before did not let go of the home"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Binds `port`, which the Kelpy before lets go of when it hands over: not another, so that the pages open there
/// reconnect by themselves.
async fn bind_released(host: IpAddr, port: u16) -> Result<tokio::net::TcpListener> {
    let deadline = tokio::time::Instant::now() + RELEASE_WAIT;
    loop {
        match kelpy_gateway::bind(host, port, None).await {
            Ok(listener) => return Ok(listener),
            Err(error) if tokio::time::Instant::now() >= deadline => {
                return Err(error).context(format!("cannot take over port {port}"));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

pub fn status(home: &Home) -> Result<ExitCode> {
    if !is_running(home)? {
        println!("Kelpy is not running.");
        println!("  Home:    {}", home.root().display());
        return Ok(ExitCode::from(1));
    }
    match read_info(home) {
        Ok(Some(info)) => {
            println!("Kelpy is running.");
            println!("  URL:     {}", info.url);
            println!("  Code:    {}", info.access_code);
            println!("  PID:     {}", info.pid);
            println!("  Port:    {}", info.port);
            println!("  Version: {}", info.version);
            println!("  Home:    {}", home.root().display());
            if let Some(stops_at) = info.stops_at {
                let left = Duration::from_secs(u64::try_from(stops_at - unix_now()).unwrap_or(0));
                println!("  Stops:   in {}", duration::format(left));
            }
            if let Some(idle) = info.idle_limit {
                let idle = duration::format(Duration::from_secs(idle));
                println!("  Idle:    stops after {idle} without activity");
            }
        }
        Ok(None) => println!("Kelpy is starting."),
        Err(error) => println!("Kelpy is running. {}", unreadable(&error)),
    }
    Ok(ExitCode::SUCCESS)
}

/// `kelpy tui`: the terminal UI of the Kelpy running for this home, which goes on when it is closed; with none
/// running, Kelpy started with its terminal UI, as `kelpy` does.
pub async fn terminal_ui(
    home: &Home,
    open_browser: bool,
    terminal: TerminalUi,
) -> Result<ExitCode> {
    if !prompt::interactive() {
        anyhow::bail!("the terminal UI needs an interactive terminal");
    }
    if !is_running(home)? {
        let launch = Launch {
            open_browser,
            terminal_ui: true,
            terminal,
            ssh: None,
        };
        return run_foreground(home, launch, Limits::default()).await;
    }
    let info = match read_info(home) {
        Ok(Some(info)) => info,
        Ok(None) => {
            println!("Kelpy is starting or being updated. Try again in a moment.");
            return Ok(ExitCode::from(1));
        }
        Err(error) => {
            println!("{}", unreadable(&error));
            return Ok(ExitCode::from(1));
        }
    };
    attach(home, info, terminal).await
}

/// Shows the terminal UI of the Kelpy running for `home`, described by `info`; closing it leaves Kelpy running.
async fn attach(home: &Home, info: RuntimeInfo, terminal: TerminalUi) -> Result<ExitCode> {
    let notes = vec![
        format!(
            "Code: {} (to sign in from another browser) · Home: {}",
            info.access_code,
            home.root().display()
        ),
        "Attached to the Kelpy running: closing this terminal UI leaves it running.".to_owned(),
    ];
    let api = kelpy_tui::Api::new(info.url.clone(), info.token.clone())?;
    let options = kelpy_tui::Options {
        banner: banner(&info.version, info.sign_in_url(), notes),
        stops_runtime: false,
        screen: terminal.screen,
        background: terminal.background,
        open: browser(),
    };
    if kelpy_tui::run_local(api, options).await? == kelpy_tui::Exit::Stopped {
        println!("Kelpy has stopped.");
    }
    Ok(ExitCode::SUCCESS)
}

pub fn open(home: &Home) -> Result<ExitCode> {
    if !is_running(home)? {
        println!("Kelpy is not running. Start it with `kelpy`.");
        return Ok(ExitCode::from(1));
    }
    match read_info(home) {
        Ok(Some(info)) => {
            open_url(&info.sign_in_url());
            Ok(ExitCode::SUCCESS)
        }
        Ok(None) => {
            println!("Kelpy is starting. Try again in a moment.");
            Ok(ExitCode::from(1))
        }
        Err(error) => {
            println!("{}", unreadable(&error));
            Ok(ExitCode::from(1))
        }
    }
}

/// How long `kelpy` waits for a Kelpy still starting, or being updated, before saying it is not ready.
const STARTING_WAIT: Duration = Duration::from_secs(30);

/// `kelpy` with a Kelpy already running for `home`: what runs, since when, and, at a terminal, what to do with it.
async fn already_running(home: &Home, launch: &Launch) -> Result<ExitCode> {
    let info = match started(home).await {
        Ok(Some(info)) => info,
        Ok(None) => {
            println!(
                "Kelpy is starting, or being updated, and is not ready yet. Try again in a moment."
            );
            return Ok(ExitCode::from(1));
        }
        Err(error) => {
            println!("Kelpy is already running. {}", unreadable(&error));
            return Ok(ExitCode::SUCCESS);
        }
    };
    println!();
    println!("Kelpy is already running.");
    for (label, value) in running_facts(&info, home, unix_now()) {
        println!("  {label:<7}  {value}");
    }
    if !prompt::interactive() {
        println!("Stop it with: kelpy stop");
        if launch.open_browser {
            open_url(&info.sign_in_url());
        }
        return Ok(ExitCode::SUCCESS);
    }
    println!();
    println!("What now?");
    println!("  1) Open the terminal UI here");
    println!("  2) Open the Web UI in the browser");
    println!("  3) Stop Kelpy");
    println!("  4) Nothing, leave it running");
    match prompt::choose(4, 1)? {
        1 => attach(home, info, launch.terminal).await,
        2 => {
            open_url(&info.sign_in_url());
            Ok(ExitCode::SUCCESS)
        }
        3 => {
            if let Some(title) = answering(&info).await.first() {
                println!();
                println!("! An answer is being written in \"{title}\".");
                if !prompt::confirm("  Stop Kelpy anyway? It is kept as interrupted.", false)? {
                    return Ok(ExitCode::SUCCESS);
                }
            }
            stop(home).await
        }
        _ => Ok(ExitCode::SUCCESS),
    }
}

/// `runtime.json` of the Kelpy running for `home`, waiting up to `STARTING_WAIT` while it starts or is updated;
/// `None` if it is still not there, or if Kelpy stopped meanwhile.
async fn started(home: &Home) -> io::Result<Option<RuntimeInfo>> {
    let deadline = Instant::now() + STARTING_WAIT;
    let mut said = false;
    loop {
        if let Some(info) = read_info(home)? {
            return Ok(Some(info));
        }
        if Instant::now() >= deadline || !is_running(home).unwrap_or(false) {
            return Ok(None);
        }
        if !said {
            println!(
                "Kelpy is starting, or being updated. Waiting for it (Ctrl+C stops waiting)..."
            );
            said = true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// What `kelpy` says of the Kelpy running for `home` at `now` (Unix time): its link, since when, and where.
fn running_facts(info: &RuntimeInfo, home: &Home, now: i64) -> [(&'static str, String); 3] {
    use chrono::TimeZone;
    let clock = chrono::Local
        .timestamp_opt(info.started_at, 0)
        .single()
        .map_or_else(String::new, |at| format!("{}, ", at.format("%H:%M")));
    [
        ("Web UI", info.sign_in_url()),
        (
            "Since",
            format!(
                "{clock}{} · PID {} · v{}",
                running_for(now - info.started_at),
                info.pid,
                info.version
            ),
        ),
        ("Home", home.root().display().to_string()),
    ]
}

/// How long ago something started, `seconds` back, as a person says it.
fn running_for(seconds: i64) -> String {
    let count = |amount: i64, unit: &str| {
        let plural = if amount == 1 { "" } else { "s" };
        format!("{amount} {unit}{plural} ago")
    };
    match seconds {
        ..60 => "just now".to_owned(),
        60..3600 => count(seconds / 60, "minute"),
        3600..86_400 => count(seconds / 3600, "hour"),
        _ => count(seconds / 86_400, "day"),
    }
}

/// The titles of the conversations with an answer being written in the Kelpy described by `info`; none if it
/// cannot be asked.
async fn answering(info: &RuntimeInfo) -> Vec<String> {
    #[derive(Deserialize)]
    struct Listed {
        title: String,
        #[serde(default)]
        running: bool,
    }
    let Ok(client) = Client::new(info.url.clone(), info.token.clone()) else {
        return Vec::new();
    };
    match client.get::<Vec<Listed>>("/api/sessions").await {
        Ok(listed) => listed
            .into_iter()
            .filter(|session| session.running)
            .map(|session| session.title)
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Asks the running Kelpy to stop, through its API with the token in `runtime.json`, and waits until it has.
pub async fn stop(home: &Home) -> Result<ExitCode> {
    if !is_running(home)? {
        println!("Kelpy is not running.");
        return Ok(ExitCode::from(1));
    }
    let info = match read_info(home) {
        Ok(Some(info)) => info,
        Ok(None) => {
            println!("Kelpy is starting. Try again in a moment.");
            return Ok(ExitCode::from(1));
        }
        Err(error) => {
            println!("{}", unreadable(&error));
            return Ok(ExitCode::from(1));
        }
    };
    if let Err(error) = request_stop(&info).await {
        println!(
            "Cannot ask Kelpy to stop ({error:#}). Press Ctrl+C in its window, or end process {}.",
            info.pid
        );
        return Ok(ExitCode::from(1));
    }
    println!("Stopping Kelpy (PID {})...", info.pid);
    // The runtime gives work in progress `SHUTDOWN_GRACE`, then releases the lock. Another process's lock can only
    // be observed by trying to take it, so this polls, within a deadline.
    let deadline = Instant::now() + SHUTDOWN_GRACE + Duration::from_secs(5);
    while is_running(home)? {
        if Instant::now() >= deadline {
            println!(
                "Kelpy has not stopped yet. Check its window, or end process {}.",
                info.pid
            );
            return Ok(ExitCode::from(1));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    println!("Kelpy stopped.");
    Ok(ExitCode::SUCCESS)
}

/// `POST /api/shutdown` over a plain loopback connection: one request does not need an HTTP client.
async fn request_stop(info: &RuntimeInfo) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let authority = info
        .url
        .strip_prefix("http://")
        .context("unexpected URL in runtime.json")?;
    let address: SocketAddr = authority
        .parse()
        .context("unexpected URL in runtime.json")?;
    let exchange = async {
        let mut stream = tokio::net::TcpStream::connect(address).await?;
        let request = format!(
            "POST /api/shutdown HTTP/1.1\r\nHost: {authority}\r\nAuthorization: Bearer {}\r\n\
             Content-Length: 0\r\nConnection: close\r\n\r\n",
            info.token
        );
        stream.write_all(request.as_bytes()).await?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await?;
        io::Result::Ok(response)
    };
    let response = tokio::time::timeout(Duration::from_secs(5), exchange)
        .await
        .context("no answer")??;
    let status = String::from_utf8_lossy(&response)
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    anyhow::ensure!(status == "202", "unexpected answer {status:?}");
    Ok(())
}

/// How long Kelpy waits at start for the SSH agent to unlock the vault: an agent may ask its user to confirm.
const VAULT_UNLOCK_WAIT: Duration = Duration::from_secs(10);

/// The vault's line for the banner, if there is a vault. A locked vault is unlocked first when the SSH agent of the
/// session holds one of its keys.
async fn describe_vault(agent: &Agent) -> Option<String> {
    let state = agent.vault().view();
    if let Some(problem) = state.problem {
        return Some(format!(
            "cannot be read ({problem}): delete it in Secrets in the Web UI"
        ));
    }
    match state.state {
        VaultState::None => None,
        VaultState::Unlocked => Some("unlocked".into()),
        VaultState::Locked => {
            let unlocked = tokio::time::timeout(VAULT_UNLOCK_WAIT, agent.unlock_vault_with_agent())
                .await
                .unwrap_or(false);
            Some(if unlocked {
                "unlocked with the SSH key of this session".into()
            } else {
                "locked (unlock it in Secrets in the Web UI)".into()
            })
        }
    }
}

/// One line for the banner: service, model in brackets, and whether answers can use them right away, in green, or
/// need a sign-in, a key or a choice first, in red.
fn describe_provider(agent: &Agent) -> String {
    let info = agent.provider_info();
    let Some(provider) = &info.configured else {
        return paint(
            false,
            "not set up: choose one in the Web UI or run `kelpy provider`",
        );
    };
    let name = |id: &str| {
        info.services
            .iter()
            .find(|service| service.id == id)
            .map(|service| service.name)
    };
    let service = provider
        .service
        .as_deref()
        .and_then(name)
        .or_else(|| name(provider.kind.id()))
        .unwrap_or(provider.kind.id());
    let (ready, access) = describe_access(agent, &info);
    format!("{service} ({}) - {}", provider.model, paint(ready, &access))
}

/// Whether answers can use the configured provider right away, and why: its sign-in or its key.
fn describe_access(agent: &Agent, info: &ProviderInfo) -> (bool, String) {
    let Some(provider) = &info.configured else {
        return (false, "not set up".to_owned());
    };
    match (info.needs, info.key) {
        (Some(Needed::SignIn), _) => (
            false,
            "not signed in: sign in from the Web UI or run `kelpy provider`".to_owned(),
        ),
        (Some(Needed::Key), _) => (
            false,
            "no API key: enter it in the Web UI or run `kelpy provider`".to_owned(),
        ),
        (None, _) if provider.kind.signs_in() => {
            let status = agent.chatgpt().status();
            let account = status.signed_in.and_then(|account| account.email);
            (
                true,
                format!(
                    "signed in as {} {}",
                    account.as_deref().unwrap_or("your ChatGPT account"),
                    wizard::lifetime(status.kept)
                ),
            )
        }
        (None, Some(KeySource::Environment)) => (
            true,
            match provider.kind.key_variable() {
                Some(variable) => format!("API key from {variable}"),
                None => "API key from the environment".to_owned(),
            },
        ),
        (None, _) => (true, "ready (no API key needed)".to_owned()),
    }
}

/// `text` in green when `ready`, in red otherwise, where the terminal shows colours.
fn paint(ready: bool, text: &str) -> String {
    if !platform::colors() {
        return text.to_owned();
    }
    let color = if ready { 32 } else { 31 };
    format!("\x1b[{color}m{text}\x1b[0m")
}

/// Reads `runtime.json`: `None` while it is missing or incomplete, an error if it cannot be opened.
/// A client of the Kelpy running for `home`, or `None` if none is.
pub fn client(home: &Home) -> Result<Option<Client>> {
    if !is_running(home)? {
        return Ok(None);
    }
    match read_info(home) {
        Ok(Some(info)) => Ok(Some(Client::new(info.url, info.token)?)),
        Ok(None) => anyhow::bail!("Kelpy is starting. Try again in a moment."),
        Err(error) => anyhow::bail!("{}", unreadable(&error)),
    }
}

fn read_info(home: &Home) -> io::Result<Option<RuntimeInfo>> {
    match fs::read(home.data_dir().join(INFO_FILE)) {
        Ok(text) => Ok(serde_json::from_slice(&text).ok()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn unreadable(error: &io::Error) -> String {
    if error.kind() == io::ErrorKind::PermissionDenied {
        "Its details are readable only by the user who started it.".to_owned()
    } else {
        format!("Cannot read its details: {error}.")
    }
}

/// The SSH command that forwards the Web UI to the admin's computer, on the same port, so that the link in the
/// banner works there as it is. `connection` is `SSH_CONNECTION`: `<client ip> <client port> <server ip> <server port>`.
fn ssh_tunnel_command(connection: &str, user: Option<&str>, address: SocketAddr) -> Option<String> {
    let mut fields = connection.split_whitespace().skip(2);
    let server: IpAddr = fields.next()?.parse().ok()?;
    let ssh_port: u16 = fields.next()?.parse().ok()?;
    let port = address.port();
    let target = match address.ip() {
        IpAddr::V6(ip) => format!("[{ip}]"),
        ip => ip.to_string(),
    };
    let port_option = if ssh_port == 22 {
        String::new()
    } else {
        format!("-p {ssh_port} ")
    };
    let destination = match user {
        Some(user) => format!("{user}@{server}"),
        None => server.to_string(),
    };
    Some(format!(
        "ssh -N -L {port}:{target}:{port} {port_option}{destination}"
    ))
}

/// What the terminal UI's start screen tells of the Kelpy running: `version`, the Web UI's `address`, `notes`.
fn banner(version: &str, address: String, notes: Vec<String>) -> kelpy_tui::Banner {
    kelpy_tui::Banner {
        version: version.to_owned(),
        web_ui: address,
        server: kelpy_common::host_name(),
        system: kelpy_common::system_name(),
        notes,
    }
}

fn login_name() -> Option<String> {
    std::env::var(if cfg!(windows) { "USERNAME" } else { "USER" })
        .ok()
        .filter(|name| !name.is_empty())
}

/// What opens a link in this computer's browser, for the terminal UI's Ctrl+O. None over SSH, where it would open
/// on the server; and on a Linux without a desktop, where the browser would be one in the terminal, or none.
fn browser() -> Option<fn(&str) -> std::io::Result<()>> {
    let set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
    let desktop = cfg!(any(windows, target_os = "macos"))
        || ["DISPLAY", "WAYLAND_DISPLAY", "WSL_DISTRO_NAME"]
            .into_iter()
            .any(set);
    (desktop && platform::ssh_connection().is_none())
        .then_some(webbrowser::open as fn(&str) -> std::io::Result<()>)
}

fn open_url(url: &str) {
    if let Err(error) = webbrowser::open(url) {
        println!("Could not open the browser ({error}). Open {url} manually.");
    }
}

/// Completes when the runtime must stop, after saying why.
async fn shutdown_signal(
    limits: Limits,
    activity: Arc<Activity>,
    stop_request: Arc<StopRequest>,
) -> String {
    let time_limit = async {
        match limits.after {
            Some(limit) => tokio::time::sleep(limit).await,
            None => std::future::pending().await,
        }
    };
    let idle_limit = async {
        match limits.idle {
            Some(limit) => wait_until_idle(&activity, limit).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        reason = stop_requested() => reason.to_owned(),
        from = stop_request.requested() => format!("requested from {from}"),
        () = time_limit => "time limit reached".to_owned(),
        () = idle_limit => "no activity".to_owned(),
    }
}

async fn wait_until_idle(activity: &Activity, limit: Duration) {
    loop {
        let idle = activity.idle_for();
        if idle >= limit {
            return;
        }
        // While busy, `idle` is zero: check again after a full period at most.
        tokio::time::sleep(limit - idle).await;
    }
}

/// Ctrl+C, and the end of the terminal session: SIGHUP/SIGTERM on Unix (an SSH session that closes or expires),
/// a closed console window on Windows.
async fn stop_requested() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut hangup = signal(SignalKind::hangup()).ok();
        let mut terminate = signal(SignalKind::terminate()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => "interrupted",
            Some(()) = async { hangup.as_mut()?.recv().await } => "terminal session ended",
            Some(()) = async { terminate.as_mut()?.recv().await } => "terminated",
        }
    }
    #[cfg(windows)]
    {
        let mut close = tokio::signal::windows::ctrl_close().ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => "interrupted",
            Some(()) = async { close.as_mut()?.recv().await } => "console window closed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_memory_chosen_from_an_interface_is_used_only_once_recorded() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join("config.toml");
        fs::write(&config, "[memory]\nuse = \"shared\"\n").unwrap();

        // Recorded first, then used.
        assert_eq!(
            record_memory(&config, MemoryUse::Shared, Some(MemoryUse::Private)),
            (MemoryUse::Private, None)
        );
        let recorded = Config::load(&config).unwrap();
        assert_eq!(recorded.memory.chosen(), MemoryUse::Private);
        // Nothing asked: the one recorded.
        assert_eq!(
            record_memory(&config, MemoryUse::Private, None),
            (MemoryUse::Private, None)
        );
        // Not recorded: the one recorded before goes on, and the interfaces say why.
        let unwritable = home.path().join("missing").join("config.toml");
        let (using, problem) =
            record_memory(&unwritable, MemoryUse::Shared, Some(MemoryUse::Private));
        assert_eq!(using, MemoryUse::Shared);
        assert!(
            problem
                .unwrap()
                .starts_with("the private memory could not be recorded as the one chosen")
        );
    }

    #[test]
    fn how_long_ago_is_said_as_a_person_says_it() {
        assert_eq!(running_for(20), "just now");
        assert_eq!(running_for(60), "1 minute ago");
        assert_eq!(running_for(7_300), "2 hours ago");
        assert_eq!(running_for(86_400), "1 day ago");
    }

    #[test]
    fn a_kelpy_already_running_is_told_by_its_link_since_when_and_its_home() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::init(dir.path()).unwrap();
        let info = RuntimeInfo {
            pid: 18244,
            port: 54321,
            url: "http://127.0.0.1:54321".into(),
            version: "0.4.1".into(),
            started_at: 1_000_000,
            stops_at: None,
            idle_limit: None,
            access_code: "K7QM-4ZP2".into(),
            token: "token".into(),
        };
        let [link, since, place] = running_facts(&info, &home, 1_000_000 + 7_300);
        assert_eq!(
            link,
            (
                "Web UI",
                "http://127.0.0.1:54321/#code=K7QM-4ZP2".to_owned()
            )
        );
        assert_eq!(since.0, "Since");
        assert!(
            since.1.ends_with("2 hours ago · PID 18244 · v0.4.1"),
            "{}",
            since.1
        );
        assert_eq!(place, ("Home", home.root().display().to_string()));
    }

    #[test]
    fn the_tunnel_command_uses_the_server_address_and_the_same_port() {
        let local: SocketAddr = "127.0.0.1:21122".parse().unwrap();
        assert_eq!(
            ssh_tunnel_command("198.51.100.7 51234 203.0.113.10 22", Some("root"), local)
                .as_deref(),
            Some("ssh -N -L 21122:127.0.0.1:21122 root@203.0.113.10")
        );
        let local: SocketAddr = "[::1]:4000".parse().unwrap();
        assert_eq!(
            ssh_tunnel_command("2001:db8::2 51234 2001:db8::1 2222", None, local).as_deref(),
            Some("ssh -N -L 4000:[::1]:4000 -p 2222 2001:db8::1")
        );
        assert_eq!(ssh_tunnel_command("garbage", Some("root"), local), None);
    }

    #[test]
    fn the_banner_says_whether_answers_can_use_the_provider() {
        use kelpy_core::config::ProviderConfig;
        use kelpy_core::providers::Kind;

        let dir = tempfile::tempdir().unwrap();
        let home = Home::init(dir.path()).unwrap();
        let agent = |kind, service: &str| {
            let provider = ProviderConfig {
                kind,
                model: "model".into(),
                base_url: None,
                service: Some(service.into()),
            };
            Agent::new(
                &home,
                Store::in_memory().unwrap(),
                Some(provider),
                Endpoints::default(),
            )
            .unwrap()
        };

        let chatgpt = agent(Kind::ChatGpt, "chatgpt");
        let (ready, access) = describe_access(&chatgpt, &chatgpt.provider_info());
        assert!(!ready);
        assert!(access.starts_with("not signed in"), "{access}");

        let ollama = agent(Kind::Ollama, "ollama");
        assert_eq!(
            describe_access(&ollama, &ollama.provider_info()),
            (true, "ready (no API key needed)".to_owned())
        );
    }
}
