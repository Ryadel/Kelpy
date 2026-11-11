//! Provider selection: which service answers, with which key or sign-in, and the models a setup offers.
//!
//! The configured provider and the key entered for it live under one lock, changed together with `config.toml`.
//! A key belongs to the exact endpoint it was entered for, and is never sent to another.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use kelpy_common::lock;
use kelpy_providers::{Kind, Provider, ProviderError, Settings};
use serde::{Deserialize, Serialize};

use super::AgentError;
use crate::chatgpt::ChatGpt;
use crate::config::{self, ProviderConfig};
use crate::ollama::Ollama;
use crate::services::{self, Service, SignInMethod, WayInfo};

/// The most models offered from a service's list, after its recommended ones: OpenRouter lists about 400. A longer
/// list is cut, in name order; any name can still be typed.
const MAX_MODELS: usize = 1000;

/// Where the API key in use comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeySource {
    /// Entered in an interface for this session: kept in memory only.
    Session,
    /// From the environment variable of the provider.
    Environment,
    /// None found; the provider may work without one.
    Missing,
}

/// What the provider still needs from the user before an answer can be sent, so that interfaces ask for it up
/// front: sign-ins and keys entered in an interface last only until Kelpy stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Needed {
    SignIn,
    Key,
}

/// The provider settings shown to the interfaces. Never contains the key.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderInfo {
    pub configured: Option<ProviderConfig>,
    pub key: Option<KeySource>,
    pub needs: Option<Needed>,
    pub kinds: Vec<KindInfo>,
    pub ways: &'static [WayInfo],
    pub services: &'static [Service],
    pub sign_in_methods: &'static [SignInMethod],
}

#[derive(Debug, Clone, Serialize)]
pub struct KindInfo {
    pub id: Kind,
    pub default_model: Option<&'static str>,
    pub default_base_url: Option<&'static str>,
    pub key_variable: Option<&'static str>,
    /// Whether an API key can be entered.
    pub accepts_key: bool,
    pub requires_key: bool,
    /// Whether the credential comes from signing in instead of a key.
    pub signs_in: bool,
    /// Whether the key variable is set in Kelpy's environment.
    pub key_in_environment: bool,
}

/// A change to the provider from an interface.
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderUpdate {
    pub kind: Kind,
    pub model: String,
    #[serde(default)]
    pub base_url: Option<String>,
    /// The service chosen in the setup; its address is used when `base_url` is not given.
    #[serde(default)]
    pub service: Option<String>,
    /// `None` keeps the session key; an empty string forgets it.
    #[serde(default)]
    pub api_key: Option<String>,
}

/// The service of a setup under way, whose models to list: its key may not be saved yet.
#[derive(Clone, Deserialize)]
pub struct ModelQuery {
    pub kind: Kind,
    #[serde(default)]
    pub service: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    /// The key entered in the setup. Without one, the key in use for this very endpoint, or its kind's environment
    /// variable.
    #[serde(default)]
    pub api_key: Option<String>,
}

/// The models a setup offers: the service's recommended ones first, then those it lists, in name order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelList {
    pub models: Vec<ModelChoice>,
    /// Why the service's own list is missing, when it is.
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelChoice {
    pub id: String,
    pub recommended: bool,
}

impl ModelList {
    pub(super) fn new(recommended: &[&str], listed: Result<Vec<String>, ProviderError>) -> Self {
        let mut models: Vec<ModelChoice> = recommended
            .iter()
            .map(|id| ModelChoice {
                id: (*id).to_owned(),
                recommended: true,
            })
            .collect();
        let error = match listed {
            Ok(mut ids) => {
                ids.sort_unstable();
                ids.dedup();
                ids.retain(|id| !recommended.contains(&id.as_str()));
                ids.truncate(MAX_MODELS);
                models.extend(ids.into_iter().map(|id| ModelChoice {
                    id,
                    recommended: false,
                }));
                None
            }
            Err(error) => Some(error.to_string()),
        };
        Self { models, error }
    }
}

/// The provider in use and the services behind it: ChatGPT sign-ins and local Ollama models.
pub(super) struct Providers {
    client: Provider,
    chatgpt: ChatGpt,
    ollama: Arc<Ollama>,
    config_path: PathBuf,
    /// The provider and its session key, changed together with `config.toml` under this one lock.
    selection: Mutex<Selection>,
}

/// The configured provider, and the key entered for it in this session. The key belongs to the exact endpoint it
/// was entered for: changing the provider or its address without a new key forgets it.
struct Selection {
    config: Option<ProviderConfig>,
    key: Option<String>,
}

impl Selection {
    fn key(&self, kind: Kind) -> (Option<String>, KeySource) {
        if let Some(key) = &self.key {
            return (Some(key.clone()), KeySource::Session);
        }
        match environment_key(kind) {
            Some(key) => (Some(key), KeySource::Environment),
            None => (None, KeySource::Missing),
        }
    }
}

impl Providers {
    /// The provider saved in `config_path`, if any. A configuration saved without its service gets the one its
    /// address names.
    pub(super) fn new(
        client: Provider,
        chatgpt: ChatGpt,
        ollama: Arc<Ollama>,
        config_path: PathBuf,
        provider: Option<ProviderConfig>,
    ) -> Self {
        let config = provider.map(|mut config| {
            config.service = config.service.or_else(|| {
                services::infer(config.kind, config.base_url.as_deref())
                    .map(|service| service.id.to_owned())
            });
            config
        });
        Self {
            client,
            chatgpt,
            ollama,
            config_path,
            selection: Mutex::new(Selection { config, key: None }),
        }
    }

    pub(super) fn chatgpt(&self) -> &ChatGpt {
        &self.chatgpt
    }

    /// The key entered for the provider in this session, for the Kelpy restarted in this one's place.
    pub(super) fn handed_over(&self) -> Option<String> {
        lock(&self.selection).key.clone()
    }

    /// Takes over the key the Kelpy before this one had for the same provider.
    pub(super) fn take_over(&self, key: Option<String>) {
        lock(&self.selection).key = key;
    }

    pub(super) fn ollama(&self) -> &Arc<Ollama> {
        &self.ollama
    }

    pub(super) fn info(&self) -> ProviderInfo {
        let signed_in = self.chatgpt.signed_in();
        info(&lock(&self.selection), signed_in)
    }

    /// Saves the provider to `config.toml`; the API key, if given, stays in memory until Kelpy stops. The file
    /// and the state in memory change together, one update at a time.
    pub(super) fn configure(&self, update: ProviderUpdate) -> Result<ProviderInfo, AgentError> {
        let model = update.model.trim();
        if model.is_empty() {
            return Err(AgentError::Invalid("choose a model".into()));
        }
        let (service, base_url) =
            chosen_endpoint(update.kind, update.service.as_deref(), update.base_url)?;
        let service = service.or_else(|| services::infer(update.kind, base_url.as_deref()));
        let config = ProviderConfig {
            kind: update.kind,
            model: model.to_owned(),
            base_url,
            service: service.map(|service| service.id.to_owned()),
        };

        let signed_in = self.chatgpt.signed_in();
        let mut selection = lock(&self.selection);
        config::save_provider(&self.config_path, &config)?;
        let same_endpoint = selection
            .config
            .as_ref()
            .is_some_and(|previous| endpoint(previous) == endpoint(&config));
        let key = match update.api_key.map(|key| key.trim().to_owned()) {
            _ if config.kind.signs_in() => None,
            Some(key) if key.is_empty() => None,
            Some(key) => Some(key),
            // A key entered for one endpoint is never sent to another.
            None if same_endpoint => selection.key.take(),
            None => None,
        };
        *selection = Selection {
            config: Some(config),
            key,
        };
        Ok(info(&selection, signed_in))
    }

    /// The models the service of a setup offers, its recommended ones first. A list the service refuses is not an
    /// error: the recommended models are offered with the reason, and any name can still be typed.
    pub(super) async fn models(&self, query: ModelQuery) -> Result<ModelList, AgentError> {
        let kind = query.kind;
        if !kind.accepts_key() {
            return Err(AgentError::Invalid(format!(
                "{} lists its models in its own panel",
                kind.id()
            )));
        }
        let (service, base_url) = chosen_endpoint(kind, query.service.as_deref(), query.base_url)?;
        let base_url = base_url
            .as_deref()
            .or(kind.default_base_url())
            .ok_or(ProviderError::MissingBaseUrl)?;
        let key = match query.api_key.map(|key| key.trim().to_owned()) {
            Some(key) if !key.is_empty() => Some(key),
            _ => self.key_for(kind, base_url),
        };
        let listed = if service.map_or(kind.requires_key(), Service::requires_key) && key.is_none()
        {
            Err(ProviderError::MissingKey {
                variable: kind.key_variable(),
            })
        } else {
            self.client.models(kind, base_url, key.as_deref()).await
        };
        Ok(ModelList::new(
            service.map_or(&[], |service| service.models),
            listed,
        ))
    }

    /// The key in use for the endpoint `kind` at `base_url`: the session's when it is the configured endpoint, else
    /// the kind's environment variable. A key never goes to another endpoint than its own.
    fn key_for(&self, kind: Kind, base_url: &str) -> Option<String> {
        let selection = lock(&self.selection);
        match &selection.config {
            Some(config) if endpoint(config) == (kind, Some(base_url)) => selection.key(kind).0,
            _ => environment_key(kind),
        }
    }

    /// The settings of the configured provider, without the sign-in a ChatGPT plan needs: see [`Providers::ready`].
    pub(super) fn settings(&self) -> Result<Settings, AgentError> {
        let selection = lock(&self.selection);
        let config = selection.config.clone().ok_or(AgentError::NotConfigured)?;
        let (api_key, _) = selection.key(config.kind);
        if requires_key(&config) && api_key.is_none() {
            return Err(ProviderError::MissingKey {
                variable: config.kind.key_variable(),
            }
            .into());
        }
        Ok(Settings {
            kind: config.kind,
            model: config.model,
            base_url: config.base_url,
            api_key,
            headers: Vec::new(),
        })
    }

    /// The settings for an answer: a ChatGPT session is renewed first if needed, so that an ended one is reported
    /// before anything is stored.
    pub(super) async fn ready(&self) -> Result<Settings, AgentError> {
        let mut settings = self.settings()?;
        if settings.kind.signs_in() {
            self.chatgpt.authorize(&mut settings).await?;
        }
        Ok(settings)
    }
}

/// `signed_in` is read by the caller before it locks the selection: the two locks are never held together.
fn info(selection: &Selection, signed_in: bool) -> ProviderInfo {
    let needs = selection.config.as_ref().and_then(|config| {
        if config.kind.signs_in() {
            (!signed_in).then_some(Needed::SignIn)
        } else {
            (requires_key(config) && selection.key(config.kind).0.is_none()).then_some(Needed::Key)
        }
    });
    ProviderInfo {
        configured: selection.config.clone(),
        key: selection
            .config
            .as_ref()
            .map(|config| selection.key(config.kind).1),
        needs,
        kinds: Kind::ALL
            .into_iter()
            .map(|kind| KindInfo {
                id: kind,
                default_model: kind.default_model(),
                default_base_url: kind.default_base_url(),
                key_variable: kind.key_variable(),
                accepts_key: kind.accepts_key(),
                requires_key: kind.requires_key(),
                signs_in: kind.signs_in(),
                key_in_environment: environment_key(kind).is_some(),
            })
            .collect(),
        ways: services::WAYS,
        services: services::SERVICES,
        sign_in_methods: services::SIGN_IN_METHODS,
    }
}

fn environment_key(kind: Kind) -> Option<String> {
    kind.key_variable()
        .and_then(crate::environment::var)
        .map(|key| key.trim().to_owned())
        .filter(|key| !key.is_empty())
}

/// The service and the API address a setup chose: the address given, or the service's; `None` for the kind's own.
/// The service must be of `kind`, and an address is required when the kind has none.
fn chosen_endpoint(
    kind: Kind,
    service: Option<&str>,
    base_url: Option<String>,
) -> Result<(Option<&'static Service>, Option<String>), AgentError> {
    let service = match service {
        Some(id) => match services::find(id) {
            Some(service) if service.kind == kind => Some(service),
            Some(_) => {
                return Err(AgentError::Invalid(format!(
                    "{id} is not a {} service",
                    kind.id()
                )));
            }
            None => return Err(AgentError::Invalid(format!("unknown service: {id}"))),
        },
        None => None,
    };
    let base_url = base_url
        .map(|url| url.trim().trim_end_matches('/').to_owned())
        .filter(|url| !url.is_empty())
        .or_else(|| {
            service
                .and_then(|service| service.base_url)
                .map(str::to_owned)
        })
        // Answers come from Ollama's OpenAI-compatible API, under `/v1`; an address is often given without it.
        .map(|url| match kind {
            Kind::Ollama if !url.ends_with("/v1") => format!("{url}/v1"),
            _ => url,
        });
    if let Some(url) = &base_url
        && !(url.starts_with("https://") || url.starts_with("http://"))
    {
        return Err(AgentError::Invalid(
            "the API address must start with https:// or http://".into(),
        ));
    }
    if kind.default_base_url().is_none() && base_url.is_none() {
        return Err(AgentError::Provider(ProviderError::MissingBaseUrl));
    }
    Ok((service, base_url))
}

/// Whether requests fail without an API key: the service decides when one was chosen, otherwise the kind.
fn requires_key(config: &ProviderConfig) -> bool {
    match config.service.as_deref().and_then(services::find) {
        Some(service) => service.requires_key(),
        None => config.kind.requires_key(),
    }
}

/// The address requests go to: the kind and its effective base URL.
fn endpoint(config: &ProviderConfig) -> (Kind, Option<&str>) {
    (
        config.kind,
        config
            .base_url
            .as_deref()
            .or(config.kind.default_base_url()),
    )
}
