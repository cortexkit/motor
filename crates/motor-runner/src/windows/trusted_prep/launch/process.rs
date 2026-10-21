use super::session::Session;
use super::{check, step, wide, Handle, Plan};
use std::{
    io,
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{HANDLE, INVALID_HANDLE_VALUE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
    Security::SECURITY_ATTRIBUTES,
    Storage::FileSystem::*,
    System::{JobObjects::*, Threading::*},
};
mod trace;
#[link(name = "ntdll")]
unsafe extern "system" {
    fn RtlGetLastNtStatus() -> i32;
}
pub(super) const CREATION_FLAGS: u32 =
    CREATE_SUSPENDED | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT;

struct Attributes {
    _storage: Vec<u128>,
    pointer: LPPROC_THREAD_ATTRIBUTE_LIST,
}
impl Attributes {
    /// `extra` holds further (attribute, value, size) entries, such as the
    /// AppContainer security capabilities; their values must outlive the list.
    fn new(
        handles: &mut [HANDLE],
        extra: &[(usize, *const std::ffi::c_void, usize)],
    ) -> io::Result<Self> {
        let count = 1 + extra.len() as u32;
        let mut size = 0;
        unsafe {
            InitializeProcThreadAttributeList(null_mut(), count, 0, &mut size);
        }
        if size == 0 {
            return Err(step("handle-allow-list", io::Error::last_os_error()));
        }
        let mut storage = vec![0u128; size.div_ceil(16)];
        let pointer = storage.as_mut_ptr().cast();
        check("handle-allow-list", unsafe {
            InitializeProcThreadAttributeList(pointer, count, 0, &mut size)
        })?;
        let result = Self {
            _storage: storage,
            pointer,
        };
        check("handle-allow-list", unsafe {
            UpdateProcThreadAttribute(
                pointer,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_mut_ptr().cast(),
                std::mem::size_of_val(handles),
                null_mut(),
                null_mut(),
            )
        })?;
        for (attribute, value, size) in extra {
            check("process-attribute", unsafe {
                UpdateProcThreadAttribute(
                    pointer,
                    0,
                    *attribute,
                    *value,
                    *size,
                    null_mut(),
                    null_mut(),
                )
            })?;
        }
        Ok(result)
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.pointer);
        }
    }
}

fn stdio(path: &str, input: bool) -> io::Result<Handle> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 1,
    };
    let handle = unsafe {
        CreateFileW(
            wide(path).as_ptr(),
            if input { 0x80000000 } else { 0x40000000 },
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &attributes,
            if input { OPEN_EXISTING } else { CREATE_NEW },
            FILE_ATTRIBUTE_NORMAL
                | if input {
                    0
                } else {
                    FILE_FLAG_OPEN_REPARSE_POINT
                },
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        Err(step("child-stdio", io::Error::last_os_error()))
    } else {
        Ok(Handle(handle))
    }
}
struct Tree<'a> {
    job: Handle,
    process: Handle,
    reaped: bool,
    released: &'a mut bool,
}
impl Tree<'_> {
    fn reap(&mut self) -> io::Result<()> {
        if self.reaped {
            return Ok(());
        }
        check("terminate-prep-job", unsafe {
            TerminateJobObject(self.job.0, 1)
        })?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
            check("reap-prep-job", unsafe {
                QueryInformationJobObject(
                    self.job.0,
                    JobObjectBasicAccountingInformation,
                    (&raw mut accounting).cast(),
                    size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    null_mut(),
                )
            })?;
            if accounting.ActiveProcesses == 0 {
                self.reaped = true;
                *self.released = true;
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(step(
                    "reap-prep-job",
                    "child tree did not terminate within 10 seconds",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Tree<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.reap() {
            eprintln!("{error}");
        }
    }
}

pub(super) fn run(plan: &Plan, session: &Session, released: &mut bool) -> io::Result<u32> {
    let input = stdio("NUL", true)?;
    let output = stdio(&plan.stdout.to_string_lossy(), false)?;
    let error = stdio(&plan.stderr.to_string_lossy(), false)?;
    let mut handles = [input.0, output.0, error.0];
    let extra = session
        .lowbox
        .as_ref()
        .map_or_else(Vec::new, |launch| launch.attributes());
    let attributes = Attributes::new(&mut handles, &extra)?;
    let job = Handle(unsafe { CreateJobObjectW(null(), null()) });
    if job.0.is_null() {
        return Err(step("create-prep-job", io::Error::last_os_error()));
    }
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    limits.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
    limits.BasicLimitInformation.ActiveProcessLimit = 32;
    check("prep-job-limits", unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    })?;
    let mut desktop = session.desktop();
    let mut startup: STARTUPINFOEXW = unsafe { zeroed() };
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.lpDesktop = desktop.as_mut_ptr();
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = input.0;
    startup.StartupInfo.hStdOutput = output.0;
    startup.StartupInfo.hStdError = error.0;
    startup.lpAttributeList = attributes.pointer;
    let mut arguments = vec![motor_winagent::quote_windows_arg(&plan.program)];
    arguments.extend(
        plan.args
            .iter()
            .map(|arg| motor_winagent::quote_windows_arg(arg)),
    );
    let mut command = wide(&arguments.join(" "));
    let mut info: PROCESS_INFORMATION = unsafe { zeroed() };
    let working_directory = if plan.diagnostic_safe_cwd {
        plan.cwd.join("fetch-temp")
    } else {
        plan.cwd.clone()
    };
    let flags = CREATION_FLAGS
        | if plan.diagnostic_loader_trace {
            DEBUG_ONLY_THIS_PROCESS
        } else {
            0
        };
    let created = unsafe {
        CreateProcessAsUserW(
            session.token(),
            wide(&plan.program).as_ptr(),
            command.as_mut_ptr(),
            null(),
            null(),
            1,
            flags,
            session.environment.as_ptr().cast(),
            wide(&working_directory.to_string_lossy()).as_ptr(),
            &startup.StartupInfo,
            &mut info,
        )
    };
    if created == 0 {
        // The Win32 code alone can hide which object was missing; the NTSTATUS
        // ntdll recorded for this thread narrows it (for example an object
        // directory path versus a file name).
        let error = io::Error::last_os_error();
        let status = unsafe { RtlGetLastNtStatus() };
        return Err(step(
            "CreateProcessAsUser",
            format!(
                "{error} (Win32 0x{:08X}; last NTSTATUS 0x{:08X})",
                error.raw_os_error().unwrap_or(0) as u32,
                status as u32
            ),
        ));
    }
    let process = Handle(info.hProcess);
    let thread = Handle(info.hThread);
    *released = false;
    if unsafe { AssignProcessToJobObject(job.0, process.0) } == 0 {
        let failure = io::Error::last_os_error();
        let terminated = unsafe { TerminateProcess(process.0, 1) };
        let waited = unsafe { WaitForSingleObject(process.0, 5000) };
        *released = waited == WAIT_OBJECT_0;
        if !*released {
            return Err(step(
                "assign-prep-job",
                format!(
                    "{failure}; termination={terminated} wait=0x{waited:08X}; child not reaped"
                ),
            ));
        }
        return Err(step("assign-prep-job", failure));
    }
    let mut tree = Tree {
        job,
        process,
        reaped: false,
        released,
    };
    if plan.diagnostic_loader_trace {
        check("DebugSetProcessKillOnExit", unsafe {
            windows_sys::Win32::System::Diagnostics::Debug::DebugSetProcessKillOnExit(1)
        })?;
        let inspect = |handle| {
            super::session::kernel_object_details(handle)
                .unwrap_or_else(|error| serde_json::json!({"error":error.to_string()}))
        };
        super::save_process_report(
            plan,
            &serde_json::json!({"pid":info.dwProcessId,"tid":info.dwThreadId,
            "creation_flags":format!("0x{flags:08X}"),"working_directory":working_directory,"process_security":inspect(tree.process.0),"thread_security":inspect(thread.0),
            "loader_trace":trace::path(plan),"native_debugger":"DEBUG_ONLY_THIS_PROCESS; Windows loader snaps from temporary IFEO flags"}),
        )?;
    }
    if unsafe { ResumeThread(thread.0) } == u32::MAX {
        return Err(step("resume-prep-child", io::Error::last_os_error()));
    }
    let deadline = Instant::now() + Duration::from_secs(600);
    if plan.diagnostic_loader_trace {
        let code = trace::wait(plan, tree.process.0, deadline)?;
        tree.reap()?;
        return Ok(code);
    }
    loop {
        match unsafe { WaitForSingleObject(tree.process.0, 50) } {
            WAIT_OBJECT_0 => break,
            WAIT_FAILED => return Err(step("wait-prep-child", io::Error::last_os_error())),
            WAIT_TIMEOUT if Instant::now() < deadline => {}
            WAIT_TIMEOUT => {
                return Err(step(
                    "wait-prep-child",
                    "Windows trusted preparation exceeded 600 seconds",
                ))
            }
            other => {
                return Err(step(
                    "wait-prep-child",
                    format!("unexpected wait status 0x{other:08X}"),
                ))
            }
        }
    }
    let mut code = 0;
    check("child-exit-status", unsafe {
        GetExitCodeProcess(tree.process.0, &mut code)
    })?;
    tree.reap()?;
    Ok(code)
}
