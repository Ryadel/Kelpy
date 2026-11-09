//! Secrets used in changes, such as a database password. The model names a secret and writes its placeholder,
//! `{{secret:name}}`, where the value goes; Kelpy puts the value in only where it writes it (a file, a database user)
//! and puts the placeholder back in everything shown or given to the model. The values are never written to Kelpy's
//! database nor to its logs.
//!
//! A conversation has the secrets it generated, which last while Kelpy runs, and shares the administrator's: those
//! added in Secrets or kept in the vault ([`Registry`]). It fills in only those, but masks the secrets of every
//! conversation: a value written by one can turn up in what another reads, such as a log.
//!
//! Locks are always taken in the same order: a conversation's own secrets, then the registry.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex};

use aho_corasick::AhoCorasick;
use kelpy_common::lock;

use crate::{ToolError, invalid};

const OPEN: &str = "{{secret:";
const CLOSE: &str = "}}";
/// The length of a secret: 32 letters and digits, about 190 bits, fit in any quotes of any configuration file.
const LENGTH: usize = 32;
const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// What stands for a secret of another conversation in what is shown or given to the model.
const ANOTHER: &str = "[a secret of another conversation]";
/// What stands for secrets whose values overlap in a text, none of them covering the others.
const OVERLAPPING: &str = "[overlapping secrets]";
/// What stands for a text that could not be checked for secrets.
const WITHHELD: &str = "[withheld: it could not be checked for secrets]";
/// Occurrences of secrets a byte of a text, at most, that masking searches on: values overlap this much only
/// repeated over each other, and the search costs as much as the occurrences.
const OCCURRENCES: usize = 4;

/// What every conversation shares. Clones share it.
#[derive(Clone, Default)]
pub struct Registry(Arc<Mutex<Shared>>);

#[derive(Default)]
struct Shared {
    /// Every value generated or added while Kelpy runs, in any conversation: what is masked. A value stays masked
    /// once its secret is gone, as it may still be on the server.
    values: BTreeSet<String>,
    /// By name: the administrator's secrets, which every conversation can use.
    common: BTreeMap<String, String>,
}

impl Registry {
    /// Makes `value` the administrator's secret `name`, in place of any other of that name.
    pub fn set(&self, name: &str, value: String) {
        let mut shared = lock(&self.0);
        shared.values.insert(value.clone());
        shared.common.insert(name.to_owned(), value);
    }

    /// Removes the administrator's secret `name`. Returns whether there was one.
    pub fn remove(&self, name: &str) -> bool {
        lock(&self.0).common.remove(name).is_some()
    }

    /// The value of the administrator's secret `name`.
    pub fn value(&self, name: &str) -> Option<String> {
        lock(&self.0).common.get(name).cloned()
    }

    /// The names of the administrator's secrets, in order.
    pub fn names(&self) -> Vec<String> {
        lock(&self.0).common.keys().cloned().collect()
    }
}

/// The secrets of one conversation. Clones share them.
#[derive(Clone, Default)]
pub struct Secrets {
    /// By name: the secrets this conversation generated.
    own: Arc<Mutex<BTreeMap<String, String>>>,
    registry: Registry,
}

impl Secrets {
    /// Secrets of their own, sharing and masking no others.
    pub fn new() -> Self {
        Self::default()
    }

    /// A conversation's secrets, sharing the administrator's in `registry` and masking every value there.
    pub fn sharing(registry: &Registry) -> Self {
        Self {
            own: Arc::default(),
            registry: registry.clone(),
        }
    }

    /// Generates the secret `name`, unless it exists, in this conversation or among the administrator's. Returns
    /// whether it is new.
    pub fn generate(&self, name: &str) -> Result<bool, ToolError> {
        let mut own = lock(&self.own);
        let mut shared = lock(&self.registry.0);
        if own.contains_key(name) || shared.common.contains_key(name) {
            return Ok(false);
        }
        let value = random()?;
        // In the registry before it can be used: from now on it is masked everywhere.
        shared.values.insert(value.clone());
        own.insert(name.to_owned(), value);
        Ok(true)
    }

    /// Whether the secret `name` can be used here.
    pub fn has(&self, name: &str) -> bool {
        let own = lock(&self.own);
        own.contains_key(name) || lock(&self.registry.0).common.contains_key(name)
    }

    /// The names of the secrets this conversation generated, in order.
    pub fn names(&self) -> Vec<String> {
        lock(&self.own).keys().cloned().collect()
    }

    /// The value of the secret `name` this conversation generated, for the administrator.
    pub fn value(&self, name: &str) -> Option<String> {
        lock(&self.own).get(name).cloned()
    }

    /// Hands the secret `name` this conversation generated over to the administrator's, under the same name, so
    /// that every conversation can use it. Returns its value. Fails if the administrator has one of that name.
    pub fn share(&self, name: &str) -> Result<String, ToolError> {
        let mut own = lock(&self.own);
        let mut shared = lock(&self.registry.0);
        if shared.common.contains_key(name) {
            return Err(invalid(format!("there is already a secret named `{name}`")));
        }
        let value = own
            .remove(name)
            .ok_or_else(|| invalid(format!("this conversation has no secret named `{name}`")))?;
        shared.common.insert(name.to_owned(), value.clone());
        Ok(value)
    }

    /// Takes back the secret `name` handed over with [`Secrets::share`], when keeping it failed.
    pub fn unshare(&self, name: &str, value: String) {
        let mut own = lock(&self.own);
        lock(&self.registry.0).common.remove(name);
        own.insert(name.to_owned(), value);
    }

    /// `text` with each placeholder replaced by its secret, and whether there was any. Fails on a secret that does
    /// not exist.
    pub fn fill(&self, text: &str) -> Result<(String, bool), ToolError> {
        let own = lock(&self.own);
        let shared = lock(&self.registry.0);
        let mut filled = String::with_capacity(text.len());
        let mut rest = text;
        let mut any = false;
        while let Some(start) = rest.find(OPEN) {
            let after = &rest[start + OPEN.len()..];
            let Some(end) = after.find(CLOSE) else {
                break;
            };
            let name = &after[..end];
            let value = own
                .get(name)
                .or_else(|| shared.common.get(name))
                .ok_or_else(|| {
                    invalid(format!(
                        "there is no secret named `{name}`: create it with generate_secret (it lasts until Kelpy \
                         stops), or ask the administrator to add it in Secrets, or to unlock the vault there if it \
                         is kept in it"
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

    /// `text` with each secret it can use replaced by its placeholder, and those of other conversations by
    /// [`ANOTHER`]. Every occurrence is found in the original text, overlapping ones included, and occurrences that
    /// overlap are replaced together: by the replacement of the one covering them all, or else by [`OVERLAPPING`].
    /// So no part of any value is left in the clear, and nothing put in is replaced again. A text with more
    /// than [`OCCURRENCES`] a byte, only values repeated over each other, is [`WITHHELD`] rather than searched on.
    pub fn redact(&self, text: &str) -> String {
        // The values and what replaces each are taken under the locks; the search runs without them.
        let replacements = {
            let own = lock(&self.own);
            let shared = lock(&self.registry.0);
            let mut replacements: BTreeMap<String, Cow<str>> = shared
                .values
                .iter()
                .map(|value| (value.clone(), Cow::Borrowed(ANOTHER)))
                .collect();
            // Those it can use keep their name; its own first.
            for (name, value) in shared.common.iter().chain(own.iter()) {
                replacements.insert(value.clone(), Cow::Owned(placeholder(name)));
            }
            replacements
        };
        if replacements.is_empty() {
            return text.to_owned();
        }
        let (values, replacements): (Vec<String>, Vec<Cow<str>>) = replacements.into_iter().unzip();
        // Building fails only past the matcher's size limit, far beyond any number of secrets.
        let Ok(matcher) = AhoCorasick::new(values) else {
            return WITHHELD.to_owned();
        };
        let Ok(found) = matcher.try_find_overlapping_iter(text) else {
            return WITHHELD.to_owned();
        };
        // The occurrences come by where they end. Each joins the groups before it that it overlaps, which are the
        // last ones: the groups stay apart and in order, at most one a byte whatever the occurrences.
        let mut groups: Vec<Group> = Vec::new();
        for (count, occurrence) in found.enumerate() {
            if count >= OCCURRENCES * text.len().max(16)
                || groups
                    .last()
                    .is_some_and(|last| occurrence.end() < last.end)
            {
                return WITHHELD.to_owned();
            }
            let mut group = Group {
                start: occurrence.start(),
                end: occurrence.end(),
                covering: Some(occurrence.pattern().as_usize()),
            };
            while let Some(last) = groups.pop_if(|last| last.end > group.start) {
                if last.start < group.start {
                    // Only a value covering the last group and ending where this one does covers them both.
                    group.covering = last.covering.filter(|_| last.end == group.end);
                    group.start = last.start;
                }
            }
            groups.push(group);
        }
        let mut redacted = String::with_capacity(text.len());
        let mut copied = 0;
        for group in groups {
            redacted.push_str(&text[copied..group.start]);
            redacted.push_str(
                group
                    .covering
                    .map_or(OVERLAPPING, |value| &replacements[value]),
            );
            copied = group.end;
        }
        redacted.push_str(&text[copied..]);
        redacted
    }

    /// Whether `text` holds a secret, of this conversation or another.
    pub fn holds_any(&self, text: &str) -> bool {
        let own = lock(&self.own);
        let shared = lock(&self.registry.0);
        own.values()
            .chain(shared.values.iter())
            .any(|value| text.contains(value.as_str()))
    }
}

/// Occurrences of secrets in a text that overlap, from `start` to `end`, and the value covering them all, if one does.
struct Group {
    start: usize,
    end: usize,
    covering: Option<usize>,
}

/// Names only: values never reach a log.
impl fmt::Debug for Registry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Registry")
            .field("names", &self.names())
            .finish_non_exhaustive()
    }
}

/// Names only: values never reach a log.
impl fmt::Debug for Secrets {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Secrets")
            .field("names", &self.names())
            .finish_non_exhaustive()
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
    fn the_administrators_secrets_serve_every_conversation() {
        let registry = Registry::default();
        let (first, second) = (Secrets::sharing(&registry), Secrets::sharing(&registry));
        registry.set("smtp", "the-smtp-password".into());
        assert_eq!(
            first.fill("{{secret:smtp}}").unwrap().0,
            "the-smtp-password"
        );
        assert_eq!(second.redact("pw the-smtp-password"), "pw {{secret:smtp}}");
        assert!(!first.generate("smtp").unwrap());
        assert!(first.has("smtp"));

        // A generated secret handed over serves the others too, under its name.
        first.generate("wp_db").unwrap();
        let (value, _) = first.fill("{{secret:wp_db}}").unwrap();
        assert!(second.fill("{{secret:wp_db}}").is_err());
        assert_eq!(first.share("wp_db").unwrap(), value);
        assert_eq!(second.fill("{{secret:wp_db}}").unwrap().0, value);
        assert!(first.names().is_empty());
        first.generate("other").unwrap();
        assert!(first.share("smtp").is_err());

        // Removed, it serves no one, but stays masked: it may still be on the server.
        assert!(registry.remove("smtp"));
        assert!(first.fill("{{secret:smtp}}").is_err());
        assert_eq!(
            first.redact("the-smtp-password"),
            "[a secret of another conversation]"
        );
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

    #[test]
    fn overlapping_secrets_are_masked_whole() {
        let registry = Registry::default();
        let secrets = Secrets::sharing(&registry);
        registry.set("a", "secret-prefix".into());
        registry.set("b", "secret-prefix-PRIVATE-TAIL".into());
        // A value inside a placeholder already put in is not replaced again.
        registry.set("c", "{{secret:a}}".into());
        assert_eq!(
            secrets.redact("x secret-prefix-PRIVATE-TAIL y secret-prefix z"),
            "x {{secret:b}} y {{secret:a}} z"
        );
        assert_eq!(
            secrets.redact("secret-prefix-PRIVATE"),
            "{{secret:a}}-PRIVATE"
        );
        assert_eq!(secrets.redact("{{secret:a}}"), "{{secret:c}}");
        // Two values overlapping with different starts: neither part stays in the clear.
        registry.set("d", "AAAA-BBBB".into());
        registry.set("e", "BBBB-CCCC".into());
        assert_eq!(
            secrets.redact("x AAAA-BBBB-CCCC y AAAA-BBBB"),
            "x [overlapping secrets] y {{secret:d}}"
        );
        assert!(!format!("{secrets:?} {registry:?}").contains("prefix"));
    }

    #[test]
    fn a_text_of_values_repeated_over_each_other_is_withheld_not_searched_on() {
        let registry = Registry::default();
        let secrets = Secrets::sharing(&registry);
        for length in 4..52 {
            registry.set(&format!("a{length}"), "a".repeat(length));
        }
        // About a million and a half occurrences, each byte in up to 48.
        assert_eq!(secrets.redact(&"a".repeat(32 * 1024)), WITHHELD);
        // As many occurrences as such a text has apart are masked each.
        let text = "aaaa ".repeat(2000);
        assert_eq!(secrets.redact(&text), "{{secret:a4}} ".repeat(2000));
    }

    #[test]
    fn overlaps_are_grouped_as_by_sorting_every_occurrence() {
        let values = ["ab", "ba", "aba", "bab", "abab", "bb"];
        let registry = Registry::default();
        let secrets = Secrets::sharing(&registry);
        for value in values {
            registry.set(value, value.into());
        }
        // The plain way, every occurrence found, sorted by start, the longest first.
        let sorted = |text: &str| {
            let matcher = AhoCorasick::new(values).unwrap();
            let mut found: Vec<_> = matcher.find_overlapping_iter(text).collect();
            found.sort_by_key(|found| (found.start(), std::cmp::Reverse(found.end())));
            let (mut redacted, mut copied, mut next) = (String::new(), 0, 0);
            while let Some(&first) = found.get(next) {
                let (mut end, mut covering) = (first.end(), Some(first.pattern()));
                next += 1;
                while let Some(other) = found.get(next).filter(|other| other.start() < end) {
                    if other.end() > end {
                        (end, covering) = (other.end(), None);
                    }
                    next += 1;
                }
                redacted.push_str(&text[copied..first.start()]);
                redacted.push_str(
                    &covering.map_or(OVERLAPPING.into(), |value| placeholder(values[value])),
                );
                copied = end;
            }
            redacted + &text[copied..]
        };
        let mut seed = 7u32;
        let mut checked = 0;
        for _ in 0..2000 {
            let text: String = (0..12)
                .map(|_| {
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                    ['a', 'b', 'b', 'x'][(seed >> 16) as usize % 4]
                })
                .collect();
            let redacted = secrets.redact(&text);
            if redacted != WITHHELD {
                assert_eq!(redacted, sorted(&text), "{text}");
                checked += 1;
            }
        }
        assert!(checked > 1900, "{checked}");
    }
}
