//! Reading files and folders for the model, never those that may hold secrets: what is read is sent to the AI
//! service. Only regular files are read: a named pipe or a device could keep the reader waiting without end, and a
//! read in progress cannot be stopped.

use std::ffi::OsStr;
use std::io;
use std::path::{Component, Path, PathBuf};

use tokio::fs::File;
use tokio::io::AsyncReadExt;

use crate::ToolError;

/// The most of a file read.
const MAX_FILE: u64 = 256 << 10;
/// The most entries of a folder listed.
const MAX_ENTRIES: usize = 500;
/// The most entries of a folder counted: the rest are not looked at.
const MAX_COUNTED: usize = 10_000;

/// The text of the file at `path`, unless it may hold secrets, is under a `protected` folder, is not a regular
/// file or is not text.
pub async fn read(path: &Path, protected: &[PathBuf]) -> Result<String, ToolError> {
    let unreadable = |source| ToolError::Read {
        path: path.to_owned(),
        source,
    };
    // The real path: a link to a protected file is just as protected.
    let real = tokio::fs::canonicalize(path).await.map_err(unreadable)?;
    if is_secret(&real) || protected.iter().any(|folder| real.starts_with(folder)) {
        return Err(ToolError::Protected(path.to_owned()));
    }
    if !tokio::fs::metadata(&real)
        .await
        .map_err(unreadable)?
        .is_file()
    {
        return Err(ToolError::NotRegularFile(path.to_owned()));
    }
    // Checked again as opened: the file may have been replaced meanwhile.
    let file = open(&real).await.map_err(unreadable)?;
    let metadata = file.metadata().await.map_err(unreadable)?;
    if !metadata.is_file() {
        return Err(ToolError::NotRegularFile(path.to_owned()));
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE)
        .read_to_end(&mut bytes)
        .await
        .map_err(unreadable)?;
    if bytes.contains(&0) {
        return Err(ToolError::NotText(path.to_owned()));
    }
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if metadata.len() > MAX_FILE {
        text.push_str(&format!(
            "\n[… {} more bytes not read …]\n",
            metadata.len() - MAX_FILE
        ));
    }
    Ok(text)
}

/// Opens `path` to read it. On Unix without waiting: opening a named pipe nobody writes to would wait.
async fn open(path: &Path) -> io::Result<File> {
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK);
    options.open(path).await
}

/// The entries of the folder at `path`: folders first, then files with their sizes, each with its last change.
pub async fn list(path: &Path) -> Result<String, ToolError> {
    let unlistable = |source| ToolError::List {
        path: path.to_owned(),
        source,
    };
    let mut entries = tokio::fs::read_dir(path).await.map_err(unlistable)?;
    let mut lines = Vec::new();
    let (mut more, mut counted) = (0, true);
    while let Some(entry) = entries.next_entry().await.map_err(unlistable)? {
        if lines.len() == MAX_ENTRIES {
            if lines.len() + more == MAX_COUNTED {
                counted = false;
                break;
            }
            more += 1;
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let line = match entry.metadata().await {
            Ok(metadata) => {
                let modified = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or_else(String::new, |since| format!("  {}", utc(since.as_secs())));
                if metadata.is_dir() {
                    (0, format!("{name}/{modified}"))
                } else {
                    (1, format!("{name}  {} bytes{modified}", metadata.len()))
                }
            }
            Err(_) => (1, name),
        };
        lines.push(line);
    }
    lines.sort();
    let mut text: String = lines.into_iter().map(|(_, line)| line + "\n").collect();
    if text.is_empty() {
        text.push_str("(empty)\n");
    }
    if !counted {
        text.push_str(&format!("[… more than {more} entries not listed …]\n"));
    } else if more > 0 {
        text.push_str(&format!("[… {more} more entries not listed …]\n"));
    }
    text.push_str("Times are UTC.\n");
    Ok(text)
}

/// A Unix time as `2026-11-01 21:05`, in UTC.
fn utc(seconds: u64) -> String {
    let (days, of_day) = (seconds / 86_400, seconds % 86_400);
    // Howard Hinnant's days-to-civil algorithm, on days since 1970-01-01.
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let of_era = shifted % 146_097;
    let year_of_era = (of_era - of_era / 1_460 + of_era / 36_524 - of_era / 146_096) / 365;
    let of_year = of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * of_year + 2) / 153;
    let day = of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    format!(
        "{year}-{month:02}-{day:02} {:02}:{:02}",
        of_day / 3_600,
        of_day % 3_600 / 60
    )
}

/// Whether the file at the real `path` may hold secrets: private keys and certificates with their keys, password
/// stores, environment files, the system's password hashes and registry hives, SSH and GnuPG folders.
pub(crate) fn is_secret(path: &Path) -> bool {
    const NAMES: &[&str] = &[
        "shadow",
        "shadow-",
        "gshadow",
        "gshadow-",
        "master.passwd",
        ".env",
        ".netrc",
        ".pgpass",
        ".my.cnf",
        ".git-credentials",
        "credentials",
        "wp-config.php",
        "master.key",
        "id_rsa",
        "id_dsa",
        "id_ecdsa",
        "id_ed25519",
    ];
    const EXTENSIONS: &[&str] = &[
        "pem", "key", "p12", "pfx", "jks", "keystore", "kdbx", "ppk", "keytab",
    ];
    /// What an SSH folder may show: no key of its own.
    const SSH_PUBLIC: &[&str] = &["known_hosts", "authorized_keys", "config"];
    /// The registry hives that hold password hashes and secrets.
    const HIVES: &[&str] = &["sam", "security", "system"];

    let lower = |name: &OsStr| name.to_string_lossy().to_lowercase();
    let Some(name) = path.file_name().map(lower) else {
        return false;
    };
    let folders: Vec<String> = path
        .parent()
        .into_iter()
        .flat_map(Path::components)
        .filter_map(|component| match component {
            Component::Normal(folder) => Some(lower(folder)),
            _ => None,
        })
        .collect();
    let extension = path.extension().map(lower).unwrap_or_default();
    NAMES.contains(&name.as_str())
        || name.starts_with(".env.")
        || EXTENSIONS.contains(&extension.as_str())
        || (name.starts_with("ssh_host_") && name.ends_with("_key"))
        || (folders.last().is_some_and(|folder| folder == ".ssh")
            && !name.ends_with(".pub")
            && !SSH_PUBLIC.contains(&name.as_str()))
        || folders.iter().any(|folder| folder == ".gnupg")
        || (HIVES.contains(&name.as_str())
            && folders.ends_with(&["system32".to_owned(), "config".to_owned()]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_are_shown_as_dates() {
        assert_eq!(utc(0), "1970-01-01 00:00");
        assert_eq!(utc(951_825_600), "2000-02-29 12:00");
        assert_eq!(utc(1_793_574_300), "2026-11-01 23:05");
    }

    #[test]
    fn files_that_may_hold_secrets_are_recognised() {
        for secret in [
            "/etc/shadow",
            "/var/www/html/wp-config.php",
            "/srv/app/.env",
            "/srv/app/.env.production",
            "/home/admin/.ssh/id_ed25519",
            "/home/admin/.ssh/deploy",
            "/etc/ssh/ssh_host_rsa_key",
            "/etc/ssl/private/site.key",
            "/root/.gnupg/pubring.kbx",
        ] {
            assert!(is_secret(Path::new(secret)), "{secret}");
        }
        if cfg!(windows) {
            assert!(is_secret(Path::new(r"C:\Windows\System32\config\SAM")));
            assert!(is_secret(Path::new(r"C:\certs\site.pfx")));
        }
        for plain in [
            "/etc/nginx/nginx.conf",
            "/etc/ssh/ssh_host_rsa_key.pub",
            "/etc/ssh/sshd_config",
            "/home/admin/.ssh/known_hosts",
            "/home/admin/.ssh/id_ed25519.pub",
            "/var/log/system",
            "/srv/app/environment.md",
        ] {
            assert!(!is_secret(Path::new(plain)), "{plain}");
        }
    }

    #[tokio::test]
    async fn files_are_read_as_text_and_protected_folders_never() {
        let dir = tempfile::tempdir().unwrap();
        let notes = dir.path().join("notes.txt");
        std::fs::write(&notes, "port 8080\n").unwrap();
        assert_eq!(read(&notes, &[]).await.unwrap(), "port 8080\n");

        let binary = dir.path().join("blob.bin");
        std::fs::write(&binary, [0, 1, 2]).unwrap();
        assert!(
            read(&binary, &[])
                .await
                .unwrap_err()
                .to_string()
                .contains("not a text file")
        );

        let data = dir.path().join("data");
        std::fs::create_dir(&data).unwrap();
        std::fs::write(data.join("runtime.json"), "{}").unwrap();
        let protected = [std::fs::canonicalize(&data).unwrap()];
        let refused = read(&data.join("runtime.json"), &protected)
            .await
            .unwrap_err()
            .to_string();
        assert!(refused.contains("never sends it"), "{refused}");
        std::fs::write(dir.path().join(".env"), "TOKEN=x").unwrap();
        let refused = read(&dir.path().join(".env"), &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(refused.contains("never sends it"), "{refused}");

        let listed = list(dir.path()).await.unwrap();
        assert!(listed.starts_with("data/"), "{listed}");
        assert!(listed.contains("notes.txt  10 bytes"), "{listed}");
    }

    #[tokio::test]
    async fn only_regular_files_are_read() {
        let dir = tempfile::tempdir().unwrap();
        let refused = read(dir.path(), &[]).await.unwrap_err().to_string();
        assert!(refused.contains("not a regular file"), "{refused}");
    }

    /// A named pipe nobody writes to would keep its reader waiting, past any stop.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_named_pipe_is_refused_without_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let pipe = dir.path().join("pipe");
        let made = std::process::Command::new("mkfifo")
            .arg(&pipe)
            .status()
            .unwrap();
        assert!(made.success());
        let read = tokio::time::timeout(std::time::Duration::from_secs(5), read(&pipe, &[]))
            .await
            .expect("reading a named pipe waited");
        let refused = read.unwrap_err().to_string();
        assert!(refused.contains("not a regular file"), "{refused}");
    }

    #[tokio::test]
    async fn a_large_folder_is_listed_without_reading_it_all() {
        let dir = tempfile::tempdir().unwrap();
        for number in 0..MAX_ENTRIES + 3 {
            std::fs::write(dir.path().join(format!("{number:04}.log")), "").unwrap();
        }
        let listed = list(dir.path()).await.unwrap();
        let shown = listed.lines().filter(|line| line.contains(".log  0 bytes"));
        assert_eq!(shown.count(), MAX_ENTRIES);
        assert!(
            listed.contains("[… 3 more entries not listed …]"),
            "{listed}"
        );
    }
}
