//! Privileged no-KVM tests run in disposable mount, PID and network namespaces.
use super::*;
use cortexkit_test_support::ScratchDir;
use std::os::unix::fs::PermissionsExt;

const CHILD: &str = "MOTOR_VM_GUEST_ROOT_TEST_CHILD";
const ROOT: &str = "MOTOR_VM_GUEST_ROOT_TEST_DIRECTORY";

fn isolated(name: &str, test: impl FnOnce()) {
    assert_eq!(
        unsafe { libc::geteuid() },
        0,
        "linux-root-tests requires root"
    );
    if std::env::var(CHILD).as_deref() == Ok(name) {
        fixture(Path::new(&std::env::var(ROOT).unwrap()));
        test();
        return;
    }
    // Only the outer process allocates under TMPDIR; the child uses this path before chroot.
    let directory = ScratchDir::new("motor-vm-guest-root");
    let root = directory.path().join("root");
    fs::create_dir(&root).unwrap();
    fs::write(directory.path().join("host-canary"), "host-secret").unwrap();
    let mut cmd = Command::new("/usr/bin/unshare");
    cmd.args(["--mount", "--pid", "--fork", "--net", "--mount-proc", "--"])
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, name)
        .env(ROOT, &root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        cmd.pre_exec(|| cvt(libc::setsid()));
    }
    let mut child = cmd.spawn().expect("root tests require util-linux unshare");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.wait();
            panic!("root isolation test timed out: {name}");
        }
        thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{name}: missing isolation prerequisites or failed assertion\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture(root: &Path) {
    assert_eq!(
        fs::read(root.parent().unwrap().join("host-canary")).unwrap(),
        b"host-secret"
    );
    cvt(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) }).unwrap();
    fs::set_permissions(root, fs::Permissions::from_mode(0o755)).unwrap();
    let mut systems = Mounts(Vec::new());
    for directory in ["bin", "usr", "lib", "lib64", "proc"] {
        let source = Path::new("/").join(directory);
        if directory == "lib64" && !source.exists() {
            continue;
        }
        systems
            .mount(
                &fs::canonicalize(source).unwrap(),
                &root.join(directory),
                true,
            )
            .unwrap();
        if directory != "proc" {
            let target = cstr(&root.join(directory)).unwrap();
            cvt(unsafe {
                libc::mount(
                    std::ptr::null(),
                    target.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND
                        | libc::MS_REMOUNT
                        | libc::MS_RDONLY
                        | libc::MS_NOSUID
                        | libc::MS_NODEV,
                    std::ptr::null(),
                )
            })
            .unwrap();
        }
    }
    for directory in [
        "etc/motor-vm",
        "opt/motor/bin",
        "run/motor-vm/fetch",
        "run/motor-vm/cargo",
        "run/motor-vm/target",
        "job/workspace",
        "tree/workspace",
    ] {
        fs::create_dir_all(root.join(directory)).unwrap();
    }
    fs::create_dir(root.join("dev")).unwrap();
    File::create(root.join("dev/null")).unwrap();
    cvt(unsafe {
        libc::mount(
            c"/dev/null".as_ptr(),
            cstr(&root.join("dev/null")).unwrap().as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        )
    })
    .unwrap();
    fs::write(
        root.join("etc/motor-vm/cargo-config.toml"),
        "[net]\nretry=2\n",
    )
    .unwrap();
    // The shell fixture at the immutable image path identifies which executable was launched.
    fs::write(
        root.join("opt/motor/bin/cargo"),
        "#!/bin/sh\nprintf trusted-image-cargo\n",
    )
    .unwrap();
    fs::set_permissions(
        root.join("opt/motor/bin/cargo"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    cvt(unsafe { libc::chroot(cstr(root).unwrap().as_ptr()) }).unwrap();
    std::env::set_current_dir("/").unwrap();
    // These system mounts die with the private mount namespace; the outer process owns the directory.
    std::mem::forget(systems);
    for directory in [
        FETCH_CWD,
        CARGO_HOME,
        TARGET,
        "/tree/workspace",
        "/job/workspace",
    ] {
        cvt(unsafe { libc::chown(cstr(Path::new(directory)).unwrap().as_ptr(), 1000, 1000) })
            .unwrap();
    }
}

fn job(script: &str) -> Start {
    Start {
        realtime_seconds: 1,
        realtime_nanoseconds: 0,
        entropy: [0; 32],
        trees: vec![crate::TreeMount {
            subdirectory: "workspace".into(),
            destination: "/job/workspace".into(),
        }],
        env: BTreeMap::new(),
        vcpus: 1,
        proxy_port: None,
        mode: Mode::Job {
            command: vec!["/bin/sh".into(), "-c".into(), script.into()],
            cwd: "/job/workspace".into(),
        },
    }
}

#[test]
fn command_is_uid_1000_new_session_without_supplementary_groups() {
    isolated(
        "linux::root_tests::command_is_uid_1000_new_session_without_supplementary_groups",
        || {
            let s = job(&format!("id -u; id -G; ps -o sid= -p $$; echo $$; grep NoNewPrivs /proc/self/status; if kill -TERM {}; then exit 9; fi", std::process::id()));
            s.validate().unwrap();
            let (mut cmd, _) = command(&s).unwrap();
            let output = cmd.output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let text = String::from_utf8(output.stdout).unwrap();
            let lines: Vec<_> = text.lines().collect();
            assert_eq!(lines[0], "1000");
            assert_eq!(lines[1], "1000");
            assert_eq!(lines[2].trim(), lines[3]);
            assert_eq!(lines[4], "NoNewPrivs:\t1");
        },
    );
}

#[test]
fn fetch_runs_image_cargo_even_when_workspace_provides_cargo() {
    isolated(
        "linux::root_tests::fetch_runs_image_cargo_even_when_workspace_provides_cargo",
        || {
            let workspace = Path::new("/tree/workspace");
            fs::write(workspace.join("Cargo.toml"), "[workspace]\n").unwrap();
            fs::write(
                workspace.join("Cargo.lock"),
                "version=3\n[[package]]\nname='fixture'\nversion='0.1.0'\n",
            )
            .unwrap();
            fs::write(
                workspace.join("cargo"),
                "#!/bin/sh\necho untrusted > /run/motor-vm/cargo/marker\nexit 77\n",
            )
            .unwrap();
            fs::set_permissions(workspace.join("cargo"), fs::Permissions::from_mode(0o755))
                .unwrap();
            let mut s = job("true");
            s.proxy_port = Some(1024);
            s.mode = Mode::Fetch {
                manifest: "/job/workspace/Cargo.toml".into(),
                git_directories: BTreeMap::new(),
            };
            s.validate().unwrap();
            let mut mounts = Mounts(Vec::new());
            mounts
                .mount(workspace, Path::new("/job/workspace"), true)
                .unwrap();
            // Put the hostile executable first even in the image PATH; fetch must use its absolute binary.
            std::env::set_var("PATH", "/job/workspace:/opt/motor/bin:/usr/bin:/bin");
            let (mut cmd, policy) = command(&s).unwrap();
            let output = cmd.output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, b"trusted-image-cargo");
            assert!(!Path::new(CARGO_HOME).join("marker").exists());
            policy
                .unwrap()
                .validate(Path::new(CARGO_HOME), Path::new(IMAGE_GIT))
                .unwrap();
            mounts.finish().unwrap();
        },
    );
}

#[test]
fn job_mounts_sync_unmount_and_refuse_symlink_components_without_host_access() {
    isolated("linux::root_tests::job_mounts_sync_unmount_and_refuse_symlink_components_without_host_access", || {
        let root = PathBuf::from(std::env::var(ROOT).unwrap());
        let host_canary = root.parent().unwrap().join("host-canary");
        assert!(!host_canary.exists(), "host path escaped the chroot");
        let mut mounts = Mounts(Vec::new());
        std::os::unix::fs::symlink("/opt/motor", "/job/escape").unwrap();
        assert!(mounts.mount(Path::new("/tree/workspace"), Path::new("/job/escape/bin"), true).is_err());
        assert!(mounts.0.is_empty());
        mounts.mount(Path::new("/tree/workspace"), Path::new("/job/workspace"), true).unwrap();
        let s = job("printf durable > new");
        s.validate().unwrap();
        let (mut cmd, _) = command(&s).unwrap();
        assert!(cmd.output().unwrap().status.success());
        mounts.finish().unwrap();
        assert!(mounts.0.is_empty());
        assert_eq!(fs::read("/tree/workspace/new").unwrap(), b"durable");
        assert!(!Path::new("/job/workspace/new").exists(), "bind mount remained after finish");
    });
}

#[test]
fn finish_requires_mount_references_to_be_closed_before_unmount() {
    isolated(
        "linux::root_tests::finish_requires_mount_references_to_be_closed_before_unmount",
        || {
            fs::write("/tree/workspace/marker", b"mounted").unwrap();
            let mut mounts = Mounts(Vec::new());
            mounts
                .mount(
                    Path::new("/tree/workspace"),
                    Path::new("/job/workspace"),
                    true,
                )
                .unwrap();
            let reference = File::open("/job/workspace").unwrap();
            // A genuine live mount reference must fail rather than be hidden by lazy detachment.
            assert_eq!(
                mounts.finish().unwrap_err().raw_os_error(),
                Some(libc::EBUSY)
            );
            assert_eq!(fs::read("/job/workspace/marker").unwrap(), b"mounted");
            assert_eq!(mounts.0, [PathBuf::from("/job/workspace")]);
            drop(reference);
            mounts.finish().unwrap();
            assert!(mounts.0.is_empty());
            assert!(!Path::new("/job/workspace/marker").exists());
            assert_eq!(fs::read("/tree/workspace/marker").unwrap(), b"mounted");
        },
    );
}

#[test]
fn cleanup_reaps_uid_1000_descendants_that_escape_the_job_session() {
    isolated(
        "linux::root_tests::cleanup_reaps_uid_1000_descendants_that_escape_the_job_session",
        || {
            // A private PID namespace keeps uid-wide cleanup from reaching unrelated host processes.
            let s = job("setsid /bin/sh -c 'echo $$ > descendant; sleep 60' & while [ ! -s descendant ]; do sleep 0.01; done; exit 0");
            s.validate().unwrap();
            let (mut cmd, _) = command(&s).unwrap();
            let mut child = cmd.spawn().unwrap();
            let mut wire = Vec::new();
            assert_eq!(
                supervise(&mut child, &mut wire, reap_jobs).unwrap(),
                Termination::Exit { code: 0 }
            );
            let descendant = fs::read_to_string("/job/workspace/descendant").unwrap();
            assert!(!Path::new("/proc").join(descendant.trim()).exists());
            for entry in fs::read_dir("/proc").unwrap() {
                let entry = entry.unwrap();
                if let Some(pid) = entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.parse::<i32>().ok())
                {
                    assert!(
                        !has_job_uid(pid).unwrap(),
                        "uid-1000 process survived cleanup"
                    );
                }
            }
        },
    );
}
