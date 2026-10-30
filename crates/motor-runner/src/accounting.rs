use std::io::{self, Read};

const MAX_REPORT_BYTES: usize = 64 * 1024;

// systemd-run prints its final summary as soon as the unit stops. A stuck
// launcher must not keep the host's capacity and settlement waiting for EOF.
pub(super) const FINAL_SUMMARY_GRACE_MS: u64 = 30_000;
pub(super) const MEMORY_PEAK_SAVE_ATTEMPTS: usize = 3;

pub(super) fn read_report(mut reader: impl Read) -> (Vec<u8>, io::Result<()>) {
    let mut report = Vec::new();
    let read = reader
        .by_ref()
        .take((MAX_REPORT_BYTES + 1) as u64)
        .read_to_end(&mut report);
    let drained = io::copy(&mut reader, &mut io::sink());
    (report, read.and(drained).map(|_| ()))
}

/// Parse the launcher's final report, never the service's output or its journal.
pub(super) fn read_cpu_time_ns(mut reader: impl Read, unit: &str) -> io::Result<u64> {
    let mut report = Vec::new();
    reader
        .by_ref()
        .take((MAX_REPORT_BYTES + 1) as u64)
        .read_to_end(&mut report)?;
    if report.len() > MAX_REPORT_BYTES {
        io::copy(&mut reader, &mut io::sink())?;
        return Err(io::Error::other(
            "systemd-run accounting report exceeds 64 KiB",
        ));
    }
    parse_cpu_time_ns(&report, unit)
}

fn summary<'a>(report: &'a [u8], unit: &str) -> io::Result<Vec<&'a str>> {
    if report.len() > MAX_REPORT_BYTES {
        return Err(io::Error::other(
            "systemd-run accounting report exceeds 64 KiB",
        ));
    }
    let text = std::str::from_utf8(report).map_err(io::Error::other)?;
    let lines: Vec<_> = text.lines().map(str::trim).collect();
    let expected = format!("Running as unit: {unit}");
    // A successful invocation-ID lookup adds a suffix to the native header.
    // Match the exact launch unit, never a prefix or an arbitrary trailing tag.
    if !lines.iter().any(|line| {
        *line == expected
            || line
                .strip_prefix(expected.as_str())
                .and_then(|suffix| suffix.strip_prefix("; invocation ID: "))
                .is_some_and(|id| id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
    }) {
        return Err(io::Error::other(
            "systemd-run accounting report has no matching launch unit",
        ));
    }
    let start = lines
        .iter()
        .rposition(|line| line.starts_with("Finished with result:"))
        .ok_or_else(|| io::Error::other("systemd-run accounting report has no final result"))?;
    Ok(lines[start + 1..].to_vec())
}

fn parse_cpu_time_ns(report: &[u8], unit: &str) -> io::Result<u64> {
    let lines = summary(report, unit)?;
    let values: Vec<_> = lines
        .iter()
        .filter_map(|line| line.strip_prefix("CPU time consumed:").map(str::trim))
        .collect();
    if values.len() != 1 {
        return Err(io::Error::other(
            "systemd-run accounting report has no unique CPU total",
        ));
    }
    timespan_ns(values[0])
}

/// Only the launcher's unit-identified final summary is trusted, not job output.
pub(super) fn memory_peak_mib(report: &[u8], unit: &str) -> Option<u64> {
    let lines = summary(report, unit).ok()?;
    let mut values = lines
        .iter()
        .filter_map(|line| line.strip_prefix("Memory peak:").map(str::trim));
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let display = value.split_once(" (swap: ").map_or(value, |(peak, _)| peak);
    let (number, suffix) = display.split_at_checked(display.len().checked_sub(1)?)?;
    let scale: u128 = match suffix {
        "B" => 1,
        "K" => 1 << 10,
        "M" => 1 << 20,
        "G" => 1 << 30,
        "T" => 1 << 40,
        "P" => 1 << 50,
        "E" => 1 << 60,
        _ => return None,
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if whole.is_empty()
        || (number.contains('.') && fraction.is_empty())
        || fraction.len() > 9
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|c| c.is_ascii_digit())
        || (suffix == "B" && !fraction.is_empty())
    {
        return None;
    }
    // systemd can omit a zero fractional digit ("1G" means "1.0G"),
    // but its scaled display still has a tenth-of-a-unit interval.
    let precision = fraction.len().max(usize::from(suffix != "B"));
    let denominator = 10u128.pow(precision as u32);
    let digits = whole
        .parse::<u128>()
        .ok()?
        .checked_mul(denominator)?
        .checked_add(if fraction.is_empty() {
            0
        } else {
            fraction.parse().ok()?
        })?;
    // systemd displays a rounded memory.peak, not raw bytes. Use the top of
    // its display interval, then round up to MiB so formatting never understates
    // a job's final peak. Byte displays are already exact.
    let upper = digits
        .checked_add(u128::from(suffix != "B"))?
        .checked_mul(scale)?;
    let mib = upper.div_ceil(denominator.checked_mul(1 << 20)?);
    u64::try_from(mib).ok().filter(|n| *n < (1 << 53))
}

fn timespan_ns(value: &str) -> io::Result<u64> {
    if value == "0" {
        return Ok(0);
    }
    let invalid = || io::Error::other(format!("unrecognized systemd CPU timespan: {value:?}"));
    let mut total = 0u128;
    let mut previous_unit = u128::MAX;
    let mut any = false;
    for token in value.split_whitespace() {
        let split = token
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .ok_or_else(invalid)?;
        let (number, unit) = token.split_at(split);
        let scale: u128 = match unit {
            "w" => 604_800_000_000_000,
            "d" => 86_400_000_000_000,
            "h" => 3_600_000_000_000,
            "min" => 60_000_000_000,
            "s" => 1_000_000_000,
            "ms" => 1_000_000,
            "us" | "µs" | "μs" => 1_000,
            _ => return Err(invalid()),
        };
        if scale >= previous_unit {
            return Err(invalid());
        }
        previous_unit = scale;
        let (whole, fractional) = number.split_once('.').unwrap_or((number, ""));
        if whole.is_empty()
            || (number.contains('.') && fractional.is_empty())
            || fractional.len() > 9
            || !whole
                .chars()
                .chain(fractional.chars())
                .all(|c| c.is_ascii_digit())
        {
            return Err(invalid());
        }
        let whole: u128 = whole.parse().map_err(|_| invalid())?;
        let fraction: u128 = if fractional.is_empty() {
            0
        } else {
            fractional.parse().map_err(|_| invalid())?
        };
        let divisor = 10u128.pow(fractional.len() as u32);
        let nanos = whole
            .checked_mul(scale)
            .and_then(|n| {
                fraction
                    .checked_mul(scale)
                    .and_then(|f| n.checked_add(f / divisor))
            })
            .ok_or_else(invalid)?;
        total = total.checked_add(nanos).ok_or_else(invalid)?;
        any = true;
    }
    if !any {
        return Err(invalid());
    }
    u64::try_from(total).map_err(|_| invalid())
}

#[cfg(test)]
mod tests {
    use super::*;
    const UNIT: &str = "motor-windows-windows-prep-01a13491-9d14-731c-bf08-474191c34215.service";
    // Captured from systemd-run 259 on a cgroup-v2 host.
    const REPORT: &str = "Running as unit: motor-windows-windows-prep-01a13491-9d14-731c-bf08-474191c34215.service\n          Finished with result: exit-code\nMain processes terminated with: code=exited, status=1/FAILURE\n               Service runtime: 62ms\n             CPU time consumed: 56ms\n                   Memory peak: 16.5M (swap: 0B)\n";

    const SHORT_UNIT: &str = "motor-job-01a137aa-a8f0-71fd-a2e3-924b6d95e6e6.service";
    const INVOCATION_REPORT: &str = "Running as unit: motor-job-01a137aa-a8f0-71fd-a2e3-924b6d95e6e6.service; invocation ID: a7ad6ff018994eb4b96f81c0ca2798c8\n          Finished with result: success\nMain processes terminated with: code=exited, status=0/SUCCESS\n               Service runtime: 118ms\n             CPU time consumed: 95ms\n                   Memory peak: 4.2M (swap: 0B)\n";

    #[test]
    fn native_systemd_259_invocation_header_keeps_exact_unit_identity() {
        assert_eq!(
            read_cpu_time_ns(INVOCATION_REPORT.as_bytes(), SHORT_UNIT).unwrap(),
            95_000_000
        );
        assert!(read_cpu_time_ns(
            INVOCATION_REPORT.as_bytes(),
            "motor-job-01a137aa-a5f9-7719-8cff-074dcbc679e4.service"
        )
        .is_err());
        assert_eq!(
            memory_peak_mib(INVOCATION_REPORT.as_bytes(), SHORT_UNIT),
            Some(5)
        );
        assert_eq!(memory_peak_mib(REPORT.as_bytes(), UNIT), Some(17));
    }

    #[test]
    fn systemd_255_and_257_peak_rows_allow_separate_or_inline_swap() {
        for swap in [" (swap: 0B)", "\nMemory swap peak: 0B"] {
            let report = format!("Running as unit: {UNIT}\nFinished with result: success\nMemory peak: 16.5M{swap}\n");
            assert_eq!(memory_peak_mib(report.as_bytes(), UNIT), Some(17));
        }
    }

    #[test]
    fn final_memory_peak_rounds_display_intervals_up_for_every_suffix() {
        for (display, expected) in [
            ("0B", 0),
            ("1048576B", 1),
            ("1048577B", 2),
            ("1023.9K", 1),
            ("1024K", 2),
            ("1.0M", 2),
            ("16.5M", 17),
            ("1.0G", 1127),
            ("1G", 1127),
            ("12.3T", 13_002_343),
            ("1.0P", 1_181_116_007),
            ("1.0E", 1_209_462_790_554),
        ] {
            let report = INVOCATION_REPORT.replace("4.2M", display);
            assert_eq!(
                memory_peak_mib(report.as_bytes(), SHORT_UNIT),
                Some(expected),
                "{display}"
            );
        }
    }

    #[test]
    fn unavailable_or_untrusted_final_memory_is_not_a_live_sample_or_zero() {
        for display in [
            "",
            "n/a",
            "-1M",
            "1.2.3M",
            "1.0Q",
            "1MiB",
            ".5M",
            "1.M",
            "1.0B",
            "999999999999999999999999999999999999999999999T",
        ] {
            assert_eq!(
                memory_peak_mib(REPORT.replace("16.5M", display).as_bytes(), UNIT),
                None,
                "{display}"
            );
        }
        for report in [
            REPORT.replace("Memory peak", "Memory current"),
            REPORT.replace("Finished with result:", "Still running:"),
            format!("{REPORT}Memory peak: 7.0G\n"),
            REPORT.replace(UNIT, "another.service"),
            format!("{REPORT}{}", "x".repeat(MAX_REPORT_BYTES)),
        ] {
            assert_eq!(memory_peak_mib(report.as_bytes(), UNIT), None);
        }
        let report = INVOCATION_REPORT.replace("a7ad6ff018994eb4b96f81c0ca2798c8", "bad-id");
        assert_eq!(memory_peak_mib(report.as_bytes(), SHORT_UNIT), None);
    }

    #[test]
    fn invocation_header_rejects_wrong_units_and_malformed_identifiers() {
        for id in [
            "",
            "a7ad6ff018994eb4b96f81c0ca2798c",
            "a7ad6ff018994eb4b96f81c0ca2798c8x",
            "z7ad6ff018994eb4b96f81c0ca2798c8",
            "a7ad6ff018994eb4b96f81c0ca2798c8; forged",
        ] {
            let report = INVOCATION_REPORT.replace("a7ad6ff018994eb4b96f81c0ca2798c8", id);
            assert!(
                read_cpu_time_ns(report.as_bytes(), SHORT_UNIT).is_err(),
                "invalid invocation ID: {id:?}"
            );
        }
        let report = INVOCATION_REPORT.replace(SHORT_UNIT, &format!("{SHORT_UNIT}-forged"));
        assert!(read_cpu_time_ns(report.as_bytes(), SHORT_UNIT).is_err());
    }

    #[test]
    fn native_systemd_259_report_and_compound_timespans() {
        for (span, expected) in [
            ("56ms", 56_000_000),
            ("1min 3.214s", 63_214_000_000),
            ("2h 5min", 7_500_000_000_000),
            ("0", 0),
        ] {
            assert_eq!(
                read_cpu_time_ns(REPORT.replace("56ms", span).as_bytes(), UNIT).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn missing_unfamiliar_truncated_and_wrong_unit_reports_are_unavailable() {
        assert!(read_cpu_time_ns(REPORT.as_bytes(), "motor-job-other.service").is_err());
        for span in [
            "",
            "n/a",
            "56",
            "56fortnights",
            "-56ms",
            "1s 1h",
            "1.2.3s",
            "99999999999999999999999999999999999999999999999h",
        ] {
            assert!(
                read_cpu_time_ns(REPORT.replace("56ms", span).as_bytes(), UNIT).is_err(),
                "{span}"
            );
        }
        assert!(read_cpu_time_ns(
            REPORT
                .replace("CPU time consumed", "CPU consumption")
                .as_bytes(),
            UNIT
        )
        .is_err());
        assert!(read_cpu_time_ns(&b"x".repeat(MAX_REPORT_BYTES + 1)[..], UNIT).is_err());
        assert!(read_cpu_time_ns(&b""[..], UNIT).is_err());
    }
}
