#[cfg(not(windows))]
fn main() {
    eprintln!("motor-winagent is a Windows service; the shared protocol library is portable");
    std::process::exit(1);
}

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(feature = "hyperv-spike")]
    if std::env::args().skip(1).eq(["--build-info"]) {
        if cfg!(feature = "hyperv-runner") {
            println!(
                "motor-winagent hyperv-runner agent_diag=3 transport=hvsock peer=parent nonce=dvd protocol=4"
            );
        } else if cfg!(feature = "hyperv-hvsock") {
            println!(
                "motor-winagent hyperv-spike agent_diag=3 transport=hvsock peer=parent protocol=4"
            );
        } else {
            println!(
                "motor-winagent hyperv-spike agent_diag=3 transport=com partial_read=4096,250ms queue_config=0,2097152 protocol=4"
            );
        }
        return Ok(());
    }
    guest::run()
}

#[cfg(all(windows, feature = "hyperv-hvsock"))]
mod hvsock;
#[cfg(all(windows, feature = "hyperv-spike"))]
mod hyperv_diag;
#[cfg(windows)]
mod storage;

#[cfg(windows)]
mod guest {
    macro_rules! diagnostic {
        ($($arg:tt)*) => {{ #[cfg(feature = "hyperv-spike")] crate::hyperv_diag::event(&format!($($arg)*)); }};
    }
    #[cfg(not(feature = "hyperv-runner"))]
    use motor_winagent::SMBIOS_NONCE_PREFIX;
    #[cfg(not(feature = "hyperv-runner"))]
    use motor_winagent::cache::{prepare_cache, select_cache_disk, storage_serial};
    use motor_winagent::{
        CACHE_INHERIT_FLAGS, CACHE_MODIFY_MASK, CACHE_MOUNT, CHUNK_BYTES, CacheAce, CacheTimings,
        Command, Frame, Inventory, JOB_USER, PROTOCOL_VERSION, Peer, Protocol, SERVICE_NAME,
        Stream, cache_has_modify_grant,
        cache_steps::{CacheLimits, CacheReporter, CacheStep, watch_cache},
        collect_changes,
        environment::{encode_block, git_native_bin, read_user_environment},
        inventory, mapped_cwd, read_frame, unpack_tree, write_frame,
    };
    use std::{
        ffi::OsString,
        fs::{File, OpenOptions},
        io::{self, Read, Write},
        mem::{size_of, size_of_val, zeroed},
        os::windows::io::{AsRawHandle, FromRawHandle},
        path::{Path, PathBuf},
        ptr::{null, null_mut},
        sync::{Arc, mpsc},
        thread,
        time::{Duration, Instant},
    };
    use windows_service::{
        define_windows_service,
        service::{
            ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
            ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
        service_dispatcher,
    };
    use windows_sys::Win32::{
        Foundation::*,
        Security::Authorization::*,
        Security::*,
        Storage::FileSystem::*,
        System::{Environment::*, IO::*, Ioctl::*, JobObjects::*, Pipes::*, Threading::*},
        UI::Shell::{GetUserProfileDirectoryW, LoadUserProfileW, PROFILEINFOW, UnloadUserProfile},
    };

    const SOURCE_ROOT: &str = "C:\\m";
    const PASSWORD_FILE: &str = "C:\\ProgramData\\Motor\\job-password.txt";
    const BASH: &str = "C:\\Program Files\\Git\\bin\\bash.exe";

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }
    fn win32(ok: i32) -> io::Result<()> {
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    fn invalid(message: impl Into<String>) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, message.into())
    }
    fn security_status(code: u32) -> io::Result<()> {
        if code == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(code as i32))
        }
    }
    fn parse_cache_aces(acl: *const ACL) -> io::Result<Vec<CacheAce>> {
        // The ordinary ACE type tags defined by Windows' winnt.h.
        const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
        const ACCESS_DENIED_ACE_TYPE: u8 = 1;
        let mut aces = Vec::new();
        if acl.is_null() {
            return Ok(aces);
        }
        unsafe {
            for index in 0..(*acl).AceCount {
                let mut ace = null_mut();
                win32(GetAce(acl, u32::from(index), &mut ace))?;
                let header = &*ace.cast::<ACE_HEADER>();
                let allow = header.AceType == ACCESS_ALLOWED_ACE_TYPE;
                if !allow && header.AceType != ACCESS_DENIED_ACE_TYPE {
                    continue;
                }
                // Ordinary allow and deny ACEs have the same mask/SID layout.
                let sid_offset = std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart);
                if usize::from(header.AceSize) < sid_offset + 8 {
                    return Err(invalid("cache ACL has a truncated SID"));
                }
                let sid = ace.cast::<u8>().add(sid_offset).cast();
                let sid_length = GetLengthSid(sid) as usize;
                if sid_length < 8
                    || sid_length > usize::from(header.AceSize) - sid_offset
                    || IsValidSid(sid) == 0
                {
                    return Err(invalid("cache ACL has an invalid SID"));
                }
                aces.push(CacheAce {
                    allow,
                    mask: (*ace.cast::<ACCESS_ALLOWED_ACE>()).Mask,
                    flags: header.AceFlags,
                    sid: std::slice::from_raw_parts(sid.cast(), sid_length).to_vec(),
                });
            }
        }
        Ok(aces)
    }

    struct Handle(HANDLE);
    // Kernel handles may be used from multiple threads. Each overlapped operation
    // owns its own event and buffer; Arc keeps the underlying handle alive.
    unsafe impl Send for Handle {}
    unsafe impl Sync for Handle {}
    impl Handle {
        fn new(handle: HANDLE) -> io::Result<Self> {
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                Err(io::Error::last_os_error())
            } else {
                Ok(Self(handle))
            }
        }
        fn event() -> io::Result<Self> {
            unsafe { Self::new(CreateEventW(null(), 1, 0, null())) }
        }
        fn signal(&self) {
            unsafe {
                SetEvent(self.0);
            }
        }
        fn signalled(&self) -> bool {
            unsafe { WaitForSingleObject(self.0, 0) == WAIT_OBJECT_0 }
        }
    }
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    #[derive(Clone)]
    struct Port {
        #[cfg(not(feature = "hyperv-hvsock"))]
        handle: Arc<Handle>,
        #[cfg(feature = "hyperv-hvsock")]
        socket: Arc<crate::hvsock::Socket>,
        stop: Arc<Handle>,
        #[cfg(feature = "hyperv-spike")]
        read_total: Arc<std::sync::atomic::AtomicU64>,
        #[cfg(feature = "hyperv-spike")]
        write_total: Arc<std::sync::atomic::AtomicU64>,
    }
    impl Port {
        #[cfg(feature = "hyperv-hvsock")]
        fn open(stop: Arc<Handle>) -> io::Result<Self> {
            let address = motor_winagent::transport::hvsock_listen_address(
                match std::fs::read_to_string(motor_winagent::transport::HVSOCK_LISTEN_FILE) {
                    Ok(text) => Some(text),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                    Err(error) => return Err(error),
                }
                .as_deref(),
            )?;
            // Same bound as the host client's overall cap: the host dials as
            // soon as the VM starts, so a longer silence means no host exists.
            let deadline = Instant::now() + Duration::from_secs(300);
            let (socket, peer) = crate::hvsock::accept_host(stop.0, deadline, address)?;
            diagnostic!("hvsock accepted peer vm_id={peer:032x} listen={address:?}");
            // Make the diagnostic log durable while the job runs, so a forced
            // VM stop still leaves the byte counters on disk.
            let sync_stop = stop.clone();
            thread::spawn(move || {
                while !sync_stop.signalled() {
                    crate::hyperv_diag::sync();
                    thread::sleep(Duration::from_millis(500));
                }
            });
            Ok(Self {
                socket: Arc::new(socket),
                stop,
                read_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                write_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            })
        }

        #[cfg(not(feature = "hyperv-hvsock"))]
        fn open(stop: Arc<Handle>) -> io::Result<Self> {
            let path = wide(&motor_winagent::transport::port_path());
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                let handle = unsafe {
                    CreateFileW(
                        path.as_ptr(),
                        GENERIC_READ | GENERIC_WRITE,
                        0,
                        null(),
                        OPEN_EXISTING,
                        FILE_FLAG_OVERLAPPED,
                        null_mut(),
                    )
                };
                match Handle::new(handle) {
                    Ok(handle) => {
                        #[cfg(feature = "hyperv-spike")]
                        {
                            let setup = motor_winagent::transport::configure_com(unsafe {
                                std::os::windows::io::BorrowedHandle::borrow_raw(handle.0)
                            })?;
                            diagnostic!("COM configured {setup}");
                        }
                        let handle = Arc::new(handle);
                        #[cfg(feature = "hyperv-spike")]
                        {
                            let monitor = handle.clone();
                            let monitor_stop = stop.clone();
                            thread::spawn(move || {
                                let mut previous = None;
                                let mut tick = 0u64;
                                while !monitor_stop.signalled() {
                                    let status = motor_winagent::transport::serial_status(unsafe {
                                        std::os::windows::io::BorrowedHandle::borrow_raw(monitor.0)
                                    });
                                    match status {
                                        Ok(value) => {
                                            if value.0 != 0
                                                || previous != Some(value)
                                                || tick.is_multiple_of(10)
                                            {
                                                diagnostic!(
                                                    "COM status errors=0x{:x} rxover={} overrun={} queued_in={} queued_out={}",
                                                    value.0,
                                                    value.0 & 1 != 0,
                                                    value.0 & 2 != 0,
                                                    value.1,
                                                    value.2
                                                );
                                            }
                                            previous = Some(value);
                                        }
                                        Err(error) => diagnostic!("COM status error: {error}"),
                                    }
                                    tick += 1;
                                    crate::hyperv_diag::sync();
                                    thread::sleep(Duration::from_millis(500));
                                }
                            });
                        }
                        return Ok(Self {
                            handle,
                            stop,
                            #[cfg(feature = "hyperv-spike")]
                            read_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                            #[cfg(feature = "hyperv-spike")]
                            write_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                        });
                    }
                    Err(error) if stop.signalled() || Instant::now() >= deadline => {
                        return Err(error);
                    }
                    Err(_) => thread::sleep(Duration::from_millis(200)),
                }
            }
        }

        #[cfg(feature = "hyperv-spike")]
        fn record_transfer(&self, count: u32, write: bool) {
            let counter = if write {
                &self.write_total
            } else {
                &self.read_total
            };
            let total = counter.fetch_add(count as u64, std::sync::atomic::Ordering::Relaxed)
                + count as u64;
            diagnostic!(
                "io {} done bytes={count} total={total}",
                if write { "write" } else { "read" }
            );
        }

        fn transfer(&self, bytes: *mut u8, length: usize, write: bool) -> io::Result<usize> {
            diagnostic!(
                "io {} start requested={length}",
                if write { "write" } else { "read" }
            );
            if self.stop.signalled() {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "service stopped",
                ));
            }
            #[cfg(feature = "hyperv-hvsock")]
            {
                let count = self.socket.transfer(self.stop.0, bytes, length, write)?;
                self.record_transfer(count as u32, write);
                Ok(count)
            }
            #[cfg(not(feature = "hyperv-hvsock"))]
            self.transfer_file(bytes, length, write)
        }

        #[cfg(not(feature = "hyperv-hvsock"))]
        fn transfer_file(&self, bytes: *mut u8, length: usize, write: bool) -> io::Result<usize> {
            let event = Handle::event()?;
            // The stack OVERLAPPED, event and caller buffer remain alive until
            // completion, including after cancellation has been requested.
            unsafe {
                let mut operation: OVERLAPPED = zeroed();
                operation.hEvent = event.0;
                let mut transferred = 0;
                let count = length.min(u32::MAX as usize) as u32;
                let result = if write {
                    WriteFile(
                        self.handle.0,
                        bytes,
                        count,
                        &mut transferred,
                        &mut operation,
                    )
                } else {
                    ReadFile(
                        self.handle.0,
                        bytes,
                        count,
                        &mut transferred,
                        &mut operation,
                    )
                };
                let last_error = if result == 0 { GetLastError() } else { 0 };
                diagnostic!(
                    "io submitted success={} transferred={} last_error={last_error}",
                    result != 0,
                    transferred
                );
                if result == 0 && last_error != ERROR_IO_PENDING {
                    let error = io::Error::from_raw_os_error(last_error as i32);
                    if !cfg!(feature = "hyperv-spike")
                        && !write
                        && error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32)
                    {
                        return Ok(0);
                    }
                    return Err(error);
                }
                #[cfg(feature = "hyperv-spike")]
                if result != 0 {
                    win32(GetOverlappedResult(
                        self.handle.0,
                        &operation,
                        &mut transferred,
                        0,
                    ))?;
                    self.record_transfer(transferred, write);
                    return Ok(transferred as usize);
                }
                let handles = [self.stop.0, event.0];
                #[cfg(feature = "hyperv-spike")]
                let ready = loop {
                    let ready = WaitForMultipleObjects(2, handles.as_ptr(), 0, 250);
                    if ready == WAIT_TIMEOUT {
                        // Poll completion after a bounded event wait. This also
                        // distinguishes a pending IRP from a completed operation.
                        if GetOverlappedResult(self.handle.0, &operation, &mut transferred, 0) != 0
                        {
                            diagnostic!("io completion observed by poll");
                            self.record_transfer(transferred, write);
                            return Ok(transferred as usize);
                        }
                        let error = io::Error::last_os_error();
                        if error.raw_os_error() == Some(ERROR_IO_INCOMPLETE as i32) {
                            continue;
                        }
                        CancelIoEx(self.handle.0, &operation);
                        GetOverlappedResult(self.handle.0, &operation, &mut transferred, 1);
                        return Err(error);
                    }
                    break ready;
                };
                #[cfg(not(feature = "hyperv-spike"))]
                let ready = WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE);
                if ready != WAIT_OBJECT_0 + 1 {
                    CancelIoEx(self.handle.0, &operation);
                    GetOverlappedResult(self.handle.0, &operation, &mut transferred, 1);
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "virtio I/O stopped",
                    ));
                }
                win32(GetOverlappedResult(
                    self.handle.0,
                    &operation,
                    &mut transferred,
                    1,
                ))?;
                #[cfg(feature = "hyperv-spike")]
                self.record_transfer(transferred, write);
                Ok(transferred as usize)
            }
        }
    }
    impl Read for Port {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            if bytes.is_empty() {
                Ok(0)
            } else {
                // Only the COM port needs timeout retries; on a socket, and on
                // virtio-serial, a zero-byte read means the host closed.
                #[cfg(all(feature = "hyperv-spike", not(feature = "hyperv-hvsock")))]
                {
                    motor_winagent::transport::read_serial(bytes, |chunk| {
                        self.transfer(chunk.as_mut_ptr(), chunk.len(), false)
                    })
                }
                #[cfg(any(not(feature = "hyperv-spike"), feature = "hyperv-hvsock"))]
                {
                    self.transfer(bytes.as_mut_ptr(), bytes.len(), false)
                }
            }
        }
    }
    impl Write for Port {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.is_empty() {
                Ok(0)
            } else {
                self.transfer(bytes.as_ptr() as *mut u8, bytes.len(), true)
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        if std::env::args_os().len() != 1 {
            return Err(
                "motor-winagent takes no arguments; install as a LocalSystem auto-start service"
                    .into(),
            );
        }
        service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
        Ok(())
    }
    define_windows_service!(ffi_service_main, service_main);
    fn service_main(_: Vec<OsString>) {
        let _ = service();
    }
    fn service() -> Result<(), Box<dyn std::error::Error>> {
        let stop = Arc::new(Handle::event()?);
        let handler_stop = stop.clone();
        let status =
            service_control_handler::register(SERVICE_NAME, move |control| match control {
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    handler_stop.signal();
                    ServiceControlHandlerResult::NoError
                }
                _ => ServiceControlHandlerResult::NotImplemented,
            })?;
        let report = |state, code| {
            status.set_service_status(ServiceStatus {
                service_type: ServiceType::OWN_PROCESS,
                current_state: state,
                controls_accepted: if state == ServiceState::Running {
                    ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
                } else {
                    ServiceControlAccept::empty()
                },
                exit_code: ServiceExitCode::Win32(code),
                checkpoint: 0,
                wait_hint: Duration::from_secs(60),
                process_id: None,
            })
        };
        report(ServiceState::Running, 0)?;
        #[cfg(feature = "hyperv-spike")]
        if let Err(error) = crate::hyperv_diag::init() {
            report(
                ServiceState::Stopped,
                error.raw_os_error().unwrap_or(1) as u32,
            )?;
            return Err(error.into());
        }
        let result = serve(stop.clone());
        diagnostic!("service result: {result:?}");
        #[cfg(feature = "hyperv-spike")]
        crate::hyperv_diag::sync();
        stop.signal();
        report(ServiceState::Stopped, if result.is_ok() { 0 } else { 1 })?;
        result.map_err(Into::into)
    }

    #[cfg(not(feature = "hyperv-runner"))]
    fn powershell(script: &str) -> io::Result<String> {
        let output = std::process::Command::new(
            "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe",
        )
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            script,
        ])
        .output()?;
        if !output.status.success() {
            return Err(invalid(format!(
                "trusted setup failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        String::from_utf8(output.stdout)
            .map(|text| text.trim().to_owned())
            .map_err(|_| invalid("PowerShell output is not UTF-8"))
    }
    fn boot_nonce() -> io::Result<String> {
        #[cfg(feature = "hyperv-runner")]
        {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            loop {
                if let Some(media) = motor_winagent::transport::nonce_dvd()? {
                    return motor_winagent::transport::hyperv_nonce(&std::fs::read_to_string(
                        media.join("nonce.txt"),
                    )?);
                }
                if std::time::Instant::now() >= deadline {
                    return Err(invalid("boot requires exactly one MOTORNONCE DVD"));
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
        #[cfg(not(feature = "hyperv-runner"))]
        {
            if cfg!(feature = "hyperv-spike") {
                return motor_winagent::transport::hyperv_nonce(&std::fs::read_to_string(
                    motor_winagent::transport::HYPERV_NONCE_FILE,
                )?);
            }
            let strings = powershell(
                "$ErrorActionPreference='Stop'; [Console]::OutputEncoding=[Text.UTF8Encoding]::new(); (Get-CimInstance Win32_ComputerSystem).OEMStringArray",
            )?;
            let nonces: Vec<_> = strings
                .lines()
                .filter_map(|line| line.trim().strip_prefix(SMBIOS_NONCE_PREFIX))
                .collect();
            if nonces.len() != 1 || nonces[0].is_empty() {
                return Err(invalid("SMBIOS must contain exactly one motor boot nonce"));
            }
            Ok(nonces[0].to_owned())
        }
    }

    struct User {
        token: Handle,
        profile_handle: HANDLE,
        profile: PathBuf,
    }
    // Token/profile handles are process-wide. Ownership moves back to the service
    // thread only after the cache worker has finished using this profile.
    unsafe impl Send for User {}
    impl User {
        fn logon() -> io::Result<Self> {
            // Provisioning creates this administrator-only credential file and
            // grants the local standard user batch logon, not interactive login.
            let password = std::fs::read_to_string(PASSWORD_FILE)?;
            let mut password = wide(password.trim_end_matches(['\r', '\n']));
            let username = wide(JOB_USER);
            let mut token = null_mut();
            let result = unsafe {
                LogonUserW(
                    username.as_ptr(),
                    wide(".").as_ptr(),
                    password.as_ptr(),
                    LOGON32_LOGON_BATCH,
                    LOGON32_PROVIDER_DEFAULT,
                    &mut token,
                )
            };
            password.fill(0);
            win32(result)?;
            let token = Handle::new(token)?;
            let mut elevation: TOKEN_ELEVATION = unsafe { zeroed() };
            let mut returned = 0;
            unsafe {
                win32(GetTokenInformation(
                    token.0,
                    TokenElevation,
                    &mut elevation as *mut _ as *mut _,
                    size_of_val(&elevation) as u32,
                    &mut returned,
                ))?;
            }
            if elevation.TokenIsElevated != 0 {
                return Err(invalid("job account must not be an administrator"));
            }
            unsafe {
                let mut profile: PROFILEINFOW = zeroed();
                profile.dwSize = size_of::<PROFILEINFOW>() as u32;
                profile.dwFlags = 1;
                profile.lpUserName = username.as_ptr() as *mut u16;
                win32(LoadUserProfileW(token.0, &mut profile))?;
                let mut user = Self {
                    token,
                    profile_handle: profile.hProfile,
                    profile: PathBuf::new(),
                };
                let mut buffer = vec![0u16; 32768];
                let mut length = buffer.len() as u32;
                win32(GetUserProfileDirectoryW(
                    user.token.0,
                    buffer.as_mut_ptr(),
                    &mut length,
                ))?;
                user.profile = PathBuf::from(
                    String::from_utf16(&buffer[..length as usize - 1])
                        .map_err(|_| invalid("invalid job profile path"))?,
                );
                Ok(user)
            }
        }
        fn impersonate(&self) -> io::Result<Impersonation> {
            unsafe {
                win32(ImpersonateLoggedOnUser(self.token.0))?;
            }
            Ok(Impersonation)
        }
    }
    impl Drop for User {
        fn drop(&mut self) {
            unsafe {
                UnloadUserProfile(self.token.0, self.profile_handle);
            }
        }
    }
    struct Impersonation;
    impl Drop for Impersonation {
        fn drop(&mut self) {
            // Continuing with an unexpected identity is unsafe even inside a VM.
            if unsafe { RevertToSelf() } == 0 {
                std::process::abort();
            }
        }
    }

    struct UserEnvironment(*mut std::ffi::c_void);
    impl UserEnvironment {
        fn create(token: HANDLE) -> io::Result<Self> {
            let mut block = null_mut();
            unsafe {
                win32(CreateEnvironmentBlock(&mut block, token, 0))?;
            }
            Ok(Self(block))
        }
    }
    impl AsRef<[u16]> for UserEnvironment {
        fn as_ref(&self) -> &[u16] {
            // CreateEnvironmentBlock owns a double-NUL-terminated UTF-16 block
            // which stays valid until DestroyEnvironmentBlock in Drop.
            unsafe {
                let block = self.0.cast::<u16>();
                let mut length = 0;
                while *block.add(length) != 0 || *block.add(length + 1) != 0 {
                    length += 1;
                }
                std::slice::from_raw_parts(block, length + 2)
            }
        }
    }
    impl Drop for UserEnvironment {
        fn drop(&mut self) {
            unsafe {
                DestroyEnvironmentBlock(self.0);
            }
        }
    }

    struct Cache {
        volume: String,
        clean: bool,
        acl_updated: bool,
        mountpoint_acl_updated: bool,
        timings: CacheTimings,
    }
    struct LocalSecurityBuffer(*mut std::ffi::c_void);
    impl Drop for LocalSecurityBuffer {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.0);
            }
        }
    }
    fn cache_disk(_reporter: &CacheReporter) -> io::Result<u32> {
        #[cfg(feature = "hyperv-runner")]
        {
            // Query the Storage and CIM providers in-process; PowerShell startup
            // consumed the previous cold-boot disk-inventory budget.
            let disks = motor_winagent::cache_format::hyperv_disk_inventory(
                Instant::now() + motor_winagent::cache_steps::HYPERV_DISK_INVENTORY_TIMEOUT,
            )?;
            diagnostic!("Hyper-V cache disk inventory: {disks:?}");
            motor_winagent::cache::select_hyperv_cache_disk(&disks)
        }
        #[cfg(not(feature = "hyperv-runner"))]
        {
            let mut disks = Vec::new();
            // QEMU assigns a fixed serial to the cache disk. Bound enumeration
            // and select only that serial, never a disk merely by its number.
            for number in 0..32 {
                let path = wide(&format!("\\\\.\\PhysicalDrive{number}"));
                let disk = _reporter.step(CacheStep::DiskOpen, || {
                    let raw = unsafe {
                        CreateFileW(
                            path.as_ptr(),
                            0,
                            FILE_SHARE_READ | FILE_SHARE_WRITE,
                            null(),
                            OPEN_EXISTING,
                            0,
                            null_mut(),
                        )
                    };
                    if raw == INVALID_HANDLE_VALUE {
                        let error = io::Error::last_os_error();
                        if matches!(error.raw_os_error(), Some(2 | 3)) {
                            return Ok(None);
                        }
                        return Err(error);
                    }
                    Ok(Some(Handle::new(raw)?))
                })?;
                let Some(disk) = disk else {
                    continue;
                };
                // StorageDeviceProperty / PropertyStandardQuery, with no additional
                // parameters. The descriptor includes the virtio block serial.
                let query = [0u32; 3];
                let mut descriptor = [0u8; 4096];
                let mut returned = 0;
                _reporter.step(CacheStep::DiskSerial, || unsafe {
                    win32(DeviceIoControl(
                        disk.0,
                        IOCTL_STORAGE_QUERY_PROPERTY,
                        query.as_ptr().cast(),
                        size_of_val(&query) as u32,
                        descriptor.as_mut_ptr().cast(),
                        descriptor.len() as u32,
                        &mut returned,
                        null_mut(),
                    ))
                })?;
                disks.push((
                    number,
                    storage_serial(&descriptor[..returned as usize])?.to_owned(),
                ));
            }
            select_cache_disk(
                disks
                    .iter()
                    .map(|(number, serial)| (*number, serial.as_str())),
            )
        }
    }
    impl Cache {
        fn mount(user: &User, reporter: &CacheReporter) -> io::Result<Self> {
            if cfg!(feature = "hyperv-spike") && !cfg!(feature = "hyperv-runner") {
                // This spike measures one disposable OS disk, not cache reuse.
                // Keep the same empty-folder checks and SID-only Modify grant;
                // C:\t and all command writes disappear with the OS overlay.
                reporter.step(CacheStep::MountDirectory, || {
                    motor_winagent::cache::empty_mount_directory(Path::new(CACHE_MOUNT))
                })?;
                let updated = reporter.step(CacheStep::FolderAcl, || {
                    Self::grant_access(
                        CACHE_MOUNT.trim_end_matches('\\'),
                        FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
                        user,
                        true,
                    )
                })?;
                return Ok(Self {
                    volume: String::new(),
                    clean: false,
                    acl_updated: false,
                    mountpoint_acl_updated: updated,
                    timings: CacheTimings {
                        folder_acl_updated: updated,
                        ..Default::default()
                    },
                });
            }
            let started = Instant::now();
            let number = reporter.step(CacheStep::DiskSelection, || cache_disk(reporter))?;
            let disk_selection_ms = started.elapsed().as_millis() as u64;
            #[cfg(not(feature = "hyperv-runner"))]
            let disk = prepare_cache(
                &mut crate::storage::WindowsStorage::open(number, reporter)?,
                number,
            )?;
            #[cfg(feature = "hyperv-runner")]
            let disk = {
                let mut storage = crate::storage::WindowsStorage::open(number, reporter)?;
                let identity = storage.capture_identity()?;
                diagnostic!("{}", motor_winagent::cache::disk_identity_log(&identity));
                motor_winagent::cache::prepare_cache_with_identity(&mut storage, number, &identity)?
            };
            let volume = disk.volume;
            if !volume.starts_with("\\\\?\\Volume{")
                || !volume.ends_with("}\\")
                || volume.contains(['\r', '\n'])
            {
                return Err(invalid("invalid cache volume identity"));
            }
            let mountpoint_started = Instant::now();
            reporter.step(CacheStep::MountDirectory, || {
                motor_winagent::cache::empty_mount_directory(Path::new(CACHE_MOUNT))
            })?;
            let folder_acl_started = Instant::now();
            // The empty OS folder is trusted only before the cache is mounted.
            // Never propagate this grant into a reused volume's children.
            let mountpoint_acl_updated = reporter.step(CacheStep::FolderAcl, || {
                Self::grant_access(
                    CACHE_MOUNT.trim_end_matches('\\'),
                    FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
                    user,
                    true,
                )
            })?;
            let folder_acl_ms = folder_acl_started.elapsed().as_millis();
            reporter.step(CacheStep::MountPoint, || unsafe {
                win32(SetVolumeMountPointW(
                    wide(CACHE_MOUNT).as_ptr(),
                    wide(&volume).as_ptr(),
                ))
            })?;
            let mountpoint_ms = mountpoint_started.elapsed().as_millis();
            let mut cache = Self {
                volume,
                clean: false,
                acl_updated: false,
                mountpoint_acl_updated,
                timings: CacheTimings::default(),
            };
            let dirty_started = Instant::now();
            let handle = reporter.step(CacheStep::DirtyOpen, || cache.open_volume())?;
            let mut dirty: u32 = 0;
            let mut returned = 0;
            reporter.step(CacheStep::DirtyCheck, || unsafe {
                win32(DeviceIoControl(
                    handle.0,
                    FSCTL_IS_VOLUME_DIRTY,
                    null(),
                    0,
                    &mut dirty as *mut _ as *mut _,
                    4,
                    &mut returned,
                    null_mut(),
                ))
            })?;
            if dirty & VOLUME_IS_DIRTY != 0 {
                return Err(invalid("cache volume has a dirty NTFS bit"));
            }
            let dirty_ms = dirty_started.elapsed().as_millis();
            let volume_acl_started = Instant::now();
            cache.acl_updated = reporter.step(CacheStep::VolumeAcl, || {
                Self::grant_access(
                    &cache.volume,
                    FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
                    user,
                    disk.freshly_formatted,
                )
            })?;
            let volume_acl_ms = volume_acl_started.elapsed().as_millis();
            cache.timings = CacheTimings {
                elapsed_ms: started.elapsed().as_millis() as u64,
                disk_selection_ms,
                powershell_ms: disk.powershell_ms,
                set_disk_ms: 0,
                native_attributes_ms: disk.native_attributes_ms,
                volume_ms: disk.volume_ms,
                mountpoint_ms: mountpoint_ms as u64,
                dirty_check_ms: dirty_ms as u64,
                folder_acl_ms: folder_acl_ms as u64,
                volume_acl_ms: volume_acl_ms as u64,
                folder_acl_updated: cache.mountpoint_acl_updated,
                volume_acl_updated: cache.acl_updated,
            };
            Ok(cache)
        }
        fn grant_access(
            path: &str,
            flags: u32,
            user: &User,
            freshly_formatted: bool,
        ) -> io::Result<bool> {
            // Apply the same exact-SID Modify policy to each security object.
            // Preserve other trustees and skip Windows' inheritance propagation
            // entirely when this object's explicit grant is already present.
            let root = unsafe {
                Handle::new(CreateFileW(
                    wide(path).as_ptr(),
                    READ_CONTROL | WRITE_DAC,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    null(),
                    OPEN_EXISTING,
                    flags,
                    null_mut(),
                ))?
            };
            let mut length = 0;
            unsafe {
                GetTokenInformation(user.token.0, TokenUser, null_mut(), 0, &mut length);
            }
            if length == 0 {
                return Err(io::Error::last_os_error());
            }
            // TOKEN_USER and its SID share this naturally aligned buffer for
            // the entire ACL update; the token's SID avoids account-name lookup.
            let mut token = vec![0usize; (length as usize).div_ceil(size_of::<usize>())];
            unsafe {
                win32(GetTokenInformation(
                    user.token.0,
                    TokenUser,
                    token.as_mut_ptr().cast(),
                    length,
                    &mut length,
                ))?;
                let sid = (*(token.as_ptr() as *const TOKEN_USER)).User.Sid;
                let mut old_acl = null_mut();
                let mut descriptor = null_mut();
                security_status(GetSecurityInfo(
                    root.0,
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    &mut old_acl,
                    null_mut(),
                    &mut descriptor,
                ))?;
                let _descriptor = LocalSecurityBuffer(descriptor);
                let sid_bytes = std::slice::from_raw_parts(sid.cast(), GetLengthSid(sid) as usize);
                if !motor_winagent::cache::cache_acl_update(
                    cache_has_modify_grant(&parse_cache_aces(old_acl)?, sid_bytes),
                    freshly_formatted,
                )? {
                    return Ok(false);
                }
                let access = EXPLICIT_ACCESS_W {
                    grfAccessPermissions: CACHE_MODIFY_MASK,
                    grfAccessMode: SET_ACCESS,
                    grfInheritance: u32::from(CACHE_INHERIT_FLAGS),
                    Trustee: TRUSTEE_W {
                        pMultipleTrustee: null_mut(),
                        MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                        TrusteeForm: TRUSTEE_IS_SID,
                        TrusteeType: TRUSTEE_IS_USER,
                        ptstrName: sid.cast(),
                    },
                };
                let mut new_acl = null_mut();
                security_status(SetEntriesInAclW(1, &access, old_acl, &mut new_acl))?;
                let _new_acl = LocalSecurityBuffer(new_acl.cast());
                security_status(SetSecurityInfo(
                    root.0,
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    new_acl,
                    null_mut(),
                ))?;
                Ok(true)
            }
        }
        fn open_volume(&self) -> io::Result<Handle> {
            unsafe {
                Handle::new(CreateFileW(
                    wide(self.volume.trim_end_matches('\\')).as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    null(),
                    OPEN_EXISTING,
                    0,
                    null_mut(),
                ))
            }
        }
        fn dismount(&mut self) -> io::Result<()> {
            if cfg!(feature = "hyperv-spike") && !cfg!(feature = "hyperv-runner") {
                self.clean = true;
                return Ok(());
            }
            let handle = self.open_volume()?;
            let mut returned = 0;
            unsafe {
                win32(DeviceIoControl(
                    handle.0,
                    FSCTL_LOCK_VOLUME,
                    null(),
                    0,
                    null_mut(),
                    0,
                    &mut returned,
                    null_mut(),
                ))?;
                win32(DeviceIoControl(
                    handle.0,
                    FSCTL_DISMOUNT_VOLUME,
                    null(),
                    0,
                    null_mut(),
                    0,
                    &mut returned,
                    null_mut(),
                ))?;
            }
            // C:\t can be replaced by the job. Leave its mount point in the
            // disposable OS overlay rather than resolving a job-controlled path.
            self.clean = true;
            Ok(())
        }
    }

    enum Event {
        Host(Frame),
        Output(Stream, Vec<u8>),
        PipeClosed,
        Lost(io::Error),
    }
    fn post(tx: &mpsc::SyncSender<Event>, stop: &Handle, mut event: Event) {
        loop {
            match tx.try_send(event) {
                Ok(()) | Err(mpsc::TrySendError::Disconnected(_)) => return,
                Err(mpsc::TrySendError::Full(value)) => event = value,
            }
            if stop.signalled() {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
    struct Reader {
        stop: Arc<Handle>,
        thread: Option<thread::JoinHandle<()>>,
    }
    impl Drop for Reader {
        fn drop(&mut self) {
            self.stop.signal();
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
    fn reader(port: Port, tx: mpsc::SyncSender<Event>) -> Reader {
        let stop = port.stop.clone();
        let reader_stop = stop.clone();
        let thread = thread::spawn(move || {
            let mut port = port;
            loop {
                match read_frame(&mut port) {
                    Ok(Some(frame)) => {
                        #[cfg(feature = "hyperv-spike")]
                        match &frame {
                            Frame::ReceiveTree { bytes } => {
                                diagnostic!("reader frame ReceiveTree bytes={}", bytes.len())
                            }
                            Frame::TreeEnd { .. } => diagnostic!("reader frame TreeEnd"),
                            Frame::Start {} => diagnostic!("reader frame Start"),
                            Frame::Collect {} => diagnostic!("reader frame Collect"),
                            Frame::Dismount {} => diagnostic!("reader frame Dismount"),
                            Frame::Cancel {} => diagnostic!("reader frame Cancel"),
                            _ => diagnostic!("reader unexpected host frame"),
                        }
                        post(&tx, &reader_stop, Event::Host(frame));
                    }
                    Ok(None) => {
                        post(
                            &tx,
                            &reader_stop,
                            Event::Lost(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "host closed virtio port",
                            )),
                        );
                        break;
                    }
                    Err(error) => {
                        diagnostic!("reader error: {error}");
                        post(&tx, &reader_stop, Event::Lost(error));
                        break;
                    }
                }
                if reader_stop.signalled() {
                    break;
                }
            }
        });
        Reader {
            stop,
            thread: Some(thread),
        }
    }
    fn send(protocol: &mut Protocol, port: &mut Port, frame: &Frame) -> io::Result<()> {
        protocol.accept(Peer::Agent, frame)?;
        write_frame(port, frame)
    }
    fn receive(
        protocol: &mut Protocol,
        rx: &mpsc::Receiver<Event>,
        stop: &Handle,
    ) -> io::Result<Frame> {
        loop {
            if stop.signalled() {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "service stopped",
                ));
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Event::Host(frame)) => {
                    protocol.accept(Peer::Host, &frame)?;
                    return Ok(frame);
                }
                Ok(Event::Lost(error)) => return Err(error),
                Ok(_) => return Err(invalid("unexpected process output outside running phase")),
                Err(mpsc::RecvTimeoutError::Timeout) => (),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(invalid("agent reader disconnected"));
                }
            }
        }
    }

    struct Attributes {
        storage: Vec<usize>,
        initialized: bool,
    }
    impl Attributes {
        fn new(handles: &[HANDLE]) -> io::Result<Self> {
            unsafe {
                let mut length = 0;
                InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut length);
                let mut attributes = Self {
                    storage: vec![0; length.div_ceil(size_of::<usize>())],
                    initialized: false,
                };
                win32(InitializeProcThreadAttributeList(
                    attributes.ptr(),
                    1,
                    0,
                    &mut length,
                ))?;
                attributes.initialized = true;
                win32(UpdateProcThreadAttribute(
                    attributes.ptr(),
                    0,
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    handles.as_ptr() as *const _,
                    size_of_val(handles),
                    null_mut(),
                    null(),
                ))?;
                Ok(attributes)
            }
        }
        fn ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
            self.storage.as_mut_ptr() as *mut _
        }
    }
    impl Drop for Attributes {
        fn drop(&mut self) {
            if self.initialized {
                unsafe {
                    DeleteProcThreadAttributeList(self.ptr());
                }
            }
        }
    }
    fn output_pipe() -> io::Result<(File, Handle)> {
        unsafe {
            let attributes = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: null_mut(),
                bInheritHandle: 1,
            };
            let (mut read, mut write) = (null_mut(), null_mut());
            win32(CreatePipe(&mut read, &mut write, &attributes, 0))?;
            let read = Handle::new(read)?;
            let write = Handle::new(write)?;
            win32(SetHandleInformation(read.0, HANDLE_FLAG_INHERIT, 0))?;
            let read = std::mem::ManuallyDrop::new(read);
            Ok((File::from_raw_handle(read.0), write))
        }
    }

    struct Job {
        job: Handle,
        process: Handle,
        readers: Vec<thread::JoinHandle<()>>,
        stop: Arc<Handle>,
        stop_on_drop: bool,
    }
    impl Job {
        fn start(
            user: &User,
            command: &Command,
            tx: mpsc::SyncSender<Event>,
            stop: Arc<Handle>,
        ) -> io::Result<Self> {
            command.validate()?;
            let (stdout, stdout_write) = output_pipe()?;
            let (stderr, stderr_write) = output_pipe()?;
            let attributes = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: null_mut(),
                bInheritHandle: 1,
            };
            let stdin = unsafe {
                Handle::new(CreateFileW(
                    wide("NUL").as_ptr(),
                    GENERIC_READ,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    &attributes,
                    OPEN_EXISTING,
                    0,
                    null_mut(),
                ))?
            };
            let handles = [stdin.0, stdout_write.0, stderr_write.0];
            let mut attributes = Attributes::new(&handles)?;
            let job = unsafe { Handle::new(CreateJobObjectW(null(), null()))? };
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            unsafe {
                win32(SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as *const _,
                    size_of_val(&limits) as u32,
                ))?;
            }
            let mut startup: STARTUPINFOEXW = unsafe { zeroed() };
            startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
            startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            startup.StartupInfo.hStdInput = stdin.0;
            startup.StartupInfo.hStdOutput = stdout_write.0;
            startup.StartupInfo.hStdError = stderr_write.0;
            startup.lpAttributeList = attributes.ptr();
            let profile = user
                .profile
                .to_str()
                .ok_or_else(|| invalid("job profile is not UTF-8"))?;
            let base = read_user_environment(|| UserEnvironment::create(user.token.0))?;
            let git_bin = git_native_bin(|path| Path::new(path).is_dir())?;
            let env = command.environment(profile, base, git_bin)?;
            let environment = encode_block(&env)?;
            let mut line = wide(&format!(
                "{} -c {}",
                motor_winagent::quote_windows_arg(BASH),
                motor_winagent::quote_windows_arg(&command.command)
            ));
            let cwd = wide(&mapped_cwd(&command.cwd)?);
            let mut process: PROCESS_INFORMATION = unsafe { zeroed() };
            // Create suspended, assign to the non-breakaway Job Object, and only
            // then resume. PROC_THREAD_ATTRIBUTE_HANDLE_LIST permits only stdin,
            // stdout and stderr to be inherited, excluding the service token and
            // virtio channel from the job's process tree.
            unsafe {
                win32(CreateProcessAsUserW(
                    user.token.0,
                    wide(BASH).as_ptr(),
                    line.as_mut_ptr(),
                    null(),
                    null(),
                    1,
                    CREATE_SUSPENDED
                        | CREATE_NO_WINDOW
                        | CREATE_UNICODE_ENVIRONMENT
                        | EXTENDED_STARTUPINFO_PRESENT,
                    environment.as_ptr() as *const _,
                    cwd.as_ptr(),
                    &startup.StartupInfo,
                    &mut process,
                ))?;
            }
            let process_handle = Handle::new(process.hProcess)?;
            let thread_handle = Handle::new(process.hThread)?;
            if unsafe { AssignProcessToJobObject(job.0, process_handle.0) } == 0 {
                let error = io::Error::last_os_error();
                unsafe {
                    TerminateProcess(process_handle.0, 1);
                    WaitForSingleObject(process_handle.0, INFINITE);
                }
                return Err(error);
            }
            let mut job = Self {
                job,
                process: process_handle,
                readers: Vec::new(),
                stop,
                stop_on_drop: true,
            };
            if unsafe { ResumeThread(thread_handle.0) } == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            drop(stdout_write);
            drop(stderr_write);
            drop(stdin);
            for (mut pipe, stream) in [(stdout, Stream::Stdout), (stderr, Stream::Stderr)] {
                let tx = tx.clone();
                let stop = job.stop.clone();
                job.readers.push(thread::spawn(move || {
                    let mut buffer = vec![0; CHUNK_BYTES];
                    loop {
                        match pipe.read(&mut buffer) {
                            Ok(0) => {
                                post(&tx, &stop, Event::PipeClosed);
                                break;
                            }
                            Ok(count) => {
                                post(&tx, &stop, Event::Output(stream, buffer[..count].to_vec()))
                            }
                            Err(error)
                                if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) =>
                            {
                                post(&tx, &stop, Event::PipeClosed);
                                break;
                            }
                            Err(error) => {
                                post(&tx, &stop, Event::Lost(error));
                                break;
                            }
                        }
                        if stop.signalled() {
                            break;
                        }
                    }
                }));
            }
            Ok(job)
        }
        fn exit_code(&self) -> io::Result<Option<u32>> {
            if unsafe { WaitForSingleObject(self.process.0, 0) } != WAIT_OBJECT_0 {
                return Ok(None);
            }
            let mut code = 0;
            unsafe {
                win32(GetExitCodeProcess(self.process.0, &mut code))?;
            }
            Ok(Some(code))
        }
        fn wait_empty(&self) -> io::Result<()> {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
                unsafe {
                    win32(QueryInformationJobObject(
                        self.job.0,
                        JobObjectBasicAccountingInformation,
                        &mut accounting as *mut _ as *mut _,
                        size_of_val(&accounting) as u32,
                        null_mut(),
                    ))?;
                }
                if accounting.ActiveProcesses == 0 {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(invalid("Job Object did not become empty after termination"));
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
        fn terminate(&self, code: u32) -> io::Result<()> {
            unsafe { win32(TerminateJobObject(self.job.0, code)) }
        }
    }
    impl Drop for Job {
        fn drop(&mut self) {
            // Stop the entire Job Object, including detached grandchildren, before
            // joining pipe readers or closing the last containment handle.
            let _ = self.terminate(1);
            unsafe {
                WaitForSingleObject(self.process.0, INFINITE);
            }
            if self.stop_on_drop {
                self.stop.signal();
            }
            for thread in self.readers.drain(..) {
                let _ = thread.join();
            }
        }
    }

    fn junction(target: &Path) -> io::Result<()> {
        std::fs::create_dir(target)?;
        let target = target
            .to_str()
            .ok_or_else(|| invalid("target mount path is not UTF-8"))?;
        let substitute = wide(&format!("\\??\\{}", CACHE_MOUNT.trim_end_matches('\\')));
        let print = wide(CACHE_MOUNT.trim_end_matches('\\'));
        let mut buffer = Vec::new();
        buffer.extend(0xa0000003u32.to_le_bytes());
        buffer.extend(((8 + (substitute.len() + print.len()) * 2) as u16).to_le_bytes());
        buffer.extend(0u16.to_le_bytes());
        for value in [
            0,
            ((substitute.len() - 1) * 2) as u16,
            (substitute.len() * 2) as u16,
            ((print.len() - 1) * 2) as u16,
        ] {
            buffer.extend(value.to_le_bytes());
        }
        for value in substitute.into_iter().chain(print) {
            buffer.extend(value.to_le_bytes());
        }
        unsafe {
            let handle = Handle::new(CreateFileW(
                wide(target).as_ptr(),
                GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
                null_mut(),
            ))?;
            let mut returned = 0;
            win32(DeviceIoControl(
                handle.0,
                FSCTL_SET_REPARSE_POINT,
                buffer.as_ptr() as *const _,
                buffer.len() as u32,
                null_mut(),
                0,
                &mut returned,
                null_mut(),
            ))
        }
    }

    fn stage(
        protocol: &mut Protocol,
        rx: &mpsc::Receiver<Event>,
        port: &mut Port,
        user: &User,
    ) -> io::Result<(Command, Inventory)> {
        let _identity = user.impersonate()?;
        let home = user.profile.join(".cargo");
        let temp = user.profile.join("AppData/Local/Temp");
        std::fs::create_dir_all(&home)?;
        std::fs::create_dir_all(&temp)?;
        let mut archive = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(temp.join("motor-tree.tar"))?;
        diagnostic!("staging archive opened");
        let command = loop {
            match receive(protocol, rx, &port.stop)? {
                Frame::ReceiveTree { bytes } => {
                    diagnostic!("staging ReceiveTree bytes={}", bytes.len());
                    archive.write_all(&bytes)?;
                }
                Frame::TreeEnd { command } => break command,
                _ => return Err(invalid("expected tree transfer")),
            }
        };
        archive.flush()?;
        unpack_tree(&mut archive, Path::new(SOURCE_ROOT), &home)?;
        // The archive's host paths do not exist in the VM; pin Cargo to the
        // absolute local-registry path under the guest user's CARGO_HOME.
        motor_winagent::rewrite_local_registry_config(&home)?;
        let before = inventory(Path::new(SOURCE_ROOT))?;
        let cwd = PathBuf::from(mapped_cwd(&command.cwd)?);
        if !cwd.is_dir() {
            return Err(invalid("command cwd is not in the transferred tree"));
        }
        // Use the impersonated job token when creating the junction: resolving
        // a path supplied by the job must never gain administrator filesystem
        // privileges. Dismount uses the volume GUID rather than the junction,
        // which the command is free to change.
        let workspace = PathBuf::from(mapped_cwd(&command.workspace)?);
        if !workspace.is_dir() {
            return Err(invalid("workspace is not in the transferred tree"));
        }
        junction(&workspace.join("target"))?;
        drop(archive);
        std::fs::remove_file(temp.join("motor-tree.tar"))?;
        send(protocol, port, &Frame::TreeReceived {})?;
        diagnostic!("staging TreeReceived sent");
        Ok((command, before))
    }

    fn execute(
        protocol: &mut Protocol,
        rx: &mpsc::Receiver<Event>,
        tx: mpsc::SyncSender<Event>,
        port: &mut Port,
        user: &User,
        command: &Command,
    ) -> io::Result<()> {
        if receive(protocol, rx, &port.stop)? != (Frame::Start {}) {
            return Err(invalid("expected the execution gate"));
        }
        let mut job = Job::start(user, command, tx, port.stop.clone())?;
        send(protocol, port, &Frame::Started {})?;
        let mut code = None;
        let mut closed = 0;
        loop {
            if port.stop.signalled() {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "service stopped",
                ));
            }
            if code.is_none()
                && let Some(exit) = job.exit_code()?
            {
                code = Some(exit);
                job.terminate(exit)?;
            }
            if code.is_some() && closed == 2 {
                job.wait_empty()?;
                break;
            }
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(Event::Output(stream, bytes)) => {
                    send(protocol, port, &Frame::Output { stream, bytes })?
                }
                Ok(Event::PipeClosed) => closed += 1,
                Ok(Event::Host(frame)) => {
                    protocol.accept(Peer::Host, &frame)?;
                    if frame != (Frame::Cancel {}) {
                        return Err(invalid("expected cancel while running"));
                    }
                    job.terminate(0xc000013a)?;
                }
                Ok(Event::Lost(error)) => return Err(error),
                Err(mpsc::RecvTimeoutError::Timeout) => (),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(invalid("execution channel disconnected"));
                }
            }
        }
        // The ordinary command exit must not stop the transport: only pipe
        // readers need joining here. Error paths retain Job's kill-on-drop guard.
        for thread in job.readers.drain(..) {
            let _ = thread.join();
        }
        send(
            protocol,
            port,
            &Frame::Exit {
                code: code.unwrap(),
            },
        )?;
        job.stop_on_drop = false;
        Ok(())
    }

    fn enable_privilege(name: &str) -> io::Result<()> {
        unsafe {
            let mut token = null_mut();
            win32(OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                &mut token,
            ))?;
            let token = Handle::new(token)?;
            let mut luid = zeroed();
            win32(LookupPrivilegeValueW(
                null(),
                wide(name).as_ptr(),
                &mut luid,
            ))?;
            let privileges = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            SetLastError(0);
            win32(AdjustTokenPrivileges(
                token.0,
                0,
                &privileges,
                0,
                null_mut(),
                null_mut(),
            ))?;
            if GetLastError() != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    fn mount_cache(protocol: &mut Protocol, port: &mut Port) -> io::Result<(User, Cache)> {
        let (steps_tx, steps_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let reporter = CacheReporter::new(steps_tx);
            match reporter.step(CacheStep::UserLogon, User::logon) {
                Ok(user) => match Cache::mount(&user, &reporter) {
                    Ok(cache) => {
                        let _ = result_tx.send(Ok((user, cache)));
                    }
                    Err(error) => {
                        // Report before profile cleanup, which may itself need I/O.
                        let _ = result_tx.send(Err(error));
                        drop(user);
                    }
                },
                Err(error) => {
                    let _ = result_tx.send(Err(error));
                }
            }
        });
        let result = watch_cache(
            steps_rx,
            result_rx,
            |step, elapsed_ms| send(protocol, port, &Frame::CacheProgress { step, elapsed_ms }),
            || unsafe {
                // Cancel only this process's cache worker I/O. Never wait for an
                // uncooperative driver before reporting failure; the host destroys
                // this job VM, and a disconnected reporter prevents further steps.
                CancelSynchronousIo(worker.as_raw_handle());
            },
            CacheLimits::default(),
        );
        match result {
            Ok(value) => Ok(value),
            Err(failure) => {
                let message = failure.to_string();
                send(protocol, port, &Frame::CacheFailed { failure })?;
                Err(invalid(message))
            }
        }
    }

    fn serve(stop: Arc<Handle>) -> io::Result<()> {
        for privilege in [
            "SeBackupPrivilege",
            "SeRestorePrivilege",
            "SeManageVolumePrivilege",
        ] {
            enable_privilege(privilege)?;
        }
        let nonce = boot_nonce()?;
        diagnostic!("boot nonce={nonce}");
        let mut protocol = Protocol::new(&nonce)?;
        let mut port = Port::open(stop)?;
        let (tx, rx) = mpsc::sync_channel(16);
        let _reader = reader(port.clone(), tx.clone());
        send(
            &mut protocol,
            &mut port,
            &Frame::Hello {
                version: PROTOCOL_VERSION,
                nonce,
            },
        )?;
        let (user, mut cache) = mount_cache(&mut protocol, &mut port)?;
        send(
            &mut protocol,
            &mut port,
            &Frame::CacheMounted {
                timings: cache.timings.clone(),
            },
        )?;
        let (command, before) = stage(&mut protocol, &rx, &mut port, &user)?;
        execute(&mut protocol, &rx, tx, &mut port, &user, &command)?;
        if receive(&mut protocol, &rx, &port.stop)? != (Frame::Collect {}) {
            return Err(invalid("expected collect"));
        }
        let (bytes, deleted) = {
            let _identity = user.impersonate()?;
            collect_changes(Path::new(SOURCE_ROOT), &before)?
        };
        for chunk in bytes.chunks(CHUNK_BYTES) {
            send(
                &mut protocol,
                &mut port,
                &Frame::Collected {
                    bytes: chunk.to_vec(),
                },
            )?;
        }
        send(&mut protocol, &mut port, &Frame::CollectEnd { deleted })?;
        if receive(&mut protocol, &rx, &port.stop)? != (Frame::Dismount {}) {
            return Err(invalid("expected dismount"));
        }
        cache.dismount()?;
        send(
            &mut protocol,
            &mut port,
            &Frame::Dismounted { clean: cache.clean },
        )?;
        // QEMU must exit normally after the clean dismount acknowledgement. A
        // service exit alone would leave the guest running until the host's
        // unit-exit deadline and force the host to discard the cache disk.
        let result = std::process::Command::new("C:\\Windows\\System32\\shutdown.exe")
            .args(["/s", "/t", "0"])
            .status()?;
        if !result.success() {
            return Err(invalid("guest shutdown request failed"));
        }
        Ok(())
    }
}
