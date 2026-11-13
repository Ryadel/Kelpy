//! `kelpy cleanup`: removes Kelpy from this computer: its home with everything Kelpy keeps there, the PATH entry
//! and the user pointer.
//!
//! Only Kelpy's own files are deleted: the marker, `config.toml`, the executable, and Kelpy's folders, which are
//! Kelpy's whole because a home is never created over names that were already there (see `Home::init`). A home in a
//! folder shared with other files ("this folder") keeps those files, and the folder stays unless it ends up empty.
//! The configuration and the marker go last, once everything else is gone: until then the folder is still a home,
//! what is left in it is still provably Kelpy's, and the cleanup can be run again, still knowing from the
//! configuration where the server's shared memory is, which stays. A server folder found in the home stays too.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Result;
use kelpy_core::config::Config;
use kelpy_core::home::{self, Home};
use kelpy_core::server_folder;

use crate::setup::same_file;
use crate::{platform, prompt, runtime};

pub fn run(home: &Home, assume_yes: bool) -> Result<ExitCode> {
    let root = home.root();
    // Held to the end: no runtime and no update can start in this home while it is being removed.
    let Some(hold) = runtime::hold(home)? else {
        println!(
            "Kelpy is running or being updated from {}. Stop it first (`kelpy stop`), then run this again.",
            root.display()
        );
        return Ok(ExitCode::from(1));
    };
    let lock = runtime::lock_path(home);
    // Kept whatever happens: the runtime's lock, and the server's memory shared with the other administrators.
    // The configuration says where that is: it goes last, so that a cleanup run again still knows.
    let mut keep = vec![lock.clone()];
    let shared = match Config::load(&home.config_path()) {
        Ok(config) => config.memory.shared,
        Err(_) if !home.config_path().exists() => None,
        Err(error) => {
            println!(
                "Cannot read {}: {error}. It says where the server's memory shared with the other administrators \
                 is, which must stay. Nothing removed: correct it, then run this again.",
                home.config_path().display()
            );
            return Ok(ExitCode::from(1));
        }
    };
    if let Some(shared) = &shared {
        match shared_folder(root, shared) {
            SharedFolder::Elsewhere => {}
            SharedFolder::Inside(path) => keep.push(path),
            SharedFolder::Around => {
                println!(
                    "Kelpy's home {} is inside the server's shared memory, {}: removing it could delete what the \
                     other administrators share. Nothing removed.",
                    root.display(),
                    shared.display()
                );
                return Ok(ExitCode::from(1));
            }
        }
    }

    let entries = own_entries(home);
    // Whatever the configuration says, a server folder found in the home stays, and so does a folder that cannot
    // be read: it might hold one.
    for entry in entries.iter().filter(|entry| is_real_dir(entry)) {
        server_folders(entry, &mut keep);
    }
    println!("This removes Kelpy from this computer:");
    println!("  - from {}:", root.display());
    for entry in &entries {
        let name = entry.file_name().unwrap_or_default().to_string_lossy();
        let slash = if entry.is_dir() { "/" } else { "" };
        println!("      {name}{slash}");
    }
    match &shared {
        None => {
            println!("    conversations, server memory and workspace files are deleted for good;")
        }
        Some(shared) => {
            println!("    conversations and workspace files are deleted for good;");
            println!(
                "    the server's memory, shared in {}, stays for the other administrators;",
                shared.display()
            );
        }
    }
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
    let config = home.config_path();
    let mut files_left = false;
    for entry in entries
        .iter()
        .filter(|entry| **entry != marker && **entry != config)
    {
        let result = if running
            .as_deref()
            .is_some_and(|running| same_file(running, entry))
        {
            // This very executable: Unix deletes it; Windows moves it to the temp folder.
            platform::remove_running_executable(entry)
        } else if entry.is_dir() {
            remove_dir_keeping(entry, &keep)
        } else {
            fs::remove_file(entry)
        };
        if let Err(error) = result {
            println!("! Could not delete {}: {error}", entry.display());
            files_left = true;
        }
    }
    let mut kept: Vec<&PathBuf> = keep
        .iter()
        .filter(|kept| **kept != lock && fs::symlink_metadata(kept).is_ok())
        .collect();
    kept.sort();
    kept.dedup();
    for kept in &kept {
        println!(
            "✓ Kept {}: it holds, or may hold, a server's shared memory",
            kept.display()
        );
    }
    if files_left {
        complete = false;
    } else if let Err(error) = fs::remove_file(&config)
        && error.kind() != io::ErrorKind::NotFound
    {
        println!("! Could not delete {}: {error}", config.display());
        complete = false;
    } else if let Err(error) = fs::remove_file(&marker) {
        println!("! Could not delete {}: {error}", marker.display());
        complete = false;
    } else {
        // No longer a home, so nothing can start a runtime here: the lock can go, and its folder with it.
        drop(hold);
        let _ = fs::remove_file(&lock);
        if let Some(dir) = lock.parent() {
            let _ = fs::remove_dir(dir);
        }
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

/// Where the server folder is, against the home.
#[derive(Debug, PartialEq, Eq)]
enum SharedFolder {
    Elsewhere,
    /// Inside the home, at this path under it: kept.
    Inside(PathBuf),
    /// Around the home, or the home itself.
    Around,
}

/// Where the server folder `shared` is against the home `root`, as the system finds both. Kelpy refuses a server
/// folder that overlaps the home, but a configuration made before, or by hand, may still have one.
fn shared_folder(root: &Path, shared: &Path) -> SharedFolder {
    let (home, shared) = (
        server_folder::resolved(root),
        server_folder::resolved(shared),
    );
    if home.starts_with(&shared) {
        SharedFolder::Around
    } else if let Ok(under) = shared.strip_prefix(&home) {
        SharedFolder::Inside(root.join(under))
    } else {
        SharedFolder::Elsewhere
    }
}

fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

/// Adds to `keep` the server folders in `dir` (marked by their marker file), and the folders that cannot be read.
/// Links are not followed: deleting one deletes the link only.
fn server_folders(dir: &Path, keep: &mut Vec<PathBuf>) {
    if dir.join(server_folder::MARKER_FILE).is_file() {
        keep.push(dir.to_path_buf());
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        keep.push(dir.to_path_buf());
        return;
    };
    for entry in entries {
        match entry {
            Ok(entry) if is_real_dir(&entry.path()) => server_folders(&entry.path(), keep),
            Ok(_) => {}
            Err(_) => {
                keep.push(dir.to_path_buf());
                return;
            }
        }
    }
}

/// Deletes `dir` with everything in it but what is in `keep` and the folders that lead to it, which stay.
fn remove_dir_keeping(dir: &Path, keep: &[PathBuf]) -> io::Result<()> {
    if keep.iter().any(|keep| dir.starts_with(keep)) {
        return Ok(());
    }
    if !keep.iter().any(|keep| keep.starts_with(dir)) {
        return fs::remove_dir_all(dir);
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if keep.contains(&path) {
            continue;
        }
        if keep.iter().any(|keep| keep.starts_with(&path)) {
            remove_dir_keeping(&path, keep)?;
        } else if fs::symlink_metadata(&path)?.is_dir() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
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
    // Updates stage and park executables in the data folder, which is Kelpy's whole: nothing next to the
    // executable is taken for Kelpy's by its name. Earlier versions parked one next to it, recognised by content.
    let executable = home.root().join(home::executable_name());
    [home.marker_path(), home.config_path()]
        .into_iter()
        .chain(home.own_dirs())
        .chain(platform::legacy_previous(&executable))
        .chain([executable])
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

        assert_eq!(run(&home, true).unwrap(), ExitCode::SUCCESS);
        assert!(root.join("holiday.jpg").is_file());
        assert!(!home.config_path().exists());
        assert!(!home.memory_dir().exists());
        assert!(!home.data_dir().exists());
    }

    #[test]
    fn the_server_s_shared_memory_in_the_home_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Kelpy");
        let home = Home::init(&root).unwrap();
        // Configured before Kelpy refused a shared folder in the home, or by hand.
        let shared = home.workspace_dir().join("server");
        fs::create_dir_all(shared.join("memory")).unwrap();
        fs::write(shared.join("server.db"), b"records").unwrap();
        fs::write(home.workspace_dir().join("draft.txt"), b"mine").unwrap();
        kelpy_core::config::save_shared_memory(&home.config_path(), &shared).unwrap();

        assert_eq!(run(&home, true).unwrap(), ExitCode::SUCCESS);
        assert!(shared.join("server.db").is_file());
        assert!(shared.join("memory").is_dir());
        assert!(!home.workspace_dir().join("draft.txt").exists());
        assert!(!home.config_path().exists());
        assert!(!home::is_home(&root));
    }

    #[test]
    fn a_cleanup_run_again_still_keeps_the_server_s_shared_memory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Kelpy");
        let home = Home::init(&root).unwrap();
        let shared = home.workspace_dir().join("server");
        fs::create_dir_all(shared.join("memory")).unwrap();
        fs::write(shared.join("server.db"), b"records").unwrap();
        kelpy_core::config::save_shared_memory(&home.config_path(), &shared).unwrap();
        let log = home.logs_dir().join("kelpy.log");
        fs::write(&log, b"log").unwrap();
        {
            let Some(_held) = hold_undeletable(&log) else {
                return; // Deleting cannot be refused here (e.g. running as root): nothing to check.
            };
            // The first stops on the log: the configuration, which says what to keep, is still there.
            assert_eq!(run(&home, true).unwrap(), ExitCode::from(1));
            assert!(home.config_path().exists());
            assert!(shared.join("server.db").is_file());
        }
        assert_eq!(run(&home, true).unwrap(), ExitCode::SUCCESS);
        assert!(shared.join("server.db").is_file());
        assert!(!log.exists());
        assert!(!home::is_home(&root));
    }

    #[test]
    fn a_configuration_that_cannot_be_read_stops_the_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::init(dir.path().join("Kelpy")).unwrap();
        let draft = home.workspace_dir().join("draft.txt");
        fs::write(&draft, b"mine").unwrap();
        fs::write(home.config_path(), "[memory\nshared = ").unwrap();

        assert_eq!(run(&home, true).unwrap(), ExitCode::from(1));
        assert!(draft.is_file());
        assert!(home::is_home(home.root()));
    }

    #[test]
    fn a_server_folder_in_the_home_is_kept_without_the_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Kelpy");
        let home = Home::init(&root).unwrap();
        let shared = home.workspace_dir().join("team").join("server");
        fs::create_dir_all(shared.join("memory")).unwrap();
        fs::write(
            shared.join(server_folder::MARKER_FILE),
            b"Kelpy server folder",
        )
        .unwrap();
        fs::write(shared.join("server.db"), b"records").unwrap();
        fs::write(home.workspace_dir().join("team").join("draft.txt"), b"mine").unwrap();
        fs::remove_file(home.config_path()).unwrap();

        assert_eq!(run(&home, true).unwrap(), ExitCode::SUCCESS);
        assert!(shared.join("server.db").is_file());
        assert!(!home.workspace_dir().join("team").join("draft.txt").exists());
        assert!(!home::is_home(&root));
    }

    #[test]
    fn a_home_inside_the_server_s_shared_memory_is_not_removed() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::init(dir.path().join("Kelpy")).unwrap();
        kelpy_core::config::save_shared_memory(&home.config_path(), dir.path()).unwrap();

        assert_eq!(run(&home, true).unwrap(), ExitCode::from(1));
        assert!(home.config_path().exists());
        assert!(home::is_home(home.root()));
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
        assert!(home.config_path().exists());
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
    fn files_named_like_kelpys_next_to_it_are_not_kelpys() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Downloads");
        fs::create_dir(&root).unwrap();
        // There before the home: names earlier versions of Kelpy used next to the executable.
        fs::write(root.join("kelpy.4242-0.new"), b"someone else's").unwrap();
        fs::create_dir(root.join("kelpy.4242-1.new")).unwrap();
        fs::write(root.join("kelpy.old"), b"someone else's").unwrap();
        let home = Home::init(&root).unwrap();

        assert_eq!(run(&home, true).unwrap(), ExitCode::SUCCESS);
        assert!(root.join("kelpy.4242-0.new").is_file());
        assert!(root.join("kelpy.4242-1.new").is_dir());
        assert!(root.join("kelpy.old").is_file());
        assert!(!home.config_path().exists());
    }

    #[test]
    fn cleanup_waits_for_a_runtime_or_an_update() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::init(dir.path().join("Kelpy")).unwrap();
        let _update = runtime::hold(&home).unwrap().unwrap();

        assert_eq!(run(&home, true).unwrap(), ExitCode::from(1));
        assert!(home.config_path().exists());
        assert!(home.data_dir().exists());
    }

    #[test]
    fn the_lock_outlives_the_rest_of_the_data_folder() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::init(dir.path().join("Kelpy")).unwrap();
        let hold = runtime::hold(&home).unwrap().unwrap();
        fs::write(home.data_dir().join("kelpy.db"), b"conversations").unwrap();
        fs::create_dir(home.data_dir().join("backups")).unwrap();

        remove_dir_keeping(&home.data_dir(), &[runtime::lock_path(&home)]).unwrap();
        assert!(!home.data_dir().join("kelpy.db").exists());
        assert!(!home.data_dir().join("backups").exists());
        // The same lock, still held: no second owner while the first one lives.
        assert!(runtime::hold(&home).unwrap().is_none());
        drop(hold);
        assert!(runtime::hold(&home).unwrap().is_some());
    }

    #[test]
    fn cleanup_deletes_a_home_of_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Kelpy");
        let home = Home::init(&root).unwrap();
        assert_eq!(run(&home, true).unwrap(), ExitCode::SUCCESS);
        assert!(!root.exists());
    }
}
