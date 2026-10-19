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
/// listener form; it accepts any partition, so the agent refuses loopback
/// peers (`hvsock_peer_allowed`) and accepts only one connection. `Parent`
/// binds HV_GUID_PARENT to accept only the host, but Microsoft documents that
/// ID as a connect address, so it is kept as an alternative to try if the
/// wildcard bind does not work.
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

/// A job process inside the guest could reach the agent's listener over
/// loopback; only a connection from another partition (the host) is accepted.
pub fn hvsock_peer_allowed(peer_vm_id: u128) -> bool {
    peer_vm_id != HV_GUID_LOOPBACK
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
    fn loopback_peer_is_refused_and_other_partitions_are_not() {
        assert!(!hvsock_peer_allowed(HV_GUID_LOOPBACK));
        assert!(hvsock_peer_allowed(HV_GUID_PARENT));
        assert!(hvsock_peer_allowed(0x1234));
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
