//! Kelpy core: the home folder, configuration and types shared by every Kelpy crate.

pub mod agent;
pub mod chatgpt;
pub mod config;
pub mod connections;
pub mod environment;
pub mod events;
pub mod files;
pub mod home;
pub mod ollama;
pub mod private_file;
pub mod services;
pub mod ssh_agent;
pub mod vault;

pub use kelpy_providers as providers;
pub use kelpy_storage as storage;
pub use kelpy_tools::Account;

/// Kelpy version, shown by `kelpy --version` and reported by the runtime.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
