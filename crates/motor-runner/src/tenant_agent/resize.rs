//! Activation runs after attachment, while the helper still withholds the wake
//! reply and all Run traffic. The old runner must be idle before it is stopped.
use super::*;
use serde::Deserialize;
use std::{future::Future, os::unix::fs::OpenOptionsExt};

const MAX_ARGS_BYTES: usize = 16 * 1024;

#[derive(Deserialize)]
#[serde(tag = "op", deny_unknown_fields)]
enum Request {
    #[serde(rename = "apply_resize")]
    ApplyResize { runner_args: String },
}

pub(super) fn parse(value: Value) -> io::Result<String> {
    let Request::ApplyResize { runner_args } = serde_json::from_value(value)?;
    if runner_args.len() > MAX_ARGS_BYTES {
        return Err(io::Error::other("resize runner_args exceeds 16384 bytes"));
    }
    RunnerArgs::parse(&runner_args)?;
    Ok(runner_args)
}

async fn activate<F, Fut, A, AFut>(mut control: F, apply: A) -> io::Result<Value>
where
    F: FnMut(Frame) -> Fut,
    Fut: Future<Output = io::Result<Value>>,
    A: FnOnce() -> AFut,
    AFut: Future<Output = io::Result<()>>,
{
    if let Some(reason) = snapshot::busy(&control(Frame::Status).await?) {
        return Err(io::Error::other(format!("resize_busy: {reason}")));
    }
    let drained = control(Frame::Drain {
        deadline_s: 1,
        exit_after: false,
    })
    .await?;
    if drained["ok"] != true
        || drained["timed_out"] != false
        || drained["restart_recoverable_jobs"] != 0
    {
        control(Frame::Resume).await?;
        return Err(io::Error::other(
            "resize_busy: runner did not drain all accepted work",
        ));
    }
    // Drain closes admission under the runner lock. Recheck accepted jobs that
    // raced the first status; never stop a runner that owns work.
    if let Some(reason) = snapshot::busy(&control(Frame::Status).await?) {
        control(Frame::Resume).await?;
        return Err(io::Error::other(format!("resize_busy: {reason}")));
    }
    apply().await?;
    let health = control(Frame::Status).await?;
    if health["ok"] != true || !health["health"].is_object() {
        return Err(io::Error::other("resize runner health unavailable"));
    }
    Ok(json!({"ok":true}))
}

pub(super) async fn apply(text: String) -> io::Result<Value> {
    if !Path::new("/run/motor-tenant-generation").exists() {
        return Err(io::Error::other("resize requires tenant attachment"));
    }
    let previous = RunnerArgs::read_text(Path::new(ARGS))?;
    reversible(
        super::forward,
        || async move { apply_arguments(text).await },
        || async move { apply_arguments(previous).await },
    )
    .await
}

async fn apply_arguments(text: String) -> io::Result<()> {
    tokio::task::spawn_blocking(move || {
        command("systemctl", &["stop", "motor-tenant-runner.service"])?;
        let mut disk = Disk::new();
        steps(|step| disk.run(step, &text))
    })
    .await
    .map_err(io::Error::other)?
}

async fn reversible<F, Fut, A, AFut, R, RFut>(
    control: F,
    apply: A,
    rollback: R,
) -> io::Result<Value>
where
    F: FnMut(Frame) -> Fut,
    Fut: Future<Output = io::Result<Value>>,
    A: FnOnce() -> AFut,
    AFut: Future<Output = io::Result<()>>,
    R: FnOnce() -> RFut,
    RFut: Future<Output = io::Result<()>>,
{
    let changed = std::sync::atomic::AtomicBool::new(false);
    let result = activate(control, || async {
        changed.store(true, std::sync::atomic::Ordering::Release);
        apply().await
    })
    .await;
    if result.is_err() && changed.load(std::sync::atomic::Ordering::Acquire) {
        // A failed activation must not make its arguments the next boot's
        // default. Restore the old file with fsync, rename and parent fsync,
        // then start its runner before reporting the activation failure.
        rollback().await?;
    }
    result
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Mount,
    Grow,
    Write,
    FileSync,
    Rename,
    DirectorySync,
    Syncfs,
    Start,
}

fn steps(mut run: impl FnMut(Step) -> io::Result<()>) -> io::Result<()> {
    for step in [
        Step::Mount,
        Step::Grow,
        Step::Write,
        Step::FileSync,
        Step::Rename,
        Step::DirectorySync,
        Step::Syncfs,
        Step::Start,
    ] {
        run(step).map_err(|error| io::Error::other(format!("resize {step:?}: {error}")))?;
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct Geometry {
    block_size: u64,
    blocks: u64,
}

impl Geometry {
    fn parse(info: &str) -> io::Result<Self> {
        let data = info
            .lines()
            .find(|line| line.starts_with("data "))
            .ok_or_else(|| io::Error::other("xfs_info has no data geometry"))?;
        let number = |key: &str| -> io::Result<u64> {
            data.split_whitespace()
                .find_map(|word| word.strip_prefix(key))
                .ok_or_else(|| io::Error::other("missing XFS geometry field"))?
                .trim_end_matches(',')
                .parse()
                .map_err(io::Error::other)
        };
        let geometry = Self {
            block_size: number("bsize=")?,
            blocks: number("blocks=")?,
        };
        if geometry.block_size == 0 || geometry.blocks == 0 {
            return Err(io::Error::other("zero XFS geometry"));
        }
        Ok(geometry)
    }

    fn needs_grow(&self, device_bytes: u64) -> bool {
        device_bytes / self.block_size > self.blocks
    }
}

fn grow_if_needed(
    geometry: &Geometry,
    bytes: u64,
    grow: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    if geometry.needs_grow(bytes) {
        grow()?;
    }
    Ok(())
}

struct Disk {
    directory: Option<paths::Directory>,
    temporary: Option<fs::File>,
}

impl Disk {
    fn new() -> Self {
        Self {
            directory: None,
            temporary: None,
        }
    }

    fn run(&mut self, step: Step, text: &str) -> io::Result<()> {
        match step {
            Step::Mount => {
                paths::Directory::absolute(Path::new("/tenant"), 0)?;
                if !snapshot::mounted("/tenant")? {
                    command("mount", &["-t", "xfs", "/dev/vdb", "/tenant"])?;
                }
                self.directory = Some(paths::Directory::absolute(Path::new("/tenant"), 0)?);
            }
            Step::Grow => {
                let device = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open("/dev/vdb")?;
                let mut bytes: u64 = 0;
                // BLKGETSIZE64 reports the patched block device's current size.
                if unsafe {
                    libc::ioctl(
                        device.as_raw_fd(),
                        0x80081272_u32 as libc::Ioctl,
                        &mut bytes,
                    )
                } < 0
                {
                    return Err(io::Error::last_os_error());
                }
                let info = Command::new("xfs_info").arg("/tenant").output()?;
                if !info.status.success() {
                    return Err(io::Error::other("xfs_info failed"));
                }
                let geometry = Geometry::parse(&String::from_utf8_lossy(&info.stdout))?;
                grow_if_needed(&geometry, bytes, || command("xfs_growfs", &["/tenant"]))?;
            }
            Step::Write => {
                // A fixed name is safe only with create_new: never follow a
                // tenant-created symlink or truncate a hard-linked file.
                let path = self
                    .directory
                    .as_ref()
                    .unwrap()
                    .fd_path()
                    .join("runner-args.resize");
                match fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(path)?;
                file.write_all(text.as_bytes())?;
                self.temporary = Some(file);
            }
            Step::FileSync => self.temporary.as_ref().unwrap().sync_all()?,
            Step::Rename => {
                let directory = self.directory.as_ref().unwrap().fd_path();
                fs::rename(
                    directory.join("runner-args.resize"),
                    directory.join("runner-args"),
                )?;
            }
            Step::DirectorySync => {
                fs::File::open(self.directory.as_ref().unwrap().fd_path())?.sync_all()?
            }
            Step::Syncfs => {
                if unsafe { libc::syncfs(self.temporary.as_ref().unwrap().as_raw_fd()) } < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Step::Start => {
                let args = RunnerArgs::parse(text)?;
                start_runner(&args, Path::new("/tenant/state/main/runner.sock"))?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, os::unix::fs::MetadataExt};

    fn args(weight: u32) -> String {
        format!("--backend host --tenant-vm --job-network veth --max-weight {weight} --rustc-slots {weight} --sccache-size-gib 1 --free-space-floor-bytes 1024 --workspace-root /Users --owned-workspace-root /Users --workspace-root /home --owned-workspace-root /home\n")
    }

    fn idle() -> Value {
        json!({"ok":true,"queue_depth":0,"running_jobs":[],"health":{"runner_version":"test"},"timed_out":false,"restart_recoverable_jobs":0})
    }

    #[test]
    fn resize_extension_is_strict_bounded_and_uses_tenant_args_validation() {
        let text = args(64);
        assert_eq!(
            parse(json!({"op":"apply_resize","runner_args":text})).unwrap(),
            text
        );
        for value in [
            json!({"op":"apply_resize","runner_args":args(64),"unknown":true}),
            json!({"op":"apply_resize","runner_args":args(64).repeat(100)}),
            json!({"op":"apply_resize","runner_args":args(64).replace("--max-weight 64", "--max-weight 4294967296")}),
            json!({"op":"apply_resize","runner_args":args(64).replace("--rustc-slots 64", "--rustc-slots 8")}),
            json!({"op":"apply_resize","runner_args":"--max-weight 64"}),
            json!({"op":"apply_resize","runner_args":[]}),
            json!({"op":"apply_resize_extra","runner_args":args(64)}),
        ] {
            assert!(parse(value.clone()).is_err(), "{value}");
        }
    }

    #[tokio::test]
    async fn resize_agent_channel_dispatches_the_strict_extension() {
        let (mut host, guest) = tokio::io::duplex(65536);
        let task = tokio::spawn(super::super::agent_session(
            guest,
            std::sync::Arc::new(tokio::sync::Mutex::new(())),
        ));
        super::super::write_json(
            &mut host,
            &json!({"op":"apply_resize","runner_args":args(64),"unknown":true}),
        )
        .await
        .unwrap();
        let reply = super::super::read_json(&mut host).await.unwrap().unwrap();
        assert_eq!(reply["ok"], false);
        assert!(
            reply["error"].as_str().unwrap().contains("unknown field"),
            "{reply}"
        );
        super::super::write_json(&mut host, &json!({"op":"apply_resize","runner_args":args(64).replace("--max-weight 64", "--max-weight 4294967296")})).await.unwrap();
        let reply = super::super::read_json(&mut host).await.unwrap().unwrap();
        assert_eq!(reply["ok"], false);
        assert!(
            reply["error"]
                .as_str()
                .unwrap()
                .contains("number too large"),
            "{reply}"
        );
        drop(host);
        task.await.unwrap().unwrap();
    }

    #[test]
    fn resize_geometry_grows_only_a_larger_device() {
        let geometry = Geometry::parse("meta-data=/dev/vdb isize=512\ndata     = bsize=4096   blocks=26214400, imaxpct=25\nlog =internal bsize=4096 blocks=16384\n").unwrap();
        assert_eq!(
            geometry,
            Geometry {
                block_size: 4096,
                blocks: 26214400
            }
        );
        assert!(!geometry.needs_grow(100 << 30));
        assert!(!geometry.needs_grow((100 << 30) + 4095));
        assert!(geometry.needs_grow(600 << 30));
        assert!(!geometry.needs_grow(99 << 30));
        for info in [
            "",
            "data = bsize=0 blocks=1",
            "data = bsize=4096 blocks=oops",
        ] {
            assert!(Geometry::parse(info).is_err());
        }
    }

    #[test]
    fn resize_redelivery_does_not_grow_matching_geometry() {
        let mut grows = 0;
        for blocks in [26214400, 157286400, 157286400] {
            let geometry = Geometry {
                block_size: 4096,
                blocks,
            };
            grow_if_needed(&geometry, 600 << 30, || {
                grows += 1;
                Ok(())
            })
            .unwrap();
        }
        assert_eq!(grows, 1);
    }

    #[tokio::test]
    async fn resize_busy_runner_is_never_stopped() {
        for busy in [
            json!({"ok":true,"queue_depth":1,"running_jobs":[]}),
            json!({"ok":true,"queue_depth":0,"running_jobs":[{"job_id":"accepted"}]}),
            json!({"ok":true}),
        ] {
            let calls = RefCell::new(vec![]);
            let reply = agent_result(
                activate(
                    |frame| {
                        calls.borrow_mut().push(frame);
                        std::future::ready(Ok(busy.clone()))
                    },
                    || async { panic!("busy runner must not be stopped") },
                )
                .await,
            );
            assert_eq!(reply["ok"], false);
            assert!(reply["error"].as_str().unwrap().starts_with("resize_busy:"));
            assert_eq!(*calls.borrow(), vec![Frame::Status]);
        }
    }

    #[tokio::test]
    async fn resize_admission_race_is_refused_before_stop() {
        let calls = RefCell::new(vec![]);
        let reply = agent_result(
            activate(
                |frame| {
                    calls.borrow_mut().push(frame);
                    std::future::ready(Ok(if calls.borrow().len() == 3 {
                        json!({"ok":true,"queue_depth":1,"running_jobs":[]})
                    } else {
                        idle()
                    }))
                },
                || async { panic!("a job accepted before drain must prevent stop") },
            )
            .await,
        );
        assert_eq!(reply["ok"], false);
        assert_eq!(
            *calls.borrow(),
            vec![
                Frame::Status,
                Frame::Drain {
                    deadline_s: 1,
                    exit_after: false
                },
                Frame::Status,
                Frame::Resume
            ]
        );
    }

    #[tokio::test]
    async fn resize_requires_a_complete_drain_not_only_empty_status() {
        for drain in [
            json!({"ok":true,"timed_out":true,"restart_recoverable_jobs":0}),
            json!({"ok":true,"timed_out":false,"restart_recoverable_jobs":1}),
            json!({"ok":true}),
            json!({"ok":false}),
        ] {
            let reply = agent_result(
                activate(
                    |frame| {
                        std::future::ready(Ok(if matches!(frame, Frame::Drain { .. }) {
                            drain.clone()
                        } else {
                            idle()
                        }))
                    },
                    || async { panic!("incomplete drain must prevent stop") },
                )
                .await,
            );
            assert_eq!(reply["ok"], false);
            assert_eq!(
                reply["error"],
                "resize_busy: runner did not drain all accepted work"
            );
        }
    }

    #[tokio::test]
    async fn resize_keeps_real_runner_admission_closed_through_apply() {
        let scratch = cortexkit_test_support::ScratchDir::new("resize-drain-admission");
        let root = scratch.join("root");
        let mut config = crate::Config::new(root.join("state"), "resize-test");
        config.manual_tick = true;
        config.free_space_floor_bytes = 0;
        config.reserve_daemon_core = false;
        config.cpu_topology = Some(crate::cpu::CpuTopology::new(vec![vec![0]]).unwrap());
        config.workspace_roots = vec![root.clone()];
        config.owned_workspace_roots = vec![root.clone()];
        let work = root.join("work");
        fs::create_dir_all(&work).unwrap();
        let work = work.to_string_lossy().into_owned();
        let request = motor_protocol::Run {
            job_id: motor_protocol::Uuid::now_v7(),
            gate_job: false,
            network: None,
            platform: None,
            workspace_key: work.clone(),
            workspace_kind: None,
            repository_root: work.clone(),
            cwd: work,
            command: "true".into(),
            env: Default::default(),
            weight_hint: Some(1),
            timeout: None,
            return_paths: vec![],
            return_max_bytes: motor_protocol::RETURN_MAX_BYTES,
            return_chunked: false,
            queue_wait_limit_s: 300,
            snapshot_commit: "0".repeat(40),
            tree_hash: "0".repeat(40),
            integration_tip: "0".repeat(40),
            siblings: vec![],
            bundles: vec![],
            git_dependencies: vec![],
            base_commit: None,
        };
        let runner = crate::Runner::open(config).unwrap();
        let applying = runner.clone();
        let result = RefCell::new(None);
        let reply = activate(|frame| {
            let value = match frame {
                Frame::Status => idle(),
                Frame::Drain {deadline_s, exit_after} => {
                    let waiter = runner.begin_drain_request(std::time::Duration::from_secs(deadline_s), exit_after);
                    let drained = runner.drain_reply_for(&waiter).unwrap();
                    *result.borrow_mut() = Some(drained);
                    json!({"ok":true,"timed_out":drained.0,"restart_recoverable_jobs":drained.1})
                }
                other => panic!("unexpected frame: {other:?}"),
            };
            std::future::ready(Ok(value))
        }, move || async move {
            assert!(matches!(applying.accept(request).unwrap(), crate::engine::Admission::Refused(motor_protocol::RefusalReason::RunnerDraining)));
            Ok(())
        }).await.unwrap();
        assert_eq!(reply, json!({"ok":true}));
        assert_eq!(result.into_inner(), Some((false, 0)));
        runner.resume();
    }

    #[tokio::test]
    async fn resize_reply_follows_all_barriers_start_and_health() {
        let events = RefCell::new(vec![]);
        let reply = activate(
            |frame| {
                events.borrow_mut().push(format!("{frame:?}"));
                std::future::ready(Ok(idle()))
            },
            || async {
                steps(|step| {
                    events.borrow_mut().push(format!("{step:?}"));
                    Ok(())
                })
            },
        )
        .await
        .unwrap();
        assert_eq!(reply, json!({"ok":true}));
        assert_eq!(
            *events.borrow(),
            [
                "Status",
                "Drain { deadline_s: 1, exit_after: false }",
                "Status",
                "Mount",
                "Grow",
                "Write",
                "FileSync",
                "Rename",
                "DirectorySync",
                "Syncfs",
                "Start",
                "Status"
            ]
        );
    }

    #[tokio::test]
    async fn resize_grow_write_and_every_barrier_failure_refuses_before_start() {
        for failure in [
            Step::Mount,
            Step::Grow,
            Step::Write,
            Step::FileSync,
            Step::Rename,
            Step::DirectorySync,
            Step::Syncfs,
            Step::Start,
        ] {
            let seen = RefCell::new(vec![]);
            let reply = agent_result(
                activate(
                    |_| std::future::ready(Ok(idle())),
                    || async {
                        steps(|step| {
                            seen.borrow_mut().push(step);
                            if step == failure {
                                Err(io::Error::other("injected failure"))
                            } else {
                                Ok(())
                            }
                        })
                    },
                )
                .await,
            );
            assert_eq!(reply["ok"], false, "{failure:?}");
            assert!(reply["error"]
                .as_str()
                .unwrap()
                .contains(&format!("{failure:?}")));
            assert_eq!(seen.borrow().last(), Some(&failure));
            if failure != Step::Start {
                assert!(!seen.borrow().contains(&Step::Start));
            }
        }
    }

    #[tokio::test]
    async fn resize_missing_new_runner_health_refuses() {
        let mut calls = 0;
        let reply = agent_result(
            activate(
                |_| {
                    calls += 1;
                    std::future::ready(Ok(if calls == 4 {
                        json!({"ok":true})
                    } else {
                        idle()
                    }))
                },
                || async { Ok(()) },
            )
            .await,
        );
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"], "resize runner health unavailable");
    }

    #[tokio::test]
    async fn resize_failed_start_or_health_restores_previous_arguments_but_busy_does_not_restart() {
        for fail_start in [false, true] {
            let arguments = RefCell::new("previous");
            let mut controls = 0;
            let result = reversible(
                |_| {
                    controls += 1;
                    std::future::ready(Ok(if controls == 4 {
                        json!({"ok":false})
                    } else {
                        idle()
                    }))
                },
                || async {
                    *arguments.borrow_mut() = "new";
                    if fail_start {
                        Err(io::Error::other("start failed"))
                    } else {
                        Ok(())
                    }
                },
                || async {
                    *arguments.borrow_mut() = "previous";
                    Ok(())
                },
            )
            .await;
            assert!(result.is_err());
            assert_eq!(*arguments.borrow(), "previous");
        }
        assert!(reversible(
            |_| std::future::ready(Ok(json!({"ok":true,"queue_depth":1,"running_jobs":[]}))),
            || async { panic!("busy runner must not be stopped") },
            || async { panic!("busy runner must not be restarted") }
        )
        .await
        .is_err());
    }

    #[test]
    fn resize_redelivery_rewrites_identical_args_without_following_links() {
        use std::os::unix::fs::PermissionsExt;
        let temporary = cortexkit_test_support::ScratchDir::new("resize-redelivery");
        let root = temporary.join("tenant");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let victim = root.join("victim");
        fs::write(&victim, "untouched").unwrap();
        std::os::unix::fs::symlink(&victim, root.join("runner-args.resize")).unwrap();
        std::os::unix::fs::symlink(&victim, root.join("runner-args")).unwrap();
        let text = args(64);
        let mut previous_inode = 0;
        for _ in 0..2 {
            let mut disk = Disk::new();
            disk.directory =
                Some(paths::Directory::open(&root, unsafe { libc::geteuid() }).unwrap());
            for step in [
                Step::Write,
                Step::FileSync,
                Step::Rename,
                Step::DirectorySync,
                Step::Syncfs,
            ] {
                disk.run(step, &text).unwrap();
            }
            assert_eq!(fs::read_to_string(root.join("runner-args")).unwrap(), text);
            let metadata = fs::symlink_metadata(root.join("runner-args")).unwrap();
            assert!(metadata.is_file());
            assert_ne!(metadata.ino(), previous_inode);
            previous_inode = metadata.ino();
            assert_eq!(metadata.mode() & 0o777, 0o600);
            assert!(!root.join("runner-args.resize").exists());
        }
        assert_eq!(fs::read_to_string(victim).unwrap(), "untouched");
    }
}
