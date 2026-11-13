//! The server folder: the server's memory, shared by the administrators of a computer, each with a home of their own.
//!
//! ```text
//! Kelpy/                %ProgramData%\Kelpy, /var/lib/kelpy
//!   kelpy-server.txt    the marker
//!   memory/             the pages, MEMORY.md and server.md
//!   server.db           what Kelpy changed and expects to find, and the differences found since
//! ```
//!
//! Only administrators can open it: on Windows it is owned by Administrators and open to SYSTEM and Administrators
//! alone; elsewhere it is root's, mode 0700. It is created that way, and checked each time it is opened, so that a
//! folder another account made in its place is refused: its memory would be a way to steer Kelpy.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::home::{MEMORY_INDEX, MEMORY_INDEX_FILE};
use crate::private_file;

pub const MARKER_FILE: &str = "kelpy-server.txt";
const MARKER: &str = "Kelpy server folder
The server's memory, shared by the administrators of this computer: memory/ and server.db. Only administrators can open this folder. Each administrator's settings, sign-ins and conversations stay in their own Kelpy home.
";
const MEMORY_DIR: &str = "memory";
const DATABASE_FILE: &str = "server.db";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerFolder {
    root: PathBuf,
}

impl ServerFolder {
    /// Where the first run proposes it: `%ProgramData%\Kelpy` on Windows, `/var/lib/kelpy` elsewhere.
    pub fn default_path() -> Option<PathBuf> {
        #[cfg(windows)]
        {
            std::env::var_os("ProgramData").map(|data| PathBuf::from(data).join("Kelpy"))
        }
        #[cfg(not(windows))]
        {
            Some(PathBuf::from("/var/lib/kelpy"))
        }
    }

    /// Opens the server folder at `root`, creating it if missing. Kelpy must run as administrator: only then can it
    /// open the folder, or make it as only administrators can open it. A folder that is there already must be
    /// administrators-only, and empty or a server folder.
    pub fn open_or_create(root: &Path) -> io::Result<Self> {
        let folder = Self {
            root: root.to_path_buf(),
        };
        match fs::symlink_metadata(root) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if let Some(parent) = root.parent() {
                    fs::create_dir_all(parent)?;
                }
                private_file::create_dir_for_administrators(root).map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!(
                            "cannot create {} for administrators only ({error}): Kelpy must run as administrator",
                            root.display()
                        ),
                    )
                })?;
            }
            Err(error) => return Err(error),
            Ok(_) => {}
        }
        if !private_file::administrators_only(root)? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} can be opened or was made by an account that is not an administrator, so Kelpy does not \
                     trust the memory in it: remove the folder, then start Kelpy as administrator to make it again",
                    root.display()
                ),
            ));
        }
        if !folder.marker_path().is_file() {
            let empty = fs::read_dir(root)?.next().is_none();
            if !empty {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "{} already holds files Kelpy did not make: choose another folder",
                        root.display()
                    ),
                ));
            }
            fs::write(folder.marker_path(), MARKER)?;
        }
        fs::create_dir_all(folder.memory_dir())?;
        let index = folder.memory_dir().join(MEMORY_INDEX_FILE);
        if !index.exists() {
            fs::write(index, MEMORY_INDEX)?;
        }
        Ok(folder)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn marker_path(&self) -> PathBuf {
        self.root.join(MARKER_FILE)
    }

    /// The server's memory: its pages.
    pub fn memory_dir(&self) -> PathBuf {
        self.root.join(MEMORY_DIR)
    }

    /// The database of the server's records.
    pub fn database_path(&self) -> PathBuf {
        self.root.join(DATABASE_FILE)
    }
}

/// Whether the server folder `shared` and the home `home` overlap: one inside the other, or the same. The server
/// folder is the other administrators' too, so it must stay out of a home, which `kelpy cleanup` deletes.
pub fn overlaps(shared: &Path, home: &Path) -> bool {
    let (shared, home) = (comparable(&resolved(shared)), comparable(&resolved(home)));
    shared.starts_with(&home) || home.starts_with(&shared)
}

/// `path` as the system finds it: absolute, its links followed as far as it exists, the rest added as written.
pub fn resolved(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut base = absolute.as_path();
    let mut rest = Vec::new();
    let mut found = loop {
        if let Ok(real) = fs::canonicalize(base) {
            break real;
        }
        match base.parent() {
            Some(parent) => {
                rest.push(base.components().next_back());
                base = parent;
            }
            None => break PathBuf::new(),
        }
    };
    // What does not exist yet cannot be a link: `..` there goes back as written.
    for component in rest.into_iter().rev().flatten() {
        match component {
            std::path::Component::ParentDir => {
                found.pop();
            }
            std::path::Component::CurDir => {}
            other => found.push(other),
        }
    }
    found
}

/// A path for comparing: on Windows, where names do not differ by case, in lower case.
fn comparable(path: &Path) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(path.to_string_lossy().to_lowercase())
    } else {
        path.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shared_folder_in_a_home_or_around_it_overlaps_it() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("Kelpy");
        fs::create_dir(&home).unwrap();
        fs::create_dir(home.join("workspace")).unwrap();
        assert!(overlaps(&home.join("workspace").join("shared"), &home));
        assert!(overlaps(&home, &home));
        assert!(overlaps(dir.path(), &home));
        assert!(overlaps(
            &dir.path()
                .join("other")
                .join("..")
                .join("Kelpy")
                .join("memory"),
            &home
        ));
        assert!(!overlaps(&dir.path().join("Kelpy-server"), &home));
        assert!(!overlaps(&dir.path().join("server"), &home));
        if cfg!(windows) {
            assert!(overlaps(&dir.path().join("KELPY").join("shared"), &home));
        }
    }

    #[test]
    fn a_folder_another_account_could_change_is_refused() {
        // Made as this test runs, by its account: not administrators-only (unless the tests run as root).
        let dir = tempfile::tempdir().unwrap();
        if private_file::administrators_only(dir.path()).unwrap() {
            return;
        }
        let error = ServerFolder::open_or_create(dir.path()).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(!dir.path().join(MARKER_FILE).exists());
    }

    #[test]
    fn made_by_an_administrator_it_is_administrators_only_and_holds_the_memory() {
        let elevated = kelpy_tools::Account::current().elevated;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Kelpy");
        if !elevated {
            // Without the rights it cannot be made as it must be.
            assert!(ServerFolder::open_or_create(&root).is_err());
            return;
        }
        let folder = ServerFolder::open_or_create(&root).unwrap();
        assert!(private_file::administrators_only(&root).unwrap());
        assert!(folder.memory_dir().join(MEMORY_INDEX_FILE).is_file());
        // Opened again, as it is.
        fs::write(folder.memory_dir().join("nginx.md"), "# Nginx\n").unwrap();
        let again = ServerFolder::open_or_create(&root).unwrap();
        assert!(again.memory_dir().join("nginx.md").is_file());
    }
}
