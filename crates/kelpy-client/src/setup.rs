//! The provider setup's questions, as the runtime describes them, and its requests: one flow for its two front ends,
//! the questions of `kelpy provider` in a terminal and the setup window of the terminal UI. Both ask the same
//! questions in the same order, from the same choices; each only asks them its own way.
//!
//! The order: how Kelpy connects to AI ([`ProviderInfo::ways`]), the service when the way has more than one, then
//! - with a key: the address when the service has none ([`asks_address`]), the key ([`key_question`]) and the model
//!   ([`model_options`]);
//! - with a ChatGPT plan: the sign-in, unless a session is open ([`SignInMethod`], [`KEEP_WARNING`]), then the model;
//! - with Ollama: its address, its installation if it is missing, and the model ([`ollama_options`]), downloaded if
//!   it is not there yet.

use futures_util::Stream;
use reqwest::Method;
use serde::Deserialize;
use serde_json::json;

use crate::{Client, ClientError};

/// How long a sign-in as an agent may take in the browser, as the runtime allows.
pub const SIGN_IN_MINUTES: u64 = 10;
/// The most models listed at once in a terminal: a longer list is searched by part of the name instead.
pub const SHOWN_MODELS: usize = 20;
/// What saving a ChatGPT sign-in on this computer means, before the administrator chooses.
pub const KEEP_WARNING: &str = "Kelpy can save the sign-in on this computer, so that ChatGPT works again after a \
    restart. It is then saved in the Kelpy folder (data/), readable only by you: anyone who can read it as you or as \
    an administrator can use your ChatGPT plan until you sign out. Not recommended on a server you hand over to \
    others.";
/// Where Ollama is downloaded when Kelpy cannot install it.
pub const OLLAMA_DOWNLOAD: &str = "https://ollama.com/download";

/// The provider in use and the setup's choices, as `GET /api/provider` describes them.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProviderInfo {
    pub configured: Option<Configured>,
    /// What the provider still needs to answer: `sign_in` or `key`.
    #[serde(default)]
    pub needs: Option<String>,
    #[serde(default)]
    pub kinds: Vec<KindInfo>,
    #[serde(default)]
    pub ways: Vec<WayInfo>,
    #[serde(default)]
    pub services: Vec<Service>,
    #[serde(default)]
    pub sign_in_methods: Vec<SignInMethod>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Configured {
    pub kind: String,
    pub model: String,
    pub service: Option<String>,
    /// The address given, for a service without one of its own.
    #[serde(default)]
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct KindInfo {
    pub id: String,
    pub default_base_url: Option<String>,
    pub key_variable: Option<String>,
    pub accepts_key: bool,
    pub requires_key: bool,
    pub key_in_environment: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WayInfo {
    pub id: String,
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Service {
    pub id: String,
    pub name: String,
    pub way: String,
    pub kind: String,
    pub base_url: Option<String>,
    pub description: String,
    pub key_page: Option<String>,
    pub steps: Vec<String>,
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SignInMethod {
    pub id: String,
    pub name: String,
    pub pros: String,
    pub cons: String,
    pub recommended: String,
}

impl SignInMethod {
    /// When to choose it, its pros and its cons, a line each.
    pub fn details(&self) -> [String; 3] {
        [
            format!("Recommended {}", self.recommended),
            format!("Pros. {}", self.pros),
            format!("Cons. {}", self.cons),
        ]
    }
}

impl ProviderInfo {
    /// The services of `way`, in the order shown.
    pub fn services_of(&self, way: &str) -> Vec<&Service> {
        self.services
            .iter()
            .filter(|service| service.way == way)
            .collect()
    }

    pub fn service(&self, id: &str) -> Option<&Service> {
        self.services.iter().find(|service| service.id == id)
    }

    /// The kind of provider `service` is.
    pub fn kind_of(&self, service: &Service) -> Option<&KindInfo> {
        self.kinds.iter().find(|kind| kind.id == service.kind)
    }

    /// The service in use: the one configured, or the first of its kind.
    pub fn configured_service(&self) -> Option<&Service> {
        let configured = self.configured.as_ref()?;
        configured
            .service
            .as_deref()
            .and_then(|id| self.service(id))
            .or_else(|| {
                self.services
                    .iter()
                    .find(|service| service.kind == configured.kind)
            })
    }

    /// The provider in use, as `claude-sonnet-5-5 (Anthropic (Claude))`.
    pub fn in_use(&self) -> Option<String> {
        let configured = self.configured.as_ref()?;
        let service = self
            .configured_service()
            .map_or(configured.kind.as_str(), |service| service.name.as_str());
        Some(format!("{} ({service})", configured.model))
    }
}

/// Whether the API address is asked: the service and its kind have none.
pub fn asks_address(service: &Service, kind: &KindInfo) -> bool {
    service.base_url.is_none() && kind.default_base_url.is_none()
}

/// Whether `address` can be an API address.
pub fn is_address(address: &str) -> bool {
    address.starts_with("http://") || address.starts_with("https://")
}

/// How the API key is asked, if it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyQuestion {
    /// What follows "API key, hidden".
    pub note: Option<String>,
    /// Whether an empty answer is refused.
    pub needed: bool,
}

/// How the key of `service` is asked; none if its kind takes no key.
pub fn key_question(service: &Service, kind: &KindInfo) -> Option<KeyQuestion> {
    if !kind.accepts_key {
        return None;
    }
    let from_environment = kind
        .key_variable
        .as_deref()
        .filter(|_| kind.key_in_environment);
    let note = match from_environment {
        Some(variable) => Some(format!("empty to use {variable}")),
        None if kind.requires_key || service.way != "local" => None,
        None => Some("empty if the server needs none".to_owned()),
    };
    Some(KeyQuestion {
        note,
        needed: from_environment.is_none() && (kind.requires_key || service.way == "free"),
    })
}

/// The models a setup offers, as the runtime lists them.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelList {
    pub models: Vec<ModelChoice>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelChoice {
    pub id: String,
    pub recommended: bool,
}

impl ModelList {
    /// The service's suggested models, when its own list could not be read, and why.
    pub fn suggested(service: &Service, error: String) -> Self {
        Self {
            models: service
                .models
                .iter()
                .map(|id| ModelChoice {
                    id: id.clone(),
                    recommended: true,
                })
                .collect(),
            error: Some(error),
        }
    }
}

/// The models offered: the recommended ones, then the others the service lists.
pub fn model_options(list: ModelList) -> (Vec<String>, Vec<String>) {
    let (recommended, others): (Vec<ModelChoice>, Vec<ModelChoice>) = list
        .models
        .into_iter()
        .partition(|choice| choice.recommended);
    let ids = |choices: Vec<ModelChoice>| choices.into_iter().map(|choice| choice.id).collect();
    (ids(recommended), ids(others))
}

/// The models whose name contains `term`, ignoring case.
pub fn matching<'a>(models: &'a [String], term: &str) -> Vec<&'a String> {
    let term = term.to_lowercase();
    models
        .iter()
        .filter(|model| model.to_lowercase().contains(&term))
        .collect()
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatGptStatus {
    pub signed_in: Option<Account>,
    pub registered: Option<String>,
    pub kept: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Account {
    pub email: Option<String>,
}

impl Account {
    pub fn name(&self) -> &str {
        self.email.as_deref().unwrap_or("your ChatGPT account")
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatGptModel {
    pub slug: String,
    pub display_name: String,
}

/// An event of a sign-in with a code.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CodeEvent {
    Code {
        verification_url: String,
        user_code: String,
    },
    SignedIn {
        account: Account,
    },
    Failed {
        error: String,
    },
}

/// Whether a ChatGPT session is saved on disk, after the account it is signed in as: if not, it ends when Kelpy stops.
pub fn lifetime(kept: bool) -> &'static str {
    if kept {
        "(saved - log out to clear)"
    } else {
        "(not saved - close to clear)"
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct OllamaStatus {
    pub base_url: String,
    pub running: bool,
    pub error: Option<String>,
    pub installed: bool,
    pub models: Vec<LocalModel>,
    pub memory: Option<u64>,
    pub suggestions: Vec<OllamaSuggestion>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LocalModel {
    pub name: String,
    pub size: u64,
    #[serde(default)]
    pub cloud: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OllamaSuggestion {
    pub name: String,
    pub size: u64,
    pub description: String,
}

impl OllamaStatus {
    /// Why no model can be chosen yet, if Ollama does not answer.
    pub fn problem(&self) -> Option<OllamaProblem> {
        if self.running {
            None
        } else if self.installed {
            Some(OllamaProblem::NotAnswering)
        } else if self.memory.is_none() {
            // Elsewhere: whether it is installed there is not known.
            Some(OllamaProblem::Unreachable)
        } else {
            Some(OllamaProblem::NotInstalled)
        }
    }

    /// What is wrong, in a sentence.
    pub fn explain(&self) -> Option<String> {
        Some(match self.problem()? {
            OllamaProblem::NotAnswering => format!(
                "Ollama is installed but does not answer at {}. Start it.",
                self.base_url
            ),
            OllamaProblem::Unreachable => format!(
                "No Ollama answers at {}: {}",
                self.base_url,
                self.error.as_deref().unwrap_or("no details")
            ),
            OllamaProblem::NotInstalled => "Ollama is not installed on this computer.".to_owned(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OllamaProblem {
    /// Installed here, not running.
    NotAnswering,
    /// Nothing answers at another address.
    Unreachable,
    /// Not installed on this computer: Kelpy may install it.
    NotInstalled,
}

/// A model to choose with Ollama: installed, or to download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OllamaOption {
    pub name: String,
    /// Its size, `6.6 GB`, or where it runs.
    pub detail: String,
    pub description: Option<String>,
    /// Downloaded once chosen.
    pub download: bool,
}

/// The models installed, then those suggested for this computer's memory that are not.
pub fn ollama_options(status: &OllamaStatus) -> Vec<OllamaOption> {
    let mut options: Vec<OllamaOption> = status
        .models
        .iter()
        .map(|model| OllamaOption {
            name: model.name.clone(),
            detail: if model.cloud {
                "runs on ollama.com".to_owned()
            } else {
                gigabytes(model.size)
            },
            description: None,
            download: false,
        })
        .collect();
    let downloadable: Vec<OllamaOption> = status
        .suggestions
        .iter()
        .filter(|suggestion| {
            status
                .models
                .iter()
                .all(|model| model.name != suggestion.name)
        })
        .map(|suggestion| OllamaOption {
            name: suggestion.name.clone(),
            detail: format!("download {}", gigabytes(suggestion.size)),
            description: Some(suggestion.description.clone()),
            download: true,
        })
        .collect();
    options.extend(downloadable);
    options
}

/// How Ollama would be installed, as `POST /api/ollama/install` shows it.
#[derive(Debug, Clone, Deserialize)]
pub struct Installer {
    pub command: String,
    pub notes: Vec<String>,
}

/// An installation's progress: the output so far each time.
#[derive(Debug, Clone, Deserialize)]
pub struct Installation {
    pub output: String,
    pub done: bool,
    pub error: Option<String>,
}

/// A download's progress.
#[derive(Debug, Clone, Deserialize)]
pub struct Download {
    pub status: String,
    pub completed: u64,
    pub total: u64,
    pub done: bool,
    pub error: Option<String>,
}

impl Download {
    /// `pulling 2.9 GB of 6.6 GB`, as far as known.
    pub fn progress(&self) -> String {
        if self.total > 0 {
            format!(
                "{}  {} of {}",
                self.status,
                gigabytes(self.completed),
                gigabytes(self.total)
            )
        } else {
            self.status.clone()
        }
    }
}

/// What a setup chose, saved with `PUT /api/provider`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chosen {
    pub model: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
}

impl Chosen {
    /// The body of `PUT /api/provider` for `service`.
    pub fn request(&self, service: &Service) -> serde_json::Value {
        json!({
            "kind": service.kind,
            "service": service.id,
            "model": self.model,
            "base_url": self.base_url,
            "api_key": self.api_key,
        })
    }
}

/// The setup's requests to the running Kelpy.
impl Client {
    pub async fn provider_info(&self) -> Result<ProviderInfo, ClientError> {
        self.get("/api/provider").await
    }

    /// Saves the provider: Kelpy keeps the key in memory only.
    pub async fn save_provider(&self, request: &serde_json::Value) -> Result<(), ClientError> {
        let _: serde::de::IgnoredAny = self.send(Method::PUT, "/api/provider", request).await?;
        Ok(())
    }

    /// The models `service` offers, with the address and key entered for it; its suggested ones if its own list
    /// cannot be read.
    pub async fn models(
        &self,
        service: &Service,
        base_url: Option<&str>,
        api_key: Option<&str>,
    ) -> ModelList {
        let query = json!({
            "kind": service.kind, "service": service.id, "base_url": base_url, "api_key": api_key,
        });
        self.send(Method::POST, "/api/provider/models", &query)
            .await
            .unwrap_or_else(|error: ClientError| ModelList::suggested(service, error.to_string()))
    }

    pub async fn chatgpt_status(&self) -> Result<ChatGptStatus, ClientError> {
        self.get("/api/chatgpt").await
    }

    pub async fn chatgpt_models(&self) -> Result<Vec<ChatGptModel>, ClientError> {
        self.get("/api/chatgpt/models").await
    }

    /// Signs in as the Codex CLI: the code to approve, then the outcome. Closing the stream abandons the sign-in.
    pub async fn sign_in_with_code(
        &self,
        keep: bool,
    ) -> Result<impl Stream<Item = Result<String, ClientError>> + use<>, ClientError> {
        self.events(
            Method::POST,
            "/api/chatgpt/code",
            Some(&json!({ "keep": keep })),
        )
        .await
    }

    /// Starts a sign-in as an agent: the OpenAI address to open.
    pub async fn start_sign_in(&self, keep: bool) -> Result<String, ClientError> {
        #[derive(Deserialize)]
        struct Started {
            url: String,
        }
        let started: Started = self
            .send(
                Method::POST,
                "/api/chatgpt/sign-in",
                &json!({ "keep": keep }),
            )
            .await?;
        Ok(started.url)
    }

    /// Completes a sign-in as an agent with the address OpenAI sent the browser back to.
    pub async fn finish_sign_in(&self, address: &str) -> Result<Account, ClientError> {
        let address = reqwest::Url::parse(address.trim())
            .map_err(|_| ClientError::Refused("not a web address".into()))?;
        let param = |name: &str| {
            address
                .query_pairs()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.into_owned())
        };
        self.send(
            Method::POST,
            "/api/chatgpt/callback",
            &json!({
                "code": param("code"),
                "state": param("state"),
                "client_id": param("client_id"),
                "error": param("error"),
            }),
        )
        .await
    }

    /// Forgets the agent registration: the next sign-in as an agent registers again, with any account.
    pub async fn forget_registration(&self) -> Result<(), ClientError> {
        let _: serde::de::IgnoredAny = self.delete("/api/chatgpt/agent").await?;
        Ok(())
    }

    /// Ollama at `address`, or on this computer when it is empty.
    pub async fn ollama(&self, address: &str) -> Result<OllamaStatus, ClientError> {
        let query = if address.is_empty() {
            String::new()
        } else {
            format!("?base_url={}", form_encode(address))
        };
        self.get(&format!("/api/ollama{query}")).await
    }

    /// How Kelpy would install Ollama; an error says why it cannot.
    pub async fn ollama_installer(&self) -> Result<Installer, ClientError> {
        self.send(
            Method::POST,
            "/api/ollama/install",
            &json!({ "preview": true }),
        )
        .await
    }

    /// Installs Ollama: its [`Installation`] at each change. It goes on if the stream closes.
    pub async fn install_ollama(
        &self,
    ) -> Result<impl Stream<Item = Result<String, ClientError>> + use<>, ClientError> {
        self.events(Method::POST, "/api/ollama/install", Some(&json!({})))
            .await
    }

    /// Downloads `model`: its [`Download`] at each change. It goes on if the stream closes.
    pub async fn download_model(
        &self,
        base_url: &str,
        model: &str,
    ) -> Result<impl Stream<Item = Result<String, ClientError>> + use<>, ClientError> {
        let body = json!({ "base_url": base_url, "model": model });
        self.events(Method::POST, "/api/ollama/download", Some(&body))
            .await
    }
}

/// `text` as a QR code in text, light on dark as most terminals show it, with its quiet zone, two modules a line; empty
/// if it does not fit in one.
pub fn qr_code(text: &str) -> String {
    use qrcode::render::unicode::Dense1x2;
    qrcode::QrCode::new(text.as_bytes())
        .map(|code| {
            code.render::<Dense1x2>()
                .dark_color(Dense1x2::Light)
                .light_color(Dense1x2::Dark)
                .build()
        })
        .unwrap_or_default()
}

/// A size in bytes, in gigabytes with one decimal: `6.6 GB`.
pub fn gigabytes(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

/// Encodes `text` for a URL's query.
pub fn form_encode(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(accepts: bool, requires: bool, variable: Option<&str>, set: bool) -> KindInfo {
        KindInfo {
            id: "openai_compatible".into(),
            default_base_url: None,
            key_variable: variable.map(str::to_owned),
            accepts_key: accepts,
            requires_key: requires,
            key_in_environment: set,
        }
    }

    fn service(way: &str) -> Service {
        Service {
            id: "x".into(),
            name: "X".into(),
            way: way.into(),
            kind: "openai_compatible".into(),
            base_url: None,
            description: String::new(),
            key_page: None,
            steps: Vec::new(),
            models: vec!["m1".into()],
        }
    }

    #[test]
    fn the_key_is_asked_as_the_service_and_the_environment_allow() {
        assert_eq!(
            key_question(&service("key"), &kind(false, false, None, false)),
            None
        );
        let free = key_question(&service("free"), &kind(true, false, None, false)).unwrap();
        assert_eq!((free.note, free.needed), (None, true));
        let local = key_question(&service("local"), &kind(true, false, None, false)).unwrap();
        assert_eq!(
            local.note.as_deref(),
            Some("empty if the server needs none")
        );
        assert!(!local.needed);
        let set = key_question(
            &service("key"),
            &kind(true, true, Some("OPENAI_API_KEY"), true),
        );
        assert_eq!(
            set.map(|question| (question.note, question.needed)),
            Some((Some("empty to use OPENAI_API_KEY".into()), false))
        );
    }

    #[test]
    fn models_are_searched_by_part_of_the_name_ignoring_case() {
        let models =
            ["openai/gpt-oss-120b", "qwen/Qwen3.8-27b", "google/gemma-4"].map(String::from);
        assert_eq!(matching(&models, "QWEN"), [&models[1]]);
        assert_eq!(matching(&models, "/g").len(), 2);
        assert!(matching(&models, "claude").is_empty());
    }

    #[test]
    fn the_recommended_models_come_first_and_the_suggested_ones_stand_in_for_a_missing_list() {
        let list: ModelList = serde_json::from_value(json!({
            "models": [{"id": "a", "recommended": true}, {"id": "b", "recommended": false}], "error": null,
        }))
        .unwrap();
        assert_eq!(model_options(list), (vec!["a".into()], vec!["b".into()]));
        let missing = ModelList::suggested(&service("key"), "401".into());
        assert_eq!(missing.error.as_deref(), Some("401"));
        assert_eq!(model_options(missing).0, ["m1"]);
    }

    #[test]
    fn ollama_offers_its_models_then_those_to_download() {
        let status: OllamaStatus = serde_json::from_value(json!({
            "base_url": "http://127.0.0.1:11434", "running": true, "error": null, "installed": true,
            "models": [{"name": "qwen3.5:4b", "size": 3_400_000_000_u64}, {"name": "gpt-oss:120b-cloud", "size": 0, "cloud": true}],
            "memory": 16_000_000_000_u64,
            "suggestions": [
                {"name": "qwen3.5:4b", "size": 3_400_000_000_u64, "description": "Faster."},
                {"name": "qwen3.5:9b", "size": 6_600_000_000_u64, "description": "Good with tools."},
            ],
        }))
        .unwrap();
        assert_eq!(status.problem(), None);
        let options = ollama_options(&status);
        let shown: Vec<(&str, &str, bool)> = options
            .iter()
            .map(|option| {
                (
                    option.name.as_str(),
                    option.detail.as_str(),
                    option.download,
                )
            })
            .collect();
        assert_eq!(
            shown,
            [
                ("qwen3.5:4b", "3.4 GB", false),
                ("gpt-oss:120b-cloud", "runs on ollama.com", false),
                ("qwen3.5:9b", "download 6.6 GB", true),
            ]
        );
        let missing = OllamaStatus {
            running: false,
            installed: false,
            ..status
        };
        assert_eq!(missing.problem(), Some(OllamaProblem::NotInstalled));
        let elsewhere = OllamaStatus {
            memory: None,
            ..missing
        };
        assert_eq!(elsewhere.problem(), Some(OllamaProblem::Unreachable));
    }

    #[test]
    fn the_provider_in_use_is_named_with_its_service() {
        let info: ProviderInfo = serde_json::from_value(json!({
            "configured": {"kind": "openai_compatible", "service": "groq", "model": "llama"},
            "services": [{"id": "groq", "name": "Groq", "way": "free", "kind": "openai_compatible",
                "base_url": "https://api.groq.com/openai/v1", "description": "", "key_page": null, "steps": [],
                "models": []}],
        }))
        .unwrap();
        assert_eq!(info.in_use().as_deref(), Some("llama (Groq)"));
        assert_eq!(info.services_of("free").len(), 1);
        assert!(ProviderInfo::default().in_use().is_none());
    }

    #[test]
    fn addresses_are_encoded_for_the_query() {
        assert_eq!(
            form_encode("http://10.0.0.5:11434/v1"),
            "http%3A%2F%2F10.0.0.5%3A11434%2Fv1"
        );
    }

    #[test]
    fn qr_codes_are_drawn_as_square_text() {
        let qr = qr_code("https://auth.openai.com/codex/device");
        let lines: Vec<usize> = qr.lines().map(|line| line.chars().count()).collect();
        assert!(lines.len() > 10);
        // Two modules per line: as many lines as half the width, rounded up.
        assert!(lines.iter().all(|width| *width == lines[0]));
        assert_eq!(lines.len(), lines[0].div_ceil(2));
    }

    #[test]
    fn sizes_are_shown_in_gigabytes() {
        assert_eq!(gigabytes(6_600_000_000), "6.6 GB");
    }
}
