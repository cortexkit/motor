//! Local-only runner transport. SSH is the authenticated public boundary.
#[cfg(any(windows, test))]
fn service_owns_pipe(pipe_pid: u32, service_pid: Option<u32>, running: bool) -> bool {
    running && pipe_pid != 0 && service_pid == Some(pipe_pid)
}

#[cfg(unix)]
pub use tokio::net::{UnixListener as Listener, UnixStream as Stream};
#[cfg(unix)]
pub async fn connect(path: &std::path::Path) -> std::io::Result<Stream> {
    Stream::connect(path).await
}

#[cfg(windows)]
pub use native::{connect, Listener, Stream};
#[cfg(windows)]
mod native {
    use std::{
        io,
        os::windows::io::AsRawHandle,
        path::{Path, PathBuf},
        ptr::null_mut,
    };
    use tokio::{
        net::windows::named_pipe::{
            ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
        },
        sync::Mutex,
    };
    use windows_service::{
        service::{ServiceAccess, ServiceState},
        service_manager::{ServiceManager, ServiceManagerAccess},
    };
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{
            Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SECURITY_ATTRIBUTES,
        },
        System::Pipes::GetNamedPipeServerProcessId,
    };
    pub type Stream = NamedPipeServer;
    pub struct Listener {
        name: PathBuf,
        sid: Option<String>,
        next: Mutex<NamedPipeServer>,
    }
    fn pipe(name: &Path, sid: Option<&str>, first: bool) -> io::Result<NamedPipeServer> {
        let descriptor: Vec<_> = super::pipe_descriptor(sid)?
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut security = null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                descriptor.as_ptr(),
                1,
                &mut security,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: security,
            bInheritHandle: 0,
        };
        let result = unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(
                    name,
                    (&raw const attributes).cast_mut().cast(),
                )
        };
        unsafe { LocalFree(security) };
        result
    }
    impl Listener {
        pub fn bind(name: &Path, sid: String) -> io::Result<Self> {
            Self::bind_with_sid(name, Some(sid))
        }
        pub fn bind_qualification(name: &Path) -> io::Result<Self> {
            Self::bind_with_sid(name, None)
        }
        pub fn is_qualification(&self) -> bool {
            self.sid.is_none()
        }
        fn bind_with_sid(name: &Path, sid: Option<String>) -> io::Result<Self> {
            Ok(Self {
                name: name.into(),
                next: Mutex::new(pipe(name, sid.as_deref(), true)?),
                sid,
            })
        }
        pub async fn accept(&self) -> io::Result<(Stream, ())> {
            let mut next = self.next.lock().await;
            next.connect().await?;
            let replacement = pipe(&self.name, self.sid.as_deref(), false)?;
            Ok((std::mem::replace(&mut *next, replacement), ()))
        }
    }
    pub async fn connect(path: &Path) -> io::Result<NamedPipeClient> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match ClientOptions::new().open(path) {
                Ok(pipe) => {
                    // A local user can pre-create a pipe while the service is
                    // offline. Authenticate its server before sending any job
                    // data; the name alone is not the runner's identity.
                    let mut pid = 0;
                    if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) } == 0 {
                        return Err(io::Error::last_os_error());
                    }
                    let manager =
                        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
                            .map_err(io::Error::other)?;
                    let status = manager
                        .open_service("MotorRunner", ServiceAccess::QUERY_STATUS)
                        .and_then(|service| service.query_status())
                        .map_err(io::Error::other)?;
                    if !super::service_owns_pipe(
                        pid,
                        status.process_id,
                        status.current_state == ServiceState::Running,
                    ) {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "local pipe is not owned by the running MotorRunner service",
                        ));
                    }
                    return Ok(pipe);
                }
                Err(error)
                    if error.raw_os_error() == Some(231)
                        && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await
                }
                Err(error) => return Err(error),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::service_owns_pipe;
    #[test]
    fn pipe_identity_accepts_the_running_service_process() {
        assert!(service_owns_pipe(42, Some(42), true));
        assert!(service_owns_pipe(9001, Some(9001), true));
    }
    #[test]
    fn pipe_identity_refuses_stopped_mismatched_or_missing_service() {
        assert!(!service_owns_pipe(42, Some(42), false));
        assert!(!service_owns_pipe(42, Some(43), true));
        assert!(!service_owns_pipe(42, None, true));
        assert!(!service_owns_pipe(0, Some(0), true));
    }
}
#[cfg(any(windows, test))]
fn pipe_descriptor(sid: Option<&str>) -> std::io::Result<String> {
    let mut descriptor = "D:P(A;;GA;;;SY)(A;;GA;;;BA)".to_owned();
    if let Some(sid) = sid {
        if !sid.starts_with("S-1-5-")
            || !sid
                .bytes()
                .all(|b| b.is_ascii_digit() || b"S-".contains(&b))
        {
            return Err(std::io::Error::other("invalid SSH entry SID"));
        }
        descriptor.push_str(&format!("(A;;GRGW;;;{sid})"));
    }
    Ok(descriptor)
}

#[cfg(test)]
mod qualification_tests {
    #[test]
    fn qualification_pipe_descriptor_excludes_the_fleet_sid() {
        let fleet_sid = "S-1-5-21-123-456-789-1001";
        let descriptor = super::pipe_descriptor(None).unwrap();
        assert_eq!(descriptor, "D:P(A;;GA;;;SY)(A;;GA;;;BA)");
        assert!(!descriptor.contains(fleet_sid));
        assert_eq!(
            super::pipe_descriptor(Some(fleet_sid)).unwrap(),
            format!("{descriptor}(A;;GRGW;;;{fleet_sid})")
        );
        assert!(super::pipe_descriptor(Some("SY)(A;;GA;;;WD)")).is_err());
    }
}
