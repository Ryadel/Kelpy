//! The channel between a Kelpy and the one restarted in its place as administrator, and what goes over it.
//!
//! An elevation through User Account Control or `sudo` inherits no handle, so the Kelpy before opens a channel only
//! the new one may use: on Windows a named pipe with a random name, one instance, local clients only, where only the
//! process the elevation started (by its PID) is answered; elsewhere a Unix socket in a folder only the user can
//! open, where only a process of root's is answered. The new Kelpy checks that the channel is the old one's, by its
//! PID. Nothing secret is on the command line: only the channel's name and the old Kelpy's PID.
//!
//! The exchange: the new Kelpy says hello, with its version and PID; the old one, once the answers being written
//! have ended, stops and lets go of the runtime's lock, then sends what it held in memory; the new one starts with
//! it and says it is ready, or why it failed. Every step has a deadline, connecting included: a Kelpy whose other
//! end stops answering gives up rather than wait.

use std::collections::BTreeMap;
use std::future::Future;
use std::io;

use kelpy_core::agent::AgentState;
use kelpy_gateway::AuthState;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::Instant;

/// The largest message, in bytes: the state with every secret is far smaller.
const MAX_MESSAGE: u32 = 64 << 20;

/// What a Kelpy hands over to the one restarted in its place: what it held only in memory, and how it was started.
#[derive(Serialize, Deserialize)]
pub struct Handover {
    pub auth: AuthState,
    pub agent: AgentState,
    /// Its values of the variables credentials come from (`kelpy_core::environment`).
    pub environment: BTreeMap<String, String>,
    /// The port the Web UI is on: the new Kelpy takes the same.
    pub port: u16,
    pub started_at: i64,
    pub stops_at: Option<i64>,
    pub idle_limit: Option<u64>,
    pub ssh: Option<crate::runtime::Ssh>,
    /// The memory of the server chosen for the new Kelpy, which records it in the configuration once it runs.
    #[serde(default)]
    pub memory: Option<kelpy_core::config::MemoryUse>,
}

/// One message of the exchange.
#[derive(Serialize, Deserialize)]
pub enum Message {
    /// From the new Kelpy, first.
    Hello { version: String, pid: u32 },
    /// From the old Kelpy, once it stopped and let go of the lock.
    State(Box<Handover>),
    /// From the new Kelpy: it runs in the old one's place.
    Ready,
    /// From the new Kelpy: it could not start, and why.
    Failed(String),
    /// From the old Kelpy: it goes on itself after all, and why.
    Refused(String),
}

/// Messages over a stream, each its length then its JSON, each sent or received by the channel's deadline.
pub struct Channel<S> {
    stream: S,
    deadline: Instant,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Channel<S> {
    /// From now on, what is sent or received must be by `deadline`: past it, it fails as timed out.
    pub fn until(&mut self, deadline: Instant) {
        self.deadline = deadline;
    }

    pub async fn send(&mut self, message: &Message) -> io::Result<()> {
        let bytes = serde_json::to_vec(message)?;
        self.write(&bytes).await
    }

    /// Sends the state, `state` being a [`Handover`] as JSON.
    pub async fn send_state(&mut self, state: &[u8]) -> io::Result<()> {
        let mut message = Vec::with_capacity(state.len() + 10);
        message.extend_from_slice(br#"{"State":"#);
        message.extend_from_slice(state);
        message.push(b'}');
        self.write(&message).await
    }

    pub async fn receive(&mut self) -> io::Result<Message> {
        let deadline = self.deadline;
        let bytes = by(deadline, async {
            let length = self.stream.read_u32_le().await?;
            if length > MAX_MESSAGE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the message is too large",
                ));
            }
            let mut bytes = vec![0; length as usize];
            self.stream.read_exact(&mut bytes).await?;
            Ok(bytes)
        })
        .await?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    async fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        let length = u32::try_from(bytes.len())
            .ok()
            .filter(|&length| length <= MAX_MESSAGE)
            .ok_or_else(|| io::Error::other("the state to hand over is too large"))?;
        let deadline = self.deadline;
        by(deadline, async {
            self.stream.write_u32_le(length).await?;
            self.stream.write_all(bytes).await?;
            self.stream.flush().await
        })
        .await
    }
}

/// `work`, failing as timed out if not done by `deadline`.
async fn by<T>(deadline: Instant, work: impl Future<Output = io::Result<T>>) -> io::Result<T> {
    tokio::time::timeout_at(deadline, work).await.map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "the other Kelpy did not answer in time",
        )
    })?
}

/// Who the old Kelpy answers.
#[derive(Debug, Clone, Copy)]
pub enum Peer {
    /// The process with this PID: the one User Account Control started.
    Process(u32),
    /// Any process of root's: `sudo` starts Kelpy under a PID the old one does not learn.
    #[cfg_attr(windows, allow(dead_code))]
    Root,
}

fn refused(peer: Peer) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("another process than the one expected ({peer:?}) took the channel"),
    )
}

/// A random name of 128 bits.
fn random_name() -> io::Result<String> {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 16];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| io::Error::other("no random numbers"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub use imp::{Listener, Stream, connect};

#[cfg(windows)]
mod imp {
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::time::Duration;

    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };
    use windows_sys::Win32::Foundation::{ERROR_PIPE_BUSY, HANDLE};
    use windows_sys::Win32::System::Pipes::{
        GetNamedPipeClientProcessId, GetNamedPipeServerProcessId,
    };

    use super::{Channel, Instant, Peer, by, random_name, refused};

    /// The old Kelpy's end of the channel.
    pub type Stream = NamedPipeServer;

    /// The old Kelpy's end, until the new one connects.
    pub struct Listener {
        server: NamedPipeServer,
        name: String,
    }

    impl Listener {
        pub fn open() -> io::Result<Self> {
            let name = format!(r"\\.\pipe\kelpy-handover-{}", random_name()?);
            let server = ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .max_instances(1)
                .create(&name)?;
            Ok(Self { server, name })
        }

        /// What the new Kelpy is given to connect.
        pub fn name(&self) -> &str {
            &self.name
        }

        /// Waits for the new Kelpy until `deadline`: the channel is closed to anyone else.
        pub async fn accept(
            self,
            peer: Peer,
            deadline: Instant,
        ) -> io::Result<Channel<NamedPipeServer>> {
            by(deadline, self.server.connect()).await?;
            let mut pid = 0;
            // SAFETY: the handle is the server's, open while `self.server` lives.
            let asked = unsafe {
                GetNamedPipeClientProcessId(self.server.as_raw_handle() as HANDLE, &mut pid)
            };
            if asked == 0 {
                return Err(io::Error::last_os_error());
            }
            match peer {
                Peer::Process(expected) if pid == expected => Ok(Channel {
                    stream: self.server,
                    deadline,
                }),
                _ => Err(refused(peer)),
            }
        }
    }

    /// Connects to the old Kelpy's channel `name` until `deadline`, checking that `server_pid` holds it.
    pub async fn connect(
        name: &str,
        server_pid: u32,
        deadline: Instant,
    ) -> io::Result<Channel<NamedPipeClient>> {
        let client = by(deadline, async {
            loop {
                match ClientOptions::new().open(name) {
                    Ok(client) => return Ok(client),
                    Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    Err(error) => return Err(error),
                }
            }
        })
        .await?;
        let mut pid = 0;
        // SAFETY: the handle is the client's, open while `client` lives.
        let asked =
            unsafe { GetNamedPipeServerProcessId(client.as_raw_handle() as HANDLE, &mut pid) };
        if asked == 0 {
            return Err(io::Error::last_os_error());
        }
        if pid != server_pid {
            return Err(refused(Peer::Process(server_pid)));
        }
        Ok(Channel {
            stream: client,
            deadline,
        })
    }
}

#[cfg(unix)]
mod imp {
    use std::io;
    use std::os::unix::fs::DirBuilderExt;
    use std::path::PathBuf;

    use tokio::net::{UnixListener, UnixStream};

    use super::{Channel, Instant, Peer, by, random_name, refused};

    /// The old Kelpy's end of the channel.
    pub type Stream = UnixStream;

    /// The old Kelpy's end, until the new one connects. Its folder goes with it.
    pub struct Listener {
        listener: UnixListener,
        folder: PathBuf,
        name: String,
    }

    impl Listener {
        pub fn open() -> io::Result<Self> {
            let folder = std::env::temp_dir().join(format!("kelpy-handover-{}", random_name()?));
            std::fs::DirBuilder::new().mode(0o700).create(&folder)?;
            let path = folder.join("socket");
            let listener = UnixListener::bind(&path)?;
            Ok(Self {
                listener,
                name: path.to_string_lossy().into_owned(),
                folder,
            })
        }

        /// What the new Kelpy is given to connect.
        pub fn name(&self) -> &str {
            &self.name
        }

        /// Waits for the new Kelpy until `deadline`: anyone else is refused, and the channel then closed.
        pub async fn accept(
            self,
            peer: Peer,
            deadline: Instant,
        ) -> io::Result<Channel<UnixStream>> {
            let (stream, _) = by(deadline, self.listener.accept()).await?;
            let credentials = stream.peer_cred()?;
            let expected = match peer {
                Peer::Process(pid) => credentials.pid() == i32::try_from(pid).ok(),
                Peer::Root => credentials.uid() == 0,
            };
            if expected {
                Ok(Channel { stream, deadline })
            } else {
                Err(refused(peer))
            }
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            // Best effort: the folder is in the temporary folder, and empty once the socket is gone.
            let _ = std::fs::remove_dir_all(&self.folder);
        }
    }

    /// Connects to the old Kelpy's channel `name` until `deadline`, checking that `server_pid` holds it.
    pub async fn connect(
        name: &str,
        server_pid: u32,
        deadline: Instant,
    ) -> io::Result<Channel<UnixStream>> {
        let stream = by(deadline, UnixStream::connect(name)).await?;
        if stream.peer_cred()?.pid() != i32::try_from(server_pid).ok() {
            return Err(refused(Peer::Process(server_pid)));
        }
        Ok(Channel { stream, deadline })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(10)
    }

    #[tokio::test]
    async fn only_the_process_expected_gets_the_channel_and_messages_go_both_ways() {
        let listener = Listener::open().unwrap();
        let name = listener.name().to_owned();
        let me = std::process::id();
        let new = tokio::spawn(async move {
            let mut channel = connect(&name, me, soon()).await.unwrap();
            channel
                .send(&Message::Hello {
                    version: "1.2.3".into(),
                    pid: me,
                })
                .await
                .unwrap();
            let Message::Refused(why) = channel.receive().await.unwrap() else {
                panic!("not refused");
            };
            why
        });
        let mut old = listener.accept(Peer::Process(me), soon()).await.unwrap();
        let Message::Hello { version, pid } = old.receive().await.unwrap() else {
            panic!("no hello");
        };
        assert_eq!((version.as_str(), pid), ("1.2.3", me));
        old.send(&Message::Refused("cancelled".into()))
            .await
            .unwrap();
        assert_eq!(new.await.unwrap(), "cancelled");

        // Another process than the one expected is refused.
        let listener = Listener::open().unwrap();
        let name = listener.name().to_owned();
        let other = tokio::spawn(async move { connect(&name, me, soon()).await.map(|_| ()) });
        let error = listener
            .accept(Peer::Process(me + 1), soon())
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        let _ = other.await;

        // The new Kelpy refuses a channel another process holds.
        let listener = Listener::open().unwrap();
        let error = connect(listener.name(), me + 1, soon())
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn an_end_that_stops_answering_is_given_up_on_at_the_deadline() {
        let me = std::process::id();
        let shortly = || Instant::now() + Duration::from_millis(300);

        // Nobody connects.
        let error = Listener::open()
            .unwrap()
            .accept(Peer::Process(me), shortly())
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);

        // Connected, then silent: a read gives up at the deadline, on both sides.
        let listener = Listener::open().unwrap();
        let name = listener.name().to_owned();
        let new = tokio::spawn(async move {
            let mut channel = connect(&name, me, soon()).await.unwrap();
            channel.until(shortly());
            let error = channel.receive().await.err().unwrap();
            // Kept open until the old end gave up too.
            tokio::time::sleep(Duration::from_millis(600)).await;
            error.kind()
        });
        let mut old = listener.accept(Peer::Process(me), soon()).await.unwrap();
        old.until(shortly());
        let started = Instant::now();
        assert_eq!(
            old.receive().await.err().unwrap().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(new.await.unwrap(), io::ErrorKind::TimedOut);
    }
}
