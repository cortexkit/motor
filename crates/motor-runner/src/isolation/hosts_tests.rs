use super::*;
use std::{fs, os::unix::fs::symlink, path::PathBuf};

const SPECIMEN: &str = "127.0.0.1 daily-cloudcode-pa.googleapis.com cloudcode-pa.googleapis.com\n";
const NORMALIZED: &str =
    "127.0.0.1\tdaily-cloudcode-pa.googleapis.com\tcloudcode-pa.googleapis.com\n";

fn rejects(blob: &[u8], line: usize, reason: &str) {
    let error = parse(blob).unwrap_err().to_string();
    assert!(
        error.contains(&format!(".motor/hosts: line {line}:")),
        "{error}"
    );
    assert!(error.contains(reason), "{error}");
}

#[test]
fn parser_accepts_google_specimen() {
    assert_eq!(parse(SPECIMEN.as_bytes()).unwrap(), [NORMALIZED]);
}

#[test]
fn parser_accepts_comments_blank_lines_and_normalizes_names() {
    assert_eq!(
        parse(b"\n  # comment\n\t\n127.0.0.1 Daily-Cloudcode-PA.Googleapis.com cloudcode-pa.googleapis.com # local server\n").unwrap(),
        [NORMALIZED]
    );
    assert!(parse(b"# no declarations\n\n").unwrap().is_empty());
}

#[test]
fn parser_accepts_other_ipv4_loopback_and_ipv6_localhost() {
    assert_eq!(
        parse(b"127.12.34.56 A.Example\n::1 b.example\n").unwrap(),
        ["127.12.34.56\ta.example\n", "::1\tb.example\n"]
    );
}

#[test]
fn parser_refuses_non_loopback_addresses() {
    for address in [
        "8.8.8.8",
        "0.0.0.0",
        "10.0.0.1",
        "::ffff:127.0.0.1",
        "2001:db8::1",
    ] {
        rejects(
            format!("# header\n{address} example.com\n").as_bytes(),
            2,
            "address must be",
        );
    }
}

#[test]
fn parser_refuses_invalid_addresses() {
    rejects(b"not-an-address example.com\n", 1, "invalid IP address");
}

#[test]
fn parser_refuses_invalid_hostnames() {
    for name in [
        "-bad.example",
        "bad-.example",
        "bad_label.example",
        "bad..example",
        "example.com.",
        "*.example.com",
        ".example",
        "caf\u{e9}.example",
    ] {
        rejects(
            format!("127.0.0.1 {name}\n").as_bytes(),
            1,
            "invalid DNS hostname",
        );
    }
    rejects(
        format!("127.0.0.1 {}.example\n", "a".repeat(64)).as_bytes(),
        1,
        "invalid DNS hostname",
    );
    let too_long = [
        "a".repeat(63),
        "b".repeat(63),
        "c".repeat(63),
        "d".repeat(62),
    ]
    .join(".");
    assert_eq!(too_long.len(), 254);
    rejects(
        format!("127.0.0.1 {too_long}\n").as_bytes(),
        1,
        "invalid DNS hostname",
    );
}

#[test]
fn parser_accepts_hostname_length_boundaries() {
    let name = [
        "a".repeat(63),
        "b".repeat(63),
        "c".repeat(63),
        "d".repeat(61),
    ]
    .join(".");
    assert_eq!(name.len(), 253);
    assert_eq!(
        parse(format!("127.0.0.1 {name}\n").as_bytes()).unwrap(),
        [format!("127.0.0.1\t{name}\n")]
    );
    assert_eq!(
        parse(b"127.0.0.1 a 9 x-y\n").unwrap(),
        ["127.0.0.1\ta\t9\tx-y\n"]
    );
}

#[test]
fn parser_refuses_case_insensitive_duplicates() {
    rejects(
        b"127.0.0.1 Example.Com\n::1 eXAMPLE.cOM\n",
        2,
        "duplicate hostname",
    );
    rejects(
        b"127.0.0.1 example.com example.com\n",
        1,
        "duplicate hostname",
    );
}

#[test]
fn parser_refuses_protected_default_names() {
    for name in ["LOCALHOST", "Ip6-Localhost", "iP6-LoOpBaCk"] {
        rejects(
            format!("127.0.0.1 {name}\n").as_bytes(),
            1,
            "protected default name",
        );
    }
}

#[test]
fn parser_counts_names_not_lines() {
    let names = (0..64)
        .map(|i| format!("n{i}.example"))
        .collect::<Vec<_>>()
        .join(" ");
    let allowed = format!("127.0.0.1 {names}\n");
    assert_eq!(parse(allowed.as_bytes()).unwrap().len(), 1);
    rejects(
        format!("{allowed}::1 sixty-fifth.example\n").as_bytes(),
        2,
        "64-name limit",
    );
}

#[test]
fn parser_enforces_byte_cap_before_parsing() {
    let allowed = format!("#{}", "x".repeat(4095));
    assert!(parse(allowed.as_bytes()).unwrap().is_empty());
    let oversized = vec![0xff; 4097];
    rejects(&oversized, 1, "4096-byte limit");
}

#[test]
fn parser_refuses_invalid_utf8_with_line_number() {
    rejects(b"# header\n127.0.0.1 \xff\n", 2, "not valid UTF-8");
}

#[test]
fn parser_refuses_address_without_names() {
    rejects(
        b"# header\n127.0.0.1 # no aliases\n",
        2,
        "at least one hostname",
    );
}

struct Repository(PathBuf);

impl Repository {
    fn new() -> Self {
        let path = cortexkit_test_support::scratch_root()
            .join(format!("motor-hosts-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&path).unwrap();
        let repo = Self(path);
        repo.git(&["init", "-q"]);
        repo.git(&["config", "user.name", "Hosts Test"]);
        repo.git(&["config", "user.email", "hosts@example.invalid"]);
        repo
    }

    fn git(&self, args: &[&str]) -> String {
        let args = args.iter().map(std::ffi::OsStr::new).collect::<Vec<_>>();
        String::from_utf8(super::super::javascript_git(&self.0, &args).unwrap())
            .unwrap()
            .trim()
            .to_owned()
    }

    fn write(&self, content: &[u8]) {
        fs::create_dir_all(self.0.join(".motor")).unwrap();
        fs::write(self.0.join(HOSTS_PATH), content).unwrap();
    }

    fn commit(&self) -> String {
        self.git(&["add", "."]);
        self.git(&["commit", "-q", "--allow-empty", "-m", "fixture"]);
        self.git(&["rev-parse", "HEAD^{tree}"])
    }

    fn entries(&self, tree: &str) -> Vec<String> {
        from_tree(&self.0, tree).unwrap()
    }

    fn refuses(&self, tree: &str, reason: &str) {
        let error = from_tree(&self.0, tree).unwrap_err().to_string();
        assert!(error.contains(".motor/hosts: line 1:"), "{error}");
        assert!(error.contains(reason), "{error}");
    }
}

impl Drop for Repository {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn tree_without_hosts_has_no_entries() {
    let repo = Repository::new();
    let tree = repo.commit();
    assert!(repo.entries(&tree).is_empty());
}

#[test]
fn tree_reads_snapshot_hosts_including_executable_blob() {
    let repo = Repository::new();
    repo.write(SPECIMEN.as_bytes());
    let tree = repo.commit();
    assert_eq!(repo.entries(&tree), [NORMALIZED]);
    repo.git(&["update-index", "--chmod=+x", HOSTS_PATH]);
    repo.git(&["commit", "-q", "-m", "executable"]);
    assert_eq!(
        repo.entries(&repo.git(&["rev-parse", "HEAD^{tree}"])),
        [NORMALIZED]
    );
}

#[test]
fn tree_reads_snapshot_without_head_commit() {
    let repo = Repository::new();
    repo.write(SPECIMEN.as_bytes());
    repo.git(&["add", "."]);
    let tree = repo.git(&["write-tree"]);
    assert_eq!(repo.entries(&tree), [NORMALIZED]);
}

#[test]
fn tree_ignores_server_file_absent_from_snapshot() {
    let repo = Repository::new();
    let tree = repo.commit();
    repo.write(SPECIMEN.as_bytes());
    assert!(repo.entries(&tree).is_empty());
}

#[test]
fn tree_ignores_server_edit_that_differs_from_snapshot() {
    let repo = Repository::new();
    repo.write(SPECIMEN.as_bytes());
    let tree = repo.commit();
    repo.write(b"8.8.8.8 server-edit.example\n");
    assert_eq!(repo.entries(&tree), [NORMALIZED]);
}

#[test]
fn tree_refuses_symlink_hosts() {
    let repo = Repository::new();
    fs::create_dir_all(repo.0.join(".motor")).unwrap();
    fs::write(repo.0.join("outside-hosts"), SPECIMEN).unwrap();
    symlink("../outside-hosts", repo.0.join(HOSTS_PATH)).unwrap();
    let tree = repo.commit();
    repo.refuses(&tree, "regular blob");
}

#[test]
fn tree_refuses_gitlink_hosts() {
    let repo = Repository::new();
    repo.commit();
    let commit = repo.git(&["rev-parse", "HEAD"]);
    repo.git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        "160000",
        &commit,
        HOSTS_PATH,
    ]);
    repo.git(&["commit", "-q", "-m", "gitlink"]);
    repo.refuses(&repo.git(&["rev-parse", "HEAD^{tree}"]), "regular blob");
}

#[test]
fn tree_refuses_directory_hosts() {
    let repo = Repository::new();
    fs::create_dir_all(repo.0.join(HOSTS_PATH)).unwrap();
    fs::write(repo.0.join(HOSTS_PATH).join("entry"), SPECIMEN).unwrap();
    let tree = repo.commit();
    repo.refuses(&tree, "regular blob");
}

#[test]
fn tree_refuses_oversized_hosts_before_decoding() {
    let repo = Repository::new();
    repo.write(&vec![0xff; 4097]);
    let tree = repo.commit();
    repo.refuses(&tree, "4096-byte limit");
}

#[test]
fn tree_refuses_invalid_utf8_hosts() {
    let repo = Repository::new();
    repo.write(b"\xff");
    let tree = repo.commit();
    repo.refuses(&tree, "not valid UTF-8");
}
