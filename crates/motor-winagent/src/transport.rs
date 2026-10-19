//! Transport selection for the standalone Hyper-V experiment, not a runner backend.
use std::io;

pub const HYPERV_NONCE_FILE: &str = r"C:\ProgramData\Motor\hyperv-nonce.txt";

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
pub fn configure_com(handle: std::os::windows::io::BorrowedHandle<'_>) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Devices::Communication::{
        BuildCommDCBW, COMMTIMEOUTS, DCB, SetCommState, SetCommTimeouts,
    };
    let handle = handle.as_raw_handle();
    let mode: Vec<u16> =
        "baud=115200 parity=n data=8 stop=1 xon=off odsr=off octs=off dtr=off rts=off idsr=off\0"
            .encode_utf16()
            .collect();
    unsafe {
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
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
