//! Owned native subprocess trees. Closing the Job Object terminates its managed
//! PowerShell/Git processes, including when the runner crashes.
use std::{
    io::{self, Read},
    mem::{size_of, zeroed},
    ops::{Deref, DerefMut},
    os::windows::{io::AsRawHandle, process::CommandExt},
    process::{Command, Output, Stdio},
    ptr::null,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE},
    System::{Diagnostics::ToolHelp::*, JobObjects::*, Threading::*},
};
struct Handle(HANDLE);
// SAFETY: Win32 process, thread, snapshot and Job Object handles are not
// thread-affine. Moving this sole owner transfers its one CloseHandle duty.
unsafe impl Send for Handle {}
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}
pub(crate) struct Child {
    child: std::process::Child,
    job: Handle,
}
impl Deref for Child {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.child
    }
}
impl DerefMut for Child {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.child
    }
}
impl Child {
    pub(crate) fn spawn(command: &mut Command) -> io::Result<Self> {
        unsafe {
            let job = Handle(CreateJobObjectW(null(), null()));
            if job.0.is_null() {
                return Err(io::Error::last_os_error());
            }
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
            limits.BasicLimitInformation.LimitFlags =
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
            limits.BasicLimitInformation.ActiveProcessLimit = 64;
            if SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let mut child = command
                .creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW)
                .spawn()?;
            if AssignProcessToJobObject(job.0, child.as_raw_handle()) == 0 {
                let error = io::Error::last_os_error();
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
            let owned = Self { child, job };
            let snapshot = Handle(CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0));
            if snapshot.0 == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            let mut entry: THREADENTRY32 = zeroed();
            entry.dwSize = size_of::<THREADENTRY32>() as u32;
            let mut more = Thread32First(snapshot.0, &mut entry);
            let mut threads = Vec::new();
            while more != 0 {
                if entry.th32OwnerProcessID == owned.id() {
                    threads.push(entry.th32ThreadID);
                }
                more = Thread32Next(snapshot.0, &mut entry);
            }
            if threads.len() != 1 {
                return Err(io::Error::other(
                    "suspended native child has an unexpected thread topology",
                ));
            }
            let thread = Handle(OpenThread(THREAD_SUSPEND_RESUME, 0, threads[0]));
            if thread.0.is_null() || ResumeThread(thread.0) == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            Ok(owned)
        }
    }
    pub(crate) fn kill(&mut self) -> io::Result<()> {
        if unsafe { TerminateJobObject(self.job.0, 1) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.kill();
        let _ = self.child.wait();
    }
}
fn capture(mut reader: impl Read) -> Vec<u8> {
    let mut result = Vec::new();
    let mut buffer = [0; 8192];
    while let Ok(count) = reader.read(&mut buffer) {
        if count == 0 {
            break;
        }
        result.extend_from_slice(&buffer[..count.min(65536 - result.len())]);
    }
    result
}
pub(crate) fn output(command: &mut Command, cancelled: impl Fn() -> bool) -> io::Result<Output> {
    output_with_timeout(command, Duration::from_secs(120), cancelled)
}
pub(crate) fn output_with_timeout(
    command: &mut Command,
    timeout: Duration,
    cancelled: impl Fn() -> bool,
) -> io::Result<Output> {
    let mut child = Child::spawn(command.stdout(Stdio::piped()).stderr(Stdio::piped()))?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || capture(stdout));
    let err = std::thread::spawn(move || capture(stderr));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if cancelled() || Instant::now() >= deadline {
            child.kill()?;
            let _ = child.wait();
            let _ = out.join();
            let _ = err.join();
            return Err(io::Error::other(format!(
                "native control process cancelled or exceeded {} seconds",
                timeout.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // Kill a leftover descendant before joining readers, even if its parent
    // exited successfully while that descendant still held a stdout handle.
    child.kill()?;
    Ok(Output {
        status,
        stdout: out
            .join()
            .map_err(|_| io::Error::other("native stdout reader stopped"))?,
        stderr: err
            .join()
            .map_err(|_| io::Error::other("native stderr reader stopped"))?,
    })
}
