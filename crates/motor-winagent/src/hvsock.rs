//! Hyper-V socket channel for the opt-in Hyper-V experiment.
//!
//! The guest agent listens on Motor's service ID and the host dials this VM's
//! ID. The agent accepts exactly one host connection and then closes its
//! listener, so nothing inside the guest (in particular a job process, which
//! only starts after the host is connected) can connect later. The framing and
//! protocol on top of the stream are the same as on virtio-serial.
use motor_winagent::transport::{self, ListenAddress};
use std::{
    io,
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
    sync::OnceLock,
    time::Instant,
};
use windows_sys::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0},
        Networking::WinSock::{
            AF_HYPERV, FD_SET, INVALID_SOCKET, SOCK_STREAM, SOCKADDR, SOCKET, SOCKET_ERROR,
            TIMEVAL, WSA_FLAG_OVERLAPPED, WSA_IO_PENDING, WSABUF, WSADATA, WSAGetLastError,
            WSAGetOverlappedResult, WSARecv, WSASend, WSASocketW, WSAStartup, accept, bind,
            closesocket, listen, select,
        },
        System::{
            Hypervisor::{HV_PROTOCOL_RAW, SOCKADDR_HV},
            IO::{CancelIoEx, OVERLAPPED},
            Threading::{CreateEventW, INFINITE, WaitForMultipleObjects, WaitForSingleObject},
        },
    },
    core::GUID,
};

fn wsa_error() -> io::Error {
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

fn stopped(stop: HANDLE) -> bool {
    unsafe { WaitForSingleObject(stop, 0) == WAIT_OBJECT_0 }
}

fn guid_u128(guid: GUID) -> u128 {
    (u128::from(guid.data1) << 96)
        | (u128::from(guid.data2) << 80)
        | (u128::from(guid.data3) << 64)
        | u128::from(u64::from_be_bytes(guid.data4))
}

fn startup() -> io::Result<()> {
    // Winsock is initialized once for the life of the service process.
    static STARTED: OnceLock<i32> = OnceLock::new();
    let code = *STARTED.get_or_init(|| unsafe {
        let mut data: WSADATA = zeroed();
        WSAStartup(0x0202, &mut data)
    });
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code))
    }
}

pub struct Socket(SOCKET);
// A connected socket may be read on the reader thread while the main thread
// writes; each overlapped operation owns its own event and buffer.
unsafe impl Send for Socket {}
unsafe impl Sync for Socket {}

impl Socket {
    fn new(socket: SOCKET) -> io::Result<Self> {
        if socket == INVALID_SOCKET {
            Err(wsa_error())
        } else {
            Ok(Self(socket))
        }
    }

    /// One overlapped send or receive. Returns 0 only when the host closed the
    /// connection; a signalled `stop` cancels the operation and waits for the
    /// cancellation to finish before the caller's buffer can be released.
    pub fn transfer(
        &self,
        stop: HANDLE,
        bytes: *mut u8,
        length: usize,
        write: bool,
    ) -> io::Result<usize> {
        struct Event(HANDLE);
        impl Drop for Event {
            fn drop(&mut self) {
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
        unsafe {
            let event = CreateEventW(null(), 1, 0, null());
            if event.is_null() {
                return Err(io::Error::last_os_error());
            }
            let event = Event(event);
            let mut operation: OVERLAPPED = zeroed();
            operation.hEvent = event.0;
            let buffer = WSABUF {
                len: length.min(u32::MAX as usize) as u32,
                buf: bytes,
            };
            let mut transferred = 0u32;
            let mut flags = 0u32;
            let result = if write {
                WSASend(
                    self.0,
                    &buffer,
                    1,
                    &mut transferred,
                    0,
                    &mut operation,
                    None,
                )
            } else {
                WSARecv(
                    self.0,
                    &buffer,
                    1,
                    &mut transferred,
                    &mut flags,
                    &mut operation,
                    None,
                )
            };
            if result == SOCKET_ERROR {
                let error = WSAGetLastError();
                if error != WSA_IO_PENDING {
                    return Err(io::Error::from_raw_os_error(error));
                }
                let handles = [stop, event.0];
                if WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) != WAIT_OBJECT_0 + 1 {
                    CancelIoEx(self.0 as HANDLE, &operation);
                    WSAGetOverlappedResult(self.0, &operation, &mut transferred, 1, &mut flags);
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "Hyper-V socket I/O stopped",
                    ));
                }
            }
            if WSAGetOverlappedResult(self.0, &operation, &mut transferred, 0, &mut flags) == 0 {
                return Err(wsa_error());
            }
            Ok(transferred as usize)
        }
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        unsafe {
            closesocket(self.0);
        }
    }
}

/// Listen on Motor's service ID and return the first connection from another
/// partition, with the peer's VM ID for the diagnostic log. The listener is
/// closed when this returns.
pub fn accept_host(
    stop: HANDLE,
    deadline: Instant,
    address: ListenAddress,
) -> io::Result<(Socket, u128)> {
    startup()?;
    let service = transport::parse_guid(transport::HVSOCK_SERVICE_ID)?;
    unsafe {
        let listener = Socket::new(WSASocketW(
            i32::from(AF_HYPERV),
            SOCK_STREAM,
            HV_PROTOCOL_RAW as i32,
            null(),
            0,
            WSA_FLAG_OVERLAPPED,
        ))?;
        let local = SOCKADDR_HV {
            Family: AF_HYPERV,
            Reserved: 0,
            VmId: GUID::from_u128(address.vm_id()),
            ServiceId: GUID::from_u128(service),
        };
        if bind(
            listener.0,
            (&raw const local).cast::<SOCKADDR>(),
            size_of::<SOCKADDR_HV>() as i32,
        ) == SOCKET_ERROR
        {
            return Err(wsa_error());
        }
        if listen(listener.0, 1) == SOCKET_ERROR {
            return Err(wsa_error());
        }
        crate::hyperv_diag::event(&format!(
            "hvsock listening vm_id={:032x} service_id={:032x}",
            address.vm_id(),
            service
        ));
        loop {
            if stopped(stop) {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "service stopped",
                ));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "no host connected to the Hyper-V socket",
                ));
            }
            // Wait in short slices so a service stop is noticed promptly.
            let mut ready: FD_SET = zeroed();
            ready.fd_count = 1;
            ready.fd_array[0] = listener.0;
            let slice = TIMEVAL {
                tv_sec: 0,
                tv_usec: 250_000,
            };
            match select(0, &mut ready, null_mut(), null_mut(), &slice) {
                SOCKET_ERROR => return Err(wsa_error()),
                0 => continue,
                _ => {}
            }
            let mut peer: SOCKADDR_HV = zeroed();
            let mut peer_length = size_of::<SOCKADDR_HV>() as i32;
            let connection = Socket::new(accept(
                listener.0,
                (&raw mut peer).cast::<SOCKADDR>(),
                &mut peer_length,
            ))?;
            let peer_id = guid_u128(peer.VmId);
            if !transport::hvsock_peer_allowed(peer_id) {
                crate::hyperv_diag::event(&format!("hvsock refused peer vm_id={peer_id:032x}"));
                continue;
            }
            return Ok((connection, peer_id));
        }
    }
}
