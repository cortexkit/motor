use std::io;
use std::path::Path;

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
