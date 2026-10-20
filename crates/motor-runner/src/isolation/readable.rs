//! Whether a shared read-only tree already lets an account read it, so that
//! runner startup can skip rewriting the ACL of every file in it.
//!
//! In a tenant VM the toolchain lives on the read-only image under a RAM
//! overlay. Changing an ACL there copies each file up into memory, and a full
//! grant over the toolchain does not finish within the attach budget. The
//! image therefore ships those trees root-owned and readable by everyone, and
//! this check accepts them without writing anything. A tree that an earlier
//! start already granted (a named-user ACL entry on every entry, plus a
//! default entry on every directory where new content must inherit it) is
//! accepted too. Anything else, or any doubt, gets the normal grant.
//!
//! The walk never follows symbolic links, matching `setfacl -P`.

use std::{
    ffi::CString,
    fs, io,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::Path,
};

const ACL_ACCESS: &str = "system.posix_acl_access";
const ACL_DEFAULT: &str = "system.posix_acl_default";
/// POSIX ACL xattr encoding (linux/posix_acl_xattr.h): a version word, then
/// entries of tag, permission bits and id.
const ACL_XATTR_VERSION: u32 = 2;
const ACL_USER: u16 = 0x02;
const ACL_MASK: u16 = 0x10;
const READ: u32 = 0o4;
const READ_SEARCH: u32 = 0o5;

/// True only when every entry under `root` is already readable (directories
/// also searchable) by `uid`, and, with `inherit`, every directory's default
/// ACL already passes that read access to new entries.
pub(super) fn already_readable(root: &Path, uid: u32, inherit: bool) -> bool {
    matches!(walk(root, uid, inherit), Ok(true))
}

fn walk(path: &Path, uid: u32, inherit: bool) -> io::Result<bool> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(true);
    }
    let directory = metadata.is_dir();
    let needed = if directory { READ_SEARCH } else { READ };
    // When the account owns an entry, its owner bits decide access. That is
    // unexpected for shared trees, so report "not verified" and let the
    // caller run its normal setfacl grant.
    if metadata.uid() == uid {
        return Ok(false);
    }
    let readable = match acl(path, ACL_ACCESS)? {
        // Without an extended ACL the account falls in the group or the
        // other class, depending on its group membership. Requiring both
        // classes to allow access makes the answer independent of groups.
        None => {
            let mode = metadata.mode();
            (mode >> 3) & needed == needed && mode & needed == needed
        }
        Some(entries) => named_user_grants(&entries, uid, needed),
    };
    if !readable {
        return Ok(false);
    }
    if directory {
        if inherit
            && !acl(path, ACL_DEFAULT)?
                .is_some_and(|entries| named_user_grants(&entries, uid, READ_SEARCH))
        {
            return Ok(false);
        }
        for entry in fs::read_dir(path)? {
            if !walk(&entry?.path(), uid, inherit)? {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

type AclEntry = (u16, u16, u32);

fn acl(path: &Path, name: &str) -> io::Result<Option<Vec<AclEntry>>> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    let name = CString::new(name)?;
    let mut buffer = vec![0_u8; 512];
    loop {
        // SAFETY: both names are NUL-terminated and the buffer pointer and
        // length describe writable memory owned by this frame.
        let length = unsafe {
            libc::lgetxattr(
                path.as_ptr(),
                name.as_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            )
        };
        if length >= 0 {
            return parse_acl(&buffer[..length as usize]).map(Some);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ENODATA) | Some(libc::EOPNOTSUPP) => return Ok(None),
            Some(libc::ERANGE) if buffer.len() < 1 << 20 => buffer.resize(buffer.len() * 4, 0),
            _ => return Err(error),
        }
    }
}

fn parse_acl(bytes: &[u8]) -> io::Result<Vec<AclEntry>> {
    let malformed = || io::Error::other("unexpected POSIX ACL encoding");
    let (version, entries) = bytes.split_at_checked(4).ok_or_else(malformed)?;
    let (entries, rest) = entries.as_chunks::<8>();
    if u32::from_le_bytes(version.try_into().map_err(|_| malformed())?) != ACL_XATTR_VERSION
        || !rest.is_empty()
    {
        return Err(malformed());
    }
    Ok(entries
        .iter()
        .map(|entry| {
            (
                u16::from_le_bytes([entry[0], entry[1]]),
                u16::from_le_bytes([entry[2], entry[3]]),
                u32::from_le_bytes([entry[4], entry[5], entry[6], entry[7]]),
            )
        })
        .collect())
}

/// A named-user entry decides access for that account, limited by the mask.
/// Without such an entry the answer would depend on group entries, so the
/// caller's normal grant handles it.
fn named_user_grants(entries: &[AclEntry], uid: u32, needed: u32) -> bool {
    let Some(permissions) = entries
        .iter()
        .find(|(tag, _, id)| *tag == ACL_USER && *id == uid)
        .map(|entry| u32::from(entry.1))
    else {
        return false;
    };
    let mask = entries
        .iter()
        .find(|(tag, _, _)| *tag == ACL_MASK)
        .map_or(0o7, |entry| u32::from(entry.1));
    permissions & mask & needed == needed
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn encoded(entries: &[AclEntry]) -> Vec<u8> {
        let mut bytes = ACL_XATTR_VERSION.to_le_bytes().to_vec();
        for (tag, permissions, id) in entries {
            bytes.extend(tag.to_le_bytes());
            bytes.extend(permissions.to_le_bytes());
            bytes.extend(id.to_le_bytes());
        }
        bytes
    }

    fn mode(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    /// A world-readable tree like the image toolchain, owned by the test
    /// process and checked for a different account.
    fn plain_tree(name: &str) -> (ScratchDir, std::path::PathBuf, u32) {
        let scratch = ScratchDir::new(name);
        let root = scratch.join("toolchain");
        fs::create_dir_all(root.join("lib/rustlib")).unwrap();
        fs::write(root.join("lib/rustlib/librustc.so"), "toolchain\n").unwrap();
        fs::write(root.join("rustc"), "rustc\n").unwrap();
        for directory in [root.as_path(), &root.join("lib"), &root.join("lib/rustlib")] {
            mode(directory, 0o755);
        }
        mode(&root.join("lib/rustlib/librustc.so"), 0o644);
        mode(&root.join("rustc"), 0o755);
        // SAFETY: geteuid has no preconditions.
        let other = unsafe { libc::geteuid() } + 1;
        (scratch, root, other)
    }

    #[test]
    fn world_readable_tree_needs_no_grant_but_not_for_inherited_content() {
        let (_scratch, root, other) = plain_tree("motor-readable-plain");
        assert!(already_readable(&root, other, false));
        // Without a default ACL, content added later would not be readable.
        assert!(!already_readable(&root, other, true));
    }

    #[test]
    fn one_unreadable_file_or_unsearchable_directory_requires_the_grant() {
        let (_scratch, root, other) = plain_tree("motor-readable-file");
        mode(&root.join("lib/rustlib/librustc.so"), 0o640);
        assert!(!already_readable(&root, other, false));
        let (_scratch, root, other) = plain_tree("motor-readable-dir");
        mode(&root.join("lib/rustlib"), 0o754);
        assert!(!already_readable(&root, other, false));
    }

    #[test]
    fn links_are_not_followed_and_owned_or_missing_trees_require_the_grant() {
        let (scratch, root, other) = plain_tree("motor-readable-link");
        let private = scratch.join("private");
        fs::write(&private, "secret\n").unwrap();
        mode(&private, 0o600);
        symlink(&private, root.join("link")).unwrap();
        assert!(already_readable(&root, other, false));
        // SAFETY: geteuid has no preconditions.
        assert!(!already_readable(&root, unsafe { libc::geteuid() }, false));
        assert!(!already_readable(&scratch.join("missing"), other, false));
    }

    #[test]
    fn named_user_entries_grant_only_through_the_mask() {
        let entries = parse_acl(&encoded(&[
            (0x01, 0o6, u32::MAX),
            (ACL_USER, 0o5, 1234),
            (0x04, 0o0, u32::MAX),
            (ACL_MASK, 0o5, u32::MAX),
            (0x20, 0o0, u32::MAX),
        ]))
        .unwrap();
        assert!(named_user_grants(&entries, 1234, READ_SEARCH));
        assert!(!named_user_grants(&entries, 4321, READ));
        let masked = parse_acl(&encoded(&[
            (ACL_USER, 0o5, 1234),
            (ACL_MASK, 0o1, u32::MAX),
        ]))
        .unwrap();
        assert!(!named_user_grants(&masked, 1234, READ));
        assert!(parse_acl(&[2, 0, 0, 0, 1]).is_err());
        assert!(parse_acl(&[1, 0, 0, 0]).is_err());
    }
}
