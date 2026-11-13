//! Kelpy providers: one interface over the AI model APIs, streaming the answer as it is written.
//!
//! Three protocols: Anthropic's Messages API, OpenAI's Responses API, and Chat Completions, which serves Ollama and
//! the other OpenAI-compatible servers. Each protocol module builds the request and reads the streamed events into
//! [`Step`]s and the tool calls they carry; [`Provider::stream`] is the same for all of them. A request may offer
//! tools ([`ToolSpec`]): the model then answers with text, [`ToolCall`]s, or both, and the next request carries the
//! calls and their [`ToolResult`]s. [`chatgpt`] signs in with a ChatGPT plan, whose
//! answers then use the Responses API; [`ollama`] lists and downloads local models; [`Provider::models`] lists a
//! service's models and [`Provider::context_window`] tells how much of a conversation a model reads.

mod anthropic;
mod body;
mod chat_completions;
pub mod chatgpt;
mod models;
pub mod ollama;
mod responses;
mod sse;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Which API a provider speaks, and its defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Anthropic,
    #[serde(rename = "openai")]
    OpenAi,
    Ollama,
    #[serde(rename = "openai-compatible")]
    OpenAiCompatible,
    /// OpenAI, paid by the user's ChatGPT plan after signing in (see [`chatgpt`]).
    #[serde(rename = "chatgpt")]
    ChatGpt,
}

impl Kind {
    pub const ALL: [Self; 5] = [
        Self::Anthropic,
        Self::ChatGpt,
        Self::OpenAi,
        Self::Ollama,
        Self::OpenAiCompatible,
    ];

    /// The name used in the configuration and the API, such as `openai-compatible`.
    pub fn id(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::Ollama => "ollama",
            Self::OpenAiCompatible => "openai-compatible",
            Self::ChatGpt => "chatgpt",
        }
    }

    /// The API address used when none is configured. OpenAI-compatible servers have none: it must be given.
    pub fn default_base_url(self) -> Option<&'static str> {
        match self {
            Self::Anthropic => Some(anthropic::DEFAULT_BASE_URL),
            Self::OpenAi | Self::ChatGpt => Some(responses::OPENAI_BASE_URL),
            Self::Ollama => Some(chat_completions::OLLAMA_BASE_URL),
            Self::OpenAiCompatible => None,
        }
    }

    /// The model suggested when none is configured.
    pub fn default_model(self) -> Option<&'static str> {
        match self {
            Self::Anthropic => Some(anthropic::DEFAULT_MODEL),
            _ => None,
        }
    }

    /// The environment variable an API key may come from. None for OpenAI-compatible services: they belong to
    /// other companies, and an OpenAI key must never be sent to them.
    pub fn key_variable(self) -> Option<&'static str> {
        match self {
            Self::Anthropic => Some("ANTHROPIC_API_KEY"),
            Self::OpenAi => Some("OPENAI_API_KEY"),
            Self::Ollama | Self::OpenAiCompatible | Self::ChatGpt => None,
        }
    }

    /// Whether requests can carry an API key: Ollama takes none, and a ChatGPT plan signs in instead.
    pub fn accepts_key(self) -> bool {
        !matches!(self, Self::Ollama | Self::ChatGpt)
    }

    /// Whether requests fail without an API key. Ollama and many compatible local servers need none; a ChatGPT
    /// plan needs a sign-in instead.
    pub fn requires_key(self) -> bool {
        matches!(self, Self::Anthropic | Self::OpenAi)
    }

    /// Whether the credential comes from signing in rather than from an API key.
    pub fn signs_in(self) -> bool {
        self == Self::ChatGpt
    }

    fn protocol(self) -> Protocol {
        match self {
            Self::Anthropic => Protocol::Messages,
            Self::OpenAi | Self::ChatGpt => Protocol::Responses,
            Self::Ollama | Self::OpenAiCompatible => Protocol::ChatCompletions,
        }
    }
}

/// The API a provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Protocol {
    Messages,
    Responses,
    ChatCompletions,
}

impl Protocol {
    /// The request of `kind`, which speaks this protocol, for `chat`.
    #[allow(clippy::too_many_arguments)]
    fn request(
        self,
        kind: Kind,
        client: &reqwest::Client,
        base_url: &str,
        api_key: Option<&str>,
        model: &str,
        chat: &ChatRequest<'_>,
        report_usage: bool,
    ) -> reqwest::RequestBuilder {
        match self {
            Self::Messages => anthropic::request(client, base_url, api_key, model, chat),
            Self::Responses => {
                let ask_reasoning = kind == Kind::ChatGpt;
                responses::request(client, base_url, api_key, model, chat, ask_reasoning)
            }
            Self::ChatCompletions => {
                chat_completions::request(client, base_url, api_key, model, chat, report_usage)
            }
        }
    }

    fn parse(self, data: &str, collected: &mut Collected) -> Result<Step, ProviderError> {
        match self {
            Self::Messages => anthropic::parse(data, collected),
            Self::Responses => responses::parse(data, collected),
            Self::ChatCompletions => chat_completions::parse(data, collected),
        }
    }
}

/// The most tool calls one answer of the model may hold. Kelpy asks for one at a time.
pub(crate) const MAX_CALLS: usize = 16;
/// The most an answer's tool calls and the blocks sent back with them may hold together: far more than tools take
/// and models reason, so only a runaway stream reaches it. The text is limited by Kelpy as it arrives.
pub(crate) const MAX_COLLECTED: usize = 2 << 20;

/// What a stream builds across its events: the tool calls, and the blocks to send back with them, within
/// [`MAX_CALLS`] and [`MAX_COLLECTED`], checked before anything is kept.
#[derive(Debug, Default)]
struct Collected {
    /// Each call with its index in the stream.
    calls: Vec<(u64, ToolCall)>,
    /// Blocks the model wrote besides text and calls, by index, that the next request must carry back verbatim:
    /// Anthropic's thinking, the Responses API's reasoning.
    replay: Vec<(u64, Value)>,
    /// Bytes kept in `calls` and `replay`.
    size: usize,
}

impl Collected {
    /// The call at `index`: the one under way, or a new one when `id` names another (some servers number every
    /// call 0).
    fn call(&mut self, index: u64, id: Option<&str>) -> Result<&mut ToolCall, ProviderError> {
        let id = id.filter(|id| !id.is_empty());
        let found = self.calls.iter().rposition(|(at, call)| {
            *at == index && id.is_none_or(|id| call.id.is_empty() || call.id == id)
        });
        let position = match found {
            Some(position) => position,
            None if self.calls.len() >= MAX_CALLS => {
                return Err(ProviderError::TooLarge(format!(
                    "the model called more than {MAX_CALLS} tools at once"
                )));
            }
            None => {
                self.calls.push((index, ToolCall::default()));
                self.calls.len() - 1
            }
        };
        if let Some(id) = id {
            self.keep(id.len())?;
        }
        let call = &mut self.calls[position].1;
        if let Some(id) = id {
            id.clone_into(&mut call.id);
        }
        Ok(call)
    }

    /// Adds `piece` to the arguments of the call at `index`.
    fn arguments(
        &mut self,
        index: u64,
        id: Option<&str>,
        piece: &str,
    ) -> Result<(), ProviderError> {
        self.keep(piece.len())?;
        self.call(index, id)?.arguments.push_str(piece);
        Ok(())
    }

    /// The block at `index`, `block` if it starts now.
    fn replay(
        &mut self,
        index: u64,
        block: Option<&Value>,
    ) -> Result<Option<&mut Value>, ProviderError> {
        if let Some(block) = block {
            self.keep(block.to_string().len())?;
            self.replay.push((index, block.clone()));
        }
        Ok(self
            .replay
            .iter_mut()
            .rfind(|(at, _)| *at == index)
            .map(|(_, block)| block))
    }

    /// Counts `bytes` about to be kept, failing past [`MAX_COLLECTED`].
    fn keep(&mut self, bytes: usize) -> Result<(), ProviderError> {
        self.size += bytes;
        if self.size > MAX_COLLECTED {
            return Err(ProviderError::TooLarge(format!(
                "the model's tool calls and reasoning passed {} MiB",
                MAX_COLLECTED >> 20
            )));
        }
        Ok(())
    }

    /// The calls in order, each with an id: a server that gave none gets one, which the next request names.
    fn finish(self) -> (Vec<ToolCall>, Vec<Value>) {
        let calls = self
            .calls
            .into_iter()
            .enumerate()
            .map(|(number, (_, mut call))| {
                if call.id.is_empty() {
                    call.id = format!("call_{number}");
                }
                call
            })
            .collect();
        let replay = self.replay.into_iter().map(|(_, block)| block).collect();
        (calls, replay)
    }
}

/// What one streamed event adds to the answer.
#[derive(Debug, Default, PartialEq, Eq)]
struct Step {
    text: Option<String>,
    /// Why the model stopped, when this event tells.
    stop: Option<StopReason>,
    /// The tokens the request took, when this event reports them.
    input_tokens: Option<u64>,
    /// Whether this event ends the stream.
    finished: bool,
}

impl Step {
    fn finished(stop: Option<StopReason>) -> Self {
        Self {
            stop,
            finished: true,
            ..Self::default()
        }
    }
}

/// Everything needed to call a model.
#[derive(Clone, PartialEq, Eq)]
pub struct Settings {
    pub kind: Kind,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    /// Headers that requests carry besides the key, such as the ChatGPT account the session belongs to.
    pub headers: Vec<(&'static str, String)>,
}

impl fmt::Debug for Settings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Settings")
            .field("kind", &self.kind)
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<hidden>"))
            .field("headers", &self.headers)
            .finish()
    }
}

impl Settings {
    fn base_url(&self) -> Result<&str, ProviderError> {
        self.base_url
            .as_deref()
            .or(self.kind.default_base_url())
            .ok_or(ProviderError::MissingBaseUrl)
    }
}

/// An image a message shows the model.
#[derive(Debug, PartialEq, Eq)]
pub struct Image {
    /// `image/png` or `image/jpeg`.
    pub media_type: &'static str,
    /// The image, in Base64.
    pub data: String,
}

impl Image {
    /// The image as a `data:` URL, as OpenAI's APIs take it.
    fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.media_type, self.data)
    }
}

/// The images of a user's message, for a model that sees them, and the message's text that goes with them: without
/// what stands in for them in its text alone, such as the text read from them.
#[derive(Debug, Clone, PartialEq)]
pub struct Images {
    pub text: String,
    pub images: Vec<Arc<Image>>,
}

/// A message of the conversation, as the model reads it.
#[derive(Debug, Clone, PartialEq)]
pub enum ChatMessage {
    /// The user's message: its text alone, and its images with their own text when the model sees them.
    User {
        text: String,
        images: Option<Images>,
    },
    Assistant {
        text: String,
        /// The tools the model called after the text.
        calls: Vec<ToolCall>,
        /// What the model wrote besides, to send back as it came (see [`Outcome::replay`]).
        replay: Vec<Value>,
    },
    /// The result of a call of the message before.
    Tool(ToolResult),
}

impl ChatMessage {
    pub fn user(text: impl Into<String>) -> Self {
        Self::User {
            text: text.into(),
            images: None,
        }
    }

    /// A user's message as sent: its text, and the images it shows, if any.
    fn user_parts<'m>(text: &'m str, images: Option<&'m Images>) -> (&'m str, &'m [Arc<Image>]) {
        match images {
            Some(images) => (&images.text, &images.images),
            None => (text, &[]),
        }
    }

    /// The message without images: its text alone.
    fn without_images(&self) -> Self {
        match self {
            Self::User { text, .. } => Self::user(text.clone()),
            other => other.clone(),
        }
    }

    fn has_images(&self) -> bool {
        matches!(self, Self::User { images: Some(images), .. } if !images.images.is_empty())
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self::Assistant {
            text: text.into(),
            calls: Vec::new(),
            replay: Vec::new(),
        }
    }
}

/// A tool the model may call: what it does, for the model to choose, and the JSON Schema of its arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// A call of a tool, as the model asked for it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolCall {
    /// How the result names the call.
    pub id: String,
    pub name: String,
    /// JSON, as the model wrote it: it may be invalid.
    pub arguments: String,
}

/// What a call gave, for the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub call_id: String,
    pub output: String,
    /// The call failed or was refused: `output` says why.
    pub is_error: bool,
}

/// One turn of a conversation: the instructions, the messages so far and the tools the model may call.
#[derive(Debug, Clone, Copy)]
pub struct ChatRequest<'a> {
    pub system: &'a str,
    pub messages: &'a [ChatMessage],
    pub tools: &'a [ToolSpec],
}

/// How an answer ended.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub stop: StopReason,
    /// The tokens the request took, as the provider counted them, when it says.
    pub input_tokens: Option<u64>,
    /// The tools the model called, in order: their results go into the next request.
    pub calls: Vec<ToolCall>,
    /// What the next request must carry back with the calls, in the assistant's message.
    pub replay: Vec<Value>,
    /// The service refused the tools, so the request was sent again without them: this model cannot use tools.
    pub tools_refused: bool,
    /// The service refused the images, so the request was sent again with the messages' text alone: this model
    /// cannot see images.
    pub images_refused: bool,
}

/// Why the model stopped writing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    Complete,
    /// The answer reached the output limit.
    TooLong,
    /// The model, or the provider's safety system, declined to answer.
    Refused,
    /// The provider ended the answer early, for a reason Kelpy does not know.
    Incomplete(String),
    /// The answer is over, with a reason Kelpy does not know.
    Other(String),
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("no API key: {}", match variable {
        Some(variable) => format!("set {variable}, enter one in the Web UI or run `kelpy provider`"),
        None => "enter one in the Web UI or run `kelpy provider`".to_owned(),
    })]
    MissingKey { variable: Option<&'static str> },
    #[error("an OpenAI-compatible provider needs the address of its API")]
    MissingBaseUrl,
    #[error("cannot reach the provider: {0}")]
    Network(String),
    #[error("the provider answered{}: {message}", status.map(|status| format!(" {status}")).unwrap_or_default())]
    Api {
        status: Option<u16>,
        message: String,
    },
    #[error("unexpected response from the provider: {0}")]
    Protocol(String),
    /// The answer went past one of Kelpy's limits.
    #[error("the answer is larger than Kelpy accepts: {0}")]
    TooLarge(String),
}

impl ProviderError {
    /// Whether the provider refused the request for being longer than the model's context window. Each service
    /// words it its own way, so this reads the message.
    pub fn is_context_overflow(&self) -> bool {
        const SIGNS: &[&str] = &[
            "context_length_exceeded",
            "context length",
            "context window",
            "maximum context",
            "prompt is too long",
            "input is too long",
            "too many tokens",
            "reduce the length",
        ];
        let Self::Api { status, message } = self else {
            return false;
        };
        let message = message.to_lowercase();
        status.is_none_or(|status| (400..500).contains(&status))
            && SIGNS.iter().any(|sign| message.contains(sign))
    }

    /// Whether the service refused the request for offering tools, which the model or the server does not support.
    fn refuses_tools(&self) -> bool {
        let Self::Api {
            status: Some(400..500),
            message,
        } = self
        else {
            return false;
        };
        let message = message.to_lowercase();
        message.contains("tool")
            && ["support", "requires", "not allowed"]
                .iter()
                .any(|sign| message.contains(sign))
    }

    /// Whether the service refused the request for showing images, which the model or the server does not take.
    /// Servers for text alone also refuse a message's content given as parts.
    fn refuses_images(&self) -> bool {
        let Self::Api {
            status: Some(400..500),
            message,
        } = self
        else {
            return false;
        };
        let message = message.to_lowercase();
        ["image", "vision", "multimodal", "must be a string"]
            .iter()
            .any(|sign| message.contains(sign))
    }

    /// Whether a Chat Completions server refused the request for asking it to report usage, which strict servers
    /// do not know.
    fn refuses_usage(&self) -> bool {
        matches!(self, Self::Api { status: Some(400..500), message }
            if message.contains("stream_options") || message.contains("include_usage"))
    }
}

/// The HTTP client shared by every request. Cloning shares the same connections.
#[derive(Clone)]
pub struct Provider {
    client: reqwest::Client,
}

/// Installs the TLS backend of every HTTP client in the program: rustls with `ring`, which builds everywhere
/// without extra tools. Called before building any `reqwest` client, even one for plain HTTP.
pub(crate) fn install_tls() {
    // Fails only if a backend is already installed, which is just as good.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Builds an HTTP client, every one in the program. Servers are checked against the system's certificate
/// authorities, so a company's own authority is trusted too. A system without any (a minimal container) gets
/// Mozilla's, built into Kelpy: otherwise building fails, even a client that only speaks plain HTTP.
pub fn http_client(
    configure: impl Fn(reqwest::ClientBuilder) -> reqwest::ClientBuilder,
) -> reqwest::Result<reqwest::Client> {
    install_tls();
    configure(reqwest::Client::builder())
        .build()
        .or_else(|error| {
            // The authorities are the only difference, so if this works they were the reason.
            let mozilla = webpki_root_certs::TLS_SERVER_ROOT_CERTS
                .iter()
                .filter_map(|der| reqwest::Certificate::from_der(der).ok());
            configure(reqwest::Client::builder())
                .tls_certs_only(mozilla)
                .build()
                .map_err(|_| error)
        })
}

impl Provider {
    pub fn new() -> Result<Self, ProviderError> {
        let client = http_client(|builder| {
            builder
                .user_agent(concat!("Kelpy/", env!("CARGO_PKG_VERSION")))
                .connect_timeout(Duration::from_secs(20))
                // Between two pieces of a streamed answer; providers send keep-alive events while the model thinks.
                .read_timeout(Duration::from_secs(300))
        })
        .map_err(|error| ProviderError::Network(error.to_string()))?;
        Ok(Self { client })
    }

    /// Streams the answer to `chat`, calling `on_text` with each piece of text as it arrives. The tools called come
    /// with the outcome, once the answer is complete.
    ///
    /// Dropping the returned future cancels the request.
    pub async fn stream(
        &self,
        settings: &Settings,
        chat: &ChatRequest<'_>,
        mut on_text: impl FnMut(&str) + Send,
    ) -> Result<Outcome, ProviderError> {
        let protocol = settings.kind.protocol();
        // A service that refuses the tools or the images is asked again without them.
        let mut tools = chat.tools;
        let mut text_alone: Option<Vec<ChatMessage>> = None;
        let response = loop {
            let request = ChatRequest {
                messages: text_alone.as_deref().unwrap_or(chat.messages),
                tools,
                ..*chat
            };
            match self.send_reporting(settings, &request).await {
                Err(error) if !tools.is_empty() && error.refuses_tools() => tools = &[],
                Err(error)
                    if text_alone.is_none()
                        && chat.messages.iter().any(ChatMessage::has_images)
                        && error.refuses_images() =>
                {
                    text_alone = Some(
                        chat.messages
                            .iter()
                            .map(ChatMessage::without_images)
                            .collect(),
                    );
                }
                response => break response?,
            }
        };
        let tools_refused = tools.is_empty() && !chat.tools.is_empty();
        let images_refused = text_alone.is_some();

        let mut parser = sse::Parser::default();
        let mut bytes = response.bytes_stream();
        let mut collected = Collected::default();
        let mut stop = None;
        let mut input_tokens = None;
        let mut finished = false;
        'read: while let Some(chunk) = bytes.next().await {
            let chunk = chunk.map_err(network_error)?;
            for event in parser.push(&chunk)? {
                let step = protocol.parse(&event.data, &mut collected)?;
                if let Some(text) = &step.text {
                    on_text(text);
                }
                if step.stop.is_some() {
                    stop = step.stop;
                }
                if step.input_tokens.is_some() {
                    input_tokens = step.input_tokens;
                }
                if step.finished {
                    finished = true;
                    break 'read;
                }
            }
        }
        // Some compatible servers end the stream without `[DONE]` after the finish reason.
        if !finished && (stop.is_none() || protocol != Protocol::ChatCompletions) {
            return Err(ProviderError::Network(
                "the connection closed before the answer was complete".into(),
            ));
        }
        let (calls, replay) = collected.finish();
        Ok(Outcome {
            stop: stop.unwrap_or(StopReason::Complete),
            input_tokens,
            calls,
            replay,
            tools_refused,
            images_refused,
        })
    }

    /// Sends the request for `chat`, asking a Chat Completions server for the tokens used unless it refuses to
    /// tell.
    async fn send_reporting(
        &self,
        settings: &Settings,
        chat: &ChatRequest<'_>,
    ) -> Result<reqwest::Response, ProviderError> {
        match self.send(settings, chat, true).await {
            Err(error)
                if settings.kind.protocol() == Protocol::ChatCompletions
                    && error.refuses_usage() =>
            {
                self.send(settings, chat, false).await
            }
            response => response,
        }
    }

    /// Sends the request for `chat` and returns the response once it has succeeded.
    async fn send(
        &self,
        settings: &Settings,
        chat: &ChatRequest<'_>,
        report_usage: bool,
    ) -> Result<reqwest::Response, ProviderError> {
        let base_url = settings.base_url()?;
        let key = settings.api_key.as_deref().filter(|key| !key.is_empty());
        if settings.kind.requires_key() && key.is_none() {
            return Err(ProviderError::MissingKey {
                variable: settings.kind.key_variable(),
            });
        }
        let request = settings.kind.protocol().request(
            settings.kind,
            &self.client,
            base_url,
            key,
            &settings.model,
            chat,
            report_usage,
        );
        let request = settings
            .headers
            .iter()
            .fold(request, |request, (name, value)| {
                request.header(*name, value)
            });
        let response = request.send().await.map_err(network_error)?;
        if !response.status().is_success() {
            return Err(api_error(response).await);
        }
        Ok(response)
    }
}

/// An error answer, with its status and the message in its body.
async fn api_error(response: reqwest::Response) -> ProviderError {
    let status = response.status().as_u16();
    ProviderError::Api {
        status: Some(status),
        message: error_message(&body::error_text(response).await),
    }
}

fn network_error(error: reqwest::Error) -> ProviderError {
    let mut message = error.to_string();
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        message = format!("{message}: {cause}");
        source = cause.source();
    }
    ProviderError::Network(message)
}

/// What a ChatGPT plan's refusals mean for the user, by their `error.code`.
fn plan_error(code: &str) -> Option<&'static str> {
    Some(match code {
        "subscription_sharing_usage_limit_exceeded" => {
            "the ChatGPT plan's usage limit for Kelpy has been reached: see https://chatgpt.com/settings/usage"
        }
        "subscription_sharing_usage_unavailable" | "subscription_sharing_user_unavailable" => {
            "the ChatGPT plan cannot be used right now: try again in a moment"
        }
        "subscription_sharing_user_not_eligible" => {
            "this ChatGPT plan cannot pay for Kelpy's answers"
        }
        _ => return None,
    })
}

/// The readable part of an error response: a ChatGPT plan's refusal, `error.message`, `error`, `message` or
/// `detail`, else the start of the body.
fn error_message(body: &str) -> String {
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(body) {
        let message = json["error"]["code"]
            .as_str()
            .and_then(plan_error)
            .or_else(|| json["error"]["message"].as_str())
            .or_else(|| json["error"].as_str())
            .or_else(|| json["message"].as_str())
            .or_else(|| json["detail"].as_str());
        if let Some(message) = message {
            return message.to_owned();
        }
    }
    let body = body.trim();
    if body.is_empty() {
        return "no details".to_owned();
    }
    body.chars().take(300).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kelpy_test_support::serve_once;

    fn settings(kind: Kind, base_url: String) -> Settings {
        Settings {
            kind,
            model: "test-model".into(),
            base_url: Some(base_url),
            api_key: Some("key".into()),
            headers: Vec::new(),
        }
    }

    fn chat() -> Vec<ChatMessage> {
        vec![ChatMessage::user("Hello")]
    }

    fn disks() -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "disk_usage".into(),
            description: "Shows the disks.".into(),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
        }]
    }

    #[test]
    fn an_openai_key_is_never_offered_to_other_services() {
        assert_eq!(Kind::OpenAiCompatible.key_variable(), None);
        assert!(Kind::OpenAiCompatible.accepts_key());
        assert_eq!(Kind::OpenAi.key_variable(), Some("OPENAI_API_KEY"));
    }

    #[tokio::test]
    async fn anthropic_streams_are_read_to_the_end() {
        let base = serve_once(
            "200 OK",
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":12,\"cache_read_input_tokens\":30}}}\n\n\
             event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n\
             event: ping\ndata: {\"type\":\"ping\"}\n\n\
             event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n\n\
             event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"}}\n\n\
             event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        )
        .await;
        let messages = chat();
        let request = ChatRequest {
            system: "",
            messages: &messages,
            tools: &[],
        };
        let mut text = String::new();
        let outcome = Provider::new()
            .unwrap()
            .stream(&settings(Kind::Anthropic, base), &request, |piece| {
                text.push_str(piece)
            })
            .await
            .unwrap();
        assert_eq!(text, "Hello");
        assert_eq!(outcome.stop, StopReason::TooLong);
        assert_eq!(outcome.input_tokens, Some(42));
    }

    #[tokio::test]
    async fn openai_compatible_streams_are_read_to_the_end() {
        let base = serve_once(
            "200 OK",
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"Ciao\"},\"finish_reason\":null}]}\n\n\
             data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
             data: {\"choices\":[],\"usage\":{\"prompt_tokens\":17,\"completion_tokens\":2}}\n\n\
             data: [DONE]\n\n",
        )
        .await;
        let messages = chat();
        let request = ChatRequest {
            system: "Be brief.",
            messages: &messages,
            tools: &[],
        };
        let mut text = String::new();
        let outcome = Provider::new()
            .unwrap()
            .stream(&settings(Kind::Ollama, base), &request, |piece| {
                text.push_str(piece)
            })
            .await
            .unwrap();
        assert_eq!(text, "Ciao");
        assert_eq!(outcome.stop, StopReason::Complete);
        assert_eq!(outcome.input_tokens, Some(17));
    }

    #[tokio::test]
    async fn openai_responses_streams_are_read_to_the_end() {
        let base = serve_once(
            "200 OK",
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"status\":\"in_progress\"}}\n\n\
             event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Sa\"}\n\n\
             event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"lve\"}\n\n\
             event: response.output_text.done\ndata: {\"type\":\"response.output_text.done\",\"text\":\"Salve\"}\n\n\
             event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":25}}}\n\n",
        )
        .await;
        let messages = chat();
        let request = ChatRequest {
            system: "Be brief.",
            messages: &messages,
            tools: &[],
        };
        let mut text = String::new();
        let outcome = Provider::new()
            .unwrap()
            .stream(&settings(Kind::OpenAi, base), &request, |piece| {
                text.push_str(piece)
            })
            .await
            .unwrap();
        assert_eq!(text, "Salve");
        assert_eq!(outcome.stop, StopReason::Complete);
        assert_eq!(outcome.input_tokens, Some(25));
    }

    #[tokio::test]
    async fn a_responses_stream_without_its_final_event_is_not_complete() {
        let base = serve_once(
            "200 OK",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Half\"}\n\n",
        )
        .await;
        let messages = chat();
        let request = ChatRequest {
            system: "",
            messages: &messages,
            tools: &[],
        };
        let mut text = String::new();
        let error = Provider::new()
            .unwrap()
            .stream(&settings(Kind::OpenAi, base), &request, |piece| {
                text.push_str(piece)
            })
            .await
            .unwrap_err();
        assert_eq!(text, "Half");
        assert!(matches!(error, ProviderError::Network(_)));
    }

    #[tokio::test]
    async fn api_errors_carry_the_providers_message() {
        let base = serve_once(
            "401 Unauthorized",
            "{\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",\"message\":\"invalid x-api-key\"}}",
        )
        .await;
        let messages = chat();
        let request = ChatRequest {
            system: "",
            messages: &messages,
            tools: &[],
        };
        let error = Provider::new()
            .unwrap()
            .stream(&settings(Kind::Anthropic, base), &request, |_| {})
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "the provider answered 401: invalid x-api-key"
        );
    }

    #[tokio::test]
    async fn missing_keys_and_addresses_are_reported_before_any_request() {
        let messages = chat();
        let request = ChatRequest {
            system: "",
            messages: &messages,
            tools: &[],
        };
        let provider = Provider::new().unwrap();
        let mut no_key = settings(Kind::Anthropic, "http://127.0.0.1:9/v1".into());
        no_key.api_key = None;
        let error = provider
            .stream(&no_key, &request, |_| {})
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ProviderError::MissingKey {
                variable: Some("ANTHROPIC_API_KEY")
            }
        ));

        let mut no_address = settings(Kind::OpenAiCompatible, String::new());
        no_address.base_url = None;
        let error = provider
            .stream(&no_address, &request, |_| {})
            .await
            .unwrap_err();
        assert!(matches!(error, ProviderError::MissingBaseUrl));
    }

    #[tokio::test]
    async fn a_server_refusing_to_report_usage_is_asked_again_without() {
        let server = kelpy_test_support::FakeServer::start(vec![(
            "POST /v1/chat/completions",
            "200 OK",
            "data: {\"choices\":[{\"delta\":{\"content\":\"Ok\"},\"finish_reason\":\"stop\"}]}\n\n\
             data: [DONE]\n\n"
                .to_owned(),
        )])
        .await;
        server.respond_once(
            "POST /v1/chat/completions",
            "400 Bad Request",
            r#"{"error":{"message":"Unrecognized request argument supplied: stream_options"}}"#
                .to_owned(),
        );
        let messages = chat();
        let request = ChatRequest {
            system: "",
            messages: &messages,
            tools: &[],
        };
        let base = format!("{}/v1", server.url);
        let mut text = String::new();
        let outcome = Provider::new()
            .unwrap()
            .stream(&settings(Kind::OpenAiCompatible, base), &request, |piece| {
                text.push_str(piece)
            })
            .await
            .unwrap();
        assert_eq!((text.as_str(), outcome.input_tokens), ("Ok", None));
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].contains("\"include_usage\":true"));
        assert!(!requests[1].contains("stream_options"));
    }

    #[tokio::test]
    async fn tool_calls_come_with_the_outcome_and_a_model_without_tools_is_asked_again() {
        let server = kelpy_test_support::FakeServer::start(vec![(
            "POST /v1/chat/completions",
            "200 OK",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"disk_usage\",\"arguments\":\"{}\"}}]}}]}\n\n\
             data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
             data: [DONE]\n\n"
                .to_owned(),
        )])
        .await;
        let base = format!("{}/v1", server.url);
        let (messages, tools) = (chat(), disks());
        let request = ChatRequest {
            system: "",
            messages: &messages,
            tools: &tools,
        };
        let provider = Provider::new().unwrap();
        let settings = settings(Kind::OpenAiCompatible, base);
        let outcome = provider.stream(&settings, &request, |_| {}).await.unwrap();
        assert_eq!(outcome.calls.len(), 1);
        assert_eq!(outcome.calls[0].name, "disk_usage");
        assert!(!outcome.tools_refused);
        assert!(server.requests()[0].contains("\"tools\":[{"));

        server.respond_once(
            "POST /v1/chat/completions",
            "400 Bad Request",
            r#"{"error":{"message":"registry.ollama.ai/library/gemma3:4b does not support tools"}}"#
                .to_owned(),
        );
        let outcome = provider.stream(&settings, &request, |_| {}).await.unwrap();
        assert!(outcome.tools_refused);
        let requests = server.requests();
        assert_eq!(requests.len(), 3);
        assert!(!requests[2].contains("\"tools\""));
    }

    /// A message with a screenshot: its text alone holds the text read from it, which the model seeing it does not
    /// get.
    fn pictured() -> Vec<ChatMessage> {
        vec![ChatMessage::User {
            text: "Why?\n\n[Image: error.png]\nText read from it: Access denied".into(),
            images: Some(Images {
                text: "Why?\n\n[Image: error.png]".into(),
                images: vec![Arc::new(Image {
                    media_type: "image/png",
                    data: "iVBORw0K".into(),
                })],
            }),
        }]
    }

    #[test]
    fn each_protocol_shows_the_images_before_the_text_that_goes_with_them() {
        let messages = pictured();
        let chat = ChatRequest {
            system: "",
            messages: &messages,
            tools: &[],
        };
        let client = Provider::new().unwrap().client;
        let body = |kind: Kind| -> Value {
            let built = kind
                .protocol()
                .request(
                    kind,
                    &client,
                    kind.default_base_url().unwrap_or("http://127.0.0.1:1/v1"),
                    Some("key"),
                    "model",
                    &chat,
                    false,
                )
                .build()
                .unwrap();
            serde_json::from_slice(built.body().unwrap().as_bytes().unwrap()).unwrap()
        };
        let url = "data:image/png;base64,iVBORw0K";

        let anthropic = &body(Kind::Anthropic)["messages"][0]["content"];
        assert_eq!(anthropic[0]["type"], "image");
        assert_eq!(anthropic[0]["source"]["media_type"], "image/png");
        assert_eq!(anthropic[0]["source"]["data"], "iVBORw0K");
        assert_eq!(anthropic[1]["text"], "Why?\n\n[Image: error.png]");

        let responses = body(Kind::OpenAi);
        let content = &responses["input"][0]["content"];
        assert_eq!(content[0]["type"], "input_image");
        assert_eq!(content[0]["image_url"], url);
        assert_eq!(content[1]["text"], "Why?\n\n[Image: error.png]");

        let completions = &body(Kind::Ollama)["messages"][0]["content"];
        assert_eq!(completions[0]["image_url"]["url"], url);
        assert_eq!(completions[1]["text"], "Why?\n\n[Image: error.png]");
    }

    #[tokio::test]
    async fn a_service_refusing_images_is_asked_again_with_the_text_alone() {
        let server = kelpy_test_support::FakeServer::start(vec![(
            "POST /v1/chat/completions",
            "200 OK",
            "data: {\"choices\":[{\"delta\":{\"content\":\"Ok\"},\"finish_reason\":\"stop\"}]}\n\n\
             data: [DONE]\n\n"
                .to_owned(),
        )])
        .await;
        server.respond_once(
            "POST /v1/chat/completions",
            "400 Bad Request",
            r#"{"error":{"message":"image input is not supported - hint: you may need to provide the mmproj"}}"#
                .to_owned(),
        );
        let messages = pictured();
        let request = ChatRequest {
            system: "",
            messages: &messages,
            tools: &[],
        };
        let base = format!("{}/v1", server.url);
        let outcome = Provider::new()
            .unwrap()
            .stream(&settings(Kind::OpenAiCompatible, base), &request, |_| {})
            .await
            .unwrap();
        assert!(outcome.images_refused);
        assert!(!outcome.tools_refused);
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].contains("image_url"));
        assert!(!requests[1].contains("image_url"));
        // As a string, with the text read from the image.
        assert!(requests[1].contains(
            r#""content":"Why?\n\n[Image: error.png]\nText read from it: Access denied""#
        ));
    }

    #[test]
    fn context_overflows_are_recognised_in_each_services_words() {
        let api = |status, message: &str| ProviderError::Api {
            status,
            message: message.to_owned(),
        };
        for overflow in [
            api(
                Some(400),
                "prompt is too long: 210000 tokens > 200000 maximum",
            ),
            api(
                Some(400),
                "This model's maximum context length is 128000 tokens. However, your messages resulted in 130000 tokens.",
            ),
            api(None, "Your input exceeds the context window of this model."),
            api(Some(413), "Request too large: too many tokens"),
        ] {
            assert!(overflow.is_context_overflow(), "{overflow}");
        }
        for other in [
            api(Some(401), "invalid x-api-key"),
            api(Some(500), "maximum context length exceeded"),
            ProviderError::Network("context window".to_owned()),
        ] {
            assert!(!other.is_context_overflow(), "{other}");
        }
    }

    #[test]
    fn error_bodies_are_read_in_the_users_terms() {
        let limit = r#"{"error":{"code":"subscription_sharing_usage_limit_exceeded","message":"Usage limit reached"}}"#;
        assert!(error_message(limit).contains("https://chatgpt.com/settings/usage"));
        assert_eq!(
            error_message(r#"{"detail":"Unauthorized"}"#),
            "Unauthorized"
        );
        assert_eq!(
            error_message(r#"{"error":{"message":"bad model"}}"#),
            "bad model"
        );
        assert_eq!(error_message("  "), "no details");
    }

    #[test]
    fn settings_never_print_the_key() {
        let printed = format!("{:?}", settings(Kind::OpenAi, String::new()));
        assert!(!printed.contains("\"key\""));
        assert!(printed.contains("<hidden>"));
    }
}
