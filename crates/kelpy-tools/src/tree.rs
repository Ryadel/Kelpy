//! The processes a command starts end with it: when it ends, is stopped or runs out of time. A program meant to keep
//! running is started as a service, by the system's service manager, which does not belong to the command.
//!
//! Without this, a program left running in the background would also keep the command's output open, and Kelpy would
//! wait for it until the time limit.

/// The processes of one command: a process group on Unix, a job object on Windows.
pub(crate) struct Tree {
    #[cfg(unix)]
    group: Option<libc::pid_t>,
    /// Null when the job could not be made: the command then runs without it.
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(unix)]
impl Tree {
    /// Makes the command lead a process group of its own, which what it starts joins.
    pub(crate) fn prepare(command: &mut tokio::process::Command) {
        command.process_group(0);
    }

    pub(crate) fn of(child: &tokio::process::Child) -> Self {
        Self {
            group: child.id().and_then(|id| libc::pid_t::try_from(id).ok()),
        }
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
    pub(crate) fn prepare(_command: &mut tokio::process::Command) {}

    /// Puts the command in a job that ends its processes when closed. The processes it starts join the job; the
    /// command is added a moment after it starts, before a shell gets to run anything.
    pub(crate) fn of(child: &tokio::process::Child) -> Self {
        use std::{mem, ptr};
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };

        // SAFETY: an unnamed job with default security; the handle is closed by `Drop`, or below.
        let job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if job.is_null() {
            return Self { job };
        }
        // SAFETY: a plain C structure, for which all zeros means no limit.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `limits` is the structure the information class names, with its size.
        let limited = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                mem::size_of_val(&limits) as u32,
            )
        } != 0;
        // SAFETY: `process` is the child's handle, open while `child` lives.
        let assigned = child
            .raw_handle()
            .is_some_and(|process| unsafe { AssignProcessToJobObject(job, process) } != 0);
        if limited && assigned {
            Self { job }
        } else {
            // SAFETY: made above and used nowhere else.
            unsafe { CloseHandle(job) };
            Self {
                job: ptr::null_mut(),
            }
        }
    }

    /// Ends every process of the tree still running.
    pub(crate) fn end(&self) {
        if !self.job.is_null() {
            // SAFETY: the job's handle is open until `Drop`.
            unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job, 1) };
        }
    }
}

// SAFETY: a job's handle may be used and closed from any thread.
#[cfg(windows)]
unsafe impl Send for Tree {}
// SAFETY: `end`, the only use through a shared reference, is safe from several threads at once.
#[cfg(windows)]
unsafe impl Sync for Tree {}

impl Drop for Tree {
    fn drop(&mut self) {
        self.end();
        #[cfg(windows)]
        if !self.job.is_null() {
            // SAFETY: the handle is not used after this.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.job) };
        }
    }
}
