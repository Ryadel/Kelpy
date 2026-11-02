//! `config.toml`: the user-editable configuration stored in the Kelpy home. It never holds secrets.

use std::fs;
use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};

use kelpy_providers::Kind;
use serde::{Deserialize, Serialize};

/// Written to `config.toml` when a home is created. Kept as text so that its comments survive.
pub const DEFAULT_CONFIG: &str = r#"# Kelpy configuration.
# Secrets such as API keys are never stored here: they come from the environment or are entered in the Web UI,
# and stay in memory only.

[server]
# Address the local server listens on. Only loopback addresses are allowed.
host = "127.0.0.1"
# TCP port. 0: the port of the last start if it is free, otherwise one the operating system picks.
port = 0

# The AI provider, also editable from the Web UI and with `kelpy provider`. Example:
# [provider]
# kind = "anthropic"            # anthropic, chatgpt, openai, ollama or openai-compatible
# model = "claude-opus-5-5"
# base_url = "https://..."      # only to override the provider's default address
# service = "anthropic"         # the service chosen in the setup, such as groq; optional
"#;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub provider: Option<ProviderConfig>,
}

/// The `[provider]` section: which API and model to use. The API key is never stored here.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ProviderConfig {
    pub kind: Kind,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// The service chosen in the setup (see [`crate::services`]), which several services sharing one kind tell
    /// apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub host: IpAddr,
    pub port: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 0,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid configuration in {}: {source}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("cannot update {}: {message}", path.display())]
    Write { path: PathBuf, message: String },
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }
}

/// Replaces the `[provider]` section of the configuration file, keeping everything else, comments included.
pub fn save_provider(path: &Path, provider: &ProviderConfig) -> Result<(), ConfigError> {
    let failed = |message: String| ConfigError::Write {
        path: path.to_path_buf(),
        message,
    };
    let text = fs::read_to_string(path).map_err(|error| failed(error.to_string()))?;
    let mut document: toml_edit::DocumentMut = text
        .parse()
        .map_err(|error: toml_edit::TomlError| failed(error.to_string()))?;
    let mut table = toml_edit::Table::new();
    table["kind"] = toml_edit::value(provider.kind.id());
    table["model"] = toml_edit::value(provider.model.as_str());
    if let Some(base_url) = &provider.base_url {
        table["base_url"] = toml_edit::value(base_url.as_str());
    }
    if let Some(service) = &provider.service {
        table["service"] = toml_edit::value(service.as_str());
    }
    document["provider"] = toml_edit::Item::Table(table);
    // Written beside the original and renamed over it, so a crash never leaves half a file.
    let staged = path.with_extension("toml.new");
    fs::write(&staged, document.to_string()).map_err(|error| failed(error.to_string()))?;
    fs::rename(&staged, path).map_err(|error| failed(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_file_matches_default_values() {
        let config = Config::parse(DEFAULT_CONFIG).unwrap();
        assert_eq!(config.server.host, ServerConfig::default().host);
        assert_eq!(config.server.port, 0);
    }

    #[test]
    fn missing_values_fall_back_to_defaults() {
        let config = Config::parse("").unwrap();
        assert!(config.server.host.is_loopback());
        assert_eq!(config.server.port, 0);
    }

    #[test]
    fn the_provider_is_saved_without_losing_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, DEFAULT_CONFIG).unwrap();
        let provider = ProviderConfig {
            kind: Kind::OpenAiCompatible,
            model: "llama".into(),
            base_url: Some("http://10.0.0.5:8000/v1".into()),
            service: Some("vllm".into()),
        };
        save_provider(&path, &provider).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("# Address the local server listens on."));
        assert_eq!(Config::parse(&text).unwrap().provider, Some(provider));

        let replaced = ProviderConfig {
            kind: Kind::Anthropic,
            model: "claude-opus-5-5".into(),
            base_url: None,
            service: None,
        };
        save_provider(&path, &replaced).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("kind = \"anthropic\""));
        assert_eq!(Config::parse(&text).unwrap().provider, Some(replaced));
    }

    #[test]
    fn explicit_values_are_read() {
        let config = Config::parse("[server]\nhost = \"::1\"\nport = 4321\n").unwrap();
        assert_eq!(config.server.host.to_string(), "::1");
        assert_eq!(config.server.port, 4321);
    }
}
