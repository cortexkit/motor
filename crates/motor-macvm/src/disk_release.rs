use std::io;
use std::path::Path;

pub(super) fn collect_file_keys<K: PartialEq>(
    owned_process: bool,
    owned_disks: &[K],
    descriptors: impl IntoIterator<Item = io::Result<Option<K>>>,
) -> io::Result<Vec<K>> {
    let mut keys = Vec::new();
    let mut vanished = None;
    for descriptor in descriptors {
        match descriptor {
            Ok(Some(key)) => keys.push(key),
            Ok(None) => {}
            // libproc's descriptor list is a snapshot. Another process can
            // close a listed file descriptor before proc_pidfdinfo inspects
            // it. Keep scanning: another descriptor may prove that
            // this is an owner of our disks, which must still fail closed.
            Err(e) if e.raw_os_error() == Some(libc::EBADF) => vanished = Some(e),
            Err(e) => return Err(e),
        }
    }
    if (owned_process || keys.iter().any(|key| owned_disks.contains(key)))
        && let Some(e) = vanished
    {
        return Err(e);
    }
    Ok(keys)
}

pub(super) fn skip_denied_descriptor(executable: &Path, error: &io::Error) -> bool {
    // Apple system processes in GUI sessions can deny inspection of their open
    // file descriptors, even to the same user. Guest jobs never run host code;
    // only our helper and the VirtualMachine service open private VM disks.
    // Tolerate EPERM only for executables on the read-only system volume, never
    // those two possible disk owners, writable locations, or other errors.
    error.raw_os_error() == Some(libc::EPERM)
        && super::absolute_normal(executable)
        && (executable.starts_with("/System") || executable.starts_with("/usr"))
        // Other volumes, including the writable Data volume, are mounted here.
        && !executable.starts_with("/System/Volumes")
        && !executable.starts_with("/usr/local")
        && executable.file_name().is_some_and(|name| {
            name != "com.apple.Virtualization.VirtualMachine" && name != "motor-macvm"
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_churn_in_unrelated_process_does_not_abort_scan() {
        let disk = 42;
        let mut holders = Vec::new();
        for (pid, descriptors) in [
            (
                100,
                vec![
                    Ok(Some(1)),
                    Err(io::Error::from_raw_os_error(libc::EBADF)),
                    Ok(Some(2)),
                ],
            ),
            (200, vec![Ok(Some(disk))]),
        ] {
            let keys = collect_file_keys(false, &[disk], descriptors).unwrap();
            if pid == 100 {
                assert_eq!(keys, [1, 2]);
            }
            if keys.contains(&disk) {
                holders.push(pid);
            }
        }
        assert_eq!(holders, [200]);
    }

    #[test]
    fn descriptor_churn_in_owned_process_still_fails() {
        let disk = 42;
        for (known_owner, keys) in [(true, vec![1]), (false, vec![disk])] {
            // Put the vanished descriptor first so a later disk match cannot
            // be hidden by an early return from the scan.
            let descriptors = std::iter::once(Err(io::Error::from_raw_os_error(libc::EBADF)))
                .chain(keys.into_iter().map(|key| Ok(Some(key))));
            let error = collect_file_keys(known_owner, &[disk], descriptors).unwrap_err();
            assert_eq!(error.raw_os_error(), Some(libc::EBADF));
        }
    }

    #[test]
    fn unrelated_process_inspection_keeps_other_errors_fatal() {
        for errno in [libc::EPERM, libc::EACCES, libc::EIO, libc::ESRCH] {
            let error = collect_file_keys(
                false,
                &[42],
                [Ok(Some(1)), Err(io::Error::from_raw_os_error(errno))],
            )
            .unwrap_err();
            assert_eq!(error.raw_os_error(), Some(errno));
        }
    }

    #[test]
    fn denied_descriptor_policy_is_limited_to_sealed_non_vm_executables() {
        let denied = io::Error::from_raw_os_error(libc::EPERM);
        for path in [
            "/System/Library/CoreServices/UserEventAgent.app/Contents/MacOS/UserEventAgent",
            "/usr/libexec/nsurlsessiond",
            "/usr/bin/true",
        ] {
            assert!(skip_denied_descriptor(Path::new(path), &denied), "{path}");
            for errno in [libc::EACCES, libc::EIO, libc::ESRCH] {
                assert!(
                    !skip_denied_descriptor(Path::new(path), &io::Error::from_raw_os_error(errno)),
                    "{path}: errno {errno}"
                );
            }
        }
        for path in [
            "/usr/local/bin/agent",
            "/Users/motor/bin/agent",
            "/Library/Apple/System/Library/CoreServices/agent",
            "/System/Volumes/Data/Users/motor/agent",
            "/System/Volumes/Data/usr/local/bin/agent",
            "/System/Library/Frameworks/Virtualization.framework/Versions/A/XPCServices/com.apple.Virtualization.VirtualMachine.xpc/Contents/MacOS/com.apple.Virtualization.VirtualMachine",
            "/usr/libexec/com.apple.Virtualization.VirtualMachine",
            "/usr/bin/motor-macvm",
            "/System/Library/motor-macvm",
            "/System/../Users/motor/agent",
            "/usr/../Library/agent",
            "/usr/local/../bin/agent",
            "/Systemish/agent",
            "/usr-local/agent",
            "usr/libexec/agent",
        ] {
            assert!(!skip_denied_descriptor(Path::new(path), &denied), "{path}");
        }
    }
}
