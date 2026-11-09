//! Kelpy's remote access: an SSH server inside Kelpy, for the administrator to reach it from outside without an SSH
//! server of the system.
//!
//! What it allows is narrower than SSH: a key, never a password; once the key is proven, the connection waits for the
//! administrator's approval in an interface already signed in ([`Connections`]), as approving a key is not
//! approving whoever holds it today. Allowed, a session gets Kelpy's terminal UI and nothing else (no shell, no
//! command), SFTP into the folder of the files uploaded to Kelpy, and forwarding to the Web UI's port only.
//!
//! Every task of the server is tracked, down to what each session starts, and ends with [`Remote::stop`]: each
//! session is told it ends, then its socket is closed under it if it does not, within a deadline.

mod sftp;

use std::collections::HashMap;
use std::io::{self, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use kelpy_common::lock;
use kelpy_core::connections::{APPROVAL_WAIT, Connections, Open, Request};
use kelpy_core::files::Files;
use kelpy_core::private_file;
use kelpy_tui::{Api, Background, Banner, Options, RemoteInput, RemoteTerminal, Screen};
use russh::keys::ssh_key::LineEnding;
use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::{HashAlg, PrivateKey};
use russh::server::{Auth, ChannelOpenHandle, Config, Handler, Msg, Session, VerifiedPublicKey};
use russh::{Channel, ChannelId, ChannelOpenFailure, Disconnect, MethodKind, MethodSet};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc, watch};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// How many sessions may be allowed at once.
const MAX_SESSIONS: usize = 8;
/// How many channels a session may have open at once, each a terminal UI, an SFTP session or a forward: with
/// [`MAX_SESSIONS`], what the whole server runs is bounded.
const MAX_CHANNELS: usize = 8;
/// How many connections may be not yet allowed at once, apart from the sessions: a client that never logs in
/// cannot keep an allowed one out.
const MAX_WAITING: usize = 16;
/// How long a client has to send its identification, the first thing it sends.
const HELLO_WAIT: Duration = if cfg!(test) {
    Duration::from_millis(300)
} else {
    Duration::from_secs(10)
};
/// How long a connection has to prove its key and be allowed: the administrator's time to decide, and a margin.
const LOGIN_WAIT: Duration = APPROVAL_WAIT.saturating_add(Duration::from_secs(30));
/// How long a session told to end gets before its socket is closed under it.
const CLOSE_WAIT: Duration = Duration::from_secs(1);
/// How many key presses may wait for the terminal UI: more are dropped, as a terminal cannot type that fast.
const MAX_INPUT: usize = 256;
/// The largest terminal drawn, in columns and rows: wider than any screen, and the terminal UI's buffers stay small.
const MAX_COLUMNS: u16 = 512;
const MAX_ROWS: u16 = 256;
/// How much drawn output may wait for a client that does not read it: past this, the terminal UI ends.
const MAX_BEHIND: usize = 8 << 20;
/// A session doing nothing for this long is closed.
const IDLE: Duration = Duration::from_secs(60 * 60);
/// What a client is told before its key, so that whoever connects knows someone must allow it.
const BANNER: &str = "Kelpy: once your key is checked, this connection waits until the administrator allows it in \
                      the terminal or the Web UI where Kelpy runs, for at most 2 minutes.\r\n";

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error(transparent)]
    Ssh(#[from] russh::Error),
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("the SSH host key in {0} cannot be read: {1}")]
    HostKey(PathBuf, String),
}

/// What the SSH server serves.
pub struct RemoteOptions {
    /// The port to listen on, on every address; 0 lets the system choose.
    pub port: u16,
    /// Where the server's own key is kept, created at the first start.
    pub host_key: PathBuf,
    /// The files uploaded to Kelpy, which SFTP reaches.
    pub files: Arc<Files>,
    /// The Web UI's port on loopback, the only one forwarding reaches.
    pub web_port: u16,
    /// The runtime's API, for the terminal UI of each session.
    pub api_url: String,
    pub token: String,
    /// What the terminal UI's start screen tells.
    pub banner: Banner,
    pub connections: Arc<Connections>,
}

/// The SSH server, running until [stopped](Remote::stop) or dropped.
pub struct Remote {
    pub port: u16,
    /// The host key's fingerprint, `SHA256:…`, for the administrator to check at the first connection.
    pub fingerprint: String,
    server: Arc<Server>,
}

/// What every connection shares.
struct Server {
    options: RemoteOptions,
    sessions: Arc<Semaphore>,
    /// Cancelled when the server stops: the listener, then every session and what it started.
    stop: CancellationToken,
    /// Every task of the server, its sessions and what they start.
    tasks: TaskTracker,
}

impl Remote {
    /// Stops the server: no new connection, and every session told it ends, then closed. Returns whether all of
    /// it ended within `grace`.
    pub async fn stop(self, grace: Duration) -> bool {
        self.server.stop.cancel();
        self.server.tasks.close();
        tokio::time::timeout(grace, self.server.tasks.wait())
            .await
            .is_ok()
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        // Without waiting: the tasks end on their own once told to.
        self.server.stop.cancel();
    }
}

/// Starts the SSH server.
pub async fn start(options: RemoteOptions) -> Result<Remote, RemoteError> {
    let key = host_key(&options.host_key)?;
    let fingerprint = key.public_key().fingerprint(HashAlg::Sha256).to_string();
    let config = Arc::new(Config {
        methods: MethodSet::from(&[MethodKind::PublicKey][..]),
        keys: vec![key],
        auth_rejection_time: Duration::from_secs(1),
        auth_rejection_time_initial: Some(Duration::ZERO),
        inactivity_timeout: Some(IDLE),
        keepalive_interval: Some(Duration::from_secs(30)),
        max_auth_attempts: 6,
        ..Config::default()
    });
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, options.port)).await?;
    let port = listener.local_addr()?.port();
    let server = Arc::new(Server {
        options,
        sessions: Arc::new(Semaphore::new(MAX_SESSIONS)),
        stop: CancellationToken::new(),
        tasks: TaskTracker::new(),
    });
    server
        .tasks
        .spawn(serve(listener, config, Arc::clone(&server)));
    Ok(Remote {
        port,
        fingerprint,
        server,
    })
}

/// Accepts connections until the server stops.
async fn serve(listener: TcpListener, config: Arc<Config>, server: Arc<Server>) {
    let waiting = Arc::new(Semaphore::new(MAX_WAITING));
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            () = server.stop.cancelled() => return,
        };
        let Ok((stream, address)) = accepted else {
            continue;
        };
        // Over the limit, the connection is closed at once.
        let Ok(slot) = Arc::clone(&waiting).try_acquire_owned() else {
            continue;
        };
        server.tasks.spawn(session(
            stream,
            address,
            Arc::clone(&config),
            Arc::clone(&server),
            slot,
        ));
    }
}

/// One connection, from its first byte to its end: within [`HELLO_WAIT`] and [`LOGIN_WAIT`] until allowed, then
/// until it ends or the server stops.
async fn session(
    stream: TcpStream,
    address: SocketAddr,
    config: Arc<Config>,
    server: Arc<Server>,
    waiting: OwnedSemaphorePermit,
) {
    let _ = stream.set_nodelay(true);
    // A second handle on the socket, to close it under the session when it does not end when told.
    let Ok((stream, socket)) = second_handle(stream) else {
        return;
    };
    let (allowed, mut allowing) = watch::channel(false);
    let connection = Connection {
        server: Arc::clone(&server),
        address,
        waiting: Some(waiting),
        slot: None,
        allowed,
        open: None,
        refused: false,
        running: HashMap::new(),
        channels: HashMap::new(),
        sizes: HashMap::new(),
        inputs: HashMap::new(),
    };
    let started = tokio::select! {
        started = tokio::time::timeout(HELLO_WAIT, russh::server::run_stream(config, stream, connection)) => started,
        () = server.stop.cancelled() => return,
    };
    // A session that fails, such as a client that goes away, only ends itself.
    let Ok(Ok(mut running)) = started else {
        return;
    };
    let login = tokio::time::sleep(LOGIN_WAIT);
    tokio::pin!(login);
    let mut logging_in = true;
    loop {
        tokio::select! {
            _ = &mut running => return,
            () = server.stop.cancelled() => break,
            allowed = allowing.wait_for(|allowed| *allowed), if logging_in => {
                // Not allowed only when the connection already ended: `running` follows.
                logging_in = false;
                drop(allowed);
            }
            () = &mut login, if logging_in => break,
        }
    }
    let handle = running.handle();
    let told = handle.disconnect(
        Disconnect::ByApplication,
        "Kelpy closed this connection".into(),
        "en".into(),
    );
    if tokio::time::timeout(CLOSE_WAIT, async {
        let _ = told.await;
        let _ = (&mut running).await;
    })
    .await
    .is_err()
    {
        // Best effort: the socket may be closed already.
        let _ = socket.shutdown(Shutdown::Both);
        let _ = tokio::time::timeout(CLOSE_WAIT, running).await;
    }
}

/// `stream`, and a second handle on its socket.
fn second_handle(stream: TcpStream) -> io::Result<(TcpStream, std::net::TcpStream)> {
    let stream = stream.into_std()?;
    let second = stream.try_clone()?;
    Ok((TcpStream::from_std(stream)?, second))
}

/// The server's key: read from `path`, or made at the first start and kept there, readable by Kelpy's account only.
fn host_key(path: &Path) -> Result<PrivateKey, RemoteError> {
    match std::fs::read_to_string(path) {
        Ok(pem) => PrivateKey::from_openssh(pem)
            .map_err(|error| RemoteError::HostKey(path.to_owned(), error.to_string())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut seed = [0; 32];
            getrandom::fill(&mut seed)
                .map_err(|error| RemoteError::Io(io::Error::other(error.to_string())))?;
            let mut key = PrivateKey::from(Ed25519Keypair::from_seed(&seed));
            key.set_comment("kelpy");
            let pem = key
                .to_openssh(LineEnding::LF)
                .map_err(|error| RemoteError::HostKey(path.to_owned(), error.to_string()))?;
            private_file::write(path, pem.as_bytes())?;
            Ok(key)
        }
        Err(error) => Err(RemoteError::Io(error)),
    }
}

/// One SSH connection.
struct Connection {
    server: Arc<Server>,
    address: SocketAddr,
    /// Its place among the connections not yet allowed, until allowed.
    waiting: Option<OwnedSemaphorePermit>,
    /// Its place among the sessions, from the approval's request until the connection ends.
    slot: Option<OwnedSemaphorePermit>,
    /// Set once allowed: the login's deadline no longer applies.
    allowed: watch::Sender<bool>,
    /// Set once allowed, until the connection ends.
    open: Option<Open>,
    /// Refused once: no other key of the same connection is asked for.
    refused: bool,
    /// The channels open, at most [`MAX_CHANNELS`], each with whether something runs on it: one thing per channel.
    running: HashMap<ChannelId, bool>,
    /// The session channels not yet given to SFTP.
    channels: HashMap<ChannelId, Channel<Msg>>,
    /// The terminals asked for: their size in columns and rows, and their name (`TERM`).
    sizes: HashMap<ChannelId, ((u16, u16), String)>,
    /// Where each terminal UI reads its keys.
    inputs: HashMap<ChannelId, mpsc::Sender<RemoteInput>>,
}

impl Handler for Connection {
    type Error = RemoteError;

    async fn auth_none(&mut self, _user: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Reject {
            proceed_with_methods: Some(MethodSet::from(&[MethodKind::PublicKey][..])),
            partial_success: false,
        })
    }

    async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::reject())
    }

    async fn authentication_banner(&mut self) -> Result<Option<String>, Self::Error> {
        Ok(Some(BANNER.to_owned()))
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        key: VerifiedPublicKey<'_>,
    ) -> Result<Auth, Self::Error> {
        if self.refused {
            return Ok(Auth::reject());
        }
        // The administrator is not asked about a connection there would be no room for.
        let Ok(slot) = Arc::clone(&self.server.sessions).try_acquire_owned() else {
            self.refused = true;
            return Ok(Auth::reject());
        };
        let public = key.public_key;
        let fingerprint = public.fingerprint(HashAlg::Sha256).to_string();
        let comment = public.comment().to_string();
        let label = if comment.is_empty() {
            format!("{} {fingerprint}", public.algorithm().as_str())
        } else {
            format!("{} {fingerprint} ({comment})", public.algorithm().as_str())
        };
        let request = Request {
            address: self.address.to_string(),
            user: user.to_owned(),
            key: label,
            fingerprint,
        };
        let open = tokio::select! {
            open = self.server.options.connections.ask(request) => open,
            () = self.server.stop.cancelled() => None,
        };
        match open {
            Some(open) => {
                self.open = Some(open);
                self.slot = Some(slot);
                self.waiting = None;
                self.allowed.send_replace(true);
                Ok(Auth::Accept)
            }
            None => {
                self.refused = true;
                Ok(Auth::reject())
            }
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.open.is_none() || self.server.stop.is_cancelled() {
            reply
                .reject(ChannelOpenFailure::AdministrativelyProhibited)
                .await;
            return Ok(());
        }
        if !self.admit(channel.id()) {
            reply.reject(ChannelOpenFailure::ResourceShortage).await;
            return Ok(());
        }
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        term: &str,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        // Bounded as any name: the terminal UI only looks for a few words in it.
        let name = term.chars().take(64).collect();
        self.sizes
            .insert(channel, (size(col_width, row_height), name));
        session.channel_success(channel)?;
        Ok(())
    }

    async fn window_change_request(
        &mut self,
        channel: ChannelId,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let (columns, rows) = size(col_width, row_height);
        if let Some(input) = self.inputs.get(&channel) {
            let _ = input.try_send(RemoteInput::Resize(columns, rows));
        }
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if !self.start(channel) {
            session.channel_failure(channel)?;
            return Ok(());
        }
        let Some((size, name)) = self.sizes.get(&channel).cloned() else {
            session.channel_failure(channel)?;
            session.data(
                channel,
                "Kelpy needs a terminal: connect with ssh -t.\r\n".to_owned(),
            )?;
            session.close(channel)?;
            return Ok(());
        };
        session.channel_success(channel)?;
        let (keys, typed) = mpsc::channel(MAX_INPUT);
        self.inputs.insert(channel, keys);
        let terminal = RemoteTerminal {
            columns: size.0,
            rows: size.1,
            name,
        };
        self.server.tasks.spawn(terminal_ui(
            session.handle(),
            channel,
            terminal,
            typed,
            Arc::clone(&self.server),
        ));
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        _data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel)?;
        session.extended_data(
            channel,
            1,
            "Kelpy runs no commands over SSH: connect without one for its terminal UI, or use SFTP.\r\n".to_owned(),
        )?;
        session.close(channel)?;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if name != "sftp" || !self.start(channel) {
            session.channel_failure(channel)?;
            return Ok(());
        }
        let Some(opened) = self.channels.remove(&channel) else {
            session.channel_failure(channel)?;
            return Ok(());
        };
        session.channel_success(channel)?;
        // It runs in a task of its own, which ends when the channel does; the handler's token tracks it.
        let handler = sftp::Sftp::new(
            Arc::clone(&self.server.options.files),
            self.server.tasks.token(),
        );
        russh_sftp::server::run(opened.into_stream(), handler).await;
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(input) = self.inputs.get(&channel) {
            let _ = input.try_send(RemoteInput::Bytes(data.to_vec()));
        }
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        // The terminal UI's keys end: it ends too.
        self.inputs.remove(&channel);
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.running.remove(&channel);
        self.inputs.remove(&channel);
        self.channels.remove(&channel);
        self.sizes.remove(&channel);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host_to_connect: &str,
        port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let loopback = matches!(host_to_connect, "127.0.0.1" | "localhost" | "::1");
        let web_port = self.server.options.web_port;
        if self.open.is_none()
            || self.server.stop.is_cancelled()
            || !loopback
            || port_to_connect != u32::from(web_port)
        {
            reply
                .reject(ChannelOpenFailure::AdministrativelyProhibited)
                .await;
            return Ok(());
        }
        let id = channel.id();
        if !self.admit(id) {
            reply.reject(ChannelOpenFailure::ResourceShortage).await;
            return Ok(());
        }
        let web = match TcpStream::connect((Ipv4Addr::LOCALHOST, web_port)).await {
            Ok(web) => web,
            Err(_) => {
                self.running.remove(&id);
                reply.reject(ChannelOpenFailure::ConnectFailed).await;
                return Ok(());
            }
        };
        self.start(id);
        reply.accept().await;
        let stop = self.server.stop.clone();
        self.server.tasks.spawn(async move {
            let (mut forwarded, mut web) = (channel.into_stream(), web);
            // Ends when either side closes, or the server stops.
            tokio::select! {
                _ = tokio::io::copy_bidirectional(&mut forwarded, &mut web) => {}
                () = stop.cancelled() => {}
            }
        });
        Ok(())
    }
}

impl Connection {
    /// Counts the channel `id` as open, unless the session has [`MAX_CHANNELS`] already.
    fn admit(&mut self, id: ChannelId) -> bool {
        if self.running.len() >= MAX_CHANNELS {
            return false;
        }
        self.running.insert(id, false);
        true
    }

    /// Marks something as running on `channel`, unless something already does or it is not open.
    fn start(&mut self, channel: ChannelId) -> bool {
        match self.running.get_mut(&channel) {
            Some(running) if !*running => {
                *running = true;
                true
            }
            _ => false,
        }
    }
}

/// A terminal size from SSH's, within [`MAX_COLUMNS`] and [`MAX_ROWS`] and at least one cell.
fn size(columns: u32, rows: u32) -> (u16, u16) {
    let fit = |value: u32, max: u16| u16::try_from(value).unwrap_or(max).clamp(1, max);
    (fit(columns, MAX_COLUMNS), fit(rows, MAX_ROWS))
}

/// Runs the terminal UI on `channel` until it ends or the server stops, then closes the channel.
async fn terminal_ui(
    handle: russh::server::Handle,
    channel: ChannelId,
    terminal: RemoteTerminal,
    typed: mpsc::Receiver<RemoteInput>,
    server: Arc<Server>,
) {
    let output = Arc::new(Output::default());
    let drawing = async {
        let input = futures_util::stream::unfold(typed, |mut typed| async move {
            typed.recv().await.map(|input| (input, typed))
        })
        .boxed();
        let Ok(api) = Api::new(server.options.api_url.clone(), server.options.token.clone()) else {
            return;
        };
        let options = Options {
            banner: server.options.banner.clone(),
            stops_runtime: false,
            // Inline: what is said stays in the history of the terminal that connects.
            screen: Screen::Auto,
            background: Background::Ask,
        };
        let frames = Frames {
            output: Arc::clone(&output),
            pending: Vec::new(),
        };
        // It ends on its own when the keys end, the client stops reading or Kelpy stops.
        let _ = kelpy_tui::run_remote(frames, input, terminal, api, options).await;
    };
    let ran = tokio::select! {
        ((), ()) = async { tokio::join!(drawing, send(&handle, channel, &output)) } => true,
        () = server.stop.cancelled() => false,
    };
    // Stopping, the whole connection closes.
    if ran {
        let _ = handle.exit_status_request(channel, 0).await;
        let _ = handle.eof(channel).await;
        let _ = handle.close(channel).await;
    }
}

/// What the terminal UI drew and the channel did not take yet. Frames drawn while the client is slow join it, so
/// that they cost their bytes, not a message each, up to [`MAX_BEHIND`].
#[derive(Default)]
struct Output {
    backlog: Mutex<Backlog>,
    written: Notify,
}

#[derive(Default)]
struct Backlog {
    bytes: Vec<u8>,
    /// No more output: the terminal UI ended, or the channel did.
    closed: bool,
}

/// Sends what the terminal UI draws to `channel`, until it ends or the channel does.
async fn send(handle: &russh::server::Handle, channel: ChannelId, output: &Output) {
    loop {
        output.written.notified().await;
        let (bytes, closed) = {
            let mut backlog = lock(&output.backlog);
            (std::mem::take(&mut backlog.bytes), backlog.closed)
        };
        if !bytes.is_empty() && handle.data(channel, bytes).await.is_err() {
            lock(&output.backlog).closed = true;
            return;
        }
        if closed {
            return;
        }
    }
}

/// What the terminal UI writes, handed to [`Output`] at each flush.
struct Frames {
    output: Arc<Output>,
    pending: Vec<u8>,
}

impl Write for Frames {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.pending.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let mut backlog = lock(&self.output.backlog);
        if backlog.closed {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        if backlog.bytes.len() + self.pending.len() > MAX_BEHIND {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the client does not read what the terminal UI draws",
            ));
        }
        backlog.bytes.append(&mut self.pending);
        drop(backlog);
        self.output.written.notify_one();
        Ok(())
    }
}

impl Drop for Frames {
    fn drop(&mut self) {
        // Best effort: the last frame, unless the channel is gone.
        let _ = self.flush();
        lock(&self.output.backlog).closed = true;
        self.output.written.notify_one();
    }
}

#[cfg(test)]
mod tests;
