//! A client of the running Kelpy's local API, for terminal commands that change its state: the runtime holds what
//! lives in memory only, such as API keys and sign-ins.

use anyhow::{Context, Result, anyhow};
use futures_util::StreamExt;
use reqwest::{Method, RequestBuilder, Response};
use serde::Serialize;
use serde::de::DeserializeOwned;

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    /// Such as `http://127.0.0.1:4321`.
    url: String,
    token: String,
}

impl Client {
    pub fn new(url: String, token: String) -> Result<Self> {
        kelpy_core::providers::install_tls();
        let http = reqwest::Client::builder()
            .no_proxy()
            .build()
            .context("cannot create the HTTP client")?;
        Ok(Self { http, url, token })
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        read(self.request(Method::GET, path)).await
    }

    pub async fn delete<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        read(self.request(Method::DELETE, path)).await
    }

    pub async fn send<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: &impl Serialize,
    ) -> Result<T> {
        read(self.request(method, path).json(body)).await
    }

    /// Posts `body` and calls `on_event` with each Server-Sent Event's data until the stream ends.
    pub async fn events<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &impl Serialize,
        mut on_event: impl FnMut(T),
    ) -> Result<()> {
        let response = checked(
            self.request(Method::POST, path)
                .header("Accept", "text/event-stream")
                .json(body),
        )
        .await?;
        // Bytes until a whole event is in, so that a character split between two pieces stays whole. Kelpy's server
        // ends lines with `\n`.
        let mut pending = Vec::new();
        let mut bytes = response.bytes_stream();
        while let Some(chunk) = bytes.next().await {
            pending.extend_from_slice(&chunk.context("the stream broke")?);
            while let Some(end) = pending.windows(2).position(|pair| pair == b"\n\n") {
                let block: Vec<u8> = pending.drain(..end + 2).collect();
                let block = String::from_utf8_lossy(&block);
                let data: Vec<&str> = block
                    .lines()
                    .filter_map(|line| line.strip_prefix("data:"))
                    .map(str::trim_start)
                    .collect();
                if !data.is_empty() {
                    on_event(serde_json::from_str(&data.join("\n")).context("unexpected event")?);
                }
            }
        }
        Ok(())
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.url))
            .bearer_auth(&self.token)
    }
}

async fn read<T: DeserializeOwned>(request: RequestBuilder) -> Result<T> {
    checked(request)
        .await?
        .json()
        .await
        .context("unexpected answer from Kelpy")
}

/// Sends `request`; an error answer becomes its message, as the runtime words it.
async fn checked(request: RequestBuilder) -> Result<Response> {
    let response = request.send().await.context("cannot reach Kelpy")?;
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let body: serde_json::Value = response.json().await.unwrap_or_default();
    Err(match body["error"].as_str() {
        Some(message) => anyhow!("{message}"),
        None => anyhow!("Kelpy answered {status}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_can_be_built_before_anything_else_installs_tls() {
        assert!(Client::new("http://127.0.0.1:9".into(), "token".into()).is_ok());
    }
}
