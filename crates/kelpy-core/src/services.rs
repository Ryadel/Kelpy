//! The services Kelpy can use, grouped by how they are paid for: the catalogue behind the provider setup.
//!
//! Each service is a [`Kind`] (the API it speaks) with its address, a one-line description of its trade-off, and
//! the steps to get an API key. Free tiers change often: each is one entry here. All checked on 2026-10-31.

use kelpy_providers::Kind;
use serde::Serialize;

use crate::chatgpt::Method;

/// How a service is paid for: the first question of the setup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Way {
    /// A ChatGPT plan the user already pays for.
    Plan,
    /// A provider's free tier.
    Free,
    /// An API key, billed per use.
    Key,
    /// A model on this computer or the local network.
    Local,
}

/// A way, as the setup presents it.
#[derive(Debug, Serialize)]
pub struct WayInfo {
    pub id: Way,
    pub name: &'static str,
    pub description: &'static str,
}

/// The first question's options, in the order shown.
pub const WAYS: &[WayInfo] = &[
    WayInfo {
        id: Way::Plan,
        name: "ChatGPT plan",
        description: "Answers are paid by your ChatGPT plan.",
    },
    WayInfo {
        id: Way::Free,
        name: "Free online service",
        description: "A free account with Google, Groq, OpenRouter or Mistral. Request limits apply; some use \
                      prompts for training.",
    },
    WayInfo {
        id: Way::Key,
        name: "API key",
        description: "Anthropic, OpenAI or another service, billed per use.",
    },
    WayInfo {
        id: Way::Local,
        name: "Local model",
        description: "Ollama or another server on this computer or your network. Prompts stay on your network, \
                      except with Ollama's cloud models.",
    },
];

/// A way to sign in with a ChatGPT plan, as the setup presents it.
#[derive(Debug, Serialize)]
pub struct SignInMethod {
    pub id: Method,
    pub name: &'static str,
    pub pros: &'static str,
    pub cons: &'static str,
    /// When to choose it.
    pub recommended: &'static str,
}

/// The ways to sign in with a ChatGPT plan, in the order shown.
pub const SIGN_IN_METHODS: &[SignInMethod] = &[
    SignInMethod {
        id: Method::Codex,
        name: "Sign in as Codex CLI",
        pros: "No registration. You approve a short code on any device, by address or QR code: suits servers \
               without a browser, and many servers.",
        cons: "Uses the Codex CLI's sign-in, not OpenAI's program for apps, and may stop working. Usage counts as \
               Codex usage, with no limit of its own. Sign-in with a code must be enabled in ChatGPT's security \
               settings.",
        recommended: "for starters; less friction, best when setting up many servers.",
    },
    SignInMethod {
        id: Method::Agent,
        name: "Sign in as an agent",
        pros: "OpenAI's program for apps: Kelpy is an app of its own in your ChatGPT account, its usage shown apart, \
               with a weekly limit if set, revocable on its own.",
        cons: "Each installation's first sign-in registers an app and asks for its name, unless it uses a \
               registration file. After approval the browser must reach Kelpy: through an SSH tunnel, or by \
               pasting the address it shows.",
        recommended: "for advanced users: when Kelpy's usage must be limited or tracked apart.",
    },
];

#[derive(Debug, Serialize)]
pub struct Service {
    /// Stable, saved in `config.toml`.
    pub id: &'static str,
    pub name: &'static str,
    pub way: Way,
    pub kind: Kind,
    /// The API address, when the kind has no default or the service uses another one.
    pub base_url: Option<&'static str>,
    /// What the service is, and its trade-off: cost, limits, where the data goes.
    pub description: &'static str,
    /// Where the API key is created.
    pub key_page: Option<&'static str>,
    /// How to get the key, in order.
    pub steps: &'static [&'static str],
    /// Suggested models, the first one preselected. Empty when the user names one.
    pub models: &'static [&'static str],
}

impl Service {
    /// Whether requests fail without an API key: those of the kind's that do, and every free service.
    pub fn requires_key(&self) -> bool {
        self.kind.requires_key() || self.way == Way::Free
    }
}

pub fn find(id: &str) -> Option<&'static Service> {
    SERVICES.iter().find(|service| service.id == id)
}

/// The service of a configuration that names none, made before services were saved: the one at its address, else
/// the first of its kind without an address of its own, which keeps the configured one.
pub fn infer(kind: Kind, base_url: Option<&str>) -> Option<&'static Service> {
    let of_kind = || SERVICES.iter().filter(move |service| service.kind == kind);
    of_kind()
        .find(|service| base_url.is_some() && service.base_url == base_url)
        .or_else(|| of_kind().find(|service| service.base_url.is_none()))
}

pub const SERVICES: &[Service] = &[
    Service {
        id: "chatgpt",
        name: "ChatGPT",
        way: Way::Plan,
        kind: Kind::ChatGpt,
        base_url: None,
        description: "Answers are paid by your ChatGPT plan, as Codex usage within its limits. The session ends \
                      when Kelpy stops, unless you save the sign-in on this computer.",
        key_page: None,
        steps: &[],
        models: &[],
    },
    Service {
        id: "gemini",
        name: "Google Gemini",
        way: Way::Free,
        kind: Kind::OpenAiCompatible,
        base_url: Some("https://generativelanguage.googleapis.com/v1beta/openai"),
        description: "Gemini models, free within daily limits shown in AI Studio. Outside the EEA, Switzerland and \
                      the UK, Google may use prompts and answers to improve its products, and people may read them.",
        key_page: Some("https://aistudio.google.com/apikey"),
        steps: &[
            "Sign in to Google AI Studio with a Google account.",
            "Choose Create API key, accepting the terms if asked.",
            "Copy the key and paste it here.",
        ],
        models: &["gemini-3.8-flash", "gemini-2.5-pro"],
    },
    Service {
        id: "groq",
        name: "Groq",
        way: Way::Free,
        kind: Kind::OpenAiCompatible,
        base_url: Some("https://api.groq.com/openai/v1"),
        description: "Open models, free within about 30 requests per minute and 1,000 per day for each model. Groq \
                      does not keep request data by default.",
        key_page: Some("https://console.groq.com/keys"),
        steps: &[
            "Sign in to the GroqCloud console, or create an account.",
            "Under API Keys, choose Create API Key.",
            "Copy the key and paste it here.",
        ],
        models: &["openai/gpt-oss-120b", "qwen/qwen3.8-27b"],
    },
    Service {
        id: "openrouter",
        name: "OpenRouter",
        way: Way::Free,
        kind: Kind::OpenAiCompatible,
        base_url: Some("https://openrouter.ai/api/v1"),
        description: "Models of many providers; those ending in :free cost nothing, within 20 requests per minute \
                      and 50 per day (1,000 per day once 10 credits have been bought). Providers that may train on \
                      prompts are used only if allowed under Settings, Privacy.",
        key_page: Some("https://openrouter.ai/settings/keys"),
        steps: &[
            "Sign in to OpenRouter, or create an account.",
            "Under Settings, Keys, choose Create key.",
            "Copy the key and paste it here.",
            "If a free model is refused, check Settings, Privacy: some free models need providers that may train \
             on prompts.",
        ],
        models: &[
            "nvidia/nemotron-3-ultra-550b-a55b:free",
            "google/gemma-4-31b-it:free",
        ],
    },
    Service {
        id: "mistral",
        name: "Mistral",
        way: Way::Free,
        kind: Kind::OpenAiCompatible,
        base_url: Some("https://api.mistral.ai/v1"),
        description: "Mistral models. The free Experiment plan needs a verified phone number, has low rate limits, \
                      and its requests may be used to train Mistral's models.",
        key_page: Some("https://console.mistral.ai/api-keys"),
        steps: &[
            "Sign in to the Mistral console, or create an account.",
            "Choose the Experiment plan and verify your phone number.",
            "Under API Keys, choose Create new key.",
            "Copy the key and paste it here.",
        ],
        models: &["mistral-medium-latest", "devstral-latest"],
    },
    Service {
        id: "anthropic",
        name: "Anthropic (Claude)",
        way: Way::Key,
        kind: Kind::Anthropic,
        base_url: None,
        description: "Claude models, billed per use. Claude Max, Ultra and Team plans include monthly API credits \
                      that pay for it.",
        key_page: Some("https://platform.claude.com/settings/keys"),
        steps: &[
            "Sign in to the Claude Console at platform.claude.com, or create an account.",
            "With a Max, Ultra or Team plan: in claude.ai, under Settings, Billing, link the Console organization \
             to receive the plan's API credits. Otherwise, add credit in the Console under Billing.",
            "In the Console, under Settings, API keys, create a key.",
            "Copy the key, which starts with sk-ant-, and paste it here.",
        ],
        models: &["claude-opus-5-5", "claude-sonnet-5-5"],
    },
    Service {
        id: "openai",
        name: "OpenAI",
        way: Way::Key,
        kind: Kind::OpenAi,
        base_url: None,
        description: "GPT models, billed per use. Separate from ChatGPT plans.",
        key_page: Some("https://platform.openai.com/api-keys"),
        steps: &[
            "Sign in to the OpenAI Platform.",
            "Under API keys, choose Create new secret key.",
            "Copy the key and paste it here. The Platform shows it only once.",
            "Add credit under Settings, Billing.",
        ],
        models: &[],
    },
    Service {
        id: "openai-compatible",
        name: "Other service",
        way: Way::Key,
        kind: Kind::OpenAiCompatible,
        base_url: None,
        description: "Any service with an OpenAI-compatible API: enter its address, model and key.",
        key_page: None,
        steps: &[],
        models: &[],
    },
    Service {
        id: "ollama",
        name: "Ollama",
        way: Way::Local,
        kind: Kind::Ollama,
        base_url: None,
        description: "Runs models on this computer or another one on your network, where prompts stay; quality \
                      and speed depend on the model and the hardware. Cloud models, whose names end in \"cloud\", \
                      run on ollama.com instead.",
        key_page: Some("https://ollama.com/download"),
        steps: &[
            "Set OLLAMA_CONTEXT_LENGTH=64000 in Ollama's environment, then restart Ollama. Its default context, \
             4,096 tokens on computers with less than 24 GiB of graphics memory, is too short for Kelpy, and Ollama \
             drops older text without notice.",
        ],
        models: &[],
    },
    Service {
        id: "local-server",
        name: "Other local server",
        way: Way::Local,
        kind: Kind::OpenAiCompatible,
        base_url: None,
        description: "LM Studio, llama.cpp, vLLM or another server with an OpenAI-compatible API: enter its \
                      address and model.",
        key_page: None,
        steps: &[],
        models: &[],
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_service_is_complete_and_unique() {
        for (index, service) in SERVICES.iter().enumerate() {
            assert!(
                SERVICES[..index].iter().all(|other| other.id != service.id),
                "{} twice",
                service.id
            );
            assert_eq!(find(service.id).map(|found| found.id), Some(service.id));
            if service.key_page.is_some() && service.way != Way::Local {
                assert!(!service.steps.is_empty(), "{} has no steps", service.id);
            }
            if let Some(url) = service.base_url {
                assert!(url.starts_with("https://"), "{url}");
            }
        }
        for way in WAYS {
            assert!(
                SERVICES.iter().any(|service| service.way == way.id),
                "{:?}",
                way.id
            );
        }
    }

    /// A line continuation without its `\` keeps the next line's indentation inside the text.
    #[test]
    fn texts_have_no_runs_of_spaces() {
        let texts = WAYS
            .iter()
            .map(|way| way.description)
            .chain(SERVICES.iter().flat_map(|service| {
                [service.description]
                    .into_iter()
                    .chain(service.steps.iter().copied())
            }))
            .chain(
                SIGN_IN_METHODS
                    .iter()
                    .flat_map(|method| [method.pros, method.cons, method.recommended]),
            );
        for text in texts {
            assert!(!text.contains("  "), "{text}");
        }
    }
}
