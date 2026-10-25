//! Format the identity-checked new cache volume through the Windows storage provider.

#![cfg(any(windows, test))]

use std::{io, time::Instant};

#[cfg(windows)]
mod wmi;

#[cfg(windows)]
pub fn hyperv_disk_inventory(deadline: Instant) -> io::Result<Vec<crate::cache::HypervDisk>> {
    wmi::hyperv_disk_inventory(deadline)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Volume {
    path: String,
    object_path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FormatOptions {
    filesystem: &'static str,
    label: &'static str,
    allocation_unit_size: u32,
    full: bool,
    force: bool,
}

impl Default for FormatOptions {
    fn default() -> Self {
        Self {
            filesystem: "NTFS",
            label: crate::CACHE_LABEL,
            // NTFS's default cluster size for volumes up to 16 TiB. The Storage
            // provider rejects 0 as "invalid cluster size" (ReturnValue 43000)
            // instead of treating it as "choose the default", so it is explicit.
            allocation_unit_size: 4096,
            full: false,
            force: true,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum FormatValue {
    Text(&'static str),
    Number(u32),
    Boolean(bool),
}

impl FormatOptions {
    fn parameters(&self) -> [(&'static str, FormatValue); 5] {
        [
            ("FileSystem", FormatValue::Text(self.filesystem)),
            ("FileSystemLabel", FormatValue::Text(self.label)),
            (
                "AllocationUnitSize",
                FormatValue::Number(self.allocation_unit_size),
            ),
            ("Full", FormatValue::Boolean(self.full)),
            ("Force", FormatValue::Boolean(self.force)),
        ]
    }
}

// Share constructor dispatch with a portable Put recorder, including both integer
// constructors so it can distinguish VT_I4 from VT_UI4 without a COM runtime.
// Format's string inputs use VT_BSTR and its booleans use VT_BOOL (-1/0):
// https://learn.microsoft.com/en-us/windows/win32/wmisdk/strings
// https://learn.microsoft.com/en-us/windows/win32/wmisdk/boolean
fn put_format_parameters<V>(
    options: &FormatOptions,
    mut put: impl FnMut(&str, &V) -> io::Result<()>,
) -> io::Result<()>
where
    V: From<&'static str> + From<u32> + From<i32> + From<bool>,
{
    for (name, value) in options.parameters() {
        let value = match value {
            FormatValue::Text(value) => V::from(value),
            // WMI carries CIM_UINT32 as VT_I4, not VT_UI4. Preserve all 32 bits.
            // https://learn.microsoft.com/en-us/windows/win32/wmisdk/numbers
            FormatValue::Number(value) => V::from(value as i32),
            FormatValue::Boolean(value) => V::from(value),
        };
        put(name, &value).map_err(|error| {
            io::Error::other(format!("cache WMI set Format parameter {name}: {error}"))
        })?;
    }
    Ok(())
}

struct FormatResult {
    return_value: u32,
    extended_status: String,
}

/// The provider operations are replaceable so selection, parameters and deadlines
/// can be tested without access to Windows disks.
trait StorageProvider {
    fn volumes(&mut self, deadline: Instant) -> io::Result<Vec<Volume>>;
    fn start(
        &mut self,
        object_path: &str,
        options: &FormatOptions,
        deadline: Instant,
    ) -> io::Result<()>;
    fn poll(&mut self, timeout_ms: i32) -> io::Result<Option<FormatResult>>;
}

fn remaining_ms(deadline: Instant) -> io::Result<i32> {
    remaining_ms_at(deadline, Instant::now())
}

fn remaining_ms_at(deadline: Instant, now: Instant) -> io::Result<i32> {
    let remaining = deadline
        .checked_duration_since(now)
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "cache storage format timed out"))?;
    Ok(remaining.as_millis().clamp(1, 100) as i32)
}

fn validate_volume(volume: &str) -> io::Result<()> {
    if !crate::cache::is_volume_guid_path(volume) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid cache volume identity for format",
        ));
    }
    Ok(())
}

fn format_with(
    provider: &mut impl StorageProvider,
    volume: &str,
    deadline: Instant,
) -> io::Result<()> {
    format_with_clock(provider, volume, deadline, Instant::now)
}

fn format_with_clock(
    provider: &mut impl StorageProvider,
    volume: &str,
    deadline: Instant,
    mut now: impl FnMut() -> Instant,
) -> io::Result<()> {
    validate_volume(volume)?;
    remaining_ms_at(deadline, now())?;
    let volumes = provider.volumes(deadline)?;
    let mut matching = volumes
        .iter()
        .filter(|candidate| candidate.path.eq_ignore_ascii_case(volume));
    let selected = matching
        .next()
        .ok_or_else(|| io::Error::other("cache volume absent from MSFT_Volume"))?;
    if selected.object_path.is_empty() || matching.next().is_some() {
        return Err(io::Error::other(
            "cache volume has no unique WMI object identity",
        ));
    }
    remaining_ms_at(deadline, now())?;
    provider.start(&selected.object_path, &FormatOptions::default(), deadline)?;
    loop {
        let result = provider.poll(remaining_ms_at(deadline, now())?)?;
        // A completion observed after the deadline is still ambiguous. The host
        // must discard the disk, even if the provider eventually finishes it.
        remaining_ms_at(deadline, now())?;
        if let Some(result) = result {
            return format_result(result);
        }
    }
}

fn format_result(result: FormatResult) -> io::Result<()> {
    if result.return_value == 0 {
        return Ok(());
    }
    let (kind, description) = match result.return_value {
        1 | 7 => (io::ErrorKind::Unsupported, "not supported"),
        2 => (io::ErrorKind::Other, "unspecified error"),
        3 => (io::ErrorKind::TimedOut, "provider timeout"),
        4 => (io::ErrorKind::Other, "failed"),
        5 => (io::ErrorKind::InvalidInput, "invalid parameter"),
        40001 => (io::ErrorKind::PermissionDenied, "access denied"),
        40004 => (io::ErrorKind::Other, "unexpected I/O error"),
        40018 => (io::ErrorKind::Other, "failover cluster management required"),
        42010 => (io::ErrorKind::Other, "system or critical partition"),
        43000 => (io::ErrorKind::Other, "invalid cluster size"),
        43001 => (io::ErrorKind::Other, "unsupported filesystem"),
        43002 => (io::ErrorKind::Other, "quick format unavailable"),
        43003 => (io::ErrorKind::Other, "cluster count exceeds 32 bits"),
        43004 => (io::ErrorKind::Other, "unsupported UDF version"),
        43005 => (
            io::ErrorKind::Other,
            "cluster size is not a physical-sector multiple",
        ),
        43006 => (io::ErrorKind::Other, "read-only volume"),
        _ => (io::ErrorKind::Other, "provider failure"),
    };
    // Keep the return code ahead of the provider diagnostic so it survives
    // truncation to the cache error message's 1024-byte limit.
    Err(io::Error::new(
        kind,
        format!(
            "MSFT_Volume.Format returned {} ({description}); ExtendedStatus={:?}",
            result.return_value, result.extended_status,
        ),
    ))
}

fn call_complete(hresult: i32, operation_status: i32) -> io::Result<bool> {
    const WBEM_S_TIMEDOUT: i32 = 0x40004;
    if hresult == WBEM_S_TIMEDOUT {
        return Ok(false);
    }
    if hresult != 0 || operation_status != 0 {
        return Err(io::Error::other(format!(
            "cache WMI wait failed: HRESULT 0x{:08x}; operation status 0x{:08x}",
            hresult as u32, operation_status as u32,
        )));
    }
    Ok(true)
}

/// No shell, drive letter or mount folder is needed: WMI invokes Format on the
/// volume instance whose Path matches the GUID already selected by disk extents.
/// The deadline comes unchanged from WindowsStorage::initialize_raw and includes
/// partition creation in its 30-second budget.
#[cfg(windows)]
pub fn run(volume: &str, deadline: Instant) -> io::Result<()> {
    validate_volume(volume)?;
    remaining_ms(deadline)?;
    let mut provider = wmi::Provider::connect()?;
    format_with(&mut provider, volume, deadline)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::VecDeque, time::Duration};

    const VOLUME: &str = "\\\\?\\Volume{0123abcd-0000-4000-8000-00000000cafe}\\";
    const OBJECT: &str = "MSFT_Volume.ObjectId=\"opaque-selected-object-key\"";

    struct FakeProvider {
        volumes: Vec<Volume>,
        results: VecDeque<io::Result<Option<FormatResult>>>,
        calls: Vec<&'static str>,
        started: Option<(String, FormatOptions, Instant)>,
        query_deadline: Option<Instant>,
        poll_timeouts: Vec<i32>,
        start_error: bool,
    }
    impl Default for FakeProvider {
        fn default() -> Self {
            Self {
                volumes: vec![Volume {
                    path: VOLUME.into(),
                    object_path: OBJECT.into(),
                }],
                results: [Ok(Some(FormatResult {
                    return_value: 0,
                    extended_status: String::new(),
                }))]
                .into(),
                calls: Vec::new(),
                started: None,
                query_deadline: None,
                poll_timeouts: Vec::new(),
                start_error: false,
            }
        }
    }
    impl StorageProvider for FakeProvider {
        fn volumes(&mut self, deadline: Instant) -> io::Result<Vec<Volume>> {
            self.calls.push("volumes");
            self.query_deadline = Some(deadline);
            Ok(self.volumes.clone())
        }
        fn start(
            &mut self,
            path: &str,
            options: &FormatOptions,
            deadline: Instant,
        ) -> io::Result<()> {
            self.calls.push("start");
            self.started = Some((path.into(), options.clone(), deadline));
            if self.start_error {
                return Err(io::Error::other("HRESULT 0x80041003"));
            }
            Ok(())
        }
        fn poll(&mut self, timeout_ms: i32) -> io::Result<Option<FormatResult>> {
            self.calls.push("poll");
            self.poll_timeouts.push(timeout_ms);
            self.results
                .pop_front()
                .expect("unexpected extra provider poll")
        }
    }

    #[test]
    fn formats_only_the_selected_volume_object_as_quick_ntfs() {
        let mut provider = FakeProvider::default();
        provider.volumes.insert(
            0,
            Volume {
                path: "\\\\?\\Volume{aaaaaaaa-0000-4000-8000-00000000cafe}\\".into(),
                object_path: "wrong-volume".into(),
            },
        );
        provider.volumes[1].path.make_ascii_uppercase();
        let deadline = Instant::now() + Duration::from_secs(30);
        format_with(&mut provider, VOLUME, deadline).unwrap();
        assert_eq!(provider.calls, ["volumes", "start", "poll"]);
        assert_eq!(provider.query_deadline, Some(deadline));
        assert_eq!(
            provider.started,
            Some((
                OBJECT.into(),
                FormatOptions {
                    filesystem: "NTFS",
                    label: "motor-target",
                    allocation_unit_size: 4096,
                    full: false,
                    force: true,
                },
                deadline
            ))
        );
        assert!(
            provider
                .poll_timeouts
                .iter()
                .all(|wait| (1..=100).contains(wait))
        );
    }

    #[test]
    fn missing_duplicate_or_empty_object_identity_never_starts_format() {
        for volumes in [
            vec![],
            vec![Volume {
                path: VOLUME.into(),
                object_path: String::new(),
            }],
            vec![FakeProvider::default().volumes[0].clone(); 2],
        ] {
            let mut provider = FakeProvider {
                volumes,
                ..Default::default()
            };
            assert!(
                format_with(
                    &mut provider,
                    VOLUME,
                    Instant::now() + Duration::from_secs(30)
                )
                .is_err()
            );
            assert_eq!(provider.calls, ["volumes"]);
            assert!(provider.started.is_none());
        }
        let mut provider = FakeProvider::default();
        provider.volumes[0].path = "\\\\?\\Volume{aaaaaaaa-0000-4000-8000-00000000cafe}\\".into();
        assert!(
            format_with(
                &mut provider,
                VOLUME,
                Instant::now() + Duration::from_secs(30)
            )
            .is_err()
        );
        assert!(provider.started.is_none());
    }

    #[test]
    fn malformed_volume_paths_fail_before_contacting_the_provider() {
        for path in [
            "C:",
            "C:\\t\\",
            "\\\\.\\PhysicalDrive1",
            "\\\\?\\Volume{not-a-guid}\\",
            "\\\\?\\Volume{0123abcd-0000-4000-8000-00000000cafe}",
        ] {
            let mut provider = FakeProvider::default();
            assert_eq!(
                format_with(
                    &mut provider,
                    path,
                    Instant::now() + Duration::from_secs(30)
                )
                .unwrap_err()
                .kind(),
                io::ErrorKind::InvalidInput
            );
            assert!(provider.calls.is_empty());
        }
    }

    #[test]
    fn pending_calls_are_polled_until_the_method_result_arrives() {
        let mut provider = FakeProvider::default();
        provider.results.push_front(Ok(None));
        format_with(
            &mut provider,
            VOLUME,
            Instant::now() + Duration::from_secs(30),
        )
        .unwrap();
        assert_eq!(provider.calls, ["volumes", "start", "poll", "poll"]);
    }

    #[test]
    fn every_nonzero_format_return_fails_with_the_provider_diagnostic() {
        for code in [
            1,
            2,
            3,
            4,
            5,
            7,
            4096,
            40001,
            40004,
            40018,
            42010,
            43000,
            43001,
            43002,
            43003,
            43004,
            43005,
            43006,
            u32::MAX,
        ] {
            let mut provider = FakeProvider {
                results: [Ok(Some(FormatResult {
                    return_value: code,
                    extended_status: "storage-detail".into(),
                }))]
                .into(),
                ..Default::default()
            };
            let error = format_with(
                &mut provider,
                VOLUME,
                Instant::now() + Duration::from_secs(30),
            )
            .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains(&format!("MSFT_Volume.Format returned {code}")),
                "{error}"
            );
            assert!(error.to_string().contains("storage-detail"), "{error}");
            if code == 3 {
                assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            }
            if code == 40001 {
                assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            }
        }
    }

    #[test]
    fn transport_failure_never_becomes_a_successful_format() {
        let mut provider = FakeProvider {
            start_error: true,
            ..Default::default()
        };
        assert!(
            format_with(
                &mut provider,
                VOLUME,
                Instant::now() + Duration::from_secs(30)
            )
            .unwrap_err()
            .to_string()
            .contains("0x80041003")
        );
        assert_eq!(provider.calls, ["volumes", "start"]);
        let mut provider = FakeProvider {
            results: [Err(io::Error::other("HRESULT 0x80041001"))].into(),
            ..Default::default()
        };
        assert!(
            format_with(
                &mut provider,
                VOLUME,
                Instant::now() + Duration::from_secs(30)
            )
            .unwrap_err()
            .to_string()
            .contains("0x80041001")
        );
    }

    #[test]
    fn expired_deadline_prevents_queries_and_formatting() {
        let mut provider = FakeProvider::default();
        assert_eq!(
            format_with(&mut provider, VOLUME, Instant::now())
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(provider.calls.is_empty());
        assert!(remaining_ms(Instant::now()).is_err());
    }

    #[test]
    fn discovery_cannot_restart_the_original_format_deadline() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(30);
        let mut times = [now, deadline].into_iter();
        let mut provider = FakeProvider::default();
        assert_eq!(
            format_with_clock(&mut provider, VOLUME, deadline, || times.next().unwrap())
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(provider.calls, ["volumes"]);
        assert!(provider.started.is_none());
    }

    #[test]
    fn completion_at_the_deadline_is_ambiguous_even_if_the_provider_succeeded() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(30);
        let mut times = [now, now, deadline - Duration::from_millis(7), deadline].into_iter();
        let mut provider = FakeProvider::default();
        assert_eq!(
            format_with_clock(&mut provider, VOLUME, deadline, || times.next().unwrap())
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(provider.poll_timeouts, [7]);
        assert_eq!(provider.calls, ["volumes", "start", "poll"]);
    }

    #[test]
    fn positive_wmi_timeout_hresult_is_pending_not_success() {
        assert!(!call_complete(0x40004, 0).unwrap());
        assert!(call_complete(0, 0).unwrap());
        assert!(call_complete(0x80041001u32 as i32, 0).is_err());
        assert!(call_complete(0, 0x80041003u32 as i32).is_err());
        assert!(call_complete(1, 0).is_err());
    }
}

#[cfg(test)]
mod provider_contract_tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum RecordedVariant {
        Bstr(&'static str),
        I4(i32),
        Ui4(u32),
        Bool(i16),
    }

    impl From<&'static str> for RecordedVariant {
        fn from(value: &'static str) -> Self {
            Self::Bstr(value)
        }
    }

    impl From<i32> for RecordedVariant {
        fn from(value: i32) -> Self {
            Self::I4(value)
        }
    }

    impl From<u32> for RecordedVariant {
        fn from(value: u32) -> Self {
            Self::Ui4(value)
        }
    }

    impl From<bool> for RecordedVariant {
        fn from(value: bool) -> Self {
            Self::Bool(if value { -1 } else { 0 })
        }
    }

    #[test]
    fn format_put_uses_documented_variant_types() {
        let cases = [
            (
                FormatOptions::default(),
                [
                    ("FileSystem", RecordedVariant::Bstr("NTFS")),
                    ("FileSystemLabel", RecordedVariant::Bstr("motor-target")),
                    ("AllocationUnitSize", RecordedVariant::I4(4096)),
                    ("Full", RecordedVariant::Bool(0)),
                    ("Force", RecordedVariant::Bool(-1)),
                ],
            ),
            (
                FormatOptions {
                    filesystem: "ReFS",
                    label: "other-label",
                    allocation_unit_size: u32::MAX,
                    full: true,
                    force: false,
                },
                [
                    ("FileSystem", RecordedVariant::Bstr("ReFS")),
                    ("FileSystemLabel", RecordedVariant::Bstr("other-label")),
                    ("AllocationUnitSize", RecordedVariant::I4(-1)),
                    ("Full", RecordedVariant::Bool(-1)),
                    ("Force", RecordedVariant::Bool(0)),
                ],
            ),
        ];
        for (options, expected) in cases {
            let mut puts = Vec::new();
            put_format_parameters(&options, |name, value: &RecordedVariant| {
                puts.push((name.to_owned(), value.clone()));
                Ok(())
            })
            .unwrap();
            let expected = expected.map(|(name, value)| (name.to_owned(), value));
            assert_eq!(puts, expected);
        }
    }

    #[test]
    fn format_put_failure_names_the_parameter_and_stops() {
        let mut names = Vec::new();
        let error =
            put_format_parameters(&FormatOptions::default(), |name, _: &RecordedVariant| {
                names.push(name.to_owned());
                if name == "AllocationUnitSize" {
                    Err(io::Error::other("HRESULT 0x80041005: type mismatch"))
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
        assert_eq!(
            names,
            ["FileSystem", "FileSystemLabel", "AllocationUnitSize"]
        );
        assert_eq!(
            error.to_string(),
            "cache WMI set Format parameter AllocationUnitSize: HRESULT 0x80041005: type mismatch"
        );
    }

    #[test]
    fn format_method_parameters_match_the_documented_storage_schema() {
        assert_eq!(
            FormatOptions::default().parameters(),
            [
                ("FileSystem", FormatValue::Text("NTFS")),
                ("FileSystemLabel", FormatValue::Text("motor-target")),
                ("AllocationUnitSize", FormatValue::Number(4096)),
                ("Full", FormatValue::Boolean(false)),
                ("Force", FormatValue::Boolean(true)),
            ]
        );
    }

    #[test]
    fn provider_return_code_survives_the_cache_failure_wire_budget() {
        use crate::cache_steps::{CacheLimits, CacheReporter, CacheStep, watch_cache};
        use std::sync::mpsc;
        let (tx, events) = mpsc::channel();
        let reporter = CacheReporter::new(tx);
        let result = reporter.step(CacheStep::InitializeRaw, || {
            format_result(FormatResult {
                return_value: 43006,
                extended_status: "provider-detail-🦀".repeat(1024),
            })
        });
        let (tx, rx) = mpsc::channel();
        tx.send(result).unwrap();
        let failure = watch_cache(
            events,
            rx,
            |_, _| Ok(()),
            || panic!("format finished"),
            CacheLimits::default(),
        )
        .unwrap_err();
        failure.validate().unwrap();
        assert_eq!(failure.step, CacheStep::InitializeRaw);
        assert!(
            failure
                .message
                .contains("MSFT_Volume.Format returned 43006 (read-only volume)"),
            "{failure}"
        );
        assert!(failure.message.contains("ExtendedStatus="), "{failure}");
        let decoded: crate::cache_steps::CacheFailure =
            serde_json::from_str(&serde_json::to_string(&failure).unwrap()).unwrap();
        assert_eq!(decoded, failure);
    }
}
