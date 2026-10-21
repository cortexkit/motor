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
pub(super) const CREATION_FLAGS: u32 =
    CREATE_SUSPENDED | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT;

struct Attributes {
    _storage: Vec<u128>,
    pointer: LPPROC_THREAD_ATTRIBUTE_LIST,
}
impl Attributes {
    fn new(handles: &mut [HANDLE]) -> io::Result<Self> {
        let mut size = 0;
        unsafe {
            InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut size);
        }
        if size == 0 {
            return Err(step("handle-allow-list", io::Error::last_os_error()));
        }
        let mut storage = vec![0u128; size.div_ceil(16)];
        let pointer = storage.as_mut_ptr().cast();
        check("handle-allow-list", unsafe {
            InitializeProcThreadAttributeList(pointer, 1, 0, &mut size)
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
    let attributes = Attributes::new(&mut handles)?;
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
    check("CreateProcessAsUser", unsafe {
        CreateProcessAsUserW(
            session.token(),
            wide(&plan.program).as_ptr(),
            command.as_mut_ptr(),
            null(),
            null(),
            1,
            CREATION_FLAGS,
            session.environment.as_ptr().cast(),
            wide(&plan.cwd.to_string_lossy()).as_ptr(),
            &startup.StartupInfo,
            &mut info,
        )
    })?;
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
    if unsafe { ResumeThread(thread.0) } == u32::MAX {
        return Err(step("resume-prep-child", io::Error::last_os_error()));
    }
    let deadline = Instant::now() + Duration::from_secs(600);
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
