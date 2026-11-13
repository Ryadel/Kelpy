//! The models a service offers, as it lists them: `GET /models`, which OpenAI, the OpenAI-compatible services and
//! Anthropic all answer with `{"data": [{"id": …}, …]}`. Many entries also give the model's context window.

use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::{
    Kind, Protocol, Provider, ProviderError, Settings, anthropic, api_error, body, network_error,
};

/// How long listing may take: the list is one small request, and the setup waits for it.
const LIST_TIME: Duration = Duration::from_secs(15);
/// Where list entries give the context window, in tokens, by service: OpenRouter and Together, Groq, Mistral,
/// Anthropic, vLLM.
const WINDOW_FIELDS: &[&str] = &[
    "context_length",
    "context_window",
    "max_context_length",
    "max_input_tokens",
    "max_model_len",
];

/// A model as its service lists it.
struct Listed {
    name: String,
    /// Its context window in tokens, when the entry gives it.
    window: Option<u64>,
}

impl Provider {
    /// The names of the models the `kind` service at `base_url` offers, as requests name them, in its order.
    pub async fn models(
        &self,
        kind: Kind,
        base_url: &str,
        api_key: Option<&str>,
    ) -> Result<Vec<String>, ProviderError> {
        let listed = self.list(kind, base_url, api_key).await?;
        Ok(listed.into_iter().map(|model| model.name).collect())
    }

    /// The context window of the model `settings` name, in tokens, as its service declares it: in its list of
    /// models, or for Ollama the window it runs the model with. `None` when the service does not say. ChatGPT's
    /// backend is not asked: its list gives no window.
    pub async fn context_window(&self, settings: &Settings) -> Result<Option<u64>, ProviderError> {
        let base_url = settings.base_url()?;
        let key = settings.api_key.as_deref().filter(|key| !key.is_empty());
        match settings.kind {
            Kind::ChatGpt => Ok(None),
            Kind::Ollama => self.ollama_window(base_url, &settings.model).await,
            kind => Ok(self
                .list(kind, base_url, key)
                .await?
                .into_iter()
                .find(|model| model.name == settings.model)
                .and_then(|model| model.window)),
        }
    }

    /// Whether the model `settings` name can call tools, when its service says: only Ollama does, in the model's
    /// capabilities. `None` when unknown; a service that refuses tools is then told apart by
    /// [`Outcome::tools_refused`](crate::Outcome::tools_refused).
    pub async fn supports_tools(&self, settings: &Settings) -> Result<Option<bool>, ProviderError> {
        match settings.kind {
            Kind::Ollama => {
                self.ollama_tools(settings.base_url()?, &settings.model)
                    .await
            }
            _ => Ok(None),
        }
    }

    /// Whether the model `settings` name can see images: ChatGPT's, OpenAI's and Anthropic's models do, an Ollama
    /// model when its capabilities list `vision`. `None` when unknown, for an OpenAI-compatible service: one that
    /// refuses images is then told apart by [`Outcome::images_refused`](crate::Outcome::images_refused).
    pub async fn sees_images(&self, settings: &Settings) -> Result<Option<bool>, ProviderError> {
        match settings.kind {
            Kind::Ollama => {
                self.ollama_vision(settings.base_url()?, &settings.model)
                    .await
            }
            Kind::OpenAiCompatible => Ok(None),
            Kind::ChatGpt | Kind::OpenAi | Kind::Anthropic => Ok(Some(true)),
        }
    }

    /// The models of the `kind` service at `base_url`. Anthropic's list comes in pages: the first one, of the
    /// largest size, holds them all.
    async fn list(
        &self,
        kind: Kind,
        base_url: &str,
        api_key: Option<&str>,
    ) -> Result<Vec<Listed>, ProviderError> {
        #[derive(Deserialize)]
        struct List {
            data: Vec<Value>,
        }

        let base_url = base_url.trim_end_matches('/');
        let request = match kind.protocol() {
            Protocol::Messages => anthropic::authorize(
                self.client.get(format!("{base_url}/models?limit=1000")),
                api_key,
            ),
            Protocol::Responses | Protocol::ChatCompletions => {
                let request = self.client.get(format!("{base_url}/models"));
                match api_key {
                    Some(key) => request.bearer_auth(key),
                    None => request,
                }
            }
        };
        let response = request
            .timeout(LIST_TIME)
            .send()
            .await
            .map_err(network_error)?;
        if !response.status().is_success() {
            return Err(api_error(response).await);
        }
        let body = body::read(response, body::MAX_BODY).await?;
        let list: List = serde_json::from_slice(&body)
            .map_err(|error| ProviderError::Protocol(format!("not a list of models: {error}")))?;
        list.data.iter().map(listed).collect()
    }
}

/// A list entry: its name, as requests give it, and its window if it says.
fn listed(entry: &Value) -> Result<Listed, ProviderError> {
    let id = entry["id"].as_str().ok_or_else(|| {
        ProviderError::Protocol("not a list of models: an entry has no id".into())
    })?;
    // Google's OpenAI-compatible API lists `models/gemini-…`, and takes the name without the prefix.
    let name = id.strip_prefix("models/").unwrap_or(id).to_owned();
    let window = WINDOW_FIELDS
        .iter()
        .map(|field| &entry[field])
        .chain([&entry["top_provider"]["context_length"]])
        .find_map(|value| value.as_u64().filter(|&window| window > 0));
    Ok(Listed { name, window })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kelpy_test_support::FakeServer;
    use serde_json::json;

    #[tokio::test]
    async fn compatible_services_list_their_models_with_the_key_as_bearer() {
        let list = json!({ "object": "list", "data": [
            { "id": "models/gemini-3.8-flash", "object": "model" },
            { "id": "openai/gpt-oss-120b", "object": "model" },
        ]});
        let server = FakeServer::start(vec![("GET /v1/models", "200 OK", list.to_string())]).await;
        let models = Provider::new()
            .unwrap()
            .models(
                Kind::OpenAiCompatible,
                &format!("{}/v1/", server.url),
                Some("key-1"),
            )
            .await
            .unwrap();
        assert_eq!(models, ["gemini-3.8-flash", "openai/gpt-oss-120b"]);
        assert!(server.requests()[0].contains("authorization: Bearer key-1"));
    }

    #[tokio::test]
    async fn anthropic_lists_its_models_with_its_own_headers() {
        let list =
            json!({ "data": [{ "id": "claude-opus-5-5", "type": "model" }], "has_more": false });
        let server = FakeServer::start(vec![("GET /v1/models", "200 OK", list.to_string())]).await;
        let models = Provider::new()
            .unwrap()
            .models(
                Kind::Anthropic,
                &format!("{}/v1", server.url),
                Some("sk-ant-1"),
            )
            .await
            .unwrap();
        assert_eq!(models, ["claude-opus-5-5"]);
        let request = &server.requests()[0];
        assert!(request.contains("x-api-key: sk-ant-1"), "{request}");
        assert!(request.contains("anthropic-version: "), "{request}");
        assert!(!request.contains("authorization"), "{request}");
    }

    #[tokio::test]
    async fn the_context_window_is_read_from_the_models_entry() {
        let list = json!({ "data": [
            { "id": "llama-4-scout", "context_window": 131072 },
            { "id": "openai/gpt-oss-120b", "context_length": 0, "top_provider": { "context_length": 65536 } },
            { "id": "mistral-large", "max_context_length": 262144 },
            { "id": "no-window" },
        ]});
        let server = FakeServer::start(vec![("GET /v1/models", "200 OK", list.to_string())]).await;
        let provider = Provider::new().unwrap();
        let window = |model: &str| {
            let settings = Settings {
                kind: Kind::OpenAiCompatible,
                model: model.to_owned(),
                base_url: Some(format!("{}/v1", server.url)),
                api_key: None,
                headers: Vec::new(),
            };
            let provider = provider.clone();
            async move { provider.context_window(&settings).await.unwrap() }
        };
        assert_eq!(window("llama-4-scout").await, Some(131_072));
        assert_eq!(window("mistral-large").await, Some(262_144));
        assert_eq!(window("no-window").await, None);
        assert_eq!(window("not-listed").await, None);
        // A zero is no window: the provider's own entry tells.
        assert_eq!(window("openai/gpt-oss-120b").await, Some(65_536));
    }

    #[tokio::test]
    async fn a_refused_list_carries_the_services_message() {
        let refused = json!({ "error": { "message": "Incorrect API key provided" } });
        let server = FakeServer::start(vec![(
            "GET /models",
            "401 Unauthorized",
            refused.to_string(),
        )])
        .await;
        let error = Provider::new()
            .unwrap()
            .models(Kind::OpenAi, &server.url, Some("wrong"))
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "the provider answered 401: Incorrect API key provided"
        );
    }
}
