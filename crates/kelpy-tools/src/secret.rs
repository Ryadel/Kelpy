//! Secrets Kelpy generates for a change, such as a database password. The model names a secret and writes its
//! placeholder, `{{secret:name}}`, where the value goes; Kelpy puts the value in only where it writes it (a file, a
//! database user) and puts the placeholder back in everything shown or given to the model. Secrets last while Kelpy
//! runs: they are never written to Kelpy's database nor to its logs.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use kelpy_common::lock;

use crate::{ToolError, invalid};

const OPEN: &str = "{{secret:";
const CLOSE: &str = "}}";
/// The length of a secret: 32 letters and digits, about 190 bits, fit in any quotes of any configuration file.
const LENGTH: usize = 32;
const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// The secrets of one conversation. Clones share them.
#[derive(Debug, Clone, Default)]
pub struct Secrets(Arc<Mutex<BTreeMap<String, String>>>);

impl Secrets {
    pub fn new() -> Self {
        Self::default()
    }

    /// Generates the secret `name`, unless it exists. Returns whether it is new.
    pub fn generate(&self, name: &str) -> Result<bool, ToolError> {
        let mut secrets = lock(&self.0);
        if secrets.contains_key(name) {
            return Ok(false);
        }
        secrets.insert(name.to_owned(), random()?);
        Ok(true)
    }

    /// The names of the secrets, in order.
    pub fn names(&self) -> Vec<String> {
        lock(&self.0).keys().cloned().collect()
    }

    /// `text` with each placeholder replaced by its secret, and whether there was any. Fails on a secret that does
    /// not exist.
    pub fn fill(&self, text: &str) -> Result<(String, bool), ToolError> {
        let secrets = lock(&self.0);
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

    /// `text` with each secret replaced by its placeholder.
    pub fn redact(&self, text: &str) -> String {
        let secrets = lock(&self.0);
        let mut redacted = text.to_owned();
        for (name, value) in secrets.iter() {
            if redacted.contains(value.as_str()) {
                redacted = redacted.replace(value.as_str(), &placeholder(name));
            }
        }
        redacted
    }

    /// Whether `text` holds a secret.
    pub fn holds_any(&self, text: &str) -> bool {
        lock(&self.0)
            .values()
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
}
