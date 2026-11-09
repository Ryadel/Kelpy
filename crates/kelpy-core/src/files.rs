//! The files the administrator uploads for Kelpy to use, such as a configuration, a database dump or a certificate:
//! in the home's `workspace/files/` folder, which only Kelpy's account can open. The model reads them where they
//! are; their paths reach it in the administrator's messages.
//!
//! An upload from the Web UI is written to a staged file in the folder, `.upload-<n>`, and renamed to its name once
//! complete, so it is listed only whole. A name already taken gets a number: `site.conf` becomes `site (2).conf`.
//! SFTP writes in place, as its clients expect: a file it is sending is listed while it arrives.
//!
//! Which names are valid and which files can be opened is the same for every interface. A file is opened only if it
//! is a regular file with a single link: never through a link, which could lead out of the folder. Truncating, when
//! asked, comes after that check.

use std::fmt::Display;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use futures_util::{Stream, StreamExt};
use kelpy_common::lock;
use serde::Serialize;
use tokio::io::AsyncWriteExt;

use crate::events::{Event, Events};
use crate::private_file;

/// The largest file accepted: a database dump or a backup archive, not a disk image.
pub const MAX_UPLOAD: u64 = 2 << 30;
/// How much of a file decides whether it is text.
const SAMPLE: usize = 8 * 1024;
const STAGED: &str = ".upload-";
/// The longest name kept, in bytes: most file systems allow 255.
const MAX_NAME: usize = 200;

#[derive(Debug, thiserror::Error)]
pub enum FilesError {
    #[error("there is no file named {0}")]
    NotFound(String),
    #[error("the file is larger than {} GiB", MAX_UPLOAD >> 30)]
    TooLarge,
    #[error("the file name is not valid")]
    InvalidName,
    #[error("the upload was interrupted: {0}")]
    Interrupted(String),
    #[error("cannot write the file: {0}")]
    Io(#[from] io::Error),
}

/// How to open an uploaded file.
#[derive(Debug, Clone, Copy, Default)]
pub struct Access {
    pub read: bool,
    pub write: bool,
    pub create: Create,
    pub truncate: bool,
}

impl Access {
    pub const READ: Self = Self {
        read: true,
        write: false,
        create: Create::No,
        truncate: false,
    };
}

/// Whether opening a file creates it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Create {
    #[default]
    No,
    IfMissing,
    /// Only if missing: an existing file is an error.
    New,
}

/// A file uploaded, as the interfaces show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileView {
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
    /// Unix time, in seconds.
    pub modified: i64,
    /// Whether the model can read it as text.
    pub text: bool,
}

pub struct Files {
    dir: PathBuf,
    events: Events,
    /// Choosing a free name and taking it happen under this lock, so that two uploads never take the same name.
    naming: Mutex<()>,
    staged: AtomicU64,
}

impl Files {
    /// The uploads in `dir`. Staged files left by an upload Kelpy did not finish are removed.
    pub fn new(dir: PathBuf, events: Events) -> Self {
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with(STAGED) {
                    // Best effort: a leftover is never listed, and the next start tries again.
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
        Self {
            dir,
            events,
            naming: Mutex::new(()),
            staged: AtomicU64::new(0),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The files, the most recent first. It reads the start of each: call it off the async runtime.
    pub fn list(&self) -> Result<Vec<FileView>, FilesError> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut files = Vec::new();
        for entry in entries {
            let name = entry?.file_name().to_string_lossy().into_owned();
            // What cannot be opened is not listed: staged uploads, links, folders, names the rules refuse.
            match self.open(&name, Access::READ) {
                Ok(file) => files.push(view(file, self.dir.join(&name), name)?),
                Err(FilesError::NotFound(_) | FilesError::InvalidName) => {}
                Err(error) => return Err(error),
            }
        }
        files.sort_by(|a, b| {
            b.modified
                .cmp(&a.modified)
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(files)
    }

    /// Receives the file `name` from `body`, at most [`MAX_UPLOAD`] bytes.
    pub async fn receive<S, B, E>(&self, name: &str, mut body: S) -> Result<FileView, FilesError>
    where
        S: Stream<Item = Result<B, E>> + Unpin,
        B: AsRef<[u8]>,
        E: Display,
    {
        let name = file_name(name).ok_or(FilesError::InvalidName)?;
        self.prepare()?;
        let staged = self.dir.join(format!(
            "{STAGED}{}-{}",
            std::process::id(),
            self.staged.fetch_add(1, Ordering::Relaxed)
        ));
        let received = async {
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staged)
                .await?;
            let mut size = 0u64;
            while let Some(chunk) = body.next().await {
                let chunk = chunk.map_err(|error| FilesError::Interrupted(error.to_string()))?;
                let chunk = chunk.as_ref();
                size += chunk.len() as u64;
                if size > MAX_UPLOAD {
                    return Err(FilesError::TooLarge);
                }
                file.write_all(chunk).await?;
            }
            file.sync_all().await?;
            Ok(())
        }
        .await;
        let taken = received.and_then(|()| self.take(&staged, &name));
        let path = match taken {
            Ok(path) => path,
            Err(error) => {
                // Best effort: a leftover is never listed, and removed at the next start.
                let _ = fs::remove_file(&staged);
                return Err(error);
            }
        };
        self.changed();
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        view(self.open(&name, Access::READ)?, path, name)
    }

    /// Opens the file `name`, as the [module](self) explains.
    pub fn open(&self, name: &str, access: Access) -> Result<fs::File, FilesError> {
        let path = self.dir.join(exact(name)?);
        if access.create != Create::No {
            self.prepare()?;
        }
        let mut options = fs::OpenOptions::new();
        options.read(access.read).write(access.write);
        match access.create {
            Create::No => {}
            Create::IfMissing => {
                options.create(true);
            }
            Create::New => {
                options.create_new(true);
            }
        }
        no_follow(&mut options);
        let file = options.open(&path).map_err(|error| missing(error, name))?;
        if !is_plain(&file)? {
            return Err(FilesError::NotFound(name.to_owned()));
        }
        if access.truncate {
            file.set_len(0)?;
        }
        Ok(file)
    }

    /// Removes the file `name`; a link is removed itself, never what it leads to.
    pub fn remove(&self, name: &str) -> Result<(), FilesError> {
        fs::remove_file(self.dir.join(exact(name)?)).map_err(|error| missing(error, name))?;
        self.changed();
        Ok(())
    }

    /// Renames the file `from` to `to`, in place of any file named `to`.
    pub fn rename(&self, from: &str, to: &str) -> Result<(), FilesError> {
        let target = self.dir.join(exact(to)?);
        fs::rename(self.dir.join(exact(from)?), target).map_err(|error| missing(error, from))?;
        self.changed();
        Ok(())
    }

    /// Tells the interfaces that the files changed, after a write through [`Files::open`].
    pub fn changed(&self) {
        self.events.publish(Event::FilesChanged);
    }

    /// Creates the folder, readable by Kelpy's account only, if it is missing.
    pub fn prepare(&self) -> io::Result<()> {
        if self.dir.is_dir() {
            return Ok(());
        }
        fs::create_dir_all(&self.dir)?;
        private_file::restrict_dir(&self.dir)
    }

    /// Renames `staged` to `name`, or to the first free [`numbered`] name.
    fn take(&self, staged: &Path, name: &str) -> Result<PathBuf, FilesError> {
        let _naming = lock(&self.naming);
        let free = (1..)
            .map(|n| self.dir.join(numbered(name, n)))
            .find(|path| !path.exists())
            .expect("some number is free");
        fs::rename(staged, &free)?;
        Ok(free)
    }
}

fn view(file: fs::File, path: PathBuf, name: String) -> Result<FileView, FilesError> {
    use std::io::Read;

    let metadata = file.metadata()?;
    let modified = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64);
    // Text when its first bytes hold no NUL, as `read_file` checks.
    let mut sample = Vec::with_capacity(SAMPLE);
    file.take(SAMPLE as u64).read_to_end(&mut sample)?;
    Ok(FileView {
        name,
        path,
        size: metadata.len(),
        modified,
        text: !sample.contains(&0),
    })
}

/// `name` for `n` 1, else `name (n)` before its extension, the name cut to stay within [`MAX_NAME`]: a name the
/// rules keep as it is, as `name` is.
fn numbered(name: &str, n: u32) -> String {
    if n == 1 {
        return name.to_owned();
    }
    let number = format!(" ({n})");
    let room = MAX_NAME - number.len();
    let (stem, extension) = match name.rfind('.') {
        // An extension too long to leave the stem a few bytes is cut with it.
        Some(dot) if dot > 0 && name.len() - dot + 8 <= room => name.split_at(dot),
        _ => (name, ""),
    };
    let stem = cut(stem, room - extension.len()).trim_start();
    format!("{stem}{number}{extension}")
}

/// `text` cut to at most `bytes`, on a character's boundary.
fn cut(text: &str, bytes: usize) -> &str {
    let end = (0..=bytes.min(text.len()))
        .rev()
        .find(|&end| text.is_char_boundary(end))
        .unwrap_or_default();
    &text[..end]
}

/// `name` if it is a name the rules keep as it is.
fn exact(name: &str) -> Result<&str, FilesError> {
    match file_name(name) {
        Some(valid) if valid == name => Ok(name),
        _ => Err(FilesError::InvalidName),
    }
}

/// Whether `name` is a valid name for an uploaded file, as it is.
pub fn is_valid_name(name: &str) -> bool {
    exact(name).is_ok()
}

/// `error` opening `name`, where a link refused counts as no file.
fn missing(error: io::Error, name: &str) -> FilesError {
    #[cfg(unix)]
    let link = error.raw_os_error() == Some(libc::ELOOP);
    #[cfg(windows)]
    let link = false;
    if link || error.kind() == io::ErrorKind::NotFound {
        FilesError::NotFound(name.to_owned())
    } else {
        FilesError::Io(error)
    }
}

/// Opens a link itself rather than what it leads to, and a FIFO without waiting for its other end.
#[cfg(unix)]
fn no_follow(options: &mut fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
}

#[cfg(windows)]
fn no_follow(options: &mut fs::OpenOptions) {
    use std::os::windows::fs::OpenOptionsExt;
    options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
}

/// Whether `file` is a regular file with a single link.
fn is_plain(file: &fs::File) -> io::Result<bool> {
    let metadata = file.metadata()?;
    Ok(metadata.is_file() && links(file, &metadata)? == 1)
}

#[cfg(unix)]
fn links(_: &fs::File, metadata: &fs::Metadata) -> io::Result<u64> {
    Ok(std::os::unix::fs::MetadataExt::nlink(metadata))
}

#[cfg(windows)]
fn links(file: &fs::File, _: &fs::Metadata) -> io::Result<u64> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    // SAFETY: an all-zero BY_HANDLE_FILE_INFORMATION is valid (integers only).
    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the handle is open for the duration of the call and `information` is writable.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(u64::from(information.nNumberOfLinks))
}

/// A safe file name from what the browser sent: its last part, without characters Windows forbids, spaces and dots
/// at its end, or names Windows reserves.
fn file_name(name: &str) -> Option<String> {
    let last = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let mut clean: String = last
        .chars()
        .map(|char| {
            if char.is_control() || matches!(char, ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                char
            }
        })
        .collect();
    let reserved = |name: &str| {
        let stem = name
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.as_bytes()[3].is_ascii_digit())
    };
    // Room for the `_` a reserved name gets.
    let room = if reserved(clean.trim_start()) {
        MAX_NAME - 1
    } else {
        MAX_NAME
    };
    clean.truncate(cut(&clean, room).len());
    let clean = clean.trim_end_matches([' ', '.']).trim_start();
    if clean.is_empty() || clean.starts_with(STAGED) {
        return None;
    }
    Some(if reserved(clean) {
        format!("_{clean}")
    } else {
        clean.to_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunks(
        parts: &[&'static [u8]],
    ) -> impl Stream<Item = Result<&'static [u8], String>> + Unpin {
        futures_util::stream::iter(parts.iter().map(|part| Ok(*part)).collect::<Vec<_>>())
    }

    #[tokio::test]
    async fn uploads_keep_their_name_and_never_replace_a_file() {
        let folder = tempfile::tempdir().unwrap();
        let files = Files::new(folder.path().join("files"), Events::new());
        assert!(files.list().unwrap().is_empty());
        let first = files
            .receive("C:\\Users\\me\\site.conf", chunks(&[b"server {\n", b"}\n"]))
            .await
            .unwrap();
        assert_eq!(first.name, "site.conf");
        assert_eq!(first.size, 11);
        assert!(first.text);
        let second = files
            .receive("site.conf", chunks(&[b"other"]))
            .await
            .unwrap();
        assert_eq!(second.name, "site (2).conf");
        assert_eq!(fs::read(&first.path).unwrap(), b"server {\n}\n");
        let binary = files
            .receive("dump.gz", chunks(&[b"\x1f\x8b\0\0"]))
            .await
            .unwrap();
        assert!(!binary.text);
        assert_eq!(files.list().unwrap().len(), 3);

        files.remove("site.conf").unwrap();
        assert!(matches!(
            files.remove("site.conf"),
            Err(FilesError::NotFound(_))
        ));
        assert!(matches!(
            files.open("../config.toml", Access::READ),
            Err(FilesError::InvalidName)
        ));
    }

    #[test]
    fn a_link_in_the_folder_leads_nowhere() {
        let folder = tempfile::tempdir().unwrap();
        let (dir, outside) = (
            folder.path().join("files"),
            folder.path().join("secret.txt"),
        );
        fs::create_dir(&dir).unwrap();
        fs::write(&outside, b"outside").unwrap();
        let files = Files::new(dir.clone(), Events::new());
        let write = Access {
            write: true,
            create: Create::IfMissing,
            truncate: true,
            ..Access::default()
        };

        // A hard link: the same file, inside and outside the folder.
        fs::hard_link(&outside, dir.join("hard.txt")).unwrap();
        // A symbolic link, where the system lets this account make one.
        #[cfg(unix)]
        let symbolic = std::os::unix::fs::symlink(&outside, dir.join("soft.txt")).is_ok();
        #[cfg(windows)]
        let symbolic = std::os::windows::fs::symlink_file(&outside, dir.join("soft.txt")).is_ok();
        let names: &[&str] = if symbolic {
            &["hard.txt", "soft.txt"]
        } else {
            &["hard.txt"]
        };
        for name in names {
            assert!(
                matches!(files.open(name, Access::READ), Err(FilesError::NotFound(_))),
                "{name}"
            );
            assert!(
                matches!(files.open(name, write), Err(FilesError::NotFound(_))),
                "{name}"
            );
        }
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
        assert!(files.list().unwrap().is_empty());

        // Removing a link removes the link only.
        for name in names {
            files.remove(name).unwrap();
        }
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
    }

    /// A named pipe nobody writes to would keep whoever opens it waiting, past any stop.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_named_pipe_in_the_folder_is_refused_without_waiting() {
        let folder = tempfile::tempdir().unwrap();
        let made = std::process::Command::new("mkfifo")
            .arg(folder.path().join("pipe.txt"))
            .status()
            .unwrap();
        assert!(made.success());
        let files = Files::new(folder.path().to_owned(), Events::new());
        let opened = tokio::task::spawn_blocking(move || {
            let write = Access {
                write: true,
                ..Access::default()
            };
            (
                files.open("pipe.txt", Access::READ),
                files.open("pipe.txt", write),
                files.list(),
            )
        });
        let (read, write, listed) = tokio::time::timeout(std::time::Duration::from_secs(5), opened)
            .await
            .expect("opening a named pipe waited")
            .unwrap();
        assert!(matches!(read, Err(FilesError::NotFound(_))), "{read:?}");
        assert!(write.is_err());
        assert!(listed.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_interrupted_upload_leaves_nothing() {
        let folder = tempfile::tempdir().unwrap();
        let files = Files::new(folder.path().to_owned(), Events::new());
        let failing = futures_util::stream::iter(vec![Ok(&b"part"[..]), Err("connection reset")]);
        assert!(matches!(
            files.receive("big.sql", failing).await,
            Err(FilesError::Interrupted(_))
        ));
        assert_eq!(fs::read_dir(folder.path()).unwrap().count(), 0);

        // A leftover of an upload Kelpy did not finish is removed at the next start, and never listed.
        fs::write(folder.path().join(".upload-1-0"), b"part").unwrap();
        assert!(files.list().unwrap().is_empty());
        Files::new(folder.path().to_owned(), Events::new());
        assert_eq!(fs::read_dir(folder.path()).unwrap().count(), 0);
    }

    #[test]
    fn names_are_made_safe() {
        assert_eq!(file_name("../../etc/passwd").as_deref(), Some("passwd"));
        assert_eq!(file_name("a:b*?.txt").as_deref(), Some("a_b__.txt"));
        assert_eq!(file_name("notes. . ").as_deref(), Some("notes"));
        assert_eq!(file_name("con.txt").as_deref(), Some("_con.txt"));
        assert_eq!(file_name("COM1").as_deref(), Some("_COM1"));
        assert_eq!(file_name("company.txt").as_deref(), Some("company.txt"));
        assert_eq!(file_name("..").as_deref(), None);
        assert_eq!(file_name(".upload-3-1").as_deref(), None);
        assert_eq!(file_name(&"é".repeat(150)).unwrap().len(), MAX_NAME);
        let reserved = format!("con.{}", "x".repeat(300));
        assert_eq!(file_name(&reserved).unwrap().len(), MAX_NAME);
    }

    #[test]
    fn a_numbered_name_is_one_the_rules_keep_whatever_its_length() {
        assert_eq!(numbered("site.conf", 2), "site (2).conf");
        assert_eq!(numbered(".bashrc", 3), ".bashrc (3)");
        let long = [
            "a".repeat(MAX_NAME),
            format!("{}.tar.gz", "é".repeat(96)),
            format!("a.{}", "x".repeat(MAX_NAME - 2)),
            format!("{} .txt", "b".repeat(190)),
        ];
        for name in &long {
            let name = file_name(name).unwrap();
            for n in [2, 10, 1000] {
                let numbered = numbered(&name, n);
                assert!(numbered.len() <= MAX_NAME, "{numbered}");
                assert_eq!(exact(&numbered).ok(), Some(numbered.as_str()));
                assert!(numbered.contains(&format!("({n})")), "{numbered}");
            }
        }
    }

    #[tokio::test]
    async fn a_second_upload_of_a_name_at_the_limit_gets_a_number_it_can_be_opened_by() {
        let folder = tempfile::tempdir().unwrap();
        let files = Files::new(folder.path().to_owned(), Events::new());
        let name = format!("{}.log", "n".repeat(MAX_NAME - 4));
        for _ in 0..2 {
            files.receive(&name, chunks(&[b"x"])).await.unwrap();
        }
        let listed = files.list().unwrap();
        assert_eq!(listed.len(), 2);
        for file in listed {
            files.open(&file.name, Access::READ).unwrap();
        }
    }
}
