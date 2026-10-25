//! Host-only dialer. The runner never listens on a Hyper-V socket.
use std::{io, mem::size_of, sync::OnceLock};
use windows_sys::{
    core::GUID,
    Win32::{
        Networking::WinSock::{
            closesocket, connect, ioctlsocket, recv, send, setsockopt, socket, WSAGetLastError,
            WSAStartup, AF_HYPERV, FIONBIO, INVALID_SOCKET, SOCKADDR, SOCKET, SOCKET_ERROR,
            SOCK_STREAM, WSADATA,
        },
        System::Hypervisor::{HVSOCKET_CONNECT_TIMEOUT, HV_PROTOCOL_RAW, SOCKADDR_HV},
    },
};

pub(super) struct Stream(SOCKET);
impl Drop for Stream {
    fn drop(&mut self) {
        // Full connection teardown, not shutdown(SD_SEND) followed by a read
        // drain. The Hyper-V provider can terminate the peer read with a reset
        // or abort error instead of a zero-byte EOF.
        unsafe { closesocket(self.0) };
    }
}
fn error() -> io::Error {
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}
impl Stream {
    /// One bounded attempt; the caller owns the boot deadline and cancellation.
    pub(super) fn connect(vm: uuid::Uuid) -> io::Result<Self> {
        static WINSOCK: OnceLock<i32> = OnceLock::new();
        let code = *WINSOCK.get_or_init(|| unsafe {
            let mut data: WSADATA = std::mem::zeroed();
            WSAStartup(0x0202, &mut data)
        });
        if code != 0 {
            return Err(io::Error::from_raw_os_error(code));
        }
        let raw = unsafe { socket(AF_HYPERV.into(), SOCK_STREAM, HV_PROTOCOL_RAW as i32) };
        if raw == INVALID_SOCKET {
            return Err(error());
        }
        let stream = Self(raw);
        let attempt = motor_winagent::transport::HVSOCK_CONNECT_ATTEMPT_MS;
        let service =
            motor_winagent::transport::parse_guid(motor_winagent::transport::HVSOCK_SERVICE_ID)?;
        let address = SOCKADDR_HV {
            Family: AF_HYPERV,
            Reserved: 0,
            VmId: GUID::from_u128(vm.as_u128()),
            ServiceId: GUID::from_u128(service),
        };
        unsafe {
            if setsockopt(
                raw,
                HV_PROTOCOL_RAW as i32,
                HVSOCKET_CONNECT_TIMEOUT as i32,
                (&raw const attempt).cast(),
                size_of::<u32>() as i32,
            ) == SOCKET_ERROR
            {
                return Err(error());
            }
            if connect(
                raw,
                (&raw const address).cast::<SOCKADDR>(),
                size_of::<SOCKADDR_HV>() as i32,
            ) == SOCKET_ERROR
            {
                return Err(error());
            }
        }
        Ok(stream)
    }
    pub(super) fn set_nonblocking(&self, enabled: bool) -> io::Result<()> {
        let mut value = u32::from(enabled);
        if unsafe { ioctlsocket(self.0, FIONBIO, &mut value) } == SOCKET_ERROR {
            Err(error())
        } else {
            Ok(())
        }
    }
}
impl io::Read for Stream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match unsafe {
            recv(
                self.0,
                bytes.as_mut_ptr(),
                bytes.len().min(i32::MAX as usize) as i32,
                0,
            )
        } {
            SOCKET_ERROR => Err(error()),
            n => Ok(n as usize),
        }
    }
}
impl io::Write for Stream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match unsafe {
            send(
                self.0,
                bytes.as_ptr(),
                bytes.len().min(i32::MAX as usize) as i32,
                0,
            )
        } {
            SOCKET_ERROR => Err(error()),
            n => Ok(n as usize),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Qualification startup task: contacts only targets on the parent's read-only
/// nonce DVD. The refusal must also appear in the victim's
/// C:\ProgramData\Motor\hyperv-agent.log; a failed connect alone is not proof.
pub(super) fn probe_peers() -> io::Result<()> {
    use io::{Read, Write};
    use std::time::{Duration, Instant};
    let media_deadline = Instant::now() + Duration::from_secs(20);
    let media = loop {
        if let Some(media) = motor_winagent::transport::nonce_dvd()? {
            break media;
        }
        if Instant::now() >= media_deadline {
            return Err(io::Error::other("nonce DVD did not appear"));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let input = media.join("peer-targets.json");
    if !input.exists() {
        return Ok(());
    }
    let targets: Vec<uuid::Uuid> =
        serde_json::from_slice(&std::fs::read(&input)?).map_err(io::Error::other)?;
    let mut results = Vec::new();
    for target in targets {
        let deadline = Instant::now() + Duration::from_secs(12);
        let mut reached = false;
        while Instant::now() < deadline {
            match Stream::connect(target) {
                Ok(mut stream) => {
                    reached = true;
                    stream.set_nonblocking(true)?;
                    let read_deadline = Instant::now() + Duration::from_secs(2);
                    loop {
                        match stream.read(&mut [0u8; 1]) {
                            Ok(0) => break,
                            Ok(_) => {
                                return Err(io::Error::other("untrusted peer received agent bytes"))
                            }
                            Err(error)
                                if error.kind() == io::ErrorKind::WouldBlock
                                    && Instant::now() < read_deadline =>
                            {
                                std::thread::sleep(Duration::from_millis(25))
                            }
                            Err(error) if matches!(error.raw_os_error(), Some(10054 | 10053)) => {
                                break
                            }
                            Err(error) => return Err(error),
                        }
                    }
                    break;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        results.push(serde_json::json!({"target":target,"transport_connected":reached}));
    }
    let bytes = serde_json::to_vec_pretty(&results).map_err(io::Error::other)?;
    let mut proof = std::fs::File::create(r"C:\ProgramData\Motor\peer-proof.json")?;
    proof.write_all(&bytes)?;
    proof.sync_all()
}
