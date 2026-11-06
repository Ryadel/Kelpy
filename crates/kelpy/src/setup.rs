//! Finding the Kelpy home, the first run that installs Kelpy, and handing over to the installed copy.
//!
//! The first run asks where Kelpy should live, creates the home there, copies the executable to the home root,
//! adds the home to the user PATH if the user wants it, and records it in the user pointer. A copy of Kelpy found outside the home
//! (typically the installer, if the user kept it) runs the installed copy instead of itself.

use std::cmp::Ordering;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use kelpy_core::VERSION;
use kelpy_core::home::{self, Home, HomeSource, Lookup, LookupInput};

use crate::platform::{self, PathChange};
use crate::{prompt, runtime};

pub struct Options<'a> {
    /// `--home` or `KELPY_HOME`.
    pub explicit_home: Option<&'a Path>,
    /// `--yes`: accept every first-run default.
    pub assume_yes: bool,
    /// Only the commands that start Kelpy may trigger the first run.
    pub allow_first_run: bool,
}

pub enum Resolved {
    /// Continue in this process with this home. `open_browser` is set when the first run chose the Web UI.
    Ready { home: Home, open_browser: bool },
    /// The installed copy ran the command; this is its exit code.
    Delegated(ExitCode),
}

pub async fn resolve(options: &Options<'_>) -> Result<Resolved> {
    let executable = env::current_exe().context("cannot locate the Kelpy executable")?;
    let executable_dir = executable
        .parent()
        .context("the Kelpy executable has no parent folder")?
        .to_path_buf();
    let user_pointer = home::user_pointer_path();
    let lookup = home::locate(&LookupInput {
        explicit: options.explicit_home,
        executable_dir: &executable_dir,
        user_pointer: user_pointer.as_deref(),
    });

    match lookup {
        Lookup::Found {
            root,
            source: HomeSource::Explicit,
        } => Ok(ready(explicit_home(&root, options.allow_first_run)?)),
        Lookup::Found {
            root,
            source: HomeSource::NextToExecutable,
        } => Ok(ready(Home::open(root)?)),
        Lookup::Found { root, .. } => {
            let installed = root.join(home::executable_name());
            if installed.is_file() && !same_file(&installed, &executable) {
                return delegate(&installed, &root, &executable, options)
                    .await
                    .map(Resolved::Delegated);
            }
            Ok(ready(Home::open(root)?))
        }
        Lookup::Missing { recorded } => relocate(&recorded, &executable, &executable_dir, options),
        Lookup::FirstRun if options.allow_first_run => {
            first_run(&executable, &executable_dir, options)
        }
        Lookup::FirstRun => bail!("Kelpy is not set up yet. Run `kelpy` to set it up."),
    }
}

/// The home named by `--home` or `KELPY_HOME`. Like the first run, only the commands that start Kelpy create it:
/// `kelpy stop --home <folder>` on a folder without a home must not leave a new, empty home behind.
fn explicit_home(root: &Path, create: bool) -> Result<Home> {
    let existed = home::is_home(root);
    if !existed && !create {
        bail!(
            "there is no Kelpy home in {}. Start Kelpy with `--home` to create it.",
            root.display()
        );
    }
    let home = Home::init(root)
        .with_context(|| format!("cannot create a Kelpy home in {}", root.display()))?;
    if !existed {
        println!("Created a Kelpy home in {}", root.display());
    }
    Ok(home)
}

fn ready(home: Home) -> Resolved {
    Resolved::Ready {
        home,
        open_browser: false,
    }
}

fn first_run(executable: &Path, executable_dir: &Path, options: &Options<'_>) -> Result<Resolved> {
    let interactive = prompt::interactive() && !options.assume_yes;
    let default_home = home::default_home().context("cannot find your user folder")?;
    let target = if interactive {
        ask_home(&default_home, executable, executable_dir)?
    } else {
        default_home
    };
    let in_place = same_file(&target, executable_dir);
    // Optional: on a server, running `<home>/kelpy` by path is often enough.
    let add_to_path = !platform::is_removable(&target)
        && (!interactive
            || prompt::confirm(
                "Add the Kelpy folder to your PATH, so that `kelpy` works from any folder?",
                true,
            )?);

    let home = install(executable, &target, in_place, add_to_path)?;
    if !in_place && !temporary_installer() {
        if interactive {
            offer_installer_removal(executable, &home)?;
        } else {
            keep_installer(executable, &home);
        }
    }
    let open_browser = interactive && ask_interface()?;
    Ok(Resolved::Ready { home, open_browser })
}

fn ask_home(default_home: &Path, executable: &Path, executable_dir: &Path) -> Result<PathBuf> {
    println!("Kelpy v{VERSION}: first run");
    println!();
    println!("Where should Kelpy live? It keeps its data, memory and workspace there.");
    println!("  1) {}   (recommended)", default_home.display());
    println!("  2) This folder: {}", executable_dir.display());
    println!("  3) Another folder...");
    loop {
        let target = match prompt::choose(3, 1)? {
            1 => default_home.to_path_buf(),
            2 => executable_dir.to_path_buf(),
            _ => prompt::ask_path("Folder: ")?,
        };
        if accept_folder(&target, executable)? {
            return Ok(target);
        }
    }
}

/// A folder can become the home if it already is one, does not exist yet, holds nothing but this executable,
/// or the user agrees to share it with other files.
fn accept_folder(target: &Path, executable: &Path) -> Result<bool> {
    if home::is_home(target) || !target.exists() {
        return Ok(true);
    }
    let taken = home::conflicts(target);
    if !taken.is_empty() {
        println!(
            "{} already has {}, which Kelpy would take over. Choose another folder.",
            target.display(),
            taken.join(", ")
        );
        return Ok(false);
    }
    if !target.is_dir() {
        println!("{} is a file, not a folder.", target.display());
        return Ok(false);
    }
    let others = fs::read_dir(target)?
        .flatten()
        .filter(|entry| !same_file(&entry.path(), executable))
        .count();
    if others == 0 {
        return Ok(true);
    }
    Ok(prompt::confirm(
        &format!(
            "{} already contains other files. Kelpy will add its folders next to them. Continue?",
            target.display()
        ),
        false,
    )?)
}

/// Creates the home, copies the executable into it unless it is already there, adds it to the user PATH if asked
/// and records it in the user pointer. PATH and pointer are skipped on removable media.
fn install(executable: &Path, target: &Path, in_place: bool, add_to_path: bool) -> Result<Home> {
    let existed = home::is_home(target);
    let home = Home::init(target)
        .with_context(|| format!("cannot create the Kelpy home in {}", target.display()))?;
    println!();
    if existed {
        println!("✓ Using the existing Kelpy home in {}", target.display());
    } else {
        println!("✓ Created {}", target.display());
    }

    if !in_place {
        let name = home::executable_name();
        platform::replace_executable(executable, &target.join(&name), &home.data_dir())
            .with_context(|| format!("cannot copy Kelpy to {}", target.display()))?;
        println!("✓ Installed {name} to {}", target.display());
    }

    if platform::is_removable(target) {
        println!(
            "  {} is on removable media: Kelpy is not added to your PATH.",
            target.display()
        );
        return Ok(home);
    }
    if add_to_path {
        report_path_change(platform::add_to_path(target), target);
    } else {
        println!(
            "  Not added to your PATH: run Kelpy as {}",
            target.join(home::executable_name()).display()
        );
    }
    if let Some(pointer) = home::user_pointer_path() {
        home::write_user_pointer(&pointer, target)
            .with_context(|| format!("cannot write {}", pointer.display()))?;
    }
    Ok(home)
}

fn report_path_change(change: std::io::Result<PathChange>, target: &Path) {
    match change {
        Ok(PathChange::Added) => println!(
            "✓ Added {} to your PATH (new terminals will find `kelpy`)",
            target.display()
        ),
        Ok(PathChange::AlreadyPresent) => {
            println!("✓ {} is already in your PATH", target.display())
        }
        Ok(PathChange::Linked {
            link,
            on_path: true,
        }) => println!("✓ Linked {}", link.display()),
        Ok(PathChange::Linked {
            link,
            on_path: false,
        }) => {
            println!("✓ Linked {}", link.display());
            if let Some(dir) = link.parent() {
                println!(
                    "  Add {} to your PATH to run `kelpy` from anywhere.",
                    dir.display()
                );
            }
        }
        Err(error) => println!("! Could not add Kelpy to your PATH: {error}"),
    }
}

/// Set by the install scripts (`scripts/install.sh`, `scripts/install.ps1`), which download Kelpy into a temporary
/// folder and remove it when Kelpy exits: there is no installer file to offer to delete, nor to point back home.
/// An environment variable rather than an option, because an older installed copy receives the same arguments.
fn temporary_installer() -> bool {
    env::var_os("KELPY_TEMPORARY_INSTALLER").is_some_and(|value| value == "1")
}

fn offer_installer_removal(installer: &Path, home: &Home) -> Result<()> {
    println!();
    if !prompt::confirm(
        &format!("Delete the installer file ({})?", installer.display()),
        true,
    )? {
        keep_installer(installer, home);
        return Ok(());
    }
    match platform::remove_running_executable(installer) {
        Ok(()) => println!("✓ Deleted"),
        Err(error) => {
            println!("! Could not delete it: {error}");
            keep_installer(installer, home);
        }
    }
    Ok(())
}

/// Leaves a pointer next to a kept installer, so that running it again starts the installed copy. Best effort:
/// the installer may be on read-only media, and the user pointer covers that case.
fn keep_installer(installer: &Path, home: &Home) {
    let (Some(dir), Some(name)) = (installer.parent(), installer.file_name()) else {
        return;
    };
    if let Err(error) = home::write_local_pointer(dir, home.root(), name) {
        tracing::debug!("cannot write the local pointer next to the installer: {error}");
    }
}

/// Returns whether the Web UI should be opened in the browser. Over SSH there is no browser to open on this side:
/// the banner shows how to reach the Web UI from the admin's computer instead.
fn ask_interface() -> Result<bool> {
    println!();
    if platform::ssh_connection().is_some() {
        return Ok(false);
    }
    println!("How do you want to use Kelpy?");
    println!("  1) Web UI         (opens your browser)");
    println!("  2) Terminal UI");
    if prompt::choose(2, 1)? == 2 {
        println!(
            "The terminal UI is not available yet in this version: starting the Web UI instead."
        );
    }
    println!();
    Ok(true)
}

/// The user pointer names a home that is gone: find it again, or start over.
fn relocate(
    recorded: &Path,
    executable: &Path,
    executable_dir: &Path,
    options: &Options<'_>,
) -> Result<Resolved> {
    if !prompt::interactive() || options.assume_yes {
        bail!(
            "the Kelpy home recorded for this user is missing: {}. Use --home to choose one.",
            recorded.display()
        );
    }
    println!(
        "Kelpy home not found at {}. Was it moved?",
        recorded.display()
    );
    println!("  1) Locate it");
    println!("  2) Start fresh");
    if prompt::choose(2, 1)? == 2 {
        return first_run(executable, executable_dir, options);
    }
    loop {
        let root = prompt::ask_path("Kelpy home folder: ")?;
        if home::is_home(&root) {
            report_path_change(platform::add_to_path(&root), &root);
            if let Some(pointer) = home::user_pointer_path() {
                home::write_user_pointer(&pointer, &root)?;
            }
            return Ok(ready(Home::open(root)?));
        }
        println!(
            "{} is not a Kelpy home (it has no {}).",
            root.display(),
            home::MARKER_FILE
        );
    }
}

/// Runs the installed copy instead of this one, offering to update it first when this copy is newer.
async fn delegate(
    installed: &Path,
    root: &Path,
    this_copy: &Path,
    options: &Options<'_>,
) -> Result<ExitCode> {
    let installed_version = installed_version(installed).await?;
    match compare_versions(VERSION, &installed_version) {
        Ordering::Equal => println!(
            "Kelpy is installed in {}: starting it from there.",
            root.display()
        ),
        Ordering::Less => println!(
            "A newer Kelpy ({installed_version}) is installed in {}: starting it from there.",
            root.display()
        ),
        Ordering::Greater => {
            let update = if prompt::interactive() && !options.assume_yes {
                prompt::confirm(
                    &format!(
                        "Kelpy {installed_version} is installed in {}. Update it to {VERSION}?",
                        root.display()
                    ),
                    true,
                )?
            } else {
                options.assume_yes
            };
            if update {
                // Held until the new executable is in place, so that no Kelpy starts meanwhile.
                let home = Home::open(root)?;
                let Some(hold) = runtime::hold(&home)? else {
                    bail!(
                        "Kelpy is running from {}. Stop it first (Ctrl+C in its window), then run this again.",
                        root.display()
                    );
                };
                platform::replace_executable(this_copy, installed, &home.data_dir())
                    .with_context(|| format!("cannot update {}", installed.display()))?;
                drop(hold);
                println!("✓ Updated Kelpy in {} to {VERSION}", root.display());
            }
        }
    }
    run_installed(installed).await
}

async fn installed_version(installed: &Path) -> Result<String> {
    let output = tokio::process::Command::new(installed)
        .arg("--version")
        .output()
        .await
        .with_context(|| format!("cannot run {}", installed.display()))?;
    // `kelpy --version` prints "kelpy <version>".
    let text = String::from_utf8_lossy(&output.stdout);
    text.split_whitespace()
        .nth(1)
        .map(str::to_owned)
        .with_context(|| {
            format!(
                "unexpected version output from {}: {text:?}",
                installed.display()
            )
        })
}

/// Runs the installed copy with this process's arguments and returns its exit code.
pub(crate) async fn run_installed(installed: &Path) -> Result<ExitCode> {
    let mut child = tokio::process::Command::new(installed)
        .args(env::args_os().skip(1))
        .spawn()
        .with_context(|| format!("cannot start {}", installed.display()))?;
    // Ctrl+C reaches both processes: this one keeps waiting while the installed copy shuts down.
    let status = loop {
        tokio::select! {
            status = child.wait() => break status?,
            _ = tokio::signal::ctrl_c() => {}
        }
    };
    Ok(status
        .code()
        .map_or(ExitCode::FAILURE, |code| ExitCode::from(code as u8)))
}

/// Compares `major.minor.patch` versions, ignoring any pre-release or build suffix.
pub(crate) fn compare_versions(left: &str, right: &str) -> Ordering {
    fn parse(version: &str) -> Option<(u64, u64, u64)> {
        let core = version.split(['-', '+']).next()?;
        let mut parts = core.split('.').map(str::parse::<u64>);
        Some((
            parts.next()?.ok()?,
            parts.next()?.ok()?,
            parts.next()?.ok()?,
        ))
    }
    match (parse(left), parse(right)) {
        (Some(left), Some(right)) => left.cmp(&right),
        _ => left.cmp(right),
    }
}

pub(crate) fn same_file(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert_eq!(compare_versions("0.10.0", "0.9.9"), Ordering::Greater);
        assert_eq!(compare_versions("0.1.0", "0.1.0"), Ordering::Equal);
        assert_eq!(compare_versions("0.1.0", "0.2.0"), Ordering::Less);
        assert_eq!(compare_versions("1.0.0-beta.1", "1.0.0"), Ordering::Equal);
    }

    #[test]
    fn same_file_requires_both_paths_to_exist() {
        let temp = tempfile::tempdir().unwrap();
        assert!(same_file(temp.path(), temp.path()));
        assert!(!same_file(temp.path(), &temp.path().join("missing")));
    }

    #[test]
    fn only_commands_that_start_kelpy_create_an_explicit_home() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("home");

        assert!(explicit_home(&root, false).is_err());
        assert!(!root.exists());

        explicit_home(&root, true).unwrap();
        assert!(home::is_home(&root));
        explicit_home(&root, false).unwrap();
    }
}
