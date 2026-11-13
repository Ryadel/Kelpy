//! Ollama's own API, beside the Chat Completions one that writes the answers: the models installed, the context
//! window they run with, and downloading more.

use std::time::Duration;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{Provider, ProviderError, api_error, body, network_error};

/// How long listing the models may take: Ollama answers at once when it is running.
const LIST_TIME: Duration = Duration::from_secs(5);
/// The most a line of download progress may hold; Ollama's are about a hundred bytes.
const MAX_LINE: usize = 64 << 10;

/// A model installed in Ollama.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocalModel {
    /// The name to choose it by, such as `qwen3:8b`.
    pub name: String,
    /// Bytes on disk.
    pub size: u64,
    /// Runs on ollama.com, not on Ollama's computer, such as `gpt-oss:120b-cloud`: prompts leave the network.
    pub cloud: bool,
}

/// How a download is going, as Ollama reports it: a status such as `pulling manifest`, and bytes for the part
/// being downloaded.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullProgress {
    pub status: String,
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub completed: u64,
}

/// Ollama's own API sits beside the OpenAI-compatible one: `http://host:11434/v1` → `http://host:11434`.
fn root(base_url: &str) -> &str {
    let base_url = base_url.trim_end_matches('/');
    base_url.strip_suffix("/v1").unwrap_or(base_url)
}

impl Provider {
    /// The models installed in the Ollama answering at `base_url`. Fails with a network error if none answers.
    pub async fn ollama_models(&self, base_url: &str) -> Result<Vec<LocalModel>, ProviderError> {
        #[derive(Deserialize)]
        struct Tags {
            models: Vec<Tag>,
        }
        #[derive(Deserialize)]
        struct Tag {
            name: String,
            size: u64,
            /// Where a cloud model runs.
            #[serde(default)]
            remote_host: Option<String>,
        }

        let response = self
            .client
            .get(format!("{}/api/tags", root(base_url)))
            .timeout(LIST_TIME)
            .send()
            .await
            .map_err(network_error)?;
        if !response.status().is_success() {
            return Err(api_error(response).await);
        }
        let body = body::read(response, body::MAX_BODY).await?;
        let tags: Tags = serde_json::from_slice(&body).map_err(|error| {
            ProviderError::Protocol(format!("not Ollama's list of models: {error}"))
        })?;
        Ok(tags
            .models
            .into_iter()
            .map(|tag| LocalModel {
                // Older versions of Ollama name the host only in the model's name.
                cloud: tag.remote_host.is_some()
                    || tag.name.ends_with("-cloud")
                    || tag.name.ends_with(":cloud"),
                name: tag.name,
                size: tag.size,
            })
            .collect())
    }

    /// Downloads `model` into the Ollama at `base_url`, calling `on_progress` as it goes. Ollama keeps what was
    /// downloaded if this stops early, and the next download of the same model resumes from there.
    pub async fn ollama_pull(
        &self,
        base_url: &str,
        model: &str,
        mut on_progress: impl FnMut(PullProgress) + Send,
    ) -> Result<(), ProviderError> {
        let response = self
            .client
            .post(format!("{}/api/pull", root(base_url)))
            .json(&json!({ "model": model, "stream": true }))
            .send()
            .await
            .map_err(network_error)?;
        if !response.status().is_success() {
            return Err(api_error(response).await);
        }

        // One JSON object per line; a line may arrive in several pieces.
        let mut pending = Vec::new();
        let mut bytes = response.bytes_stream();
        while let Some(chunk) = bytes.next().await {
            pending.extend_from_slice(&chunk.map_err(network_error)?);
            if pending.len() > MAX_LINE && !pending.contains(&b'\n') {
                return Err(ProviderError::Protocol(format!(
                    "a line of Ollama's download progress is larger than {MAX_LINE} bytes"
                )));
            }
            while let Some(end) = pending.iter().position(|&byte| byte == b'\n') {
                let line: Vec<u8> = pending.drain(..=end).collect();
                let line = String::from_utf8_lossy(&line);
                if line.trim().is_empty() {
                    continue;
                }
                let event: serde_json::Value = serde_json::from_str(&line).map_err(|error| {
                    ProviderError::Protocol(format!("not Ollama's download progress: {error}"))
                })?;
                if let Some(message) = event["error"].as_str() {
                    return Err(ProviderError::Api {
                        status: None,
                        message: message.to_owned(),
                    });
                }
                let progress: PullProgress = serde_json::from_value(event).map_err(|error| {
                    ProviderError::Protocol(format!("not Ollama's download progress: {error}"))
                })?;
                let done = progress.status == "success";
                on_progress(progress);
                if done {
                    return Ok(());
                }
            }
        }
        Err(ProviderError::Network(
            "the connection closed before the download was complete".into(),
        ))
    }

    /// The context window the Ollama at `base_url` runs `model` with: as it reports while the model is loaded,
    /// else the `num_ctx` the model's settings give. `None` when neither says, and Ollama's own default applies.
    /// The window the model was trained for is no guide: Ollama runs models with a smaller one.
    pub async fn ollama_window(
        &self,
        base_url: &str,
        model: &str,
    ) -> Result<Option<u64>, ProviderError> {
        // A model chosen without a tag runs as `:latest`.
        let named = |name: &str| name == model || name.strip_suffix(":latest") == Some(model);
        let running = self
            .ollama_json(self.client.get(format!("{}/api/ps", root(base_url))))
            .await?;
        let loaded = running["models"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|entry| entry["name"].as_str().is_some_and(named))
            .and_then(|entry| entry["context_length"].as_u64());
        if loaded.is_some() {
            return Ok(loaded);
        }
        let shown = self
            .ollama_json(
                self.client
                    .post(format!("{}/api/show", root(base_url)))
                    .json(&json!({ "model": model })),
            )
            .await?;
        // One parameter per line, such as `num_ctx                        8192`.
        Ok(shown["parameters"].as_str().and_then(|parameters| {
            parameters.lines().find_map(|line| {
                let mut words = line.split_whitespace();
                (words.next() == Some("num_ctx")).then(|| words.next()?.parse().ok())?
            })
        }))
    }

    /// Whether the Ollama at `base_url` lets `model` call tools, from the capabilities it lists. `None` from an
    /// Ollama too old to list them.
    pub async fn ollama_tools(
        &self,
        base_url: &str,
        model: &str,
    ) -> Result<Option<bool>, ProviderError> {
        self.ollama_capability(base_url, model, "tools").await
    }

    /// Whether the Ollama at `base_url` lets `model` see images, as [`Provider::ollama_tools`].
    pub async fn ollama_vision(
        &self,
        base_url: &str,
        model: &str,
    ) -> Result<Option<bool>, ProviderError> {
        self.ollama_capability(base_url, model, "vision").await
    }

    /// Whether `model` has `capability` among those the Ollama at `base_url` lists.
    async fn ollama_capability(
        &self,
        base_url: &str,
        model: &str,
        capability: &str,
    ) -> Result<Option<bool>, ProviderError> {
        let shown = self
            .ollama_json(
                self.client
                    .post(format!("{}/api/show", root(base_url)))
                    .json(&json!({ "model": model })),
            )
            .await?;
        Ok(shown["capabilities"]
            .as_array()
            .map(|listed| listed.iter().any(|listed| listed == capability)))
    }

    /// The JSON answer to an Ollama API request.
    async fn ollama_json(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<serde_json::Value, ProviderError> {
        let response = request
            .timeout(LIST_TIME)
            .send()
            .await
            .map_err(network_error)?;
        if !response.status().is_success() {
            return Err(api_error(response).await);
        }
        let body = body::read(response, body::MAX_BODY).await?;
        serde_json::from_slice(&body)
            .map_err(|error| ProviderError::Protocol(format!("not an Ollama answer: {error}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kelpy_test_support::FakeServer;

    #[tokio::test]
    async fn the_window_is_the_one_ollama_runs_the_model_with() {
        let running = serde_json::json!({ "models": [
            { "name": "qwen3:latest", "model": "qwen3:latest", "context_length": 32768 },
        ]});
        let shown = serde_json::json!({
            "parameters": "stop                           \"<|im_end|>\"\nnum_ctx                        8192",
            "model_info": { "qwen3.context_length": 40960 },
        });
        let server = FakeServer::start(vec![
            ("GET /api/ps", "200 OK", running.to_string()),
            ("POST /api/show", "200 OK", shown.to_string()),
        ])
        .await;
        let provider = Provider::new().unwrap();
        let base = format!("{}/v1", server.url);
        assert_eq!(
            provider.ollama_window(&base, "qwen3").await.unwrap(),
            Some(32_768)
        );
        assert_eq!(
            provider.ollama_window(&base, "llama3.2").await.unwrap(),
            Some(8_192)
        );
        assert!(server.requests()[2].contains("\"model\":\"llama3.2\""));

        server.respond(
            "POST /api/show",
            "200 OK",
            serde_json::json!({ "parameters": "" }).to_string(),
        );
        assert_eq!(
            provider.ollama_window(&base, "llama3.2").await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn the_capabilities_tell_whether_a_model_calls_tools_and_sees_images() {
        let shown = json!({ "capabilities": ["completion", "tools"] });
        let server = FakeServer::start(vec![("POST /api/show", "200 OK", shown.to_string())]).await;
        let provider = Provider::new().unwrap();
        let base = format!("{}/v1", server.url);
        assert_eq!(
            provider.ollama_tools(&base, "qwen3").await.unwrap(),
            Some(true)
        );
        assert_eq!(
            provider.ollama_vision(&base, "qwen3").await.unwrap(),
            Some(false)
        );
        server.respond(
            "POST /api/show",
            "200 OK",
            json!({ "capabilities": ["completion", "vision", "tools"] }).to_string(),
        );
        assert_eq!(
            provider.ollama_vision(&base, "qwen3.5").await.unwrap(),
            Some(true)
        );
        server.respond(
            "POST /api/show",
            "200 OK",
            json!({ "capabilities": ["completion"] }).to_string(),
        );
        assert_eq!(
            provider.ollama_tools(&base, "gemma").await.unwrap(),
            Some(false)
        );
        server.respond("POST /api/show", "200 OK", json!({}).to_string());
        assert_eq!(provider.ollama_tools(&base, "old").await.unwrap(), None);
    }

    #[test]
    fn the_api_is_found_beside_the_compatible_one() {
        assert_eq!(root("http://localhost:11434/v1"), "http://localhost:11434");
        assert_eq!(root("http://10.0.0.5:11434/v1/"), "http://10.0.0.5:11434");
        assert_eq!(root("http://10.0.0.5:11434"), "http://10.0.0.5:11434");
    }

    #[tokio::test]
    async fn installed_models_are_listed() {
        let tags = json!({ "models": [
            { "name": "qwen3:8b", "model": "qwen3:8b", "size": 5_200_000_000_u64, "digest": "abc" },
            { "name": "gpt-oss:120b-cloud", "size": 384, "remote_host": "https://ollama.com:443" },
            { "name": "glm-4.6:cloud", "size": 340 },
        ]});
        let server = FakeServer::start(vec![("GET /api/tags", "200 OK", tags.to_string())]).await;
        let models = Provider::new()
            .unwrap()
            .ollama_models(&format!("{}/v1", server.url))
            .await
            .unwrap();
        assert_eq!(
            models,
            vec![
                LocalModel {
                    name: "qwen3:8b".into(),
                    size: 5_200_000_000,
                    cloud: false,
                },
                LocalModel {
                    name: "gpt-oss:120b-cloud".into(),
                    size: 384,
                    cloud: true,
                },
                LocalModel {
                    name: "glm-4.6:cloud".into(),
                    size: 340,
                    cloud: true,
                },
            ]
        );
    }

    #[tokio::test]
    async fn a_download_reports_its_progress_until_it_succeeds() {
        let lines = [
            json!({ "status": "pulling manifest" }),
            json!({ "status": "pulling abc", "digest": "abc", "total": 100, "completed": 40 }),
            json!({ "status": "success" }),
        ];
        let body = lines.map(|line| format!("{line}\n")).concat();
        let server = FakeServer::start(vec![("POST /api/pull", "200 OK", body)]).await;
        let mut seen = Vec::new();
        Provider::new()
            .unwrap()
            .ollama_pull(&server.url, "qwen3:8b", |progress| seen.push(progress))
            .await
            .unwrap();
        assert_eq!(seen.len(), 3);
        assert_eq!((seen[1].completed, seen[1].total), (40, 100));
        assert!(server.requests()[0].contains("\"model\":\"qwen3:8b\""));
    }

    #[tokio::test]
    async fn a_progress_line_that_never_ends_is_an_error() {
        let body = "x".repeat(MAX_LINE + 1);
        let server = FakeServer::start(vec![("POST /api/pull", "200 OK", body)]).await;
        let error = Provider::new()
            .unwrap()
            .ollama_pull(&server.url, "qwen3:8b", |_| {})
            .await
            .unwrap_err();
        assert!(error.to_string().contains("larger than"), "{error}");
    }

    #[tokio::test]
    async fn a_download_that_fails_or_stops_early_is_an_error() {
        let failed = format!(
            "{}\n",
            json!({ "error": "pull model manifest: file does not exist" })
        );
        let server = FakeServer::start(vec![("POST /api/pull", "200 OK", failed)]).await;
        let provider = Provider::new().unwrap();
        let error = provider
            .ollama_pull(&server.url, "nope", |_| {})
            .await
            .unwrap_err();
        assert!(error.to_string().contains("file does not exist"), "{error}");

        let partial = format!(
            "{}\n",
            json!({ "status": "pulling abc", "total": 100, "completed": 10 })
        );
        server.respond("POST /api/pull", "200 OK", partial);
        assert!(matches!(
            provider.ollama_pull(&server.url, "qwen3:8b", |_| {}).await,
            Err(ProviderError::Network(_))
        ));
    }
}
