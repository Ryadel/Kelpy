//! The Kelpy home: the one folder holding everything Kelpy owns, with the executable at its root.
//!
//! ```text
//! Kelpy/
//!   kelpy-home.txt  the marker: proof that Kelpy created this home
//!   kelpy[.exe]     the executable
//!   config.toml     configuration, no secrets
//!   data/           SQLite database and runtime state
//!   memory/         persistent Markdown memory, indexed by MEMORY.md
//!   workspace/      default workspace for file and shell tools
//!   logs/
//! ```
//!
//! A folder is a home only if it holds the marker, which [`Home::init`] writes first, and only in a folder that has
//! none of the names above yet. So the configuration and the four folders next to a marker are Kelpy's entirely, and
//! `kelpy cleanup` can delete them whole. A file named `config.toml` proves nothing: other programs use the name too.
//!
//! Two pointer files record where the home is, for copies of Kelpy that do not live inside it:
//! the per-user pointer, and a local pointer left next to an installer the user kept.

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::config::DEFAULT_CONFIG;

pub const CONFIG_FILE: &str = "config.toml";
pub const MARKER_FILE: &str = "kelpy-home.txt";
/// The first line of the marker, by which Kelpy recognises it.
const MARKER_SIGNATURE: &str = "Kelpy home";
const MARKER_NOTE: &str = "Kelpy created this folder's config.toml, data/, memory/, workspace/ and logs/, and owns them: `kelpy cleanup` deletes them. Keep this file: without it, Kelpy does not recognise this folder as its home.
";
pub const LOCAL_POINTER_FILE: &str = "kelpy-installed.txt";
const DEFAULT_HOME_DIR: &str = "Kelpy";
const MEMORY_INDEX_FILE: &str = "MEMORY.md";
/// The folders Kelpy creates in a home and owns entirely.
const OWN_DIRS: [&str; 4] = ["data", "memory", "workspace", "logs"];

const MEMORY_INDEX: &str = "# Memory

Kelpy's persistent memory: one Markdown file per topic, listed in this index.
You can read and edit these files.

## Topics

_None yet._
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Home {
    root: PathBuf,
}

impl Home {
    /// Opens an existing home. Fails if `root` holds no marker.
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        if is_home(&root) {
            Ok(Self { root })
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} is not a Kelpy home (no {MARKER_FILE})", root.display()),
            ))
        }
    }

    /// Creates the home layout under `root`, or completes it. An existing home keeps all its files. A folder that is
    /// not a home yet must not hold any of Kelpy's names already: Kelpy would take over files that are not its own.
    pub fn init(root: impl Into<PathBuf>) -> io::Result<Self> {
        let home = Self { root: root.into() };
        let taken = conflicts(&home.root);
        if !taken.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "{} already has {}, which Kelpy would take over: choose another folder",
                    home.root.display(),
                    taken.join(", ")
                ),
            ));
        }
        // The marker first: from here on the folder is a home, so an interrupted setup is simply resumed.
        fs::create_dir_all(&home.root)?;
        write_if_missing(
            &home.marker_path(),
            &format!(
                "{MARKER_SIGNATURE}
{MARKER_NOTE}"
            ),
        )?;
        write_if_missing(&home.config_path(), DEFAULT_CONFIG)?;
        for dir in home.own_dirs() {
            fs::create_dir_all(dir)?;
        }
        write_if_missing(&home.memory_dir().join(MEMORY_INDEX_FILE), MEMORY_INDEX)?;
        Ok(home)
    }

    /// Kelpy's folders in this home, whose whole content is Kelpy's.
    pub fn own_dirs(&self) -> [PathBuf; 4] {
        OWN_DIRS.map(|name| self.root.join(name))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn marker_path(&self) -> PathBuf {
        self.root.join(MARKER_FILE)
    }

    pub fn config_path(&self) -> PathBuf {
        self.root.join(CONFIG_FILE)
    }

    pub fn data_dir(&self) -> PathBuf {
        self.root.join(OWN_DIRS[0])
    }

    pub fn memory_dir(&self) -> PathBuf {
        self.root.join(OWN_DIRS[1])
    }

    pub fn workspace_dir(&self) -> PathBuf {
        self.root.join(OWN_DIRS[2])
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.root.join(OWN_DIRS[3])
    }
}

/// The names Kelpy would take over if `dir` became a home: its marker, configuration and folders, already there.
/// None for a home.
pub fn conflicts(dir: &Path) -> Vec<&'static str> {
    if is_home(dir) {
        return Vec::new();
    }
    [MARKER_FILE, CONFIG_FILE]
        .into_iter()
        .chain(OWN_DIRS)
        .filter(|name| fs::symlink_metadata(dir.join(name)).is_ok())
        .collect()
}

/// Whether `dir` is a Kelpy home: it holds the marker written by [`Home::init`]. Only the signature line is read.
pub fn is_home(dir: &Path) -> bool {
    let mut start = String::new();
    fs::File::open(dir.join(MARKER_FILE))
        .and_then(|file| {
            file.take(MARKER_SIGNATURE.len() as u64 + 2)
                .read_to_string(&mut start)
        })
        .is_ok_and(|_| start.lines().next() == Some(MARKER_SIGNATURE))
}

/// File name of the installed executable: `kelpy.exe` on Windows, `kelpy` elsewhere.
pub fn executable_name() -> String {
    format!("kelpy{}", std::env::consts::EXE_SUFFIX)
}

/// Home suggested at first run: `~/Kelpy` on every platform.
pub fn default_home() -> Option<PathBuf> {
    dirs::home_dir().map(|dir| dir.join(DEFAULT_HOME_DIR))
}

/// Per-user pointer file: `%APPDATA%\Kelpy\home` on Windows, `~/Library/Application Support/Kelpy/home` on
/// macOS, `$XDG_CONFIG_HOME/kelpy/home` (default `~/.config/kelpy/home`) elsewhere.
pub fn user_pointer_path() -> Option<PathBuf> {
    let app_dir = if cfg!(any(windows, target_os = "macos")) {
        "Kelpy"
    } else {
        "kelpy"
    };
    dirs::config_dir().map(|dir| dir.join(app_dir).join("home"))
}

/// Reads a pointer file: the first line is the home path; anything after it is ignored.
pub fn read_pointer(file: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(file).ok()?;
    let first = text.lines().next()?.trim();
    (!first.is_empty()).then(|| PathBuf::from(first))
}

pub fn write_user_pointer(pointer: &Path, home: &Path) -> io::Result<()> {
    if let Some(parent) = pointer.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(pointer, format!("{}\n", home.display()))
}

/// Writes the local pointer next to a kept installer, so that running it again finds the installation.
pub fn write_local_pointer(dir: &Path, home: &Path, installer_name: &OsStr) -> io::Result<()> {
    let text = format!(
        "{}\nKelpy is installed in the folder above. This {} and this file can be deleted.\n",
        home.display(),
        Path::new(installer_name).display()
    );
    fs::write(dir.join(LOCAL_POINTER_FILE), text)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeSource {
    /// `--home` or `KELPY_HOME`.
    Explicit,
    /// A home in the folder of the running executable.
    NextToExecutable,
    LocalPointer,
    UserPointer,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Lookup {
    Found {
        root: PathBuf,
        source: HomeSource,
    },
    /// The user pointer names a folder that is no longer a home: it was moved or deleted.
    Missing {
        recorded: PathBuf,
    },
    /// No home anywhere: this is the first run.
    FirstRun,
}

pub struct LookupInput<'a> {
    pub explicit: Option<&'a Path>,
    pub executable_dir: &'a Path,
    pub user_pointer: Option<&'a Path>,
}

/// Finds the home. The first match wins: explicit path, the executable's own folder, local pointer, user pointer. A local pointer to a folder that is not a home is skipped.
pub fn locate(input: &LookupInput<'_>) -> Lookup {
    if let Some(root) = input.explicit {
        return Lookup::Found {
            root: root.to_path_buf(),
            source: HomeSource::Explicit,
        };
    }
    if is_home(input.executable_dir) {
        return Lookup::Found {
            root: input.executable_dir.to_path_buf(),
            source: HomeSource::NextToExecutable,
        };
    }
    if let Some(root) = read_pointer(&input.executable_dir.join(LOCAL_POINTER_FILE))
        && is_home(&root)
    {
        return Lookup::Found {
            root,
            source: HomeSource::LocalPointer,
        };
    }
    if let Some(recorded) = input.user_pointer.and_then(read_pointer) {
        return if is_home(&recorded) {
            Lookup::Found {
                root: recorded,
                source: HomeSource::UserPointer,
            }
        } else {
            Lookup::Missing { recorded }
        };
    }
    Lookup::FirstRun
}

fn write_if_missing(path: &Path, contents: &str) -> io::Result<()> {
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => file.write_all(contents.as_bytes()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(
        explicit: Option<&Path>,
        executable_dir: &Path,
        user_pointer: Option<&Path>,
    ) -> Lookup {
        locate(&LookupInput {
            explicit,
            executable_dir,
            user_pointer,
        })
    }

    #[test]
    fn init_creates_the_layout_and_keeps_existing_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Kelpy");
        let home = Home::init(&root).unwrap();
        for dir in [
            home.data_dir(),
            home.memory_dir(),
            home.workspace_dir(),
            home.logs_dir(),
        ] {
            assert!(dir.is_dir(), "{} missing", dir.display());
        }
        assert!(is_home(&root));

        fs::write(home.config_path(), "# edited\n").unwrap();
        Home::init(&root).unwrap();
        assert_eq!(
            fs::read_to_string(home.config_path()).unwrap(),
            "# edited\n"
        );
    }

    #[test]
    fn init_never_takes_over_folders_that_were_already_there() {
        let temp = tempfile::tempdir().unwrap();
        let shared = temp.path().join("Downloads");
        let unrelated = shared.join("logs").join("unrelated-preexisting-log.txt");
        fs::create_dir_all(unrelated.parent().unwrap()).unwrap();
        fs::write(&unrelated, "not Kelpy's").unwrap();

        assert_eq!(conflicts(&shared), ["logs"]);
        let error = Home::init(&shared).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(!is_home(&shared));
        assert!(unrelated.is_file());
    }

    #[test]
    fn a_foreign_config_does_not_make_a_home() {
        let temp = tempfile::tempdir().unwrap();
        let shared = temp.path().join("app");
        let unrelated = shared.join("logs").join("app.log");
        fs::create_dir_all(unrelated.parent().unwrap()).unwrap();
        fs::write(&unrelated, "not Kelpy's").unwrap();
        fs::write(
            shared.join(CONFIG_FILE),
            "[server]
port = 8080
",
        )
        .unwrap();

        assert!(!is_home(&shared));
        assert!(Home::open(&shared).is_err());
        assert_eq!(lookup(None, &shared, None), Lookup::FirstRun);
        assert_eq!(conflicts(&shared), [CONFIG_FILE, "logs"]);
        assert!(Home::init(&shared).is_err());
        assert!(!shared.join(MARKER_FILE).exists());
        assert!(unrelated.is_file());
    }

    #[test]
    fn a_marker_without_the_signature_does_not_make_a_home() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join(MARKER_FILE),
            "Kelpy homework
",
        )
        .unwrap();
        assert!(!is_home(temp.path()));
        assert_eq!(conflicts(temp.path()), [MARKER_FILE]);
    }

    #[test]
    fn an_interrupted_setup_is_resumed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Kelpy");
        let home = Home::init(&root).unwrap();
        fs::remove_dir(home.logs_dir()).unwrap();
        fs::remove_file(home.config_path()).unwrap();

        Home::init(&root).unwrap();
        assert!(home.logs_dir().is_dir());
        assert!(home.config_path().is_file());
    }

    #[test]
    fn open_rejects_a_folder_that_is_not_a_home() {
        let temp = tempfile::tempdir().unwrap();
        assert!(Home::open(temp.path()).is_err());
    }

    #[test]
    fn explicit_home_wins_even_if_not_created_yet() {
        let temp = tempfile::tempdir().unwrap();
        let explicit = temp.path().join("new-home");
        let found = lookup(Some(&explicit), temp.path(), None);
        assert_eq!(
            found,
            Lookup::Found {
                root: explicit,
                source: HomeSource::Explicit
            }
        );
    }

    #[test]
    fn config_next_to_the_executable_is_found() {
        let temp = tempfile::tempdir().unwrap();
        Home::init(temp.path()).unwrap();
        let found = lookup(None, temp.path(), None);
        assert!(matches!(
            found,
            Lookup::Found {
                source: HomeSource::NextToExecutable,
                ..
            }
        ));
    }

    #[test]
    fn local_pointer_comes_before_the_user_pointer() {
        let temp = tempfile::tempdir().unwrap();
        let installer_dir = temp.path().join("Downloads");
        fs::create_dir_all(&installer_dir).unwrap();
        let local_home = Home::init(temp.path().join("local")).unwrap();
        let user_home = Home::init(temp.path().join("user")).unwrap();
        write_local_pointer(&installer_dir, local_home.root(), OsStr::new("kelpy.exe")).unwrap();
        let user_pointer = temp.path().join("pointer");
        write_user_pointer(&user_pointer, user_home.root()).unwrap();

        let found = lookup(None, &installer_dir, Some(&user_pointer));
        assert_eq!(
            found,
            Lookup::Found {
                root: local_home.root().to_path_buf(),
                source: HomeSource::LocalPointer
            }
        );
    }

    #[test]
    fn stale_local_pointer_is_skipped() {
        let temp = tempfile::tempdir().unwrap();
        write_local_pointer(temp.path(), &temp.path().join("gone"), OsStr::new("kelpy")).unwrap();
        assert_eq!(lookup(None, temp.path(), None), Lookup::FirstRun);
    }

    #[test]
    fn stale_user_pointer_reports_the_missing_home() {
        let temp = tempfile::tempdir().unwrap();
        let pointer = temp.path().join("config").join("home");
        let recorded = temp.path().join("moved-away");
        write_user_pointer(&pointer, &recorded).unwrap();
        assert_eq!(
            lookup(None, temp.path(), Some(&pointer)),
            Lookup::Missing { recorded }
        );
    }

    #[test]
    fn nothing_found_means_first_run() {
        let temp = tempfile::tempdir().unwrap();
        let pointer = temp.path().join("no-pointer");
        assert_eq!(lookup(None, temp.path(), Some(&pointer)), Lookup::FirstRun);
    }

    #[test]
    fn pointer_reads_only_the_first_line() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("pointer.txt");
        fs::write(&file, "  /some/home  \nhuman readable text\n").unwrap();
        assert_eq!(read_pointer(&file), Some(PathBuf::from("/some/home")));
        fs::write(&file, "\n").unwrap();
        assert_eq!(read_pointer(&file), None);
    }
}
