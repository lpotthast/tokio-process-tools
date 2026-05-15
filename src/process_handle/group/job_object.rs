//! Windows Job Object RAII wrapper used by [`super::ProcessGroup`] for tree-wide forceful kill.
//!
//! `TerminateJobObject` is the Windows analogue of `killpg(SIGKILL)`: it forcefully terminates
//! the leader and every descendant the OS has automatically associated with the job. Holding
//! this alongside the [`tokio::process::Child`] is what lets
//! [`super::ProcessGroup::send_kill`] reach the whole tree on Windows, instead of orphaning
//! grandchildren the way `Child::start_kill` would.
//!
//! This deliberately does **not** set `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. Closing the handle
//! must not kill the assigned processes; otherwise dropping a `ProcessHandle` after
//! [`crate::ProcessHandle::into_inner`] or
//! [`crate::ProcessHandle::must_not_be_terminated`] would kill a child the caller intentionally
//! chose to detach.

use std::io;

#[derive(Debug)]
pub(super) struct JobObject {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

// SAFETY: a Windows Job Object HANDLE is a kernel object handle. The Win32 APIs we call on it
// (`TerminateJobObject`, `CloseHandle`) are documented to be thread-safe and the kernel
// synchronizes access internally. The handle is owned exclusively by this struct.
unsafe impl Send for JobObject {}
unsafe impl Sync for JobObject {}

impl JobObject {
    /// Creates an anonymous `JobObject` and assigns the spawned child to it.
    ///
    /// On Windows 8 and later, processes the child subsequently spawns are automatically
    /// associated with the same job (unless a descendant explicitly opts out via
    /// `CREATE_BREAKAWAY_FROM_JOB`), so a single assignment captures the entire tree.
    ///
    /// There is a small race window between `Command::spawn` and this assignment in which the
    /// child could already have spawned a grandchild. Tokio's `Command` does not expose
    /// `CREATE_SUSPENDED`, so we cannot eliminate that window here; the assignment is performed
    /// as soon as the child handle is available, which keeps the window microscopic.
    pub(super) fn new_with_child(child: &tokio::process::Child) -> io::Result<Self> {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
        };

        let pid = child.id().ok_or_else(|| {
            io::Error::other("child has no PID; it was already polled to completion")
        })?;

        // SAFETY: Both pointers are null. `CreateJobObjectW` accepts a null `lpJobAttributes`
        // (default security descriptor, non-inheritable handle) and a null `lpName` (anonymous
        // job).
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: `PROCESS_SET_QUOTA | PROCESS_TERMINATE` are the access rights required by
        // `AssignProcessToJobObject`. `bInheritHandle = 0` keeps the handle non-inheritable.
        let proc_handle = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
        if proc_handle.is_null() {
            let err = io::Error::last_os_error();
            // SAFETY: `job` is a valid handle returned by `CreateJobObjectW` above.
            unsafe {
                CloseHandle(job);
            }
            return Err(err);
        }

        // SAFETY: Both handles are valid and `proc_handle` was opened with the rights
        // `AssignProcessToJobObject` documents as required.
        let assigned = unsafe { AssignProcessToJobObject(job, proc_handle) } != 0;
        // SAFETY: `proc_handle` is a valid handle returned by `OpenProcess` above. We no longer
        // need it; the assignment outlives the handle.
        unsafe {
            CloseHandle(proc_handle);
        }

        if !assigned {
            let err = io::Error::last_os_error();
            // SAFETY: `job` is a valid handle returned by `CreateJobObjectW` above.
            unsafe {
                CloseHandle(job);
            }
            return Err(err);
        }

        Ok(Self { handle: job })
    }

    /// Terminates every process associated with the job, including descendants the OS has
    /// implicitly added since assignment.
    ///
    /// `exit_code` is reported as the exit code for each process the call terminates. We pass
    /// `1` to align with the conventional "killed" status.
    pub(super) fn terminate(&self, exit_code: u32) -> io::Result<()> {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        // SAFETY: `self.handle` is a valid Job Object handle owned by this struct.
        let ok = unsafe { TerminateJobObject(self.handle, exit_code) } != 0;
        if !ok {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for JobObject {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        // SAFETY: `self.handle` is a valid Job Object handle owned by this struct and is not used
        // after this call.
        unsafe {
            CloseHandle(self.handle);
        }
    }
}
