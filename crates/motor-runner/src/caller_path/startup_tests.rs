use super::startup::*;
use crate::{BackendKind, Config};
use std::{fs, path::Path};

fn config() -> Config {
    let mut config = Config::new("/state", "main");
    config.isolate_jobs = true;
    config
        .caller_roots
        .push(CallerRoot::parse("/home=/callers/home").unwrap());
    config
}

#[test]
fn caller_root_spelling_and_option_refusals_name_both_paths() {
    for (caller, host) in [
        ("home", "/host"),
        ("/", "/host"),
        ("/home", "host"),
        ("/home", "/host/./x"),
        ("/home", "/host/../x"),
        ("/home", "/host//x"),
        ("/home", "/host/"),
        ("/home/./x", "/host"),
        ("/home//x", "/host"),
        ("/home/", "/host"),
    ] {
        let error = CallerRoot::parse(&format!("{caller}={host}"))
            .unwrap_err()
            .to_string();
        assert!(error.contains(caller) && error.contains(host), "{error}");
    }
    type RefusalCase = (&'static str, &'static str, fn(&mut Config));
    let cases: Vec<RefusalCase> = vec![
        ("/home", "/home", |c| {
            c.caller_roots
                .push(CallerRoot::parse("/home=/other").unwrap())
        }),
        ("/home", "/home/u", |c| {
            c.caller_roots
                .push(CallerRoot::parse("/home/u=/other").unwrap())
        }),
        ("/callers/home", "/callers", |c| {
            c.caller_roots
                .push(CallerRoot::parse("/data=/callers").unwrap())
        }),
        ("/home", "/home/u", |c| {
            c.owned_workspace_roots.push("/home/u".into())
        }),
        ("/home", "/home", |c| c.workspace_roots.push("/home".into())),
        ("/Users", "/Users", |c| {
            c.caller_roots[0].caller_prefix = "/Users".into()
        }),
        ("/callers/home", "/callers", |c| {
            c.owned_workspace_roots.push("/callers".into())
        }),
        ("/state/main/home", "/state/main", |c| {
            c.caller_roots[0].host_root = "/state/main/home".into()
        }),
        ("/state", "/state/main", |c| {
            c.caller_roots[0].host_root = "/state".into()
        }),
        ("/home", "/callers/home", |c| c.isolate_jobs = false),
        ("/home", "/callers/home", |c| c.tenant_vm = true),
        ("/home", "/callers/home", |c| {
            c.backend = BackendKind::WindowsVm
        }),
        ("/home", "/callers/home", |c| {
            c.backend = BackendKind::MacosVm
        }),
    ];
    for (a, b, change) in cases {
        let mut c = config();
        change(&mut c);
        let error = c.check_options().unwrap_err().to_string();
        assert!(error.contains(a) && error.contains(b), "{a}, {b}: {error}");
    }
    let mut boat = config();
    boat.tenant_vm = true;
    boat.boat_profile = true;
    boat.check_options().unwrap();
    let mut portable = config();
    portable.isolate_jobs = false;
    portable.test_allow_unisolated_caller_roots = true;
    portable.check_options().unwrap();
}

#[test]
fn startup_finds_bind_sources_in_both_containment_directions() {
    let table = "10 1 8:1 / / rw - xfs /dev/root rw\n11 10 8:2 / /srv/build rw - xfs /dev/build rw\n12 10 8:2 /Users /Users rw - xfs /dev/build rw\n";
    for host in ["/srv/build/Users/hidden", "/srv/build", "/srv/build/Users"] {
        let mut c = config();
        c.caller_roots[0].host_root = host.into();
        let error = filesystem_with_mountinfo(&c, table)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(host) && error.contains("/srv/build/Users"),
            "{error}"
        );
    }
    let escaped = "12 10 8:2 /Users /srv/build\\040disk rw - xfs /dev/build rw\n";
    let parsed = mounts(escaped).unwrap();
    assert_eq!(parsed[0].mountpoint, Path::new("/srv/build disk"));
}

#[test]
fn startup_refuses_symlink_ancestors_without_following_them() {
    use std::os::unix::fs::symlink;
    let scratch = cortexkit_test_support::ScratchDir::new("motor-caller-startup-symlink");
    let outside = scratch.join("outside");
    fs::create_dir(&outside).unwrap();
    let link = scratch.join("link");
    symlink(&outside, &link).unwrap();
    let mut c = config();
    c.caller_roots[0].host_root = link.join("home");
    let error = filesystem_with_mountinfo(&c, "").unwrap_err().to_string();
    assert!(
        error.contains(link.to_str().unwrap()) && error.contains("symlink"),
        "{error}"
    );
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
}

#[test]
fn startup_reflink_probe_leaves_no_directory_entries() {
    let scratch = cortexkit_test_support::ScratchDir::new("motor-caller-startup-probe");
    let mut c = config();
    c.state_root = scratch.join("state");
    c.build_slot_root = scratch.join("slots");
    c.caller_roots[0].host_root = scratch.join("home");
    fs::create_dir(&c.caller_roots[0].host_root).unwrap();
    filesystem_with_mountinfo(&c, "").unwrap();
    assert!(!c.state_dir().exists());
    assert_eq!(
        fs::read_dir(&c.caller_roots[0].host_root).unwrap().count(),
        0
    );
    c.caller_roots[0].host_root = scratch.join("missing");
    let error = filesystem_with_mountinfo(&c, "").unwrap_err().to_string();
    assert!(
        error.contains("missing") && error.contains(c.state_dir().to_str().unwrap()),
        "{error}"
    );
    assert!(!c.state_dir().exists());
}
