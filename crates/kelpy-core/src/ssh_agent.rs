//! A client of the SSH agent, the one Kelpy's terminal session points to: `SSH_AUTH_SOCK`, or OpenSSH's agent on
//! Windows. Kelpy only lists the agent's keys and asks it to sign, as `ssh` does: the private keys never leave it.
//!
//! Only keys whose signatures are always the same for the same data can unlock the vault: Ed25519, and RSA with
//! SHA-256 (PKCS #1 v1.5). ECDSA signatures are random, and security keys (`sk-`) count their uses into theirs.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// How long the agent may take to answer: it may ask its user to confirm a signature.
const ANSWER_WAIT: Duration = Duration::from_secs(60);
/// The longest message read from the agent: a list of keys or a signature is far smaller.
const MAX_MESSAGE: usize = 256 * 1024;

const FAILURE: u8 = 5;
const REQUEST_IDENTITIES: u8 = 11;
const IDENTITIES_ANSWER: u8 = 12;
const SIGN_REQUEST: u8 = 13;
const SIGN_RESPONSE: u8 = 14;
/// Asks an RSA key for an `rsa-sha2-256` signature, in place of the SHA-1 one.
const RSA_SHA2_256: u32 = 2;

/// A key the agent holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentKey {
    /// The public key in SSH's wire format, as `authorized_keys` holds it in base64.
    pub blob: Vec<u8>,
    pub comment: String,
}

impl AgentKey {
    /// The key's algorithm, such as `ssh-ed25519`.
    pub fn algorithm(&self) -> &str {
        read_string(&mut &self.blob[..])
            .and_then(|name| std::str::from_utf8(name).ok())
            .unwrap_or("unknown")
    }

    /// Whether its signatures are always the same for the same data, so that it can unlock the vault.
    pub fn signs_the_same(&self) -> bool {
        matches!(self.algorithm(), "ssh-ed25519" | "ssh-rsa")
    }

    /// The fingerprint `ssh-keygen -l` shows, such as `SHA256:2Ckf...`.
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.blob)
    }

    /// How the key is named to the administrator: `ssh-ed25519 SHA256:2Ckf... (admin@laptop)`.
    pub fn label(&self) -> String {
        let comment = if self.comment.is_empty() {
            String::new()
        } else {
            format!(" ({})", self.comment)
        };
        format!("{} {}{comment}", self.algorithm(), self.fingerprint())
    }
}

/// The fingerprint of the public key `blob`.
pub fn fingerprint(blob: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, blob);
    format!("SHA256:{}", STANDARD_NO_PAD.encode(digest))
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error(
        "no SSH agent: start Kelpy from a session with one (ssh -A, or ssh-agent with the key added)"
    )]
    Missing,
    #[error("cannot reach the SSH agent: {0}")]
    Unreachable(io::Error),
    #[error("the SSH agent did not answer within {} seconds", ANSWER_WAIT.as_secs())]
    Timeout,
    #[error(
        "the SSH agent refused to sign: is the key still added, and was the request confirmed?"
    )]
    Refused,
    #[error("the SSH agent gave an answer Kelpy does not understand")]
    Malformed,
}

/// The SSH agent at an address.
#[derive(Debug, Clone)]
pub struct SshAgent {
    address: PathBuf,
}

impl SshAgent {
    /// The agent of Kelpy's session, if it has one: `SSH_AUTH_SOCK`, or else OpenSSH's agent on Windows.
    pub fn of_session() -> Option<Self> {
        let address = match std::env::var_os("SSH_AUTH_SOCK") {
            Some(path) if !path.is_empty() => PathBuf::from(path),
            _ if cfg!(windows) => PathBuf::from(r"\\.\pipe\openssh-ssh-agent"),
            _ => return None,
        };
        Some(Self { address })
    }

    /// The agent listening at `address`.
    pub fn at(address: PathBuf) -> Self {
        Self { address }
    }

    /// The keys the agent holds.
    pub async fn keys(&self) -> Result<Vec<AgentKey>, AgentError> {
        let answer = self.ask(&[REQUEST_IDENTITIES]).await?;
        let mut rest = match answer.split_first() {
            Some((&IDENTITIES_ANSWER, rest)) => rest,
            _ => return Err(AgentError::Malformed),
        };
        let count = read_u32(&mut rest).ok_or(AgentError::Malformed)?;
        let mut keys = Vec::new();
        for _ in 0..count {
            let blob = read_string(&mut rest).ok_or(AgentError::Malformed)?;
            let comment = read_string(&mut rest).ok_or(AgentError::Malformed)?;
            keys.push(AgentKey {
                blob: blob.to_vec(),
                comment: String::from_utf8_lossy(comment).into_owned(),
            });
        }
        Ok(keys)
    }

    /// The agent's signature of `data` with the key `blob`: the signature's format name and bytes, as the agent
    /// gives them.
    pub async fn sign(&self, blob: &[u8], data: &[u8]) -> Result<Vec<u8>, AgentError> {
        let mut request = vec![SIGN_REQUEST];
        write_string(&mut request, blob);
        write_string(&mut request, data);
        request.extend_from_slice(&RSA_SHA2_256.to_be_bytes());
        let answer = self.ask(&request).await?;
        match answer.split_first() {
            Some((&SIGN_RESPONSE, mut rest)) => read_string(&mut rest)
                .map(<[u8]>::to_vec)
                .ok_or(AgentError::Malformed),
            Some((&FAILURE, _)) => Err(AgentError::Refused),
            _ => Err(AgentError::Malformed),
        }
    }

    /// Sends `request` to the agent and returns its answer, within [`ANSWER_WAIT`].
    async fn ask(&self, request: &[u8]) -> Result<Vec<u8>, AgentError> {
        let exchange = async {
            let mut stream = connect(&self.address).await?;
            exchange(&mut stream, request).await
        };
        match tokio::time::timeout(ANSWER_WAIT, exchange).await {
            Ok(Ok(answer)) => Ok(answer),
            Ok(Err(error)) if error.kind() == io::ErrorKind::NotFound => Err(AgentError::Missing),
            Ok(Err(error)) if error.kind() == io::ErrorKind::InvalidData => {
                Err(AgentError::Malformed)
            }
            Ok(Err(error)) => Err(AgentError::Unreachable(error)),
            Err(_) => Err(AgentError::Timeout),
        }
    }
}

#[cfg(unix)]
async fn connect(address: &std::path::Path) -> io::Result<tokio::net::UnixStream> {
    tokio::net::UnixStream::connect(address).await
}

#[cfg(windows)]
async fn connect(
    address: &std::path::Path,
) -> io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    tokio::net::windows::named_pipe::ClientOptions::new().open(address)
}

/// One message out, one message in, each a length and its bytes.
async fn exchange(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    request: &[u8],
) -> io::Result<Vec<u8>> {
    let length = u32::try_from(request.len()).map_err(io::Error::other)?;
    let mut message = length.to_be_bytes().to_vec();
    message.extend_from_slice(request);
    stream.write_all(&message).await?;
    let length = stream.read_u32().await? as usize;
    if length == 0 || length > MAX_MESSAGE {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut answer = vec![0; length];
    stream.read_exact(&mut answer).await?;
    Ok(answer)
}

fn read_u32(rest: &mut &[u8]) -> Option<u32> {
    let (head, tail) = rest.split_first_chunk::<4>()?;
    *rest = tail;
    Some(u32::from_be_bytes(*head))
}

fn read_string<'a>(rest: &mut &'a [u8]) -> Option<&'a [u8]> {
    let length = read_u32(rest)? as usize;
    let value = rest.get(..length)?;
    *rest = &rest[length..];
    Some(value)
}

fn write_string(message: &mut Vec<u8>, value: &[u8]) {
    // Keys, signatures and the vault's challenge are all far below 4 GiB.
    message.extend_from_slice(&(value.len() as u32).to_be_bytes());
    message.extend_from_slice(value);
}

/// An agent for tests, holding one Ed25519 key, whose signature of some data is the data itself.
#[cfg(test)]
pub(crate) mod fake {
    use tokio::sync::watch;

    use super::*;

    pub(crate) struct FakeAgent {
        pub(crate) agent: SshAgent,
        pub(crate) fingerprint: String,
        /// Whether it signs now: while it does not, a request to sign waits.
        pub(crate) signing: watch::Sender<bool>,
        /// How many requests to sign are waiting.
        pub(crate) waiting: watch::Receiver<usize>,
        #[cfg(unix)]
        _folder: tempfile::TempDir,
    }

    pub(crate) fn start() -> FakeAgent {
        let blob = tests::blob("ssh-ed25519");
        let fingerprint = fingerprint(&blob);
        let (signing, signs) = watch::channel(true);
        let (count, waiting) = watch::channel(0);
        let serve =
            move |stream| tokio::spawn(serve(stream, blob.clone(), signs.clone(), count.clone()));
        #[cfg(unix)]
        let (address, _folder) = {
            let folder = tempfile::tempdir().unwrap();
            let address = folder.path().join("agent.sock");
            let listener = tokio::net::UnixListener::bind(&address).unwrap();
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    serve(stream);
                }
            });
            (address, folder)
        };
        #[cfg(windows)]
        let address = {
            use std::sync::atomic::{AtomicUsize, Ordering};
            use tokio::net::windows::named_pipe::ServerOptions;
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let name = format!(
                r"\\.\pipe\kelpy-test-agent-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            let mut server = ServerOptions::new()
                .first_pipe_instance(true)
                .create(&name)
                .unwrap();
            let pipe = name.clone();
            tokio::spawn(async move {
                while server.connect().await.is_ok() {
                    let connected =
                        std::mem::replace(&mut server, ServerOptions::new().create(&pipe).unwrap());
                    serve(connected);
                }
            });
            PathBuf::from(name)
        };
        FakeAgent {
            agent: SshAgent::at(address),
            fingerprint,
            signing,
            waiting,
            #[cfg(unix)]
            _folder,
        }
    }

    async fn serve(
        mut stream: impl AsyncRead + AsyncWrite + Unpin,
        blob: Vec<u8>,
        mut signs: watch::Receiver<bool>,
        waiting: watch::Sender<usize>,
    ) {
        let Ok(length) = stream.read_u32().await else {
            return;
        };
        let mut request = vec![0; length as usize];
        if stream.read_exact(&mut request).await.is_err() {
            return;
        }
        let mut answer = Vec::new();
        match request.split_first() {
            Some((&REQUEST_IDENTITIES, _)) => {
                answer.push(IDENTITIES_ANSWER);
                answer.extend_from_slice(&1u32.to_be_bytes());
                write_string(&mut answer, &blob);
                write_string(&mut answer, b"admin@laptop");
            }
            Some((&SIGN_REQUEST, mut rest)) => {
                read_string(&mut rest);
                let data = read_string(&mut rest).unwrap_or_default().to_vec();
                waiting.send_modify(|count| *count += 1);
                let signed = signs.wait_for(|on| *on).await.is_ok();
                waiting.send_modify(|count| *count -= 1);
                if !signed {
                    return;
                }
                answer.push(SIGN_RESPONSE);
                write_string(&mut answer, &data);
            }
            _ => answer.push(FAILURE),
        }
        stream.write_u32(answer.len() as u32).await.ok();
        stream.write_all(&answer).await.ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn blob(algorithm: &str) -> Vec<u8> {
        let mut blob = Vec::new();
        write_string(&mut blob, algorithm.as_bytes());
        write_string(&mut blob, &[7; 32]);
        blob
    }

    #[test]
    fn keys_are_named_as_ssh_keygen_names_them() {
        let key = AgentKey {
            blob: blob("ssh-ed25519"),
            comment: "admin@laptop".into(),
        };
        assert_eq!(key.algorithm(), "ssh-ed25519");
        assert!(key.signs_the_same());
        let label = key.label();
        assert!(label.starts_with("ssh-ed25519 SHA256:"), "{label}");
        assert!(label.ends_with(" (admin@laptop)"), "{label}");
        // Base64 without padding, as OpenSSH writes it.
        assert_eq!(key.fingerprint().len(), "SHA256:".len() + 43);
        for algorithm in ["ecdsa-sha2-nistp256", "sk-ssh-ed25519@openssh.com"] {
            let key = AgentKey {
                blob: blob(algorithm),
                comment: String::new(),
            };
            assert!(!key.signs_the_same(), "{algorithm}");
        }
    }

    #[tokio::test]
    async fn a_request_and_its_answer_are_framed_by_their_length() {
        let (mut client, mut agent) = tokio::io::duplex(1024);
        let serve = tokio::spawn(async move {
            let length = agent.read_u32().await.unwrap();
            let mut request = vec![0; length as usize];
            agent.read_exact(&mut request).await.unwrap();
            assert_eq!(request, [REQUEST_IDENTITIES]);
            let mut answer = vec![IDENTITIES_ANSWER];
            answer.extend_from_slice(&1u32.to_be_bytes());
            write_string(&mut answer, &blob("ssh-ed25519"));
            write_string(&mut answer, b"admin");
            agent.write_u32(answer.len() as u32).await.unwrap();
            agent.write_all(&answer).await.unwrap();
        });
        let answer = exchange(&mut client, &[REQUEST_IDENTITIES]).await.unwrap();
        serve.await.unwrap();
        let mut rest = &answer[1..];
        assert_eq!(read_u32(&mut rest), Some(1));
        assert_eq!(read_string(&mut rest), Some(&blob("ssh-ed25519")[..]));
        assert_eq!(read_string(&mut rest), Some(&b"admin"[..]));
        assert_eq!(read_string(&mut rest), None);
    }
}
