//! The account actions run as, and whether it has administrator rights: what a change needing them will do.

use crate::Os;

/// The account that runs Kelpy, and so its actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub name: String,
    /// With administrator rights: elevated on Windows, root elsewhere.
    pub elevated: bool,
    os: Os,
}

impl Account {
    /// The account running this process.
    pub fn current() -> Self {
        let os = Os::current();
        let elevated = elevated();
        let name = if elevated && os != Os::Windows {
            "root".to_owned()
        } else {
            ["USERNAME", "USER", "LOGNAME"]
                .iter()
                .find_map(|variable| std::env::var(variable).ok())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "unknown".to_owned())
        };
        Self { name, elevated, os }
    }

    /// The account as the interfaces show it: `jsmith (not elevated)`, `root`.
    pub fn label(&self) -> String {
        match (self.os, self.elevated) {
            (Os::Windows, true) => format!("{} (elevated)", self.name),
            (Os::Windows, false) => format!("{} (not elevated)", self.name),
            (_, true) => self.name.clone(),
            (_, false) => format!("{} (not root)", self.name),
        }
    }

    /// Without administrator rights, what fails and how to have them; `None` with them.
    pub fn limits(&self) -> Option<&'static str> {
        match (self.os, self.elevated) {
            (_, true) => None,
            (Os::Windows, false) => Some(
                "Changes that need administrator rights fail. To allow them, restart Kelpy as administrator from the \
                 System access menu, or start it from a terminal opened as administrator.",
            ),
            (_, false) => Some(
                "Changes that need root fail, unless sudo works without a password. To allow them, start Kelpy with \
                 sudo.",
            ),
        }
    }
}

#[cfg(unix)]
fn elevated() -> bool {
    // SAFETY: a plain system call that cannot fail.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(windows)]
fn elevated() -> bool {
    use std::{mem, ptr};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle; `token` receives a handle closed below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut size = 0u32;
    // SAFETY: `elevation` is the structure the information class names, with its size.
    let read = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&raw mut elevation).cast(),
            mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        )
    } != 0;
    // SAFETY: `token` is a valid handle opened above.
    unsafe { CloseHandle(token) };
    read && elevation.TokenIsElevated != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_label_and_the_limits_follow_the_rights() {
        let account = |os, elevated| Account {
            name: "admin".into(),
            elevated,
            os,
        };
        assert_eq!(account(Os::Windows, false).label(), "admin (not elevated)");
        assert_eq!(account(Os::Windows, true).label(), "admin (elevated)");
        assert_eq!(account(Os::Linux, false).label(), "admin (not root)");
        assert!(account(Os::Windows, true).limits().is_none());
        assert!(account(Os::Linux, false).limits().unwrap().contains("sudo"));
        assert!(!Account::current().name.is_empty());
    }
}
