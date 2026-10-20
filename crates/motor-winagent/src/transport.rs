//! Transport selection for the standalone Hyper-V experiment, not a runner backend.
use std::io;

pub const HYPERV_NONCE_FILE: &str = r"C:\ProgramData\Motor\hyperv-nonce.txt";
pub const HYPERV_QUEUE_FILE: &str = r"C:\ProgramData\Motor\hyperv-serial-queue.txt";
pub const HYPERV_QUEUE_BYTES: u32 = 2 * crate::MAX_FRAME_BYTES as u32;

pub fn read_serial(
    bytes: &mut [u8],
    mut operation: impl FnMut(&mut [u8]) -> io::Result<usize>,
) -> io::Result<usize> {
    if bytes.is_empty() {
        return Ok(0);
    }
    let length = bytes.len().min(4096);
    loop {
        // Serial timeouts return zero without closing the stream. The callback
        // must continue to honor service cancellation and propagate I/O errors.
        let count = operation(&mut bytes[..length])?;
        if count != 0 {
            return Ok(count);
        }
    }
}

/// Hyper-V socket service ID that Motor's agent listens on. The host must
/// register this GUID under GuestCommunicationServices before it can connect.
pub const HVSOCK_SERVICE_ID: &str = "92c268b0-cddd-40a4-a664-f32e50ad35ea";
/// Optional overlay file choosing the guest listener's VM ID: `wildcard`
/// (the default when absent) or `parent`.
pub const HVSOCK_LISTEN_FILE: &str = r"C:\ProgramData\Motor\hyperv-listen.txt";
/// HV_GUID_PARENT: names the parent partition (the Hyper-V host).
pub const HV_GUID_PARENT: u128 = 0xa42e7cda_d03f_480c_9cc2_a4de20abb878;
/// HV_GUID_LOOPBACK: a connection from inside the same partition.
pub const HV_GUID_LOOPBACK: u128 = 0xe0e16197_dd56_4a10_9195_5ee7a155a838;
/// How long one host connect attempt may wait. A Hyper-V socket connect issued
/// before the guest listens does not complete when the listener appears later;
/// it waits out its timeout (30 s by default), so the host retries short
/// attempts until its overall deadline instead.
pub const HVSOCK_CONNECT_ATTEMPT_MS: u32 = 250;
/// WSAETIMEDOUT: a connect attempt used up its whole timeout.
const WSAETIMEDOUT: i32 = 10060;

/// Pause before the next host connect attempt. An attempt that timed out has
/// already waited, so the next one starts at once; any other failure returns
/// quickly and gets a short pause so the loop does not spin.
pub fn hvsock_retry_pause_ms(os_code: i32) -> u64 {
    if os_code == WSAETIMEDOUT { 0 } else { 100 }
}

/// Parse a textual GUID (`xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`, optional
/// braces, any case) into the integer form `windows_sys::core::GUID::from_u128`
/// takes, whose fields map onto the textual groups in order.
pub fn parse_guid(text: &str) -> io::Result<u128> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid GUID");
    let text = text.trim();
    let text = match text.strip_prefix('{') {
        Some(rest) => rest.strip_suffix('}').ok_or_else(invalid)?,
        None => text,
    };
    let groups: Vec<&str> = text.split('-').collect();
    if groups.iter().map(|group| group.len()).collect::<Vec<_>>() != [8, 4, 4, 4, 12]
        || !groups
            .iter()
            .all(|group| group.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(invalid());
    }
    u128::from_str_radix(&groups.concat(), 16).map_err(|_| invalid())
}

/// Which VM ID the guest agent's listener binds. `Wildcard` is the documented
/// listener form; it accepts any partition, so `accept_parent` refuses every
/// peer except the parent partition. `Parent` binds HV_GUID_PARENT, but
/// Microsoft documents that ID as a connect address, so it is kept as an
/// alternative to try if the wildcard bind does not work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListenAddress {
    Wildcard,
    Parent,
}

impl ListenAddress {
    pub fn vm_id(self) -> u128 {
        match self {
            Self::Wildcard => 0,
            Self::Parent => HV_GUID_PARENT,
        }
    }
}

pub fn hvsock_listen_address(text: Option<&str>) -> io::Result<ListenAddress> {
    match text.map(str::trim) {
        None | Some("wildcard") => Ok(ListenAddress::Wildcard),
        Some("parent") => Ok(ListenAddress::Parent),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Hyper-V listen address must be wildcard or parent",
        )),
    }
}

/// The agent talks only to the Hyper-V host. Its connection arrives with the
/// parent partition's VM ID; a process in this guest (loopback) or in another
/// VM shows a different ID and must never reach the protocol.
pub fn hvsock_peer_allowed(peer_vm_id: u128) -> bool {
    peer_vm_id == HV_GUID_PARENT
}

/// Accept connections until one comes from the parent partition and return
/// it with its VM ID. `next` yields `None` when no connection arrived in its
/// wait slice and is responsible for stop and deadline errors. Every other
/// peer's connection is closed (dropped) and reported through `refused`.
pub fn accept_parent<C>(
    mut next: impl FnMut() -> io::Result<Option<(C, u128)>>,
    mut refused: impl FnMut(u128),
) -> io::Result<(C, u128)> {
    loop {
        match next()? {
            None => {}
            Some((connection, peer)) if hvsock_peer_allowed(peer) => return Ok((connection, peer)),
            Some((connection, peer)) => {
                drop(connection);
                refused(peer);
            }
        }
    }
}

pub fn hyperv_queue_bytes(text: Option<&str>) -> io::Result<u32> {
    match text.map(str::trim) {
        None => Ok(HYPERV_QUEUE_BYTES),
        Some("0") => Ok(0),
        Some("2097152") => Ok(HYPERV_QUEUE_BYTES),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Hyper-V serial queue must be 0 (diagnostic control) or 2097152",
        )),
    }
}

pub fn port_path() -> String {
    if cfg!(feature = "hyperv-spike") {
        r"\\.\COM1".to_owned()
    } else {
        format!(r"\\.\Global\{}", crate::VIRTIO_PORT_NAME)
    }
}

pub fn hyperv_nonce(text: &str) -> io::Result<String> {
    // The host writes exactly one fresh UUID into the disposable OS overlay.
    if text.len() != 32 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid Hyper-V nonce",
        ));
    }
    Ok(text.to_owned())
}

#[cfg(all(windows, feature = "hyperv-spike"))]
pub fn configure_com(handle: std::os::windows::io::BorrowedHandle<'_>) -> io::Result<String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Devices::Communication::{
        BuildCommDCBW, COMMPROP, COMMTIMEOUTS, DCB, GetCommProperties, SetCommState,
        SetCommTimeouts, SetupComm,
    };
    let handle = handle.as_raw_handle();
    let text = match std::fs::read_to_string(HYPERV_QUEUE_FILE) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let queue = hyperv_queue_bytes(text.as_deref())?;
    let mode: Vec<u16> =
        "baud=115200 parity=n data=8 stop=1 xon=off odsr=off octs=off dtr=off rts=off idsr=off\0"
            .encode_utf16()
            .collect();
    unsafe {
        // Request room for a complete bounded frame before touching the UART.
        // Zero preserves driver defaults only for a controlled diagnostic run.
        if queue != 0 && SetupComm(handle, queue, queue) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut dcb: DCB = std::mem::zeroed();
        dcb.DCBlength = std::mem::size_of::<DCB>() as u32;
        if BuildCommDCBW(mode.as_ptr(), &mut dcb) == 0 || SetCommState(handle, &dcb) == 0 {
            return Err(io::Error::last_os_error());
        }
        // Return partial serial input so a missing tail cannot hide all received
        // bytes inside one indefinitely pending ReadFile. A zero-byte timeout
        // is retried by Port::read, not exposed to framing as an EOF.
        let timeouts = COMMTIMEOUTS {
            ReadIntervalTimeout: 50,
            ReadTotalTimeoutMultiplier: 0,
            ReadTotalTimeoutConstant: 250,
            WriteTotalTimeoutMultiplier: 0,
            WriteTotalTimeoutConstant: 0,
        };
        if SetCommTimeouts(handle, &timeouts) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut properties: COMMPROP = std::mem::zeroed();
        properties.wPacketLength = std::mem::size_of::<COMMPROP>() as u16;
        if GetCommProperties(handle, &mut properties) == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(format!(
            "requested_queue={queue} driver_rx_queue={} driver_tx_queue={} max_rx_queue={}",
            properties.dwCurrentRxQueue, properties.dwCurrentTxQueue, properties.dwMaxRxQueue
        ))
    }
}

#[cfg(all(windows, feature = "hyperv-spike"))]
pub fn serial_status(
    handle: std::os::windows::io::BorrowedHandle<'_>,
) -> io::Result<(u32, u32, u32)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Devices::Communication::{COMSTAT, ClearCommError};
    unsafe {
        let mut errors = 0;
        let mut status: COMSTAT = std::mem::zeroed();
        if ClearCommError(handle.as_raw_handle(), &mut errors, &mut status) == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((errors, status.cbInQue, status.cbOutQue))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_text_maps_onto_from_u128_field_order() {
        // The published HV_GUID_PARENT text and its from_u128 constant must agree.
        assert_eq!(
            parse_guid("a42e7cda-d03f-480c-9cc2-a4de20abb878").unwrap(),
            HV_GUID_PARENT
        );
        assert_eq!(
            parse_guid("{E0E16197-DD56-4A10-9195-5EE7A155A838}").unwrap(),
            HV_GUID_LOOPBACK
        );
        assert_eq!(
            parse_guid(HVSOCK_SERVICE_ID).unwrap(),
            0x92c268b0_cddd_40a4_a664_f32e50ad35ea
        );
        for bad in [
            "",
            "a42e7cda-d03f-480c-9cc2",
            "a42e7cdad03f480c9cc2a4de20abb878",
            "a42e7cda-d03f-480c-9cc2-a4de20abb87g",
            "{a42e7cda-d03f-480c-9cc2-a4de20abb878",
            "a42e7cda-d03f-480c-9cc2-a4de20abb8789",
            "+42e7cda-d03f-480c-9cc2-a4de20abb878",
        ] {
            assert!(parse_guid(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn listen_address_defaults_to_wildcard_and_refuses_unknown_values() {
        assert_eq!(
            hvsock_listen_address(None).unwrap(),
            ListenAddress::Wildcard
        );
        assert_eq!(
            hvsock_listen_address(Some("wildcard\r\n")).unwrap(),
            ListenAddress::Wildcard
        );
        assert_eq!(
            hvsock_listen_address(Some("parent")).unwrap(),
            ListenAddress::Parent
        );
        assert_eq!(ListenAddress::Wildcard.vm_id(), 0);
        assert_eq!(ListenAddress::Parent.vm_id(), HV_GUID_PARENT);
        for bad in ["", "loopback", "Parent", "children"] {
            assert!(hvsock_listen_address(Some(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn only_the_parent_partition_reaches_the_agent() {
        // Track whether each refused connection was closed before the next accept.
        struct Connection<'a>(u128, &'a std::cell::RefCell<Vec<u128>>);
        impl Drop for Connection<'_> {
            fn drop(&mut self) {
                self.1.borrow_mut().push(self.0);
            }
        }
        let closed = std::cell::RefCell::new(Vec::new());
        let other_vm = 0x4ae3c7f2_1d39_4c6b_9e58_0123456789ab;
        let children = 0x90db8b89_0d35_4f79_8ce9_49ea0ac8b7cd;
        let mut arrivals = vec![
            None,
            Some(HV_GUID_LOOPBACK),
            Some(other_vm),
            None,
            Some(0),
            Some(children),
            Some(HV_GUID_PARENT),
            Some(other_vm),
        ]
        .into_iter();
        let mut refused = Vec::new();
        let (connection, peer) = accept_parent(
            || {
                Ok(arrivals
                    .next()
                    .unwrap()
                    .map(|peer| (Connection(peer, &closed), peer)))
            },
            |peer| refused.push(peer),
        )
        .unwrap();
        assert_eq!(peer, HV_GUID_PARENT);
        assert_eq!(connection.0, HV_GUID_PARENT);
        assert_eq!(refused, [HV_GUID_LOOPBACK, other_vm, 0, children]);
        assert_eq!(*closed.borrow(), refused, "every refused peer was closed");
        // Accepting stops at the parent; later arrivals are never taken.
        assert_eq!(arrivals.next(), Some(Some(other_vm)));
        assert!(!hvsock_peer_allowed(HV_GUID_LOOPBACK));
        assert!(!hvsock_peer_allowed(other_vm));
        assert!(hvsock_peer_allowed(HV_GUID_PARENT));
    }

    #[test]
    fn accept_errors_end_the_wait_without_admitting_anyone() {
        let mut calls = 0;
        let error = accept_parent::<()>(
            || {
                calls += 1;
                if calls == 1 {
                    Ok(Some(((), HV_GUID_LOOPBACK)))
                } else {
                    Err(io::Error::new(io::ErrorKind::TimedOut, "deadline"))
                }
            },
            |_| {},
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn timed_out_connects_retry_at_once_and_fast_failures_pause() {
        assert_eq!(hvsock_retry_pause_ms(10060), 0);
        assert_eq!(hvsock_retry_pause_ms(10061), 100);
        assert_eq!(hvsock_retry_pause_ms(0), 100);
    }

    #[test]
    fn serial_read_retries_timeouts_and_bounds_each_operation() {
        let mut bytes = vec![0; 10000];
        let mut calls = 0;
        let count = read_serial(&mut bytes, |buffer| {
            assert_eq!(buffer.len(), 4096);
            calls += 1;
            if calls < 3 {
                return Ok(0);
            }
            buffer[..4].copy_from_slice(&[1, 2, 3, 4]);
            Ok(4)
        })
        .unwrap();
        assert_eq!((count, calls), (4, 3));
        assert_eq!(&bytes[..4], &[1, 2, 3, 4]);
        assert_eq!(
            read_serial(&mut [], |_| panic!("empty read called transport")).unwrap(),
            0
        );
    }

    #[test]
    fn serial_read_cancellation_is_not_hidden_by_timeout_retry() {
        let mut calls = 0;
        let error = read_serial(&mut [0; 4], |_| {
            calls += 1;
            if calls == 1 {
                Ok(0)
            } else {
                Err(io::Error::new(io::ErrorKind::ConnectionAborted, "stopped"))
            }
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
        assert_eq!(calls, 2);
    }

    #[test]
    fn serial_queue_config_is_bounded_and_default_is_larger_than_one_frame() {
        assert_eq!(hyperv_queue_bytes(None).unwrap(), 2097152);
        assert_eq!(hyperv_queue_bytes(Some("0")).unwrap(), 0);
        assert_eq!(hyperv_queue_bytes(Some("2097152\n")).unwrap(), 2097152);
        for bad in ["", "1", "-1", "0\n1", "4294967296", "unlimited"] {
            assert!(hyperv_queue_bytes(Some(bad)).is_err());
        }
    }

    #[test]
    fn transport_is_explicitly_build_selected() {
        let expected = if cfg!(feature = "hyperv-spike") {
            r"\\.\COM1"
        } else {
            r"\\.\Global\org.cortexkit.motor.winagent.0"
        };
        assert_eq!(port_path(), expected);
    }

    #[test]
    fn hyperv_nonce_rejects_empty_multiline_bom_and_non_uuid_input() {
        assert_eq!(
            hyperv_nonce("0123456789abcdef0123456789abcdef")
                .unwrap()
                .len(),
            32
        );
        for bad in [
            "",
            "x",
            "0123456789abcdef0123456789abcdef\n",
            "\u{feff}0123456789abcdef0123456789abcdef",
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
        ] {
            assert!(hyperv_nonce(bad).is_err(), "{bad:?}");
        }
    }
}
