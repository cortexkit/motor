//! Windows environment blocks and the job's additional executable search paths.

use std::{collections::BTreeMap, io};

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn valid_key(key: &str) -> bool {
    let name = key.strip_prefix('=').unwrap_or(key);
    !name.is_empty() && !name.contains(['=', '\0'])
}

/// Parse into owned strings while the returned block is alive. Dropping the block
/// releases its Windows storage on both success and parse errors.
pub fn read_user_environment<B: AsRef<[u16]>>(
    create: impl FnOnce() -> io::Result<B>,
) -> io::Result<BTreeMap<String, String>> {
    let block = create()?;
    parse_block(block.as_ref())
}

pub fn parse_block(block: &[u16]) -> io::Result<BTreeMap<String, String>> {
    if block.len() < 2 || !block.ends_with(&[0, 0]) {
        return Err(invalid(
            "Windows environment block is not double-NUL terminated",
        ));
    }
    let mut env = BTreeMap::new();
    let mut offset = 0;
    while offset < block.len() - 1 {
        let length = block[offset..].iter().position(|c| *c == 0).unwrap();
        if length == 0 {
            if offset == 0 && block.len() == 2 {
                break;
            }
            return Err(invalid("empty entry inside Windows environment block"));
        }
        let entry = String::from_utf16(&block[offset..offset + length])
            .map_err(|_| invalid("Windows environment entry is not valid UTF-16"))?;
        // Windows can include hidden drive-directory entries such as =C:=C:\work.
        let separator = if let Some(hidden) = entry.strip_prefix('=') {
            hidden.find('=').map(|index| index + 1)
        } else {
            entry.find('=')
        }
        .filter(|index| *index > 0)
        .ok_or_else(|| invalid("Windows environment entry has no name/value separator"))?;
        let key = entry[..separator].to_uppercase();
        if !valid_key(&key) {
            return Err(invalid("invalid Windows environment key"));
        }
        if env.insert(key, entry[separator + 1..].into()).is_some() {
            return Err(invalid(
                "duplicate case-insensitive Windows environment key",
            ));
        }
        offset += length + 1;
    }
    Ok(env)
}

pub fn encode_block(env: &BTreeMap<String, String>) -> io::Result<Vec<u16>> {
    let mut entries: Vec<_> = env.iter().collect();
    // CreateProcess expects Unicode ordering, independent of the process locale.
    entries.sort_by(|(a, _), (b, _)| {
        a.to_uppercase()
            .encode_utf16()
            .cmp(b.to_uppercase().encode_utf16())
    });
    let mut block = Vec::new();
    for (key, value) in entries {
        if !valid_key(key) || value.contains('\0') {
            return Err(invalid("invalid Windows environment key or value"));
        }
        block.extend(format!("{key}={value}").encode_utf16());
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

pub fn git_native_bin(mut is_directory: impl FnMut(&str) -> bool) -> io::Result<&'static str> {
    for path in [
        "C:\\Program Files\\Git\\ucrt64\\bin",
        "C:\\Program Files\\Git\\mingw64\\bin",
    ] {
        if is_directory(path) {
            return Ok(path);
        }
    }
    Err(invalid(
        "Git's ucrt64/bin or mingw64/bin directory is missing",
    ))
}

pub fn job_path(base: &str, git_bin: &str) -> String {
    let mut seen = std::collections::BTreeSet::new();
    [
        "C:\\rust\\cargo\\bin",
        "C:\\Program Files\\Git\\cmd",
        git_bin,
        "C:\\Program Files\\Git\\usr\\bin",
        "C:\\Program Files\\PowerShell\\7",
    ]
    .into_iter()
    .chain(base.split(';'))
    .filter(|entry| {
        let path = entry.trim().trim_matches('"').replace('/', "\\");
        if path.is_empty() {
            return false;
        }
        // A drive root is not equivalent to a drive-relative path such as C:.
        let key = if path.len() == 3 && path.ends_with(":\\") {
            path.to_uppercase()
        } else {
            path.trim_end_matches('\\').to_uppercase()
        };
        seen.insert(key)
    })
    .collect::<Vec<_>>()
    .join(";")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, rc::Rc};

    fn block(entries: &[&str]) -> Vec<u16> {
        let mut raw = Vec::new();
        for entry in entries {
            raw.extend(entry.encode_utf16());
            raw.push(0);
        }
        if raw.is_empty() {
            raw.push(0);
        }
        raw.push(0);
        raw
    }

    #[test]
    fn block_round_trip_preserves_hidden_entries_unicode_and_equals_in_values() {
        let raw = block(&[
            "=C:=C:\\work",
            "Path=C:\\Windows;C:\\Users\\motor-job\\bin",
            "PATHEXT=.COM;.EXE;.BAT;.CMD",
            "APPDATA=C:\\Users\\motor-job\\AppData\\Roaming",
            "UNICODE=é🦀=value",
        ]);
        let env = parse_block(&raw).unwrap();
        assert_eq!(env["=C:"], "C:\\work");
        assert_eq!(env["UNICODE"], "é🦀=value");
        assert_eq!(env["PATH"], "C:\\Windows;C:\\Users\\motor-job\\bin");
        let encoded = encode_block(&env).unwrap();
        assert!(encoded.ends_with(&[0, 0]));
        assert_eq!(parse_block(&encoded).unwrap(), env);
        assert_eq!(encode_block(&BTreeMap::new()).unwrap(), [0, 0]);
        assert!(parse_block(&[0, 0]).unwrap().is_empty());
    }

    #[test]
    fn malformed_blocks_and_invalid_output_values_are_rejected() {
        for raw in [
            vec![],
            vec![0],
            vec![0, 0, 0],
            block(&["no-separator"]),
            block(&["=no-name"]),
            block(&["==no-name"]),
            block(&["PATH=a", "Path=b"]),
            block(&["A=a", "", "B=b"]),
            vec![b'A' as u16, b'=' as u16, 0xd800, 0, 0],
        ] {
            assert!(parse_block(&raw).is_err(), "accepted {raw:?}");
        }
        for (key, value) in [("", "v"), ("=", "v"), ("BAD=KEY", "v"), ("A", "bad\0value")] {
            assert!(encode_block(&BTreeMap::from([(key.into(), value.into())])).is_err());
        }
    }

    struct FakeBlock {
        raw: Vec<u16>,
        destroyed: Rc<Cell<usize>>,
    }
    impl AsRef<[u16]> for FakeBlock {
        fn as_ref(&self) -> &[u16] {
            &self.raw
        }
    }
    impl Drop for FakeBlock {
        fn drop(&mut self) {
            self.destroyed.set(self.destroyed.get() + 1);
        }
    }

    #[test]
    fn environment_source_failure_propagates_and_owned_blocks_are_always_released() {
        let destroyed = Rc::new(Cell::new(0));
        for (raw, valid) in [(block(&["PATHEXT=.EXE;.CMD"]), true), (vec![1], false)] {
            let result = read_user_environment(|| {
                Ok(FakeBlock {
                    raw,
                    destroyed: destroyed.clone(),
                })
            });
            assert_eq!(result.is_ok(), valid);
        }
        assert_eq!(destroyed.get(), 2);
        let result = read_user_environment::<FakeBlock>(|| Err(io::Error::from_raw_os_error(5)));
        assert_eq!(result.unwrap_err().raw_os_error(), Some(5));
        assert_eq!(destroyed.get(), 2);
    }

    #[test]
    fn managed_paths_win_and_windows_machine_then_user_paths_survive() {
        let path = job_path(
            "c:/RUST/cargo/bin/;C:\\Windows\\System32;C:\\Windows;C:\\Users\\motor-job\\bin;c:\\windows\\system32\\;C:;C:\\;;",
            "C:\\Program Files\\Git\\mingw64\\bin",
        );
        assert_eq!(
            path,
            "C:\\rust\\cargo\\bin;C:\\Program Files\\Git\\cmd;C:\\Program Files\\Git\\mingw64\\bin;C:\\Program Files\\Git\\usr\\bin;C:\\Program Files\\PowerShell\\7;C:\\Windows\\System32;C:\\Windows;C:\\Users\\motor-job\\bin;C:;C:\\"
        );
    }

    #[test]
    fn git_native_paths_follow_the_installed_layout_and_missing_tools_fail() {
        let ucrt = "C:\\Program Files\\Git\\ucrt64\\bin";
        let mingw = "C:\\Program Files\\Git\\mingw64\\bin";
        assert_eq!(git_native_bin(|path| path == ucrt).unwrap(), ucrt);
        assert_eq!(git_native_bin(|path| path == mingw).unwrap(), mingw);
        assert_eq!(git_native_bin(|_| true).unwrap(), ucrt);
        assert!(git_native_bin(|_| false).is_err());
    }
}
