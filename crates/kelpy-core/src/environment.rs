//! The environment variables Kelpy reads credentials from: the providers' API keys and the SSH agent of the session.
//!
//! A Kelpy restarted as administrator does not inherit the environment of the one before: User Account Control
//! gives it the user's profile, `sudo` resets it. The one before hands over its values of these variables, and they
//! come first.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use kelpy_providers::Kind;

/// The SSH agent of the session, which can unlock the vault.
const SSH_AGENT: &str = "SSH_AUTH_SOCK";

static HANDED_OVER: OnceLock<BTreeMap<String, String>> = OnceLock::new();

/// The variables handed over: the providers' keys and the SSH agent.
fn names() -> impl Iterator<Item = &'static str> {
    Kind::ALL
        .into_iter()
        .filter_map(Kind::key_variable)
        .chain([SSH_AGENT])
}

/// This process's values of the variables handed over, for the Kelpy restarted in its place.
pub fn handed_over() -> BTreeMap<String, String> {
    names()
        .filter_map(|name| Some((name.to_owned(), var(name)?)))
        .collect()
}

/// Takes over the values the Kelpy before this one had, once, before anything reads them. Other names are ignored.
pub fn take_over(values: BTreeMap<String, String>) {
    let known: BTreeMap<String, String> = values
        .into_iter()
        .filter(|(name, _)| names().any(|known| known == name))
        .collect();
    let _ = HANDED_OVER.set(known);
}

/// The value of `name`: as handed over, else this process's own; none when empty.
pub(crate) fn var(name: &str) -> Option<String> {
    HANDED_OVER
        .get()
        .and_then(|values| values.get(name).cloned())
        .or_else(|| std::env::var(name).ok())
        .filter(|value| !value.is_empty())
}
