//! Files and folders only the current user can open: the runtime's credentials and the `data/` folder now, the
//! secrets vault later.
//!
//! The file is created with its final permissions, so there is no moment when another user could open it: mode
//! 0600 on Unix, a protected access list granting the current user alone on Windows.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

/// Writes `contents` to a new file at `path` that only the current user can open, replacing any existing file.
///
/// The new file is written in full beside the old one and then renamed over it, so a failure at any point leaves
/// the old file as it was.
pub fn write(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut staged_name = path.file_name().unwrap_or_default().to_owned();
    staged_name.push(".new");
    let staged = path.with_file_name(staged_name);
    // A leftover of an interrupted write.
    match fs::remove_file(&staged) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    // If anything appears there in the meantime, creation fails rather than reusing it.
    let mut file = create_new(&staged)?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&staged, path)
}

#[cfg(unix)]
fn create_new(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(windows)]
fn create_new(path: &Path) -> io::Result<File> {
    use std::os::windows::io::FromRawHandle;
    use std::ptr;

    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL};

    let descriptor = windows::OwnerOnly::new(false)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.as_ptr(),
        bInheritHandle: 0,
    };
    let path = wide(path.as_os_str());
    // SAFETY: every pointer is valid for the duration of the call; the handle is owned by the returned `File`.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `handle` is a valid file handle that nothing else owns.
    Ok(unsafe { File::from_raw_handle(handle) })
}

/// Makes the existing directory `path` reachable by the current user only, including everything inside it.
///
/// Fails on file systems without permissions (FAT on a USB stick, some network shares): callers should warn and
/// carry on.
pub fn restrict_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
    }
    #[cfg(windows)]
    {
        use std::ptr;

        use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetNamedSecurityInfoW};
        use windows_sys::Win32::Security::{
            ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl,
            PROTECTED_DACL_SECURITY_INFORMATION,
        };

        // Inheritable for files and subfolders; applying it also replaces what is already inside.
        let descriptor = windows::OwnerOnly::new(true)?;
        let mut present = 0;
        let mut defaulted = 0;
        let mut dacl: *mut ACL = ptr::null_mut();
        // SAFETY: `descriptor` is a valid security descriptor; `dacl` points into it and is used while it lives.
        if unsafe {
            GetSecurityDescriptorDacl(descriptor.as_ptr(), &mut present, &mut dacl, &mut defaulted)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let path = wide(path.as_os_str());
        // SAFETY: `path` is NUL-terminated and `dacl` is valid; owner and group are left unchanged.
        let status = unsafe {
            SetNamedSecurityInfoW(
                path.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                dacl,
                ptr::null(),
            )
        };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(())
    }
}

/// Creates the directory `path` reachable by administrators only, and theirs, from the start, so that there is no
/// moment when another account could put something inside: on Windows owned by Administrators, with access for
/// SYSTEM and Administrators alone (inherited); elsewhere 0700, owned by root, whom Kelpy runs as to make it. Its
/// parent must exist. Needs administrator rights.
pub fn create_dir_for_administrators(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
        use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;

        let descriptor = windows::Descriptor::parse("O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)")?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.as_ptr(),
            bInheritHandle: 0,
        };
        let path = wide(path.as_os_str());
        // SAFETY: `path` is NUL-terminated and `attributes` valid for the call.
        if unsafe { CreateDirectoryW(path.as_ptr(), &attributes) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Whether only administrators could have made and changed the directory `path`: on Windows owned by Administrators
/// or SYSTEM and open to them alone; elsewhere owned by root and open to it alone. A folder another account made in
/// its place fails this, whatever it holds.
pub fn administrators_only(path: &Path) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let metadata = fs::symlink_metadata(path)?;
        Ok(metadata.is_dir() && metadata.uid() == 0 && metadata.permissions().mode() & 0o077 == 0)
    }
    #[cfg(windows)]
    {
        if !fs::symlink_metadata(path)?.is_dir() {
            return Ok(false);
        }
        windows::administrators_only(path)
    }
}

#[cfg(windows)]
fn wide(text: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    text.encode_wide().chain(Some(0)).collect()
}

#[cfg(windows)]
mod windows {
    use std::io;
    use std::ptr;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::PSECURITY_DESCRIPTOR;

    /// A security descriptor whose protected DACL (no inherited entries) has one entry: full access for the
    /// current user, inherited by files and subfolders if `inheritable`.
    pub struct OwnerOnly(PSECURITY_DESCRIPTOR);

    impl OwnerOnly {
        pub fn new(inheritable: bool) -> io::Result<Self> {
            let flags = if inheritable { "OICI" } else { "" };
            let sddl = format!("D:P(A;{flags};FA;;;{})", current_user_sid()?);
            let sddl = super::wide(sddl.as_ref());
            let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
            // SAFETY: `sddl` is NUL-terminated; on success `descriptor` is allocated by the system, freed in `drop`.
            let converted = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    ptr::null_mut(),
                )
            };
            if converted == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(descriptor))
        }

        pub fn as_ptr(&self) -> PSECURITY_DESCRIPTOR {
            self.0
        }
    }

    impl Drop for OwnerOnly {
        fn drop(&mut self) {
            // SAFETY: allocated by `ConvertStringSecurityDescriptorToSecurityDescriptorW`.
            unsafe { LocalFree(self.0) };
        }
    }

    /// A security descriptor from its SDDL text.
    pub struct Descriptor(PSECURITY_DESCRIPTOR);

    impl Descriptor {
        pub fn parse(sddl: &str) -> io::Result<Self> {
            let sddl = super::wide(sddl.as_ref());
            let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
            // SAFETY: `sddl` is NUL-terminated; on success `descriptor` is allocated by the system, freed in `drop`.
            let converted = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    ptr::null_mut(),
                )
            };
            if converted == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(descriptor))
        }

        pub fn as_ptr(&self) -> PSECURITY_DESCRIPTOR {
            self.0
        }
    }

    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: allocated by the system, to be freed with `LocalFree`.
            unsafe { LocalFree(self.0) };
        }
    }

    /// Whether the folder at `path` is owned by Administrators or SYSTEM, and its access list allows no one else.
    pub fn administrators_only(path: &std::path::Path) -> io::Result<bool> {
        use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
        use windows_sys::Win32::Security::{
            ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
            DACL_SECURITY_INFORMATION, GetAce, GetAclInformation, IsWellKnownSid,
            OWNER_SECURITY_INFORMATION, PSID, WinBuiltinAdministratorsSid, WinLocalSystemSid,
        };

        let administrators = |sid: PSID| {
            // SAFETY: `sid` points to a valid SID inside the descriptor read below.
            unsafe {
                IsWellKnownSid(sid, WinBuiltinAdministratorsSid) != 0
                    || IsWellKnownSid(sid, WinLocalSystemSid) != 0
            }
        };
        let path = super::wide(path.as_os_str());
        let mut owner: PSID = ptr::null_mut();
        let mut dacl: *mut ACL = ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: `path` is NUL-terminated; `owner` and `dacl` point into `descriptor`, freed below.
        let status = unsafe {
            GetNamedSecurityInfoW(
                path.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                ptr::null_mut(),
                &mut dacl,
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let descriptor = Descriptor(descriptor);
        // No access list at all means access for everyone.
        let mut only = !owner.is_null() && administrators(owner) && !dacl.is_null();
        if only {
            let mut size = ACL_SIZE_INFORMATION {
                AceCount: 0,
                AclBytesInUse: 0,
                AclBytesFree: 0,
            };
            // SAFETY: `dacl` is a valid ACL; `size` is the structure the class names, with its size.
            let read = unsafe {
                GetAclInformation(
                    dacl,
                    (&raw mut size).cast(),
                    size_of::<ACL_SIZE_INFORMATION>() as u32,
                    AclSizeInformation,
                )
            };
            if read == 0 {
                return Err(io::Error::last_os_error());
            }
            for index in 0..size.AceCount {
                let mut ace: *mut core::ffi::c_void = ptr::null_mut();
                // SAFETY: `index` is within the ACL's entries; `ace` points into it.
                if unsafe { GetAce(dacl, index, &mut ace) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                // SAFETY: every entry starts with its header.
                let kind = unsafe { (*ace.cast::<ACE_HEADER>()).AceType };
                // Denying (type 1) takes nothing away from being administrators-only; any kind of entry other
                // than allowing (type 0) might grant.
                if kind == 1 {
                    continue;
                }
                // SAFETY: an allowing entry holds its SID from `SidStart`.
                let allowed = kind == 0
                    && administrators(unsafe {
                        (&raw mut (*ace.cast::<ACCESS_ALLOWED_ACE>()).SidStart).cast()
                    });
                if !allowed {
                    only = false;
                    break;
                }
            }
        }
        drop(descriptor);
        Ok(only)
    }

    /// The security identifier of the user running this process, as a string such as `S-1-5-21-…`.
    pub fn current_user_sid() -> io::Result<String> {
        let mut token: HANDLE = ptr::null_mut();
        // SAFETY: `GetCurrentProcess` returns a pseudo-handle; `token` receives a handle closed below.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let sid = token_user_sid(token);
        // SAFETY: `token` is a valid handle opened above.
        unsafe { CloseHandle(token) };
        sid
    }

    fn token_user_sid(token: HANDLE) -> io::Result<String> {
        // TOKEN_USER is followed in the same buffer by the SID it points to; u64 keeps the buffer aligned.
        let mut buffer = vec![0u64; 16];
        let mut needed = 0u32;
        loop {
            let size = u32::try_from(buffer.len() * 8).unwrap_or(u32::MAX);
            // SAFETY: `buffer` is writable for `size` bytes.
            let ok = unsafe {
                GetTokenInformation(
                    token,
                    TokenUser,
                    buffer.as_mut_ptr().cast(),
                    size,
                    &mut needed,
                )
            };
            if ok != 0 {
                break;
            }
            let error = io::Error::last_os_error();
            if needed as usize <= buffer.len() * 8 {
                return Err(error);
            }
            buffer.resize((needed as usize).div_ceil(8), 0);
        }
        // SAFETY: on success the buffer starts with a TOKEN_USER whose SID points into the buffer.
        let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };

        let mut text: *mut u16 = ptr::null_mut();
        // SAFETY: `sid` is valid while `buffer` lives; `text` is allocated by the system, released below.
        if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `text` is a NUL-terminated UTF-16 string.
        let sid = unsafe {
            let length = (0..).take_while(|&index| *text.add(index) != 0).count();
            String::from_utf16_lossy(std::slice::from_raw_parts(text, length))
        };
        // SAFETY: `text` was allocated by `ConvertSidToStringSidW`.
        unsafe { LocalFree(text.cast()) };
        Ok(sid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_replaces_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.json");
        write(&path, b"first").unwrap();
        write(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");
        assert!(!dir.path().join("secret.json.new").exists());
    }

    #[test]
    fn a_failed_write_keeps_the_previous_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.json");
        write(&path, b"first").unwrap();
        // A folder where the new file is staged: writing it fails.
        fs::create_dir(dir.path().join("secret.json.new")).unwrap();
        assert!(write(&path, b"second").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"first");
    }

    #[test]
    fn restricted_folders_stay_usable_by_their_owner() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        fs::create_dir(&data).unwrap();
        fs::write(data.join("existing.txt"), b"x").unwrap();
        restrict_dir(&data).unwrap();
        fs::write(data.join("new.txt"), b"y").unwrap();
        assert_eq!(fs::read(data.join("existing.txt")).unwrap(), b"x");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&data).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
    }

    #[cfg(unix)]
    #[test]
    fn only_the_owner_can_read_it() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.json");
        write(&path, b"x").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
