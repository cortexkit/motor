//! Preserve fractional source mtimes without trusting unchecked PAX values.
use crate::invalid;
use std::{
    io::{self, Read},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub fn modified<R: Read>(entry: &mut tar::Entry<'_, R>) -> io::Result<SystemTime> {
    let mut timestamp = None;
    if let Some(extensions) = entry.pax_extensions()? {
        for extension in extensions {
            let extension = extension?;
            if extension.key_bytes() == b"mtime" {
                if timestamp.is_some() {
                    return Err(invalid("duplicate PAX mtime"));
                }
                let value = std::str::from_utf8(extension.value_bytes())
                    .map_err(|_| invalid("PAX mtime is not UTF-8"))?;
                let (seconds, fraction) = value.split_once('.').unwrap_or((value, ""));
                if seconds.is_empty()
                    || !seconds.bytes().all(|b| b.is_ascii_digit())
                    || fraction.len() > 9
                    || !fraction.bytes().all(|b| b.is_ascii_digit())
                    || (value.contains('.') && fraction.is_empty())
                {
                    return Err(invalid("malformed PAX mtime"));
                }
                let seconds: u64 = seconds
                    .parse()
                    .map_err(|_| invalid("PAX mtime seconds out of range"))?;
                let nanos: u32 = if fraction.is_empty() {
                    0
                } else {
                    fraction
                        .parse::<u32>()
                        .map_err(|_| invalid("PAX mtime fraction out of range"))?
                        * 10u32.pow(9 - fraction.len() as u32)
                };
                timestamp = Some(Duration::new(seconds, nanos));
            }
        }
    }
    let duration = timestamp.unwrap_or(Duration::from_secs(entry.header().mtime()?));
    // NTFS uses signed 100 ns ticks since 1601. Validate on every host so an
    // archive accepted on Linux cannot overflow when materialized on Windows.
    let ticks = duration.as_nanos() / 100 + 116_444_736_000_000_000;
    if ticks > i64::MAX as u128 {
        return Err(invalid("source timestamp out of file time range"));
    }
    UNIX_EPOCH
        .checked_add(duration)
        .ok_or_else(|| invalid("source timestamp out of range"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn archive(values: &[&str]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        builder
            .append_pax_extensions(values.iter().map(|v| ("mtime", v.as_bytes())))
            .unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_size(1);
        header.set_mode(0o644);
        header.set_mtime(1234);
        header.set_cksum();
        builder
            .append_data(&mut header, "m/work/file", &b"x"[..])
            .unwrap();
        builder.into_inner().unwrap()
    }
    #[test]
    fn fractional_mtime_and_header_fallback_round_trip() {
        for (values, expected) in [
            (
                vec!["1800000000.123456700"],
                Duration::new(1_800_000_000, 123_456_700),
            ),
            (vec![], Duration::from_secs(1234)),
        ] {
            let bytes = archive(&values);
            crate::validate_tree(&mut io::Cursor::new(&bytes)).unwrap();
            let mut tar = tar::Archive::new(&bytes[..]);
            let mut entry = tar.entries().unwrap().next().unwrap().unwrap();
            assert_eq!(
                modified(&mut entry)
                    .unwrap()
                    .duration_since(UNIX_EPOCH)
                    .unwrap(),
                expected
            );
        }
    }
    #[test]
    fn malformed_duplicate_and_out_of_range_pax_mtimes_are_refused() {
        for values in [
            vec!["-1"],
            vec!["1."],
            vec![".1"],
            vec!["1.0000000000"],
            vec!["1e3"],
            vec!["1.2.3"],
            vec!["18446744073709551616"],
            vec!["999999999999"],
            vec!["1", "2"],
        ] {
            assert!(
                crate::validate_tree(&mut io::Cursor::new(archive(&values))).is_err(),
                "{values:?}"
            );
        }
    }
}
