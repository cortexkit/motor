//! Transport selection for the standalone Hyper-V experiment, not a runner backend.
use std::io;

pub const HYPERV_NONCE_FILE: &str = r"C:\ProgramData\Motor\hyperv-nonce.txt";
pub const HYPERV_QUEUE_FILE: &str = r"C:\ProgramData\Motor\hyperv-serial-queue.txt";
pub const HYPERV_QUEUE_BYTES: u32 = 2 * crate::MAX_FRAME_BYTES as u32;

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
        // All-zero timeouts block until the requested bytes arrive. The existing
        // overlapped stop event still cancels reads when the service is stopped.
        let timeouts: COMMTIMEOUTS = std::mem::zeroed();
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
