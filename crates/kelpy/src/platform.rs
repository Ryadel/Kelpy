//! Operating-system specific installation steps: the user PATH, removable media, replacing or removing
//! executables that may be running, and whether Kelpy runs in an SSH session.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub enum PathChange {
    /// Windows: the folder was added to the user PATH.
    #[cfg_attr(not(windows), allow(dead_code))]
    Added,
    /// Windows: the user PATH already held the folder.
    #[cfg_attr(not(windows), allow(dead_code))]
    AlreadyPresent,
    /// Unix: a `kelpy` symlink in `~/.local/bin`, which may itself be missing from PATH.
    #[cfg_attr(windows, allow(dead_code))]
    Linked { link: PathBuf, on_path: bool },
}

/// Copies `source` over `target`, even while `target` is running.
///
/// The copy is staged next to the target and then swapped in. On Windows a running executable cannot be
/// overwritten but can be renamed, so the old one is moved aside to `<name>.old` first; `remove_leftovers`
/// deletes it at a later start.
pub fn replace_executable(source: &Path, target: &Path) -> io::Result<()> {
    let staged = target.with_extension("new");
    fs::copy(source, &staged)?;
    swap_in(&staged, target)
}

/// Writes `content` over `target` as an executable, even while `target` is running, as `replace_executable` does.
pub fn write_executable(content: &[u8], target: &Path) -> io::Result<()> {
    let staged = target.with_extension("new");
    fs::write(&staged, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))?;
    }
    swap_in(&staged, target)
}

fn swap_in(staged: &Path, target: &Path) -> io::Result<()> {
    #[cfg(windows)]
    if target.exists() {
        let old = target.with_extension("old");
        // A leftover of an earlier update, if any; if it is still in use the rename below reports it.
        let _ = fs::remove_file(&old);
        fs::rename(target, &old)?;
    }
    fs::rename(staged, target)
}

/// Deletes leftovers of earlier updates and installer removals. Files still in use are skipped.
pub fn remove_leftovers(home_root: &Path, executable_name: &str) {
    let _ = fs::remove_file(home_root.join(executable_name).with_extension("old"));
    #[cfg(windows)]
    if let Ok(entries) = fs::read_dir(std::env::temp_dir()) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(windows::INSTALLER_PREFIX) && name.ends_with(".exe") {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

/// Removes the installer that is running right now.
///
/// Unix allows deleting a running executable. Windows does not, but allows moving it within the same volume:
/// it goes to the temp folder, and `remove_leftovers` deletes it once it is no longer running.
pub fn remove_running_executable(executable: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        let parked = std::env::temp_dir().join(format!(
            "{}{}.exe",
            windows::INSTALLER_PREFIX,
            std::process::id()
        ));
        fs::rename(executable, parked)
    }
    #[cfg(not(windows))]
    {
        fs::remove_file(executable)
    }
}

/// `SSH_CONNECTION` when Kelpy runs in an SSH session (OpenSSH sets it on Unix and on Windows):
/// `<client ip> <client port> <server ip> <server port>`.
pub fn ssh_connection() -> Option<String> {
    std::env::var("SSH_CONNECTION")
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// Whether standard output may show colours: a terminal, `NO_COLOR` not set (no-color.org), and a console that
/// accepts ANSI sequences, turned on here on Windows.
pub fn colors() -> bool {
    use std::io::IsTerminal;
    io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
        && enable_ansi()
}

#[cfg(windows)]
pub use windows::{add_to_path, enable_ansi, is_removable, remove_from_path};

#[cfg(not(windows))]
pub use unix::{add_to_path, enable_ansi, is_removable, remove_from_path};

#[cfg(windows)]
mod windows {
    use std::io;
    use std::path::{Component, Path, Prefix};

    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
    use windows_sys::Win32::System::Console::{
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, STD_OUTPUT_HANDLE,
        SetConsoleMode,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
    };
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_EXPAND_SZ, REG_SZ};
    use winreg::{RegKey, RegValue};

    use super::PathChange;

    pub const INSTALLER_PREFIX: &str = "kelpy-installer-";

    /// Turns on ANSI sequences in the console of standard output; false where it cannot, such as an old console.
    pub fn enable_ansi() -> bool {
        // SAFETY: the handle comes from GetStdHandle, and `mode` is a valid place for GetConsoleMode to write.
        unsafe {
            let handle = GetStdHandle(STD_OUTPUT_HANDLE);
            let mut mode = 0;
            GetConsoleMode(handle, &mut mode) != 0
                && (mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0
                    || SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0)
        }
    }
    const DRIVE_REMOVABLE: u32 = 2;

    /// Appends `dir` to the user PATH in `HKCU\Environment`, keeping its registry type so that entries such
    /// as `%USERPROFILE%\...` still expand, then tells running programs that the environment changed.
    pub fn add_to_path(dir: &Path) -> io::Result<PathChange> {
        let environment = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags("Environment", KEY_READ | KEY_WRITE)?;
        let (current, keep_plain) = match environment.get_raw_value("Path") {
            Ok(value) => (decode(&value.bytes), value.vtype == REG_SZ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (String::new(), false),
            Err(error) => return Err(error),
        };
        let Some(updated) = path_with_dir(&current, &dir.to_string_lossy()) else {
            return Ok(PathChange::AlreadyPresent);
        };
        let value = RegValue {
            bytes: encode(&updated).into(),
            vtype: if keep_plain { REG_SZ } else { REG_EXPAND_SZ },
        };
        environment.set_raw_value("Path", &value)?;
        broadcast_environment_change();
        Ok(PathChange::Added)
    }

    /// Removes `dir` from the user PATH. Returns whether it was there.
    pub fn remove_from_path(dir: &Path) -> io::Result<bool> {
        let environment = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags("Environment", KEY_READ | KEY_WRITE)?;
        let value = match environment.get_raw_value("Path") {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        let Some(updated) = path_without_dir(&decode(&value.bytes), &dir.to_string_lossy()) else {
            return Ok(false);
        };
        environment.set_raw_value(
            "Path",
            &RegValue {
                bytes: encode(&updated).into(),
                vtype: value.vtype,
            },
        )?;
        broadcast_environment_change();
        Ok(true)
    }

    /// Whether `path` is on a removable drive, where drive letters change between sessions.
    pub fn is_removable(path: &Path) -> bool {
        let Some(Component::Prefix(prefix)) = path.components().next() else {
            return false;
        };
        let (Prefix::Disk(letter) | Prefix::VerbatimDisk(letter)) = prefix.kind() else {
            return false;
        };
        let root: Vec<u16> = format!("{}:\\", char::from(letter))
            .encode_utf16()
            .chain(Some(0))
            .collect();
        // SAFETY: `root` is a valid, NUL-terminated UTF-16 string that outlives the call.
        unsafe { GetDriveTypeW(root.as_ptr()) == DRIVE_REMOVABLE }
    }

    /// PATH entries are compared case-insensitively, ignoring a trailing separator.
    fn normalize(entry: &str) -> String {
        entry.trim().trim_end_matches(['\\', '/']).to_lowercase()
    }

    /// Returns `current` with `dir` appended, or `None` when `dir` is already listed.
    pub(super) fn path_with_dir(current: &str, dir: &str) -> Option<String> {
        let wanted = normalize(dir);
        if current.split(';').any(|entry| normalize(entry) == wanted) {
            return None;
        }
        let current = current.trim_end_matches(';');
        Some(if current.is_empty() {
            dir.to_owned()
        } else {
            format!("{current};{dir}")
        })
    }

    /// Returns `current` without `dir`, or `None` when `dir` is not listed.
    pub(super) fn path_without_dir(current: &str, dir: &str) -> Option<String> {
        let wanted = normalize(dir);
        let entries: Vec<&str> = current.split(';').collect();
        let kept: Vec<&str> = entries
            .iter()
            .copied()
            .filter(|entry| normalize(entry) != wanted)
            .collect();
        (kept.len() != entries.len()).then(|| kept.join(";"))
    }

    fn broadcast_environment_change() {
        let parameter: Vec<u16> = "Environment".encode_utf16().chain(Some(0)).collect();
        // SAFETY: `parameter` is a valid, NUL-terminated UTF-16 string that outlives the call; the result
        // pointer may be null.
        unsafe {
            SendMessageTimeoutW(
                HWND_BROADCAST,
                WM_SETTINGCHANGE,
                0,
                parameter.as_ptr() as isize,
                SMTO_ABORTIFHUNG,
                5000,
                std::ptr::null_mut(),
            );
        }
    }

    fn decode(bytes: &[u8]) -> String {
        let (pairs, _) = bytes.as_chunks::<2>();
        let units: Vec<u16> = pairs.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
        String::from_utf16_lossy(&units)
            .trim_end_matches('\0')
            .to_owned()
    }

    fn encode(text: &str) -> Vec<u8> {
        text.encode_utf16()
            .chain(Some(0))
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn appends_a_new_entry() {
            assert_eq!(
                path_with_dir(r"C:\Windows;%USERPROFILE%\bin", r"C:\Users\me\Kelpy").as_deref(),
                Some(r"C:\Windows;%USERPROFILE%\bin;C:\Users\me\Kelpy")
            );
            assert_eq!(path_with_dir("", r"C:\Kelpy").as_deref(), Some(r"C:\Kelpy"));
            assert_eq!(
                path_with_dir(r"C:\a;", r"C:\Kelpy").as_deref(),
                Some(r"C:\a;C:\Kelpy")
            );
        }

        #[test]
        fn skips_an_entry_already_present() {
            assert_eq!(
                path_with_dir(r"C:\a;c:\users\me\kelpy\;C:\b", r"C:\Users\me\Kelpy"),
                None
            );
        }

        #[test]
        fn removes_every_copy_of_an_entry() {
            assert_eq!(
                path_without_dir(
                    r"C:\a;c:\users\me\kelpy\;C:\b;C:\Users\me\Kelpy",
                    r"C:\Users\me\Kelpy"
                )
                .as_deref(),
                Some(r"C:\a;C:\b")
            );
            assert_eq!(path_without_dir(r"C:\a;C:\b", r"C:\Kelpy"), None);
        }

        #[test]
        fn registry_strings_round_trip() {
            assert_eq!(decode(&encode(r"C:\Kelpy;é")), r"C:\Kelpy;é");
        }
    }
}

#[cfg(not(windows))]
mod unix {
    use std::env;
    use std::fs;
    use std::io;
    use std::path::Path;

    use super::PathChange;

    /// Unix terminals understand ANSI sequences as they are.
    pub fn enable_ansi() -> bool {
        true
    }

    /// Links `~/.local/bin/kelpy` to the installed executable. An existing file there that is not a symlink
    /// is left alone.
    pub fn add_to_path(dir: &Path) -> io::Result<PathChange> {
        let user_home = dirs::home_dir()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home directory"))?;
        let bin = user_home.join(".local").join("bin");
        fs::create_dir_all(&bin)?;
        let link = bin.join("kelpy");
        if let Ok(metadata) = fs::symlink_metadata(&link) {
            if !metadata.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} exists and is not a link", link.display()),
                ));
            }
            fs::remove_file(&link)?;
        }
        std::os::unix::fs::symlink(dir.join("kelpy"), &link)?;
        let on_path = env::var_os("PATH")
            .is_some_and(|path| env::split_paths(&path).any(|entry| entry == bin));
        Ok(PathChange::Linked { link, on_path })
    }

    /// Removes `~/.local/bin/kelpy` if it is a link into `dir`. Returns whether it did.
    pub fn remove_from_path(dir: &Path) -> io::Result<bool> {
        let Some(user_home) = dirs::home_dir() else {
            return Ok(false);
        };
        let link = user_home.join(".local").join("bin").join("kelpy");
        match fs::read_link(&link) {
            Ok(target) if target.starts_with(dir) => {
                fs::remove_file(&link)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    pub fn is_removable(_path: &Path) -> bool {
        false
    }
}
