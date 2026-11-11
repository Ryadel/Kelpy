//! Starting Kelpy again with administrator rights, through the system's own prompt: User Account Control on
//! Windows, `sudo` elsewhere. Kelpy never asks for or sees the password.

use std::io;
use std::path::Path;

/// Starts `program` with `arguments` as administrator, without a window, once the user allows it in User Account
/// Control's prompt. Returns its PID, or `None` when the prompt was refused or closed. Blocks until it is answered.
#[cfg(windows)]
pub fn through_prompt(program: &Path, arguments: &[String]) -> io::Result<Option<u32>> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, GetLastError};
    use windows_sys::Win32::System::Threading::GetProcessId;
    use windows_sys::Win32::UI::Shell::{
        SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    let wide = |text: &std::ffi::OsStr| -> Vec<u16> { text.encode_wide().chain([0]).collect() };
    let verb = wide("runas".as_ref());
    let file = wide(program.as_os_str());
    let line = arguments
        .iter()
        .map(|argument| quoted(argument))
        .collect::<Vec<_>>()
        .join(" ");
    let parameters = wide(line.as_ref());
    // SAFETY: zero is a valid SHELLEXECUTEINFOW: no strings, no handles; those set below outlive the call.
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = parameters.as_ptr();
    info.nShow = SW_HIDE;
    // SAFETY: `info` is initialised as the call requires; the strings it points to live until it returns.
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        // SAFETY: no other call in between.
        let error = unsafe { GetLastError() };
        return if error == ERROR_CANCELLED {
            Ok(None)
        } else {
            Err(io::Error::from_raw_os_error(error as i32))
        };
    }
    if info.hProcess.is_null() {
        return Err(io::Error::other("Windows started no process"));
    }
    // SAFETY: `hProcess` is the handle SEE_MASK_NOCLOSEPROCESS asked for, closed once its PID is read.
    let pid = unsafe {
        let pid = GetProcessId(info.hProcess);
        CloseHandle(info.hProcess);
        pid
    };
    Ok(Some(pid))
}

/// `argument` as Windows reads it back from a command line (`CommandLineToArgvW`): in quotes where needed, the
/// quotes and the backslashes before them escaped.
#[cfg_attr(not(windows), allow(dead_code))]
fn quoted(argument: &str) -> String {
    if !argument.is_empty() && !argument.contains([' ', '\t', '"']) {
        return argument.to_owned();
    }
    let mut quoted = String::from('"');
    let mut backslashes = 0;
    for char in argument.chars() {
        match char {
            '\\' => backslashes += 1,
            '"' => {
                quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            other => {
                quoted.push_str(&"\\".repeat(backslashes));
                quoted.push(other);
                backslashes = 0;
            }
        }
    }
    quoted.push_str(&"\\".repeat(backslashes * 2));
    quoted.push('"');
    quoted
}

/// Starts `program` with `arguments` as root with `sudo`, in the background once allowed. With `ask`, `sudo` asks
/// for the password in this terminal; without, it only uses a permission given before (`sudo -v`), and fails if
/// there is none. Returns whether `sudo` started it.
#[cfg(unix)]
pub async fn with_sudo(program: &Path, arguments: &[String], ask: bool) -> io::Result<bool> {
    let mut sudo = tokio::process::Command::new("sudo");
    if !ask {
        sudo.arg("-n");
    }
    sudo.arg("-b").arg("--").arg(program).args(arguments);
    if !ask {
        sudo.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
    }
    Ok(sudo.status().await?.success())
}

/// Gives the files of the home in `root` back to its owner: a Kelpy restarted as root creates some there (its
/// database's journal, backups, logs), which the owner's next Kelpy must be able to open. Best effort.
#[cfg(unix)]
pub fn give_back(root: &Path) {
    use std::os::unix::fs::{MetadataExt, lchown};

    fn walk(path: &Path, uid: u32, gid: u32) {
        let Ok(metadata) = std::fs::symlink_metadata(path) else {
            return;
        };
        if metadata.uid() != uid || metadata.gid() != gid {
            let _ = lchown(path, Some(uid), Some(gid));
        }
        if metadata.is_dir() {
            for entry in std::fs::read_dir(path).into_iter().flatten().flatten() {
                walk(&entry.path(), uid, gid);
            }
        }
    }

    let Ok(owner) = std::fs::metadata(root) else {
        return;
    };
    if owner.uid() != 0 {
        walk(root, owner.uid(), owner.gid());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_reach_the_elevated_kelpy_as_given() {
        assert_eq!(quoted("handover"), "handover");
        assert_eq!(
            quoted(r"C:\Users\Dark Angel\Kelpy"),
            r#""C:\Users\Dark Angel\Kelpy""#
        );
        assert_eq!(quoted(r"C:\a b\"), r#""C:\a b\\""#);
        assert_eq!(quoted(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quoted(""), r#""""#);
    }
}
