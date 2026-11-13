//! Finds the APFS volume on the job's build disk in the guest's
//! `diskutil apfs list -plist` report, converted to JSON by `plutil`. Only the
//! guest agent uses it; it lives here because this crate already holds the
//! parsing that the agent shares with the host.
use serde_json::Value;
use std::io;

fn invalid(message: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

/// `disk` followed by digits, then optionally `s` and digits: a disk or a
/// partition/volume identifier, never a path.
fn identifier(text: &str, slice: bool) -> bool {
    let Some(rest) = text.strip_prefix("disk") else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    match rest.split_once('s') {
        Some((disk, part)) => slice && digits(disk) && digits(part),
        None => !slice && digits(rest),
    }
}

/// Whether `text` names a whole disk, such as `disk4`.
pub fn whole_disk(text: &str) -> bool {
    identifier(text, false)
}

/// The device identifier (such as `disk5s1`) of the one APFS volume whose
/// container's physical store is a partition of `disk` (such as `disk4`).
///
/// `Ok(None)` means no container on `disk` is attached: the disk holds no APFS
/// container, or the system has not attached it yet. A container on `disk`
/// that also uses another disk, more than one such container, or a container
/// without exactly one volume is an error: the agent reformats such a disk
/// rather than guess which volume holds the build.
pub fn volume_on_disk(report: &[u8], disk: &str) -> io::Result<Option<String>> {
    if !whole_disk(disk) {
        return Err(invalid(format!("not a whole disk: {disk}")));
    }
    let report: Value = serde_json::from_slice(report).map_err(invalid)?;
    let containers = report
        .get("Containers")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("APFS report has no container list"))?;
    let on_disk = |store: &str| {
        store
            .strip_prefix(disk)
            .and_then(|rest| rest.strip_prefix('s'))
            .is_some_and(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
    };
    let mut found = None;
    for container in containers {
        let stores = container
            .get("PhysicalStores")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("APFS container has no physical store list"))?
            .iter()
            .map(|store| {
                store
                    .get("DeviceIdentifier")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("APFS physical store has no identifier"))
            })
            .collect::<io::Result<Vec<_>>>()?;
        if !stores.iter().any(|store| on_disk(store)) {
            continue;
        }
        if !stores.iter().all(|store| on_disk(store)) {
            return Err(invalid(format!(
                "APFS container on {disk} also uses another disk"
            )));
        }
        if found.is_some() {
            return Err(invalid(format!("more than one APFS container on {disk}")));
        }
        let volumes = container
            .get("Volumes")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("APFS container has no volume list"))?;
        let [volume] = volumes.as_slice() else {
            return Err(invalid(format!(
                "APFS container on {disk} has {} volumes, not one",
                volumes.len()
            )));
        };
        let device = volume
            .get("DeviceIdentifier")
            .and_then(Value::as_str)
            .filter(|device| identifier(device, true))
            .ok_or_else(|| invalid("APFS volume has no valid identifier"))?;
        found = Some(device.to_owned());
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn report(containers: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({ "Containers": containers })).unwrap()
    }
    fn container(stores: &[&str], volumes: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "ContainerReference": "disk9",
            "CapacityCeiling": 137_438_953_472u64,
            "PhysicalStores": stores
                .iter()
                .map(|s| serde_json::json!({ "DeviceIdentifier": s, "Size": 1 }))
                .collect::<Vec<_>>(),
            "Volumes": volumes
                .iter()
                .map(|v| serde_json::json!({ "DeviceIdentifier": v, "Name": "MotorBuild", "Roles": [] }))
                .collect::<Vec<_>>(),
        })
    }
    #[test]
    fn finds_the_single_volume_of_the_container_on_the_build_disk() {
        let system = container(&["disk0s2"], &["disk3s1", "disk3s2", "disk3s5"]);
        let build = container(&["disk4s2"], &["disk5s1"]);
        let bytes = report(serde_json::json!([system, build]));
        assert_eq!(
            volume_on_disk(&bytes, "disk4").unwrap().as_deref(),
            Some("disk5s1")
        );
        // disk4 is not a prefix match for disk40's partitions.
        let other = report(serde_json::json!([container(&["disk40s2"], &["disk41s1"])]));
        assert_eq!(volume_on_disk(&other, "disk4").unwrap(), None);
        assert_eq!(
            volume_on_disk(&report(serde_json::json!([])), "disk4").unwrap(),
            None
        );
    }
    #[test]
    fn ambiguous_or_malformed_reports_are_errors_not_guesses() {
        for containers in [
            serde_json::json!([container(&["disk4s2"], &[])]),
            serde_json::json!([container(&["disk4s2"], &["disk5s1", "disk5s2"])]),
            serde_json::json!([container(&["disk4s2", "disk7s2"], &["disk5s1"])]),
            serde_json::json!([
                container(&["disk4s2"], &["disk5s1"]),
                container(&["disk4s3"], &["disk6s1"])
            ]),
            serde_json::json!([container(&["disk4s2"], &["/dev/disk5s1"])]),
            serde_json::json!([container(&["disk4s2"], &["disk5"])]),
            serde_json::json!([{ "Volumes": [] }]),
        ] {
            assert!(
                volume_on_disk(&report(containers.clone()), "disk4").is_err(),
                "{containers}"
            );
        }
        assert!(volume_on_disk(b"not json", "disk4").is_err());
        assert!(volume_on_disk(b"{}", "disk4").is_err());
        for disk in ["disk", "disk4s1", "/dev/disk4", "rdisk4", "disk4 "] {
            assert!(volume_on_disk(&report(serde_json::json!([])), disk).is_err());
        }
    }
}
