use super::*;
use cortexkit_test_support::ScratchDir;
use std::{
    cell::RefCell,
    os::unix::{fs::symlink, process::ExitStatusExt},
    rc::Rc,
};

fn success(text: &str) -> std::process::Output {
    std::process::Output {
        status: std::process::ExitStatus::from_raw(0),
        stdout: text.as_bytes().to_vec(),
        stderr: Vec::new(),
    }
}

#[test]
fn ordinary_job_privileged_invocations_are_bounded() {
    let scratch = ScratchDir::new("motor-privilege-count");
    // A subprocess supplies private toolchain paths without changing the
    // environment of tests running concurrently in this process.
    if std::env::var_os("MOTOR_PRIVILEGE_COUNT_CHILD").is_none() {
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "isolation::tests::privilege::ordinary_job_privileged_invocations_are_bounded",
                "--nocapture",
            ])
            .env("MOTOR_PRIVILEGE_COUNT_CHILD", "1")
            .env("CARGO_HOME", scratch.join("cargo"))
            .env("RUSTUP_HOME", scratch.join("rustup"))
            .output()
            .unwrap();
        println!("{}", String::from_utf8_lossy(&result.stdout));
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }

    let workspace = scratch.join("checkouts/project");
    let target = scratch.join("targets/project");
    let state = scratch.join("state");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&target).unwrap();
    fs::create_dir_all(&state).unwrap();
    fs::create_dir_all(state.join("slots")).unwrap();
    let wrapper = scratch.join("slot-supervisor");
    fs::write(&wrapper, "slot supervisor fixture").unwrap();
    let observed = Rc::new(RefCell::new(Vec::new()));
    let recorded = observed.clone();
    crate::systemd::with_process_hook(
        Box::new(move |command| {
            let program = command.get_program().to_str().unwrap().to_owned();
            let args: Vec<String> = command
                .get_args()
                .map(|arg| arg.to_str().unwrap().to_owned())
                .collect();
            recorded.borrow_mut().push((program.clone(), args.clone()));
            match program.as_str() {
                "/usr/bin/id" if args[0] == "-u" => {
                    Ok(success(&format!("{}\n", unsafe { libc::geteuid() } + 1001)))
                }
                "/usr/bin/id" => Ok(success("motor-job\n")),
                "/usr/bin/systemctl" if args.iter().any(|arg| arg == "--property=MainPID") => {
                    Ok(success("123\n"))
                }
                "/usr/bin/systemctl" => Ok(success("LoadState=loaded\nActiveState=active\n")),
                "/usr/bin/sudo" => Ok(success("")),
                "git" => command.output(),
                _ => panic!("unexpected preparation command: {command:?}"),
            }
        }),
        || {
            let sandbox = Sandbox::prepare(
                Uuid::now_v7(),
                &state,
                &workspace,
                &target,
                &[],
                Some(&wrapper),
                &state.join("slots"),
                4,
            )
            .unwrap();
            // Count the real launcher command, not a second hand-written model of
            // its arguments. Only process execution is replaced by the seam.
            checked(
                &mut sandbox
                    .command("true", &workspace, &[], true, &[0, 1], 2)
                    .unwrap(),
            )
            .unwrap();
            assert!(crate::systemd::wait_until_active(
                &unit(sandbox.id),
                || Ok(false),
                |_| panic!("unit is already active")
            )
            .unwrap());
            allow_low_ports(sandbox.id).unwrap();
            for _ in 0..100 {
                assert!(unit_alive(sandbox.id).unwrap());
            }
            drop(sandbox);
        },
    );
    let observed = observed.borrow();
    let privileged: Vec<_> = observed
        .iter()
        .filter(|(program, _)| program == "/usr/bin/sudo")
        .collect();
    println!(
        "ordinary cold-target job including runtime reclamation: {} privileged invocations",
        privileged.len()
    );
    assert!(
        privileged.len() < 20,
        "expected fewer than 20 privileged invocations, got {}: {privileged:?}",
        privileged.len()
    );
    assert!(privileged
        .iter()
        .all(|(_, args)| args[1] != "/usr/bin/systemctl"));
    let root_chowns: Vec<_> = privileged
        .iter()
        .filter(|(_, args)| {
            args[1] == "/usr/bin/chown" && args.iter().any(|arg| arg == "root:root")
        })
        .collect();
    assert_eq!(root_chowns.len(), 1);
    let paths = root_chowns[0].1.split(|arg| arg == "--").nth(1).unwrap();
    assert!(
        paths.len() > 10,
        "fixture must contain multiple mount ancestors"
    );
    let unique: std::collections::BTreeSet<_> = paths.iter().collect();
    assert_eq!(paths.len(), unique.len());
    for (index, path) in paths.iter().enumerate() {
        if let Some(parent) = Path::new(path).parent().and_then(Path::to_str) {
            if let Some(parent_index) = paths.iter().position(|path| path == parent) {
                assert!(parent_index < index);
            }
        }
    }
}

#[test]
fn directory_batch_refuses_symlinks_before_any_privileged_change() {
    let scratch = ScratchDir::new("motor-directory-batch-symlink");
    let physical = scratch.join("physical");
    let link = scratch.join("link");
    fs::create_dir(&physical).unwrap();
    symlink(&physical, &link).unwrap();
    crate::systemd::with_process_hook(
        Box::new(|command| panic!("validation must precede commands: {command:?}")),
        || {
            let error =
                set_sandbox_directory_owner_modes(&[(&[physical, link], "0755")]).unwrap_err();
            assert!(error.to_string().contains("not a physical directory"));
        },
    );
}

#[test]
fn target_and_checkout_refuse_symlinks_before_any_privileged_change() {
    let scratch = ScratchDir::new("motor-target-symlink");
    let physical = scratch.join("physical");
    let link = scratch.join("link");
    fs::create_dir(&physical).unwrap();
    symlink(&physical, &link).unwrap();
    crate::systemd::with_process_hook(
        Box::new(|command| panic!("validation must precede commands: {command:?}")),
        || {
            assert!(prepare_job_writable_target(scratch.path(), &link).is_err());
            assert!(prepare_job_checkout_root(&link).is_err());
        },
    );
}

#[test]
fn writer_tree_operations_batch_all_paths_without_following_links() {
    let scratch = ScratchDir::new("motor-writer-tree-batch");
    let workspace = scratch.join("workspace");
    let target = workspace.join("target");
    let home = scratch.join("home");
    let tmp = scratch.join("tmp");
    for path in [&target, &home, &tmp] {
        fs::create_dir_all(path).unwrap();
    }
    let paths = [workspace, target, home, tmp];
    let expected = paths.clone();
    let operations = Rc::new(RefCell::new(Vec::new()));
    let recorded = operations.clone();
    crate::systemd::with_process_hook(
        Box::new(move |command| {
            assert_eq!(command.get_program(), "/usr/bin/sudo");
            let args: Vec<_> = command.get_args().collect();
            assert!(args.contains(&std::ffi::OsStr::new("-R")));
            assert!(args.contains(&std::ffi::OsStr::new("-P")));
            let separator = args.iter().position(|arg| *arg == "--").unwrap();
            assert_eq!(
                &args[separator + 1..],
                expected
                    .iter()
                    .map(|path| path.as_os_str())
                    .collect::<Vec<_>>()
            );
            recorded
                .borrow_mut()
                .push(args[1].to_string_lossy().into_owned());
            Ok(success(""))
        }),
        || {
            prepare_writer_trees(
                &paths.iter().map(PathBuf::as_path).collect::<Vec<_>>(),
                WriterAccount { uid: 123 },
            )
            .unwrap();
        },
    );
    assert_eq!(
        &*operations.borrow(),
        &["/usr/bin/chown", "/usr/bin/setfacl", "/usr/bin/chmod"]
    );
}
