//! The local runtime: at most one per home, guarded by a lock file and announced through `runtime.json`.
//!
//! `runtime.json` also holds the runtime's credentials (the browser access code and the token for local clients),
//! so only the user who started Kelpy can open it.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use kelpy_common::unix_now;
use kelpy_core::VERSION;
use kelpy_core::agent::{Agent, KeySource, Needed, ProviderInfo};
use kelpy_core::config::Config;
use kelpy_core::home::{self, Home};
use kelpy_core::private_file;
use kelpy_core::providers::chatgpt::Endpoints;
use kelpy_core::storage::Store;
use kelpy_gateway::{Activity, Auth, RuntimeStatus, StopRequest};
use serde::{Deserialize, Serialize};

use crate::client::Client;
use crate::{duration, platform, prompt, wizard};

const LOCK_FILE: &str = "runtime.lock";
const INFO_FILE: &str = "runtime.json";
const DATABASE_FILE: &str = "kelpy.db";
/// The port of the last start, tried first when `config.toml` leaves the choice to the operating system.
const LAST_PORT_FILE: &str = "last-port";
/// After a stop is triggered, how long open connections and answers being written may take to finish.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

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

/// Runs the runtime and the Web server in the foreground until Ctrl+C, the end of the terminal session, or one
/// of the `limits`. In every case new work is refused at once, and work in progress gets `SHUTDOWN_GRACE` to finish.
pub async fn run_foreground(home: &Home, open_browser: bool, limits: Limits) -> Result<ExitCode> {
    platform::remove_leftovers(&home.root().join(home::executable_name()), &home.data_dir());
    let config = Config::load(&home.config_path())?;

    let Some(lock) = RuntimeLock::try_acquire(home)? else {
        match read_info(home) {
            Ok(Some(info)) => {
                println!("Kelpy is already running: {}", info.url);
                if open_browser {
                    open_url(&info.sign_in_url());
                }
            }
            // The lock without runtime.json: a runtime still starting, or an update in progress.
            Ok(None) => println!("Kelpy is already running or being updated."),
            Err(error) => println!("Kelpy is already running. {}", unreadable(&error)),
        }
        return Ok(ExitCode::SUCCESS);
    };

    // The data folder holds the conversations and the runtime's credentials.
    if let Err(error) = private_file::restrict_dir(&home.data_dir()) {
        println!(
            "Warning: cannot restrict {} to the current user ({error}). Other users may read the conversations.",
            home.data_dir().display()
        );
    }
    let store = Store::open(&home.data_dir().join(DATABASE_FILE))
        .context("cannot open the conversation database")?;
    let agent = Arc::new(Agent::new(
        home,
        store,
        config.provider.clone(),
        Endpoints::default(),
    )?);
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
    let listener = kelpy_gateway::bind(config.server.host, config.server.port, last_port)
        .await
        .context("cannot start the local server")?;
    let address = listener.local_addr()?;
    // Best effort: without it, the next start only gets a port of the operating system's choice.
    let _ = fs::write(&last_port_path, address.port().to_string());
    let auth = Arc::new(Auth::new().context("cannot create the access code")?);
    let started_at = unix_now();
    let info = RuntimeInfo {
        pid: std::process::id(),
        port: address.port(),
        url: format!("http://{address}"),
        version: VERSION.to_owned(),
        started_at,
        stops_at: limits.after.map(|limit| {
            started_at.saturating_add(i64::try_from(limit.as_secs()).unwrap_or(i64::MAX))
        }),
        idle_limit: limits.idle.map(|limit| limit.as_secs()),
        access_code: auth.access_code(),
        token: auth.token().to_owned(),
    };
    lock.announce(&info)?;

    println!("Kelpy v{VERSION}");
    println!();
    println!("Agent runtime started.");
    println!("Web UI:   {}", info.sign_in_url());
    println!(
        "Code:     {}  (to sign in from another browser)",
        info.access_code
    );
    println!("Home:     {}", home.root().display());
    println!("Provider: {}", describe_provider(&agent));
    println!("Account:  {}", agent.account().label());
    if let Some(limits) = agent.account().limits() {
        println!("          {limits}");
    }
    if let Some(last) = last_port.filter(|&last| last != address.port()) {
        println!(
            "Port {last} of the last start was not available: open the new link, and set up an SSH tunnel again."
        );
    }
    println!();
    if let Some(command) = platform::ssh_connection()
        .and_then(|connection| ssh_tunnel_command(&connection, login_name().as_deref(), address))
    {
        println!(
            "You are connected over SSH. To use the Web UI from your own computer, run there:"
        );
        println!("  {command}");
        println!("then open the Web UI link above.");
        println!();
    }
    if let Some(limit) = limits.after {
        println!("Stops automatically in {}.", duration::format(limit));
    }
    if let Some(limit) = limits.idle {
        println!(
            "Stops automatically after {} without activity.",
            duration::format(limit)
        );
    }
    println!("Press Ctrl+C to stop.");
    if open_browser {
        open_url(&info.sign_in_url());
    }
    if config.provider.is_none() && prompt::interactive() {
        let client = Client::new(info.url.clone(), info.token.clone())?;
        // A thread of its own, not a task: the questions block on the terminal, and an unanswered one must not keep
        // Kelpy from exiting.
        thread::spawn(move || {
            if let Err(error) = wizard::offer(client) {
                println!("Setup not completed: {error:#}");
            }
        });
    }

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
        auth,
        Arc::clone(&agent),
        Arc::clone(&stop_request),
    );
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = kelpy_gateway::serve(listener, router, async {
        // Either a stop was sent or the sender is gone with the runtime: both mean stop.
        let _ = stopped.await;
    });
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result.context("the local server failed")?,
        () = shutdown_signal(limits, activity, stop_request) => {
            // Access ends at once: no new answers, then no new connections or requests. Answers being written
            // are stopped and saved as interrupted; open connections, such as answer streams, get a bounded time
            // to close.
            let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
            let answers_saved = agent.stop_all(SHUTDOWN_GRACE);
            // Fails only if the server already ended on its own, which is just as stopped.
            let _ = stop.send(());
            if !answers_saved.await {
                println!("Some answers could not be saved in time.");
            }
            // Access ends with the session: the ChatGPT tokens are revoked within the same grace, unless the user
            // chose to save the sign-in on this computer.
            if !agent.chatgpt().close(deadline).await {
                println!(
                    "OpenAI did not confirm that the ChatGPT session was revoked; it ends on its own within 30 days."
                );
            }
            match tokio::time::timeout_at(deadline, &mut server).await {
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
        }
    }

    drop(lock);
    println!("Kelpy stopped.");
    Ok(ExitCode::SUCCESS)
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

fn login_name() -> Option<String> {
    std::env::var(if cfg!(windows) { "USERNAME" } else { "USER" })
        .ok()
        .filter(|name| !name.is_empty())
}

fn open_url(url: &str) {
    if let Err(error) = webbrowser::open(url) {
        println!("Could not open the browser ({error}). Open {url} manually.");
    }
}

/// Completes when the runtime must stop, after saying why.
async fn shutdown_signal(limits: Limits, activity: Arc<Activity>, stop_request: Arc<StopRequest>) {
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
    let reason = tokio::select! {
        reason = stop_requested() => reason.to_owned(),
        from = stop_request.requested() => format!("requested from {from}"),
        () = time_limit => "time limit reached".to_owned(),
        () = idle_limit => "no activity".to_owned(),
    };
    println!();
    println!("Stopping Kelpy: {reason}.");
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
