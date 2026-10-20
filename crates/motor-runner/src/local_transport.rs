//! Local-only runner transport. SSH is the authenticated public boundary.
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
        path::{Path, PathBuf},
        ptr::null_mut,
    };
    use tokio::{
        net::windows::named_pipe::{
            ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
        },
        sync::Mutex,
    };
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{
            Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SECURITY_ATTRIBUTES,
        },
    };
    pub type Stream = NamedPipeServer;
    pub struct Listener {
        name: PathBuf,
        sid: String,
        next: Mutex<NamedPipeServer>,
    }
    fn pipe(name: &Path, sid: &str, first: bool) -> io::Result<NamedPipeServer> {
        if !sid.starts_with("S-1-5-")
            || !sid
                .bytes()
                .all(|b| b.is_ascii_digit() || b"S-".contains(&b))
        {
            return Err(io::Error::other("invalid SSH entry SID"));
        }
        let descriptor: Vec<_> = format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;{sid})")
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
            Ok(Self {
                name: name.into(),
                next: Mutex::new(pipe(name, &sid, true)?),
                sid,
            })
        }
        pub async fn accept(&self) -> io::Result<(Stream, ())> {
            let mut next = self.next.lock().await;
            next.connect().await?;
            let replacement = pipe(&self.name, &self.sid, false)?;
            Ok((std::mem::replace(&mut *next, replacement), ()))
        }
    }
    pub async fn connect(path: &Path) -> io::Result<NamedPipeClient> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match ClientOptions::new().open(path) {
                Ok(pipe) => return Ok(pipe),
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
