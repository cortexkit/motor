//! Preserve unsigned process statuses, including loader failures before main.

pub(super) fn describe(code: u32) -> String {
    let name = match code {
        0 => " STATUS_SUCCESS",
        0xc000_0142 => " STATUS_DLL_INIT_FAILED",
        0xc000_0135 => " STATUS_DLL_NOT_FOUND",
        0xc000_0139 => " STATUS_ENTRYPOINT_NOT_FOUND",
        0xc000_0005 => " STATUS_ACCESS_VIOLATION",
        0xc000_0022 => " STATUS_ACCESS_DENIED",
        _ => "",
    };
    format!("0x{code:08X}{name} ({code})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loader_failure_is_unsigned_hex_and_named() {
        assert_eq!(
            describe(3_221_225_794),
            "0xC0000142 STATUS_DLL_INIT_FAILED (3221225794)"
        );
    }

    #[test]
    fn ordinary_exit_codes_are_not_mislabelled_as_loader_failures() {
        assert_eq!(describe(0), "0x00000000 STATUS_SUCCESS (0)");
        assert_eq!(describe(1), "0x00000001 (1)");
    }
}
