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

[updates]
# At start, ask GitHub for the latest release and offer to install it. `kelpy update` works either way.
check = true

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
    pub updates: UpdatesConfig,
    pub memory: MemoryConfig,
}

/// The `[memory]` section: which memory of the server Kelpy uses. Without it, the home's own.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    /// The memory chosen. Unset, the shared one if `shared` is set, as before the choice could be changed.
    #[serde(rename = "use")]
    pub using: Option<MemoryUse>,
    /// The server folder shared with the other administrators of this computer
    /// ([`crate::server_folder::ServerFolder`]); unset, the usual place.
    pub shared: Option<PathBuf>,
}

/// Which memory of the server Kelpy uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryUse {
    /// The home's own, for this administrator only. Read as `local` too.
    #[serde(alias = "local")]
    Private,
    /// The server folder's, shared with the other administrators: only administrators can open it.
    Shared,
}

impl MemoryConfig {
    /// The memory chosen.
    pub fn chosen(&self) -> MemoryUse {
        match (self.using, &self.shared) {
            (Some(using), _) => using,
            (None, Some(_)) => MemoryUse::Shared,
            (None, None) => MemoryUse::Private,
        }
    }

    /// The server folder: where it was set, or else the usual place.
    pub fn shared_folder(&self) -> Option<PathBuf> {
        self.shared
            .clone()
            .or_else(crate::server_folder::ServerFolder::default_path)
    }
}

/// The `[updates]` section. Homes created before it existed have no such section, and check.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct UpdatesConfig {
    pub check: bool,
}

impl Default for UpdatesConfig {
    fn default() -> Self {
        Self { check: true }
    }
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
    write_document(path, &document)
}

/// Keeps the server's memory in the server folder `shared` from now on, writing the `[memory]` section.
pub fn save_shared_memory(path: &Path, shared: &Path) -> Result<(), ConfigError> {
    save_memory(path, MemoryUse::Shared, Some(shared))
}

/// Records in the configuration at `path` the memory chosen, `using`, and the server folder `shared` if given,
/// keeping the rest of the file.
pub fn save_memory(
    path: &Path,
    using: MemoryUse,
    shared: Option<&Path>,
) -> Result<(), ConfigError> {
    let text = fs::read_to_string(path).map_err(|error| ConfigError::Write {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    let mut document: toml_edit::DocumentMut =
        text.parse()
            .map_err(|error: toml_edit::TomlError| ConfigError::Write {
                path: path.to_path_buf(),
                message: error.to_string(),
            })?;
    if !document.contains_table("memory") {
        let mut table = toml_edit::Table::new();
        table.decor_mut().set_prefix(
            "\n# Which memory of the server Kelpy uses: \"private\", only for you, or \"shared\" with the other \
             administrators\n# of this computer, in the folder `shared`, which only they can open.\n",
        );
        document["memory"] = toml_edit::Item::Table(table);
    }
    let using = match using {
        MemoryUse::Private => "private",
        MemoryUse::Shared => "shared",
    };
    document["memory"]["use"] = toml_edit::value(using);
    if let Some(shared) = shared {
        document["memory"]["shared"] = toml_edit::value(shared.to_string_lossy().as_ref());
    }
    write_document(path, &document)
}

/// Writes `document` to `path` beside the original and renames it over it, so a crash never leaves half a file.
fn write_document(path: &Path, document: &toml_edit::DocumentMut) -> Result<(), ConfigError> {
    let failed = |message: String| ConfigError::Write {
        path: path.to_path_buf(),
        message,
    };
    let staged = path.with_extension("toml.new");
    fs::write(&staged, document.to_string()).map_err(|error| failed(error.to_string()))?;
    fs::rename(&staged, path).map_err(|error| failed(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shared_memory_is_recorded_keeping_the_rest_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, DEFAULT_CONFIG).unwrap();
        assert!(Config::load(&path).unwrap().memory.shared.is_none());
        let shared = Path::new(r"C:\ProgramData\Kelpy");
        save_shared_memory(&path, shared).unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.memory.shared.as_deref(), Some(shared));
        assert!(config.updates.check);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("# At start, ask GitHub"), "{text}");
        assert!(text.contains("which only they can open"), "{text}");
        assert_eq!(config.memory.chosen(), MemoryUse::Shared);

        // Chosen again: the folder stays, for when it is chosen once more.
        save_memory(&path, MemoryUse::Private, None).unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.memory.chosen(), MemoryUse::Private);
        assert_eq!(config.memory.shared.as_deref(), Some(shared));
        // Written before the choice could change: the folder alone means shared.
        let before = Config::parse("[memory]\nshared = 'C:\\ProgramData\\Kelpy'\n").unwrap();
        assert_eq!(before.memory.chosen(), MemoryUse::Shared);
        assert_eq!(
            Config::parse("").unwrap().memory.chosen(),
            MemoryUse::Private
        );
    }

    #[test]
    fn default_file_matches_default_values() {
        let config = Config::parse(DEFAULT_CONFIG).unwrap();
        assert_eq!(config.server.host, ServerConfig::default().host);
        assert_eq!(config.server.port, 0);
        assert!(config.updates.check);
    }

    #[test]
    fn missing_values_fall_back_to_defaults() {
        let config = Config::parse("").unwrap();
        assert!(config.server.host.is_loopback());
        assert_eq!(config.server.port, 0);
        assert!(config.updates.check);
    }

    #[test]
    fn the_update_check_can_be_turned_off() {
        let config = Config::parse("[updates]\ncheck = false\n").unwrap();
        assert!(!config.updates.check);
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
