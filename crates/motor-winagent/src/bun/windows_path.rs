//! Extended-length names for Bun's raw Win32 filesystem calls.
use std::io;

/// Convert an absolute Windows path, supplied as UTF-16, to a NUL-terminated name.
/// Raw Win32 calls need the extended-length prefix even when std::fs can already
/// access the same file. Taking code units preserves non-Unicode Windows names
/// and keeps the conversion testable on other platforms.
pub fn extended_length(path: impl IntoIterator<Item = u16>) -> io::Result<Vec<u16>> {
    let mut path: Vec<u16> = path.into_iter().collect();
    if path.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "NUL in Windows path",
        ));
    }
    let prefix: Vec<u16> = r"\\?\".encode_utf16().collect();
    if !path.starts_with(&prefix) {
        // Verbatim names do not perform Win32's usual slash conversion.
        for unit in &mut path {
            if *unit == u16::from(b'/') {
                *unit = u16::from(b'\\');
            }
        }
        let (prefix, skip) = if path.len() >= 3
            && matches!(path[0], 65..=90 | 97..=122)
            && path[1..3] == [u16::from(b':'), u16::from(b'\\')]
        {
            (r"\\?\", 0)
        } else if path.starts_with(&[u16::from(b'\\'); 2])
            && path[2..]
                .split(|unit| *unit == u16::from(b'\\'))
                .take(2)
                .count()
                == 2
            && path[2..]
                .split(|unit| *unit == u16::from(b'\\'))
                .take(2)
                .all(|part| !part.is_empty() && part != [u16::from(b'.')])
        {
            (r"\\?\UNC\", 2)
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected an absolute Windows path",
            ));
        };
        path = prefix
            .encode_utf16()
            .chain(path.into_iter().skip(skip))
            .collect();
    }
    path.push(0);
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_path() {
        assert_eq!(
            extended_length(r"C:\cache\blob".encode_utf16()).unwrap(),
            "\\\\?\\C:\\cache\\blob\0"
                .encode_utf16()
                .collect::<Vec<_>>()
        );
        assert_eq!(
            extended_length("c:/cache/blob".encode_utf16()).unwrap(),
            "\\\\?\\c:\\cache\\blob\0"
                .encode_utf16()
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn unc_path() {
        assert_eq!(
            extended_length(r"\\server\share\blob".encode_utf16()).unwrap(),
            "\\\\?\\UNC\\server\\share\\blob\0"
                .encode_utf16()
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn already_prefixed_path() {
        for path in [r"\\?\C:\cache\blob", r"\\?\UNC\server\share\blob"] {
            assert_eq!(
                extended_length(path.encode_utf16()).unwrap(),
                path.encode_utf16().chain(Some(0)).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn refuses_relative_paths() {
        for path in [
            "",
            "cache",
            r"cache\blob",
            r"C:cache\blob",
            r"\cache\blob",
            r"\\server",
        ] {
            assert_eq!(
                extended_length(path.encode_utf16()).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "{path}"
            );
        }
    }

    #[test]
    fn preserves_non_unicode_code_units() {
        let path = [u16::from(b'C'), u16::from(b':'), u16::from(b'\\'), 0xd800];
        assert_eq!(
            extended_length(path).unwrap(),
            r"\\?\"
                .encode_utf16()
                .chain(path)
                .chain(Some(0))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn refuses_embedded_nul() {
        assert_eq!(
            extended_length("C:\\blob\0suffix".encode_utf16())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
