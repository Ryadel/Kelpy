//! Secrets Kelpy generates for a change, such as a database password. The model names a secret and writes its
//! placeholder, `{{secret:name}}`, where the value goes; Kelpy puts the value in only where it writes it (a file, a
//! database user) and puts the placeholder back in everything shown or given to the model. Secrets last while Kelpy
//! runs: they are never written to Kelpy's database nor to its logs.
//!
//! A conversation fills in only its own secrets, but masks those of every conversation ([`Registry`]): a value
//! written by one can turn up in what another reads, such as a log.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use kelpy_common::lock;

use crate::{ToolError, invalid};

const OPEN: &str = "{{secret:";
const CLOSE: &str = "}}";
/// The length of a secret: 32 letters and digits, about 190 bits, fit in any quotes of any configuration file.
const LENGTH: usize = 32;
const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// What stands for a secret of another conversation in what is shown or given to the model.
const ANOTHER: &str = "[a secret of another conversation]";

/// Every secret generated while Kelpy runs, in any conversation: what is masked. Clones share it.
#[derive(Debug, Clone, Default)]
pub struct Registry(Arc<Mutex<BTreeSet<String>>>);

/// The secrets of one conversation. Clones share them.
#[derive(Debug, Clone, Default)]
pub struct Secrets {
    /// By name: what this conversation's placeholders stand for.
    own: Arc<Mutex<BTreeMap<String, String>>>,
    registry: Registry,
}

impl Secrets {
    /// Secrets of their own, masking no others.
    pub fn new() -> Self {
        Self::default()
    }

    /// A conversation's secrets, entered in `registry` and masking the others there.
    pub fn sharing(registry: &Registry) -> Self {
        Self {
            own: Arc::default(),
            registry: registry.clone(),
        }
    }

    /// Generates the secret `name`, unless it exists. Returns whether it is new.
    pub fn generate(&self, name: &str) -> Result<bool, ToolError> {
        let mut secrets = lock(&self.own);
        if secrets.contains_key(name) {
            return Ok(false);
        }
        let value = random()?;
        // In the registry before it can be used: from now on it is masked everywhere.
        lock(&self.registry.0).insert(value.clone());
        secrets.insert(name.to_owned(), value);
        Ok(true)
    }

    /// The names of the secrets, in order.
    pub fn names(&self) -> Vec<String> {
        lock(&self.own).keys().cloned().collect()
    }

    /// `text` with each placeholder replaced by its secret, and whether there was any. Fails on a secret that does
    /// not exist.
    pub fn fill(&self, text: &str) -> Result<(String, bool), ToolError> {
        let secrets = lock(&self.own);
        let mut filled = String::with_capacity(text.len());
        let mut rest = text;
        let mut any = false;
        while let Some(start) = rest.find(OPEN) {
            let after = &rest[start + OPEN.len()..];
            let Some(end) = after.find(CLOSE) else {
                break;
            };
            let name = &after[..end];
            let value = secrets.get(name).ok_or_else(|| {
                invalid(format!(
                    "there is no secret named `{name}` in this conversation: create it with generate_secret \
                     (secrets last until Kelpy stops)"
                ))
            })?;
            filled.push_str(&rest[..start]);
            filled.push_str(value);
            rest = &after[end + CLOSE.len()..];
            any = true;
        }
        filled.push_str(rest);
        Ok((filled, any))
    }

    /// `text` with each of its secrets replaced by its placeholder, and those of other conversations by
    /// [`ANOTHER`].
    pub fn redact(&self, text: &str) -> String {
        let mut redacted = text.to_owned();
        for (name, value) in lock(&self.own).iter() {
            if redacted.contains(value.as_str()) {
                redacted = redacted.replace(value.as_str(), &placeholder(name));
            }
        }
        for value in lock(&self.registry.0).iter() {
            if redacted.contains(value.as_str()) {
                redacted = redacted.replace(value.as_str(), ANOTHER);
            }
        }
        redacted
    }

    /// Whether `text` holds a secret, of this conversation or another.
    pub fn holds_any(&self, text: &str) -> bool {
        lock(&self.own)
            .values()
            .chain(lock(&self.registry.0).iter())
            .any(|value| text.contains(value.as_str()))
    }
}

/// How the model writes the secret `name`.
pub fn placeholder(name: &str) -> String {
    format!("{OPEN}{name}{CLOSE}")
}

/// Whether `text` holds a placeholder.
pub fn has_placeholder(text: &str) -> bool {
    text.contains(OPEN)
}

/// A secret's name: letters, digits and _ only, so that it reads the same everywhere.
pub fn name(value: &str) -> Result<String, ToolError> {
    let fits = (1..=64).contains(&value.len())
        && value
            .chars()
            .all(|char| char.is_ascii_alphanumeric() || char == '_');
    if fits {
        Ok(value.to_owned())
    } else {
        Err(invalid(
            "`name` may hold only letters, digits and _, at most 64",
        ))
    }
}

/// [`LENGTH`] characters of [`ALPHABET`], drawn evenly from the system's random source.
fn random() -> Result<String, ToolError> {
    let mut value = String::with_capacity(LENGTH);
    // The largest multiple of the alphabet's size within a byte: bytes above it would favour some characters.
    let limit = (256 / ALPHABET.len() * ALPHABET.len()) as u8;
    while value.len() < LENGTH {
        let mut bytes = [0u8; 64];
        getrandom::fill(&mut bytes)
            .map_err(|error| invalid(format!("no random numbers on this system: {error}")))?;
        for byte in bytes.into_iter().filter(|byte| *byte < limit) {
            if value.len() == LENGTH {
                break;
            }
            value.push(ALPHABET[usize::from(byte) % ALPHABET.len()] as char);
        }
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_filled_where_written_and_put_back_in_what_is_shown() {
        let secrets = Secrets::new();
        assert!(secrets.generate("db_password").unwrap());
        assert!(!secrets.generate("db_password").unwrap());
        let (filled, any) = secrets
            .fill("define('DB_PASSWORD', '{{secret:db_password}}');")
            .unwrap();
        assert!(any);
        let value = &filled["define('DB_PASSWORD', '".len()..filled.len() - "');".len()];
        assert_eq!(value.len(), LENGTH);
        assert!(value.chars().all(|char| char.is_ascii_alphanumeric()));
        assert!(secrets.holds_any(&filled));
        assert_eq!(
            secrets.redact(&format!("Access denied for {value}")),
            "Access denied for {{secret:db_password}}"
        );
        assert_eq!(
            secrets.fill("no secret").unwrap(),
            ("no secret".into(), false)
        );
        let unknown = secrets.fill("{{secret:other}}").unwrap_err().to_string();
        assert!(unknown.contains("generate_secret"), "{unknown}");
        assert!(name("db password").is_err());
        // Two secrets are never the same.
        secrets.generate("other").unwrap();
        let (both, _) = secrets
            .fill("{{secret:db_password}} {{secret:other}}")
            .unwrap();
        let (first, second) = both.split_once(' ').unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn a_secret_of_another_conversation_is_masked_but_not_filled() {
        let registry = Registry::default();
        let (first, second) = (Secrets::sharing(&registry), Secrets::sharing(&registry));
        first.generate("wp_db").unwrap();
        let (value, _) = first.fill("{{secret:wp_db}}").unwrap();

        let log = format!("Access denied for user 'wp' (using password: {value})");
        assert_eq!(
            second.redact(&log),
            "Access denied for user 'wp' (using password: [a secret of another conversation])"
        );
        assert!(second.holds_any(&log));
        assert!(second.fill("{{secret:wp_db}}").is_err());
        assert_eq!(first.redact(&value), "{{secret:wp_db}}");
        // Secrets outside the registry mask only their own.
        assert_eq!(Secrets::new().redact(&value), value);
    }
}
