//! The processes a command starts end with it: when it ends, is stopped or runs out of time. A program meant to keep
//! running is started as a service, by the system's service manager, which does not belong to the command.
//!
//! Without this, a program left running in the background would also keep the command's output open, and Kelpy would
//! wait for it until the time limit.

use std::io;

/// The processes of one command: a process group on Unix, a job object on Windows.
pub(crate) struct Tree {
    #[cfg(unix)]
    group: Option<libc::pid_t>,
    #[cfg(windows)]
    job: std::os::windows::io::OwnedHandle,
}

#[cfg(unix)]
impl Tree {
    /// Makes the command lead a process group of its own, which what it starts joins.
    pub(crate) fn prepare(command: &mut tokio::process::Command) {
        command.process_group(0);
    }

    pub(crate) fn of(child: &tokio::process::Child) -> io::Result<Self> {
        Ok(Self {
            group: child.id().and_then(|id| libc::pid_t::try_from(id).ok()),
        })
    }

    /// Ends every process of the tree still running.
    pub(crate) fn end(&self) {
        if let Some(group) = self.group {
            // SAFETY: a plain system call; a group with no process left only makes it fail.
            unsafe { libc::kill(-group, libc::SIGKILL) };
        }
    }
}

#[cfg(windows)]
impl Tree {
    /// Starts the command suspended: it runs nothing until [`Tree::of`] has put it in its job.
    pub(crate) fn prepare(command: &mut tokio::process::Command) {
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_SUSPENDED);
    }

    /// Puts the command, still suspended, in a job that ends its processes when closed, then lets it run: whatever
    /// it starts joins the job from its first instruction. An error leaves the command suspended, for the caller to
    /// end.
    pub(crate) fn of(child: &tokio::process::Child) -> io::Result<Self> {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use std::{mem, ptr};
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };

        let (Some(process), Some(id)) = (child.raw_handle(), child.id()) else {
            return Err(io::Error::other("the command ended before it started"));
        };
        // SAFETY: an unnamed job with default security.
        let job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a handle just made, owned by nothing else: it is closed once, when dropped.
        let job = unsafe { OwnedHandle::from_raw_handle(job) };
        // SAFETY: a plain C structure, for which all zeros means no limit.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `limits` is the structure the information class names, with its size.
        let limited = unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                mem::size_of_val(&limits) as u32,
            )
        };
        if limited == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `process` is the child's handle, open while `child` lives.
        if unsafe { AssignProcessToJobObject(job.as_raw_handle(), process) } == 0 {
            return Err(io::Error::last_os_error());
        }
        resume(id)?;
        Ok(Self { job })
    }

    /// Ends every process of the tree still running.
    pub(crate) fn end(&self) {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: the job's handle is open while `self` lives.
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job.as_raw_handle(), 1)
        };
    }
}

/// Lets the process `id`, made suspended, run: its only thread, the first, is resumed.
#[cfg(windows)]
fn resume(id: u32) -> io::Result<()> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    // SAFETY: a snapshot of the system's threads; the handle is owned below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a handle just made, owned by nothing else.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    // SAFETY: a plain C structure; its size field is set before it is read.
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
    let mut resumed = false;
    // SAFETY: `entry` is a THREADENTRY32 with its size, and the snapshot is open.
    let mut more = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == id {
            // SAFETY: opens the thread named by the snapshot, owned below.
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if thread.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: a handle just made, owned by nothing else.
            let thread = unsafe { OwnedHandle::from_raw_handle(thread) };
            // SAFETY: the thread's handle, open, with the right to resume it.
            if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            resumed = true;
        }
        // SAFETY: as for `Thread32First`.
        more = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) } != 0;
    }
    if resumed {
        Ok(())
    } else {
        Err(io::Error::other("its first thread was not found"))
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        self.end();
    }
}
