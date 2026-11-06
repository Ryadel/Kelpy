//! Changing files: one exact replacement, or a whole new content.
//!
//! Before approval Kelpy reads the file and plans the change ([`plan`]): the content after it, a diff to show, the
//! check known for the kind of file, and whether the change always asks. Once approved ([`apply`]) it writes only if
//! the file is still as shown, runs the check, backs the file up, writes it keeping its owner and mode, reads it
//! back, and runs the check again: when the change broke the check, the previous version is put back at once. A check
//! that failed in the same way before is not the change's doing, and the change is kept.
//!
//! The model's text may name secrets ([`crate::secret`]): their values are written, and the diff shows their
//! placeholders. A file that may hold secrets can be written whole, never edited: an edit would tell the model
//! whether a text occurs in it. Its current content is not shown in the diff.

use std::borrow::Cow;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use similar::TextDiff;

use crate::executor::{Observed, RUN_TIME, run_local};
use crate::{Command, Os, Output, Recorded, RecordedKind, Secrets, ToolError, files, invalid};

/// The largest file changed: a larger one is neither read whole nor backed up.
pub const MAX_FILE: u64 = 20 << 20;
/// The most text one call may carry.
pub const MAX_TEXT: usize = 1 << 20;
/// The most of a diff shown.
const MAX_DIFF: usize = 64 << 10;
/// The largest content kept to show a later difference.
const MAX_KEPT: usize = 256 << 10;
/// The most lines of a change shown in a plan.
const PREVIEW_LINES: usize = 40;

/// What the model asked to change in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    /// Replaces `old`, which must occur exactly once, with `new`.
    Replace { old: String, new: String },
    /// The whole new content; the file is created if it does not exist.
    Write(String),
}

/// A change of one file: as asked, then as Kelpy will make it once [`plan`]ned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: PathBuf,
    pub edit: Edit,
    pub planned: Option<Planned>,
}

/// A change Kelpy read the file for: what it writes, and how it checks and undoes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    /// The path written: links resolved.
    pub real: PathBuf,
    /// The content when planned; `None` when the file does not exist yet.
    pub before: Option<String>,
    pub after: String,
    /// The change as a unified diff.
    pub diff: String,
    /// The check run before and after, if one is known for the file and installed.
    pub check: Option<Command>,
    /// Why it waits for approval in every Approval mode, if it does.
    pub floor: Option<String>,
    /// Whether the file may hold secrets, or the change writes one: its content is neither shown nor kept.
    pub secret: bool,
}

impl Planned {
    /// What the card says besides the diff: when it is written, how it is checked and undone.
    pub fn notes(&self) -> Vec<String> {
        let mut notes = vec![match self.before {
            Some(_) => "Written only if the file is still as shown, after a backup in Kelpy's backups folder.",
            None => "A new file: deleting it undoes the change.",
        }
        .to_owned()];
        if self.secret {
            notes.push(
                "The file may hold secrets: its current content is not shown, and secrets show as their names."
                    .to_owned(),
            );
        }
        if let Some(check) = &self.check {
            notes.push(format!(
                "Checked with `{}` before and after: if the change breaks it, the previous version is put back at \
                 once.",
                check.display
            ));
        }
        notes
    }

    /// What Kelpy expects to find once the change is made.
    pub fn recorded(&self) -> Recorded {
        Recorded {
            kind: RecordedKind::File,
            subject: self.real.to_string_lossy().into_owned(),
            state: digest(&self.after),
            content: (!self.secret && self.after.len() <= MAX_KEPT).then(|| self.after.clone()),
            secret: self.secret,
        }
    }
}

/// The digest Kelpy keeps of a file's content.
pub fn digest(content: &str) -> String {
    let hash = Sha256::digest(content.as_bytes());
    let hex: String = hash.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("sha256:{hex}")
}

/// The file at `path` as it is now: its digest and content, or `missing`.
pub(crate) async fn observe(path: &Path) -> Result<Observed, ToolError> {
    if tokio::fs::symlink_metadata(path).await.is_err() {
        return Ok(Observed {
            state: "missing".to_owned(),
            content: None,
        });
    }
    let content = read(path).await?;
    Ok(Observed {
        state: digest(&content),
        content: Some(content),
    })
}

/// A change as a plan shows it before the file is read: the lines replaced and their replacement, or the start of
/// the new content.
pub(crate) fn preview(edit: &Edit) -> String {
    let lines = |sign: char, text: &str| -> Vec<String> {
        text.lines().map(|line| format!("{sign}{line}")).collect()
    };
    let mut shown = match edit {
        Edit::Replace { old, new } => [lines('-', old), lines('+', new)].concat(),
        Edit::Write(content) => lines('+', content),
    };
    if shown.len() > PREVIEW_LINES {
        let more = shown.len() - PREVIEW_LINES;
        shown.truncate(PREVIEW_LINES);
        shown.push(format!("[… {more} more lines …]"));
    }
    shown.join("\n")
}

/// Reads the file `change` is about and plans the change, putting in the `secrets` it names. `installed` tells
/// whether a check's program is on this system. `Ok(Err(text))` when the file already holds what the change would
/// write. The error says, for the model, why the change cannot be made.
pub async fn plan(
    change: &FileChange,
    os: Os,
    protected: &[PathBuf],
    secrets: &Secrets,
    held: &[PathBuf],
    installed: impl Fn(&str) -> bool,
) -> Result<Result<Planned, String>, ToolError> {
    let path = &change.path;
    let unreadable = |source| ToolError::Read {
        path: path.clone(),
        source,
    };
    let exists = tokio::fs::symlink_metadata(path).await.is_ok();
    // The real path: a link to a protected file is just as protected, and the file it leads to is the one changed.
    let real = if exists {
        tokio::fs::canonicalize(path).await.map_err(unreadable)?
    } else {
        let (Some(folder), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(invalid(format!("{} is not a file's path", path.display())));
        };
        let folder = tokio::fs::canonicalize(folder)
            .await
            .map_err(|_| invalid(format!("the folder {} does not exist", folder.display())))?;
        folder.join(name)
    };
    if protected.iter().any(|folder| real.starts_with(folder)) {
        return Err(ToolError::OwnData(path.clone()));
    }
    let real = plain(real);
    // A secret file by its name, or one Kelpy wrote a secret into.
    let secret_file = files::is_secret(&real) || held.contains(&real);
    if secret_file && matches!(change.edit, Edit::Replace { .. }) {
        return Err(ToolError::Secret(path.clone()));
    }
    let before = if exists {
        Some(read(&real).await?)
    } else {
        None
    };
    // A file with Windows line ends keeps them: what the model wrote is given the same.
    let crlf = before.as_deref().is_some_and(|text| text.contains("\r\n"));
    let ends = |text: &str| -> Result<String, ToolError> {
        let (text, _) = secrets.fill(text)?;
        Ok(if crlf && !text.contains('\r') {
            text.replace('\n', "\r\n")
        } else {
            text
        })
    };
    let after = match (&change.edit, &before) {
        (Edit::Replace { .. }, None) => {
            return Err(invalid(format!(
                "{} does not exist: use write_file to create it",
                path.display()
            )));
        }
        (Edit::Replace { old, new }, Some(before)) => {
            let (old, new) = (ends(old)?, ends(new)?);
            match before.matches(old.as_str()).count() {
                1 => before.replacen(old.as_str(), &new, 1),
                0 => {
                    return Err(invalid(format!(
                        "`old` does not occur in {}: read the file again and copy the text exactly",
                        path.display()
                    )));
                }
                times => {
                    return Err(invalid(format!(
                        "`old` occurs {times} times in {}: include the lines around it, so that it occurs once",
                        path.display()
                    )));
                }
            }
        }
        (Edit::Write(content), _) => ends(content)?,
    };
    if before.as_deref() == Some(after.as_str()) {
        return Ok(Err(format!(
            "{} already holds this content: nothing to change.",
            path.display()
        )));
    }
    if is_json(&real) {
        let valid = |text: &str| serde_json::from_str::<serde_json::Value>(text).map(|_| ());
        if let Err(error) = valid(&after) {
            // Kept only when the file was not valid JSON before either.
            if before.as_deref().is_none_or(|before| valid(before).is_ok()) {
                return Err(invalid(format!(
                    "the new content is not valid JSON: {error}"
                )));
            }
        }
    }
    // The diff shows secrets by their names, and nothing of a secret file before the change.
    let shown_before = match &before {
        Some(_) if secret_file => String::new(),
        Some(before) => secrets.redact(before),
        None => String::new(),
    };
    Ok(Ok(Planned {
        diff: diff(&real, &shown_before, &secrets.redact(&after)),
        check: check(os, &real).filter(|check| installed(&check.program)),
        floor: floor(os, &real),
        secret: secret_file || secrets.holds_any(&after),
        real,
        before,
        after,
    }))
}

/// Makes a planned change, writing what it does to `output`; the previous version goes to `backups`. Returns where.
pub async fn apply(
    planned: &Planned,
    backups: &Path,
    output: &Output,
) -> Result<Option<PathBuf>, ToolError> {
    let real = &planned.real;
    let now = match tokio::fs::symlink_metadata(real).await {
        Ok(_) => Some(read(real).await?),
        Err(_) => None,
    };
    if now != planned.before {
        return Err(ToolError::ChangedMeanwhile(real.clone()));
    }
    let checked_before = match &planned.check {
        Some(check) => {
            output.set_phase("checking the file before the change");
            Some(run_check(check).await?)
        }
        None => None,
    };
    if let (Some(check), Some((passed, text))) = (&planned.check, &checked_before) {
        output.line(&if *passed {
            format!("Before the change, `{}` passed.", check.display)
        } else {
            format!(
                "Before the change, `{}` failed already:\n{text}",
                check.display
            )
        });
    }
    let saved = match &planned.before {
        Some(before) => {
            output.set_phase("backing up the file");
            let saved = backup(backups, real, before)
                .await
                .map_err(|source| ToolError::Write {
                    path: backups.to_owned(),
                    source,
                })?;
            output.line(&format!("Backed up to {}.", saved.display()));
            Some(saved)
        }
        None => None,
    };
    let written = |source| ToolError::Write {
        path: real.clone(),
        source,
    };
    output.set_phase("writing the file");
    write(real, &planned.after, planned.secret)
        .await
        .map_err(written)?;
    if read(real).await? != planned.after {
        // Something else wrote it meanwhile.
        return Err(ToolError::ChangedMeanwhile(real.clone()));
    }
    let (added, removed) = counts(&planned.diff);
    output.line(&format!(
        "Wrote {} (+{added} −{removed} lines).",
        real.display()
    ));
    let Some(check) = &planned.check else {
        return Ok(saved);
    };
    output.set_phase("checking the file after the change");
    let (passed, text) = run_check(check).await?;
    if passed {
        output.line(&format!("After the change, `{}` passed.", check.display));
        return Ok(saved);
    }
    output.line(&format!(
        "After the change, `{}` failed:\n{text}",
        check.display
    ));
    if checked_before.is_some_and(|(passed, before)| !passed && before == text) {
        output.line("It failed in the same way before: the change is kept.");
        return Ok(saved);
    }
    output.set_phase("putting the previous version back");
    match &planned.before {
        Some(before) => write(real, before, planned.secret).await.map_err(written)?,
        None => tokio::fs::remove_file(real).await.map_err(written)?,
    }
    output.line("The previous version was put back.");
    Err(ToolError::CheckFailed {
        check: check.display.clone(),
        output: text,
    })
}

/// The text of the regular file at `path`, up to [`MAX_FILE`].
async fn read(path: &Path) -> Result<String, ToolError> {
    let unreadable = |source| ToolError::Read {
        path: path.to_owned(),
        source,
    };
    let metadata = tokio::fs::metadata(path).await.map_err(unreadable)?;
    if !metadata.is_file() {
        return Err(ToolError::NotRegularFile(path.to_owned()));
    }
    if metadata.len() > MAX_FILE {
        return Err(ToolError::TooLarge(path.to_owned()));
    }
    let bytes = tokio::fs::read(path).await.map_err(unreadable)?;
    if bytes.contains(&0) {
        return Err(ToolError::NotText(path.to_owned()));
    }
    String::from_utf8(bytes).map_err(|_| ToolError::NotText(path.to_owned()))
}

/// Writes `content` to `path`, keeping an existing file's owner, mode and permissions. A new file that may hold
/// secrets is readable by Kelpy's account only, from its creation: the service that needs it is then given access
/// explicitly, never by the umask.
async fn write(path: &Path, content: &str, secret: bool) -> io::Result<()> {
    let (path, content) = (path.to_owned(), content.to_owned());
    tokio::task::spawn_blocking(move || write_now(&path, content.as_bytes(), secret))
        .await
        .map_err(io::Error::other)?
}

/// Unix: a new file beside the old one, given its mode and owner, then renamed over it, so that the file is never
/// seen half written.
#[cfg(unix)]
fn write_now(path: &Path, content: &[u8], secret: bool) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(if secret { 0o600 } else { 0o666 })
                .open(path)?;
            file.write_all(content)?;
            return file.sync_all();
        }
        Err(error) => return Err(error),
    };
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let staged = path.with_file_name(format!(".{name}.kelpy-{}", std::process::id()));
    let made = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&staged)?;
        file.write_all(content)?;
        file.sync_all()?;
        std::os::unix::fs::chown(&staged, Some(metadata.uid()), Some(metadata.gid()))?;
        std::fs::set_permissions(&staged, metadata.permissions())?;
        std::fs::rename(&staged, path)
    })();
    if made.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    made
}

/// Windows: written in place, which keeps the file's permissions (ACL) as they are; a new file inherits its
/// folder's.
#[cfg(not(unix))]
fn write_now(path: &Path, content: &[u8], _secret: bool) -> io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    file.write_all(content)?;
    file.sync_all()
}

/// Saves `content`, the version of `real` before a change, in `backups`; readable by Kelpy's account only, as
/// the file may be. Returns where.
async fn backup(backups: &Path, real: &Path, content: &str) -> io::Result<PathBuf> {
    let (backups, content) = (backups.to_owned(), content.to_owned());
    let flat: String = real
        .to_string_lossy()
        .chars()
        .map(|char| {
            if char.is_alphanumeric() || ".-".contains(char) {
                char
            } else {
                '_'
            }
        })
        .collect();
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis());
    let target = backups.join(format!("{millis}-{}", flat.trim_start_matches('_')));
    let saved = target.clone();
    tokio::task::spawn_blocking(move || {
        let mut folder = std::fs::DirBuilder::new();
        folder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut folder, 0o700);
        folder.create(&backups)?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&target)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()
    })
    .await
    .map_err(io::Error::other)??;
    Ok(saved)
}

/// `path` without Windows' `\\?\` prefix, which `canonicalize` adds: shown as the administrator would type it.
pub(crate) fn plain(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => path,
    }
}

/// Runs a file's check: whether it passed, and what it said.
async fn run_check(check: &Command) -> Result<(bool, String), ToolError> {
    let said = Output::new();
    let code = run_local(check, None, Some(RUN_TIME), true, None, &said).await?;
    Ok((code == 0, said.snapshot().1.trim().to_owned()))
}

/// The check known for the file at `path`, such as `nginx -t` for nginx's configuration.
fn check(os: Os, path: &Path) -> Option<Command> {
    let command = |program: &str, args: &[&str]| Command {
        program: program.to_owned(),
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        env: Vec::new(),
        display: std::iter::once(program)
            .chain(args.iter().copied())
            .collect::<Vec<_>>()
            .join(" "),
    };
    let text = path.to_string_lossy();
    if path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("php"))
    {
        return Some(command("php", &["-l", &text]));
    }
    if os == Os::Windows {
        return None;
    }
    let path = as_known(os, path);
    let under = |folder: &str| path.starts_with(folder);
    if under("/etc/nginx") {
        Some(command("nginx", &["-t"]))
    } else if under("/etc/apache2") {
        Some(command("apache2ctl", &["configtest"]))
    } else if under("/etc/httpd") {
        Some(command("apachectl", &["configtest"]))
    } else if under("/etc/ssh/sshd_config") || under("/etc/ssh/sshd_config.d") {
        Some(command("sshd", &["-t"]))
    } else if under("/etc/sudoers") || under("/etc/sudoers.d") {
        Some(command("visudo", &["-c"]))
    } else if os == Os::MacOs && (under("/etc/pf.conf") || under("/etc/pf.anchors")) {
        // The main rules load the anchors: parsing them, without loading, checks both.
        Some(command("pfctl", &["-nf", "/etc/pf.conf"]))
    } else {
        None
    }
}

/// `path` as the administrator knows it. macOS keeps `/etc`, `/var` and `/tmp` in `/private` and links them there,
/// so a resolved path reads `/private/etc/sudoers` where the rules here say `/etc/sudoers`.
fn as_known(os: Os, path: &Path) -> Cow<'_, Path> {
    if os == Os::MacOs
        && let Ok(rest) = path.strip_prefix("/private")
        && ["etc", "var", "tmp"]
            .iter()
            .any(|top| rest.starts_with(top))
    {
        return Cow::Owned(Path::new("/").join(rest));
    }
    Cow::Borrowed(path)
}

fn is_json(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
}

/// Why a change of the file at `path` waits for approval in every Approval mode: a mistake there could lock the
/// administrator out or keep the system from starting.
pub(crate) fn floor(os: Os, path: &Path) -> Option<String> {
    let what = if os == Os::Windows {
        // As written on disk, like `path`: the variable may differ in case, as `C:\WINDOWS`.
        let system = std::env::var_os("SystemRoot").map(PathBuf::from)?;
        let system = plain(std::fs::canonicalize(&system).unwrap_or(system));
        path.starts_with(system)
            .then_some("a Windows system file")?
    } else {
        let path = as_known(os, path);
        const FLOOR: &[(&str, &str)] = &[
            ("/etc/ssh", "the SSH configuration"),
            ("/etc/sudoers", "the sudo rules"),
            ("/etc/sudoers.d", "the sudo rules"),
            ("/etc/pam.d", "the login rules"),
            ("/etc/security", "the login rules"),
            ("/etc/ufw", "the firewall"),
            ("/etc/firewalld", "the firewall"),
            ("/etc/nftables.conf", "the firewall"),
            ("/etc/iptables", "the firewall"),
            ("/etc/hosts.allow", "who may connect"),
            ("/etc/hosts.deny", "who may connect"),
            ("/etc/network", "the network configuration"),
            ("/etc/netplan", "the network configuration"),
            ("/etc/NetworkManager", "the network configuration"),
            (
                "/etc/sysconfig/network-scripts",
                "the network configuration",
            ),
            ("/etc/resolv.conf", "the network configuration"),
            ("/etc/fstab", "the file systems mounted at start"),
            ("/etc/crypttab", "the file systems mounted at start"),
            ("/etc/default/grub", "the boot configuration"),
            ("/boot", "the boot configuration"),
            // macOS: the packet filter, the application firewall and the network settings.
            ("/etc/pf.conf", "the firewall"),
            ("/etc/pf.anchors", "the firewall"),
            ("/etc/pf.os", "the firewall"),
            ("/Library/Preferences/com.apple.alf.plist", "the firewall"),
            (
                "/Library/Preferences/SystemConfiguration",
                "the network configuration",
            ),
        ];
        FLOOR
            .iter()
            .find(|(folder, _)| path.starts_with(folder))
            .map(|(_, what)| *what)?
    };
    Some(format!(
        "It changes {what}: a mistake could lock you out or keep the system from starting, so it always asks."
    ))
}

/// The change from `before` to `after` of the file at `path` as a unified diff, at most 64 KiB. Line ends are shown
/// alike.
pub fn diff(path: &Path, before: &str, after: &str) -> String {
    let (before, after) = (before.replace("\r\n", "\n"), after.replace("\r\n", "\n"));
    let name = path.to_string_lossy();
    let mut text = TextDiff::from_lines(&before, &after)
        .unified_diff()
        .context_radius(3)
        .header(&name, &name)
        .to_string();
    if text.len() > MAX_DIFF {
        let cut = (0..=MAX_DIFF)
            .rev()
            .find(|&index| text.is_char_boundary(index))
            .unwrap_or(0);
        let more = text[cut..].lines().count();
        text.truncate(cut);
        text.push_str(&format!(
            "\n[… {more} more lines of the diff not shown …]\n"
        ));
    }
    text
}

/// The lines a diff adds and removes.
fn counts(diff: &str) -> (usize, usize) {
    diff.lines()
        .filter(|line| !line.starts_with("+++") && !line.starts_with("---"))
        .fold((0, 0), |(added, removed), line| {
            match line.as_bytes().first() {
                Some(b'+') => (added + 1, removed),
                Some(b'-') => (added, removed + 1),
                _ => (added, removed),
            }
        })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn replace(path: &Path, old: &str, new: &str) -> FileChange {
        FileChange {
            path: path.to_owned(),
            edit: Edit::Replace {
                old: old.into(),
                new: new.into(),
            },
            planned: None,
        }
    }

    fn write_all(path: &Path, content: &str) -> FileChange {
        FileChange {
            path: path.to_owned(),
            edit: Edit::Write(content.into()),
            planned: None,
        }
    }

    async fn planned(change: &FileChange) -> Result<Planned, ToolError> {
        planned_with(change, &Secrets::new()).await
    }

    async fn planned_with(change: &FileChange, secrets: &Secrets) -> Result<Planned, ToolError> {
        plan(change, Os::current(), &[], secrets, &[], |_| true)
            .await?
            .map_err(ToolError::Invalid)
    }

    /// A check that fails while the file at `path` holds "broken".
    fn not_broken(path: &Path) -> Command {
        let path = path.to_string_lossy().into_owned();
        let (program, args) = if cfg!(windows) {
            let script = format!(
                "exit [int](Select-String -Quiet -SimpleMatch broken -LiteralPath '{path}')"
            );
            (
                "powershell",
                vec!["-NoProfile".to_owned(), "-Command".to_owned(), script],
            )
        } else {
            let script = "! grep -q broken \"$1\"".to_owned();
            ("sh", vec!["-c".to_owned(), script, "sh".to_owned(), path])
        };
        Command {
            program: program.into(),
            args,
            env: Vec::new(),
            display: "check".into(),
        }
    }

    #[tokio::test]
    async fn a_change_is_shown_backed_up_and_written_keeping_line_ends_and_mode() {
        let (dir, backups) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let path = dir.path().join("site.conf");
        fs::write(&path, "listen 80;\r\nroot /var/www;\r\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        }

        let planned = planned(&replace(&path, "listen 80;\n", "listen 8080;\n"))
            .await
            .unwrap();
        assert!(
            planned.diff.contains("-listen 80;\n+listen 8080;\n"),
            "{}",
            planned.diff
        );
        assert_eq!(planned.after, "listen 8080;\r\nroot /var/www;\r\n");
        assert_eq!(
            (planned.check.as_ref(), planned.floor.as_ref()),
            (None, None)
        );

        let output = Output::new();
        let backup = apply(&planned, backups.path(), &output).await.unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), planned.after);
        let saved: Vec<_> = fs::read_dir(backups.path()).unwrap().flatten().collect();
        assert_eq!(saved.len(), 1);
        assert_eq!(backup, Some(saved[0].path()));
        assert_eq!(output.phase().as_deref(), Some("writing the file"));
        // What Kelpy expects to find from now on.
        let recorded = planned.recorded();
        assert_eq!(
            recorded.state,
            observe(&path).await.unwrap().state,
            "{recorded:?}"
        );
        assert_eq!(recorded.content.as_deref(), Some(planned.after.as_str()));
        assert_eq!(
            fs::read_to_string(saved[0].path()).unwrap(),
            "listen 80;\r\nroot /var/www;\r\n"
        );
        assert!(output.snapshot().1.contains("(+1 \u{2212}1 lines)"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!((mode(&path), mode(&saved[0].path())), (0o640, 0o600));
        }
    }

    #[tokio::test]
    async fn the_text_replaced_occurs_once_and_a_change_changes_something() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hosts.txt");
        fs::write(&path, "a\na\nb\n").unwrap();
        let problem = |result: Result<Planned, ToolError>| result.unwrap_err().to_string();

        assert!(problem(planned(&replace(&path, "a", "c")).await).contains("occurs 2 times"));
        assert!(problem(planned(&replace(&path, "z", "c")).await).contains("does not occur"));
        assert!(
            problem(planned(&write_all(&path, "a\na\nb\n")).await).contains("nothing to change")
        );
        let missing = dir.path().join("new.txt");
        assert!(problem(planned(&replace(&missing, "a", "b")).await).contains("use write_file"));
        // A new file: nothing before it, nothing to back up.
        let created = planned(&write_all(&missing, "hello\n")).await.unwrap();
        assert_eq!(created.before, None);
        assert!(created.notes()[0].contains("deleting it"));
        let nowhere = dir.path().join("no").join("such.txt");
        assert!(problem(planned(&write_all(&nowhere, "x")).await).contains("does not exist"));
    }

    #[tokio::test]
    async fn nothing_is_written_when_the_file_changed_after_it_was_shown() {
        let (dir, backups) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let path = dir.path().join("app.ini");
        fs::write(&path, "debug = false\n").unwrap();
        let planned = planned(&replace(&path, "false", "true")).await.unwrap();
        fs::write(&path, "debug = false\nport = 80\n").unwrap();

        let result = apply(&planned, backups.path(), &Output::new()).await;
        assert!(matches!(result, Err(ToolError::ChangedMeanwhile(_))));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "debug = false\nport = 80\n"
        );
        assert_eq!(fs::read_dir(backups.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn a_change_that_breaks_the_check_is_put_back_one_that_found_it_broken_is_kept() {
        let (dir, backups) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let path = dir.path().join("server.conf");
        fs::write(&path, "fine\n").unwrap();
        let mut breaking = planned(&write_all(&path, "broken\n")).await.unwrap();
        breaking.check = Some(not_broken(&path));
        let output = Output::new();
        let result = apply(&breaking, backups.path(), &output).await;
        assert!(
            matches!(result, Err(ToolError::CheckFailed { .. })),
            "{result:?}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "fine\n");
        assert!(
            output
                .snapshot()
                .1
                .contains("The previous version was put back.")
        );

        fs::write(&path, "broken\n").unwrap();
        let mut still = planned(&write_all(&path, "broken\nport 80\n"))
            .await
            .unwrap();
        still.check = Some(not_broken(&path));
        let output = Output::new();
        apply(&still, backups.path(), &output).await.unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "broken\nport 80\n");
        assert!(
            output
                .snapshot()
                .1
                .contains("failed in the same way before")
        );
    }

    #[tokio::test]
    async fn secrets_kelpys_data_and_invalid_json_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let env = dir.path().join(".env");
        assert!(matches!(
            planned(&replace(&env, "A=1", "A=2")).await,
            Err(ToolError::Secret(_))
        ));
        let own = plan(
            &write_all(&dir.path().join("notes.md"), "x"),
            Os::current(),
            &[fs::canonicalize(dir.path()).unwrap()],
            &Secrets::new(),
            &[],
            |_| true,
        )
        .await;
        assert!(matches!(own, Err(ToolError::OwnData(_))));

        let json = dir.path().join("settings.json");
        fs::write(&json, "{}").unwrap();
        let invalid = planned(&write_all(&json, "{")).await.unwrap_err();
        assert!(invalid.to_string().contains("not valid JSON"));
        // Already invalid before: the change is not refused for it.
        fs::write(&json, "{").unwrap();
        assert!(planned(&write_all(&json, "{ ")).await.is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_new_file_that_may_hold_secrets_is_readable_by_its_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let backups = tempfile::tempdir().unwrap();
        let env = dir.path().join(".env");
        let planned = planned(&write_all(
            &env,
            "DB_PASSWORD=x
",
        ))
        .await
        .unwrap();
        apply(&planned, backups.path(), &Output::new())
            .await
            .unwrap();
        let mode = fs::metadata(&env).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{mode:o}");
    }

    #[tokio::test]
    async fn secrets_are_written_never_shown_and_a_secret_file_only_whole() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("wp-config.php");
        fs::write(&config, "<?php define('DB_PASSWORD', 'old-secret');\n").unwrap();
        let secrets = Secrets::new();
        secrets.generate("wp_db").unwrap();
        let (value, _) = secrets.fill("{{secret:wp_db}}").unwrap();

        let written = planned_with(
            &write_all(
                &config,
                "<?php define('DB_PASSWORD', '{{secret:wp_db}}');\n",
            ),
            &secrets,
        )
        .await
        .unwrap();
        assert!(written.after.contains(&value));
        assert!(written.secret);
        assert!(
            written.diff.contains("{{secret:wp_db}}"),
            "{}",
            written.diff
        );
        // Neither the new value nor the old content.
        assert!(!written.diff.contains(&value));
        assert!(!written.diff.contains("old-secret"));
        assert_eq!(written.recorded().content, None);
        assert!(
            written
                .notes()
                .iter()
                .any(|note| note.contains("may hold secrets"))
        );

        // Written again with the same secret: nothing to change.
        fs::write(&config, &written.after).unwrap();
        let again = plan(
            &write_all(
                &config,
                "<?php define('DB_PASSWORD', '{{secret:wp_db}}');\n",
            ),
            Os::current(),
            &[],
            &secrets,
            &[],
            |_| true,
        )
        .await
        .unwrap();
        assert!(again.unwrap_err().contains("nothing to change"));
        // A secret that does not exist.
        let missing = planned(&write_all(&dir.path().join("a.conf"), "{{secret:nope}}"))
            .await
            .unwrap_err();
        assert!(missing.to_string().contains("generate_secret"));
    }

    #[test]
    fn a_plan_shows_the_lines_replaced_and_the_start_of_a_new_file() {
        let edit = Edit::Replace {
            old: "listen 80;".into(),
            new: "listen 8080;".into(),
        };
        assert_eq!(preview(&edit), "-listen 80;\n+listen 8080;");
        let long = Edit::Write("x\n".repeat(50));
        let shown = preview(&long);
        assert!(shown.ends_with("[… 10 more lines …]"), "{shown}");
    }

    #[test]
    fn checks_and_the_floor_follow_the_file() {
        let linux = |path: &str| {
            (
                check(Os::Linux, Path::new(path)).map(|check| check.display),
                floor(Os::Linux, Path::new(path)),
            )
        };
        assert_eq!(
            linux("/etc/nginx/sites-enabled/blog"),
            (Some("nginx -t".into()), None)
        );
        let (sshd, sshd_floor) = linux("/etc/ssh/sshd_config");
        assert_eq!(sshd.as_deref(), Some("sshd -t"));
        assert!(sshd_floor.unwrap().contains("the SSH configuration"));
        assert!(linux("/etc/fstab").1.is_some());
        assert_eq!(linux("/srv/app/config.yml"), (None, None));
    }

    #[test]
    fn on_macos_the_resolved_etc_is_still_etc() {
        // Resolving `/etc/sudoers` on macOS gives `/private/etc/sudoers`: the floor and the check still apply.
        let sudoers = Path::new("/private/etc/sudoers");
        assert!(
            floor(Os::MacOs, sudoers)
                .unwrap()
                .contains("the sudo rules")
        );
        assert_eq!(
            check(Os::MacOs, sudoers).map(|check| check.display),
            Some("visudo -c".into())
        );
        assert!(floor(Os::MacOs, Path::new("/etc/ssh/sshd_config")).is_some());
        assert!(floor(Os::MacOs, Path::new("/private/home/etc/sudoers")).is_none());
        // Elsewhere `/private` has no such meaning.
        assert!(floor(Os::Linux, sudoers).is_none());
    }

    #[test]
    fn on_macos_the_firewall_and_network_settings_always_ask() {
        for path in [
            "/etc/pf.conf",
            "/private/etc/pf.conf",
            "/private/etc/pf.anchors/com.apple",
            "/Library/Preferences/com.apple.alf.plist",
            "/Library/Preferences/SystemConfiguration/preferences.plist",
        ] {
            let why = floor(Os::MacOs, Path::new(path)).unwrap_or_else(|| panic!("{path}"));
            assert!(
                why.contains("firewall") || why.contains("network"),
                "{path}: {why}"
            );
        }
        for path in ["/private/etc/pf.conf", "/etc/pf.anchors/custom"] {
            assert_eq!(
                check(Os::MacOs, Path::new(path)).map(|check| check.display),
                Some("pfctl -nf /etc/pf.conf".into()),
                "{path}"
            );
        }
        // Not a firewall file, and no `pfctl` on Linux.
        assert!(floor(Os::MacOs, Path::new("/etc/pf.conf.bak")).is_none());
        assert!(check(Os::Linux, Path::new("/etc/pf.conf")).is_none());
    }
}
