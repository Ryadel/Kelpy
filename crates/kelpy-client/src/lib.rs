//! A client of the running Kelpy's local API, for the interfaces in the terminal: the terminal commands, which
//! change what the runtime holds in memory only (API keys, sign-ins), and the terminal UI. It calls with the owner's
//! token from `runtime.json`, and reads the Server-Sent Events of answers and of the runtime's event channel.
//!
//! Every wait is bounded. A request gets [`REQUEST_WAIT`] in all; a stream of events has no end set, as an answer
//! may take long, but ends when nothing arrives for [`STREAM_IDLE`], while the runtime sends a keep-alive every 15
//! seconds. An event is read whole up to [`MAX_EVENT`].

use std::time::Duration;

use futures_util::{Stream, StreamExt};
pub use reqwest::Method;
use reqwest::{RequestBuilder, Response};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// How long connecting to the runtime may take: it is on this computer.
const CONNECT_WAIT: Duration = Duration::from_secs(5);
/// How long a request and its answer may take in all, such as checking an API key with its service.
const REQUEST_WAIT: Duration = if cfg!(test) {
    Duration::from_millis(300)
} else {
    Duration::from_secs(120)
};
/// How long a stream may stay silent: four keep-alives missed.
const STREAM_IDLE: Duration = if cfg!(test) {
    Duration::from_millis(300)
} else {
    Duration::from_secs(60)
};
/// The largest event read: an answer with its actions is far smaller.
const MAX_EVENT: usize = 32 << 20;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("cannot reach Kelpy: {0}")]
    Unreachable(reqwest::Error),
    /// Kelpy refused the request: its own words.
    #[error("{0}")]
    Refused(String),
    #[error("unexpected answer from Kelpy: {0}")]
    Unexpected(String),
}

/// A client of one running Kelpy. Clones share its connections.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    /// Such as `http://127.0.0.1:4321`.
    url: String,
    token: String,
}

impl Client {
    pub fn new(url: String, token: String) -> Result<Self, ClientError> {
        let http = kelpy_providers::http_client(|builder| {
            builder
                .no_proxy()
                .connect_timeout(CONNECT_WAIT)
                .read_timeout(STREAM_IDLE)
        })
        .map_err(|error| ClientError::Unexpected(error.to_string()))?;
        Ok(Self { http, url, token })
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        read(self.call(Method::GET, path, None::<&()>)).await
    }

    pub async fn delete<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        read(self.call(Method::DELETE, path, None::<&()>)).await
    }

    /// Sends `body` and reads the answer.
    pub async fn send<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: &impl Serialize,
    ) -> Result<T, ClientError> {
        read(self.call(method, path, Some(body))).await
    }

    /// Sends a request whose answer says nothing but that it was done.
    pub async fn act(
        &self,
        method: Method,
        path: &str,
        body: Option<&impl Serialize>,
    ) -> Result<(), ClientError> {
        checked(self.call(method, path, body)).await?;
        Ok(())
    }

    /// The data of each Server-Sent Event the answer to the request carries.
    pub async fn events<B: Serialize>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<impl Stream<Item = Result<String, ClientError>> + use<B>, ClientError> {
        let request = self
            .request(method, path, body)
            .header("Accept", "text/event-stream");
        Ok(events(checked(request).await?))
    }

    /// Posts `body` and calls `on_event` with each event the answer carries, until it ends.
    pub async fn each_event<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &impl Serialize,
        mut on_event: impl FnMut(T),
    ) -> Result<(), ClientError> {
        let events = self.events(Method::POST, path, Some(body)).await?;
        let mut events = std::pin::pin!(events);
        while let Some(data) = events.next().await {
            let event = serde_json::from_str(&data?)
                .map_err(|error| ClientError::Unexpected(error.to_string()))?;
            on_event(event);
        }
        Ok(())
    }

    /// A request with an answer that is not a stream, within [`REQUEST_WAIT`].
    fn call(&self, method: Method, path: &str, body: Option<&impl Serialize>) -> RequestBuilder {
        self.request(method, path, body).timeout(REQUEST_WAIT)
    }

    fn request(&self, method: Method, path: &str, body: Option<&impl Serialize>) -> RequestBuilder {
        let request = self
            .http
            .request(method, format!("{}{path}", self.url))
            .bearer_auth(&self.token);
        match body {
            Some(body) => request.json(body),
            None => request,
        }
    }
}

async fn read<T: DeserializeOwned>(request: RequestBuilder) -> Result<T, ClientError> {
    checked(request)
        .await?
        .json()
        .await
        .map_err(|error| ClientError::Unexpected(error.to_string()))
}

/// Sends `request`; an error answer becomes its message, as the runtime words it.
async fn checked(request: RequestBuilder) -> Result<Response, ClientError> {
    let response = request.send().await.map_err(ClientError::Unreachable)?;
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let body: serde_json::Value = response.json().await.unwrap_or_default();
    Err(ClientError::Refused(match body["error"].as_str() {
        Some(message) => message.to_owned(),
        None => format!("Kelpy answered {status}"),
    }))
}

/// The data of each Server-Sent Event of `response`, taken whole: a character split between two pieces stays whole.
/// The stream ends after an error.
fn events(response: Response) -> impl Stream<Item = Result<String, ClientError>> {
    let bytes = response.bytes_stream();
    futures_util::stream::unfold(Some((bytes, Pending::default())), |state| async move {
        let (mut bytes, mut pending) = state?;
        loop {
            if let Some(data) = pending.take() {
                return Some((Ok(data), Some((bytes, pending))));
            }
            if pending.bytes.len() > MAX_EVENT {
                let error = format!("an event larger than {} MiB", MAX_EVENT >> 20);
                return Some((Err(ClientError::Unexpected(error)), None));
            }
            match bytes.next().await? {
                Ok(chunk) => pending.bytes.extend_from_slice(&chunk),
                Err(error) => return Some((Err(ClientError::Unreachable(error)), None)),
            }
        }
    })
}

/// What arrived of the events not yet taken. Kelpy's server ends lines with `\n`.
#[derive(Default)]
struct Pending {
    bytes: Vec<u8>,
    /// How much of `bytes` holds no end of an event: only what came after is searched.
    searched: usize,
}

impl Pending {
    /// The data of the first whole event, taken out; comments such as keep-alives are skipped.
    fn take(&mut self) -> Option<String> {
        loop {
            // One byte back: the end of an event may be split between two pieces.
            let from = self.searched.saturating_sub(1);
            let Some(at) = self.bytes[from..]
                .windows(2)
                .position(|pair| pair == b"\n\n")
            else {
                self.searched = self.bytes.len();
                return None;
            };
            let block: Vec<u8> = self.bytes.drain(..from + at + 2).collect();
            self.searched = 0;
            let block = String::from_utf8_lossy(&block);
            let data: Vec<&str> = block
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim_start)
                .collect();
            if !data.is_empty() {
                return Some(data.join("\n"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[test]
    fn events_are_taken_whole_and_keep_alives_skipped() {
        let mut pending = Pending::default();
        pending
            .bytes
            .extend_from_slice(b": keep-alive\n\nevent: delta\ndata: {\"a\":");
        assert_eq!(pending.take(), None);
        // What was searched is not searched again; an end split between two pieces is still found.
        assert_eq!(pending.searched, pending.bytes.len());
        pending.bytes.extend_from_slice(b"1}\n");
        assert_eq!(pending.take(), None);
        pending.bytes.extend_from_slice(b"\ndata: x\n\n");
        assert_eq!(pending.take().as_deref(), Some("{\"a\":1}"));
        assert_eq!(pending.take().as_deref(), Some("x"));
        assert!(pending.bytes.is_empty());
    }

    #[test]
    fn a_client_can_be_built_before_anything_else_installs_tls() {
        assert!(Client::new("http://127.0.0.1:9".into(), "token".into()).is_ok());
    }

    /// A server that answers each connection with `answer`, then says nothing more.
    async fn silent_after(answer: &'static [u8]) -> Client {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut request = [0; 4096];
                    let _ = stream.read(&mut request).await;
                    let _ = stream.write_all(answer).await;
                    std::future::pending::<()>().await;
                });
            }
        });
        Client::new(url, "token".into()).unwrap()
    }

    #[tokio::test]
    async fn a_request_or_a_stream_left_waiting_ends() {
        let client = silent_after(b"").await;
        assert!(matches!(
            client.get::<serde_json::Value>("/api/provider").await,
            Err(ClientError::Unreachable(_))
        ));

        let client = silent_after(
            b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n",
        )
        .await;
        let events = client
            .events(Method::GET, "/api/events", None::<&()>)
            .await
            .unwrap();
        let mut events = std::pin::pin!(events);
        assert!(matches!(
            events.next().await,
            Some(Err(ClientError::Unreachable(_)))
        ));
        assert!(events.next().await.is_none());
    }
}
