//! `kelpy cleanup`: removes Kelpy from this computer: its home with everything Kelpy keeps there, the PATH entry
//! and the user pointer.
//!
//! Only Kelpy's own files are deleted: the marker, `config.toml`, the executable, and Kelpy's folders, which are
//! Kelpy's whole because a home is never created over names that were already there (see `Home::init`). A home in a
//! folder shared with other files ("this folder") keeps those files, and the folder stays unless it ends up empty.
//! The marker goes last, once everything else is gone: until then the folder is still a home, what is left in it is
//! still provably Kelpy's, and the cleanup can be run again.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Result;
use kelpy_core::home::{self, Home};

use crate::setup::same_file;
use crate::{platform, prompt, runtime};

pub fn run(home: &Home, assume_yes: bool) -> Result<ExitCode> {
    let root = home.root();
    if runtime::is_running(home)? {
        println!(
            "Kelpy is running from {}. Stop it first (`kelpy stop`), then run this again.",
            root.display()
        );
        return Ok(ExitCode::from(1));
    }

    let entries = own_entries(home);
    println!("This removes Kelpy from this computer:");
    println!("  - from {}:", root.display());
    for entry in &entries {
        let name = entry.file_name().unwrap_or_default().to_string_lossy();
        let slash = if entry.is_dir() { "/" } else { "" };
        println!("      {name}{slash}");
    }
    println!("    conversations, server memory and workspace files are deleted for good;");
    println!("  - the Kelpy folder from your PATH, and the record of this home for your user.");
    if has_other_files(root, &entries) {
        println!(
            "Other files in {} are not Kelpy's and are kept.",
            root.display()
        );
    }
    println!();

    let confirmed = if assume_yes {
        true
    } else if prompt::interactive() {
        prompt::confirm("Remove Kelpy?", false)?
    } else {
        println!("Nothing removed: confirm with --yes when running without a terminal.");
        return Ok(ExitCode::from(1));
    };
    if !confirmed {
        println!("Nothing removed.");
        return Ok(ExitCode::from(1));
    }

    let mut complete = true;
    match platform::remove_from_path(root) {
        Ok(true) => println!("✓ Removed from your PATH"),
        Ok(false) => {}
        Err(error) => {
            println!("! Could not remove Kelpy from your PATH: {error}");
            complete = false;
        }
    }
    remove_user_pointer(root);

    let running = env::current_exe().ok();
    let marker = home.marker_path();
    let mut files_left = false;
    for entry in entries.iter().filter(|entry| **entry != marker) {
        let result = if running
            .as_deref()
            .is_some_and(|running| same_file(running, entry))
        {
            // This very executable: Unix deletes it; Windows moves it to the temp folder.
            platform::remove_running_executable(entry)
        } else if entry.is_dir() {
            fs::remove_dir_all(entry)
        } else {
            fs::remove_file(entry)
        };
        if let Err(error) = result {
            println!("! Could not delete {}: {error}", entry.display());
            files_left = true;
        }
    }
    if files_left {
        complete = false;
    } else if let Err(error) = fs::remove_file(&marker) {
        println!("! Could not delete {}: {error}", marker.display());
        complete = false;
    } else {
        complete &= remove_root(root);
    }

    if complete {
        println!("Kelpy has been removed.");
        Ok(ExitCode::SUCCESS)
    } else {
        println!("Kelpy has been removed in part: see the messages above.");
        Ok(ExitCode::from(1))
    }
}

/// Deletes the home folder if nothing else is left in it. Returns false if it is empty but could not be deleted.
fn remove_root(root: &Path) -> bool {
    match fs::remove_dir(root) {
        Ok(()) => println!("✓ Deleted {}", root.display()),
        Err(_) if root.exists() && !has_other_files(root, &[]) => {
            println!(
                "! {} is empty but could not be deleted (is a terminal open in it?). Delete it by hand.",
                root.display()
            );
            return false;
        }
        Err(_) => println!("✓ Deleted Kelpy's files from {}", root.display()),
    }
    true
}

/// What Kelpy keeps in its home, as far as it exists.
fn own_entries(home: &Home) -> Vec<PathBuf> {
    let executable = home.root().join(home::executable_name());
    [home.marker_path(), home.config_path()]
        .into_iter()
        .chain(home.own_dirs())
        .chain([
            executable.with_extension("new"),
            executable.with_extension("old"),
            executable,
        ])
        .filter(|path| fs::symlink_metadata(path).is_ok())
        .collect()
}

fn has_other_files(root: &Path, own: &[PathBuf]) -> bool {
    fs::read_dir(root)
        .is_ok_and(|entries| entries.flatten().any(|entry| !own.contains(&entry.path())))
}

/// Deletes the user pointer if it names this home, and its folder if nothing else is left there.
fn remove_user_pointer(root: &Path) {
    let Some(pointer) = home::user_pointer_path() else {
        return;
    };
    if !home::read_pointer(&pointer).is_some_and(|recorded| same_file(&recorded, root)) {
        return;
    }
    match fs::remove_file(&pointer) {
        Ok(()) => {
            if let Some(dir) = pointer.parent() {
                // Only if empty: the folder may hold other files of this user's Kelpy setup.
                let _ = fs::remove_dir(dir);
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => println!("! Could not delete {}: {error}", pointer.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_kelpys_own_files_are_listed() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::init(dir.path()).unwrap();
        fs::write(dir.path().join("holiday.jpg"), b"not Kelpy's").unwrap();
        fs::write(dir.path().join(home::executable_name()), b"binary").unwrap();

        let entries = own_entries(&home);
        assert!(entries.contains(&home.marker_path()));
        assert!(entries.contains(&home.config_path()));
        assert!(entries.contains(&home.data_dir()));
        assert!(entries.contains(&dir.path().join(home::executable_name())));
        assert!(!entries.iter().any(|entry| entry.ends_with("holiday.jpg")));
        assert!(has_other_files(dir.path(), &entries));
    }

    #[test]
    fn cleanup_keeps_other_files_and_the_shared_folder() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Downloads");
        let home = Home::init(&root).unwrap();
        fs::write(root.join("holiday.jpg"), b"not Kelpy's").unwrap();
        fs::write(home.memory_dir().join("notes.md"), b"memory").unwrap();

        run(&home, true).unwrap();
        assert!(root.join("holiday.jpg").is_file());
        assert!(!home.config_path().exists());
        assert!(!home.memory_dir().exists());
        assert!(!home.data_dir().exists());
    }

    #[test]
    fn the_marker_stays_while_anything_of_kelpy_is_left() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Kelpy");
        let home = Home::init(&root).unwrap();
        let log = home.logs_dir().join("kelpy.log");
        fs::write(&log, b"log").unwrap();
        let Some(_held) = hold_undeletable(&log) else {
            return; // Deleting cannot be refused here (e.g. running as root): nothing to check.
        };

        assert_eq!(run(&home, true).unwrap(), ExitCode::from(1));
        assert!(!home.config_path().exists());
        assert!(log.exists());
        assert!(home::is_home(&root), "a partial cleanup must keep the home");
    }

    /// Makes `file` impossible to delete while the returned value lives, if this system allows it.
    #[cfg(windows)]
    fn hold_undeletable(file: &Path) -> Option<fs::File> {
        use std::os::windows::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(file)
            .ok()
    }

    #[cfg(unix)]
    fn hold_undeletable(file: &Path) -> Option<impl Drop> {
        use std::os::unix::fs::PermissionsExt;
        struct ReadOnly(PathBuf);
        impl Drop for ReadOnly {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
            }
        }
        let dir = file.parent()?.to_path_buf();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).ok()?;
        let guard = ReadOnly(dir);
        // Root ignores permissions: the check below would prove nothing.
        let probe = guard.0.join("probe");
        if fs::write(&probe, b"").is_ok() {
            let _ = fs::remove_file(probe);
            return None;
        }
        Some(guard)
    }

    #[test]
    fn cleanup_deletes_a_home_of_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Kelpy");
        let home = Home::init(&root).unwrap();
        run(&home, true).unwrap();
        assert!(!root.exists());
    }
}
