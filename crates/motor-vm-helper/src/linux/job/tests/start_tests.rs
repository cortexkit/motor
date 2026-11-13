use super::exit_tests::{fixture, peer};
use super::*;
use std::sync::{mpsc, Arc, Barrier};

// Run the production reservation/launch boundary with fixture cgroups. The
// caller supplies the intent instead of replacing any permission checks in start.
fn admit(
    jobs: &LinuxJobs,
    intent: Record,
    launch: impl FnOnce() -> Result<()>,
) -> Result<Response> {
    let admission = jobs.admission.lock();
    jobs.start_available(&intent.start, intent.disk_device, intent.disk_inode)?;
    jobs.launch_record(admission, intent, |_| launch())
}

fn other_record(n: u64) -> Record {
    let mut intent = record();
    intent.start.job_id = job::test_id(n);
    intent.start.workspace_disk_id = format!("disk-{n}");
    intent.disk_inode += n;
    intent
}

fn pause(jobs: &LinuxJobs, step: ExitStep) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (entered, waiting) = mpsc::channel();
    let (release, released) = mpsc::channel();
    jobs.pause_exit_at(step, move || {
        entered.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(10)).unwrap();
    });
    (waiting, release)
}

#[test]
fn launch_does_not_hold_admission_or_delay_other_job_requests() {
    let vmm = Vmm::new();
    let jobs = Arc::new(fixture(&vmm.base));
    let mut finished = other_record(2);
    finished.released = true;
    finished.oom_kill = Some(0);
    jobs.save(&finished).unwrap();
    jobs.adopt_record(finished.clone()).unwrap();
    let relay = other_record(3);
    let group = jobs.group(&relay.start.job_id);
    std::fs::create_dir_all(&group).unwrap();
    std::fs::write(group.join("cgroup.procs"), vmm.child.id().to_string()).unwrap();
    jobs.save(&relay).unwrap();
    jobs.adopt_record(relay.clone()).unwrap();
    let root = jobs.root(&relay.start.job_id);
    std::fs::create_dir_all(&root).unwrap();
    let directory = File::open(&root).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(format!(
        "/proc/self/fd/{}/vsock.sock",
        directory.as_raw_fd()
    ))
    .unwrap();
    listener.set_nonblocking(true).unwrap();
    let relay_peer = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "relay never connected"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("relay accept: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut request = [0; 11];
        stream.read_exact(&mut request).unwrap();
        assert_eq!(&request, b"CONNECT 53\n");
        stream.write_all(b"OK 53\n").unwrap();
    });
    let (waiting, release) = pause(&jobs, ExitStep::Launch);
    let starting = jobs.clone();
    let start = std::thread::spawn(move || {
        admit(&starting, record(), || {
            starting.exit_checkpoint(ExitStep::Launch)
        })
    });
    waiting.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(
        jobs.view(&record().start.job_id).unwrap().state,
        State::Booting
    );
    let operations = [
        Operation::Stop(finished.start.job_id.clone()),
        Operation::Relay(relay.start.job_id, 53),
        Operation::AckExit(finished.pending_exit().unwrap()),
        Operation::WorkspaceUsage {
            account: "account-1".into(),
            disk_id: "idle-disk".into(),
        },
        Operation::Exits,
    ];
    let (sent, replies) = mpsc::channel();
    let mut threads = Vec::new();
    for (index, op) in operations.into_iter().enumerate() {
        let jobs = jobs.clone();
        let sent = sent.clone();
        threads.push(std::thread::spawn(move || {
            let result = jobs.handle(op, peer());
            sent.send((
                index,
                if index == 1 {
                    result.is_ok_and(|response| {
                        matches!(response.reply, Reply::Relay { fd_count: 1 })
                            && response.fds.len() == 1
                    })
                } else {
                    result.is_ok()
                },
            ))
            .unwrap();
        }));
    }
    let admitting = jobs.clone();
    threads.push(std::thread::spawn(move || {
        sent.send((5, admit(&admitting, other_record(4), || Ok(())).is_ok()))
            .unwrap();
    }));
    let mut results = Vec::new();
    for _ in 0..6 {
        match replies.recv_timeout(Duration::from_secs(1)) {
            Ok(result) => results.push(result),
            Err(_) => break,
        }
    }
    release.send(()).unwrap();
    start.join().unwrap().unwrap();
    for thread in threads {
        thread.join().unwrap();
    }
    relay_peer.join().unwrap();
    assert_eq!(results.len(), 6, "requests waited for launch: {results:?}");
    assert!(results.iter().all(|(_, ok)| *ok), "{results:?}");
}

#[test]
fn racing_starts_reserve_one_disk_or_one_id_exactly_once() {
    for same_id in [false, true] {
        let base = cortexkit_test_support::ScratchDir::new("start-reservation-race");
        let jobs = Arc::new(fixture(&base));
        let x = record();
        let mut y = other_record(2);
        if same_id {
            y.start.job_id = x.start.job_id.clone();
        } else {
            y.start.workspace_disk_id = x.start.workspace_disk_id.clone();
        }
        let barrier = Arc::new(Barrier::new(3));
        let threads: Vec<_> = [x, y]
            .into_iter()
            .map(|intent| {
                let jobs = jobs.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    admit(&jobs, intent, || Ok(()))
                })
            })
            .collect();
        barrier.wait();
        let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        let error = &results.iter().find_map(|r| r.as_ref().err()).unwrap().0;
        assert!(
            error.contains(if same_id {
                "job id already booted"
            } else {
                "workspace disk already attached"
            }),
            "{error}"
        );
        assert_eq!(jobs.records().len(), 1);
        assert_eq!(std::fs::read_dir(&jobs.records_path).unwrap().count(), 1);
    }
}

#[test]
fn finish_fill_waits_for_stop_without_holding_admission() {
    let base = cortexkit_test_support::ScratchDir::new("finish-fill-admission");
    let jobs = Arc::new(fixture(&base));
    let fill = job::Fill {
        account: "account-1".into(),
        fill_id: job::test_id(1),
        repository_key: "repo-1".into(),
        size_bytes: 1 << 30,
    };
    let mut intent = record();
    intent.start.kind = job::VmKind::Fill;
    intent.start.workspace_disk_id = fill.disk_id();
    jobs.save(&intent).unwrap();
    jobs.adopt_record(intent).unwrap();
    let (waiting, release) = pause(&jobs, ExitStep::Stop);
    let finishing = jobs.clone();
    let finish = std::thread::spawn(move || {
        finishing.handle(
            Operation::FinishFill {
                fill,
                published: false,
            },
            peer(),
        )
    });
    waiting.recv_timeout(Duration::from_secs(2)).unwrap();
    let (sent, reply) = mpsc::channel();
    let requesting = jobs.clone();
    let request = std::thread::spawn(move || {
        sent.send(requesting.handle(
            Operation::WorkspaceUsage {
                account: "account-1".into(),
                disk_id: "idle-disk".into(),
            },
            peer(),
        ))
        .unwrap();
    });
    let result = reply.recv_timeout(Duration::from_secs(1));
    release.send(()).unwrap();
    let _ = finish.join().unwrap();
    request.join().unwrap();
    result.expect("workspace waited for fill stop").unwrap();
}

fn removal_order(images: bool, removal_first: bool) {
    let base = cortexkit_test_support::ScratchDir::new("start-removal-order");
    let jobs = Arc::new(fixture(&base));
    let marker = base.join("resource");
    std::fs::write(&marker, "available").unwrap();
    let intent = record();
    let id = intent.start.job_id.clone();
    let remove = move |jobs: &LinuxJobs, operation: Box<dyn FnOnce() -> Result<()> + Send>| {
        if images {
            jobs.with_image_admission(
                crate::images::Operation::Remove(record().start.image_generation_id),
                |_| operation(),
            )
        } else {
            jobs.with_disk_admission(&record().start.workspace_disk_id, operation)
        }
    };
    if removal_first {
        let (entered, waiting) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let removing = jobs.clone();
        let path = marker.clone();
        let removal = std::thread::spawn(move || {
            remove(
                &removing,
                Box::new(move || {
                    std::fs::rename(&path, path.with_extension("deleted"))?;
                    File::open(path.parent().unwrap())?.sync_all()?;
                    entered.send(()).unwrap();
                    released.recv_timeout(Duration::from_secs(10)).unwrap();
                    Ok(())
                }),
            )
        });
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(!marker.exists());
        assert!(jobs.view(&id).is_none());
        let starting = jobs.clone();
        let (begun, begins) = mpsc::channel();
        let (sent, reply) = mpsc::channel();
        let start = std::thread::spawn(move || {
            begun.send(()).unwrap();
            let result = (|| {
                let admission = starting.admission.lock();
                starting.start_available(&intent.start, intent.disk_device, intent.disk_inode)?;
                // Model the production disk/image read inside the same A hold.
                std::fs::read(marker)?;
                starting.launch_record(admission, intent, |_| Ok(()))
            })();
            sent.send(result).unwrap();
        });
        begins.recv_timeout(Duration::from_secs(2)).unwrap();
        let early = reply.recv_timeout(Duration::from_millis(100));
        release.send(()).unwrap();
        removal.join().unwrap().unwrap();
        start.join().unwrap();
        assert!(matches!(early, Err(mpsc::RecvTimeoutError::Timeout)));
        let error = reply
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .err()
            .unwrap();
        assert!(error.0.contains("No such file"), "{error}");
        assert!(jobs.view(&id).is_none());
        assert!(!jobs.records_path.join(format!("{id}.json")).exists());
    } else {
        let (waiting, release) = pause(&jobs, ExitStep::Launch);
        let starting = jobs.clone();
        let path = marker.clone();
        let start = std::thread::spawn(move || {
            let admission = starting.admission.lock();
            starting.start_available(&intent.start, intent.disk_device, intent.disk_inode)?;
            std::fs::read(path)?;
            starting.launch_record(admission, intent, |_| {
                starting.exit_checkpoint(ExitStep::Launch)
            })
        });
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(jobs.view(&id).unwrap().state, State::Booting);
        let durable: Record = serde_json::from_reader(
            File::open(jobs.records_path.join(format!("{id}.json"))).unwrap(),
        )
        .unwrap();
        assert!(!durable.released);
        // These are the real request entry points. Reservation refusal precedes
        // privileged filesystem access, so the losing operations are portable.
        let result = if images {
            jobs.images(crate::images::Operation::Remove(
                record().start.image_generation_id,
            ))
        } else {
            jobs.handle(
                Operation::DeleteWorkspace {
                    account: record().start.account,
                    disk_id: record().start.workspace_disk_id,
                },
                peer(),
            )
        };
        release.send(()).unwrap();
        start.join().unwrap().unwrap();
        let error = result.err().unwrap();
        assert!(
            error.0.contains(if images {
                "image generation is held"
            } else {
                "workspace disk still held"
            }),
            "{error}"
        );
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "available");
    }
}

#[test]
fn start_and_workspace_deletion_refuse_the_later_operation() {
    for removal_first in [true, false] {
        removal_order(false, removal_first);
    }
}

#[test]
fn start_and_image_removal_never_both_succeed() {
    for removal_first in [true, false] {
        removal_order(true, removal_first);
    }
}

#[cfg(feature = "linux-root-tests")]
fn real_removal_before_start(images: bool) {
    assert_eq!(unsafe { libc::geteuid() }, 0);
    let base = cortexkit_test_support::ScratchDir::new("real-start-removal-order");
    let jobs = fixture(&base);
    let mut start = start();
    jobs.create_workspace(job::CreateWorkspace {
        account: start.account.clone(),
        workspace_disk_id: start.workspace_disk_id.clone(),
        size_bytes: 1 << 30,
        cache_write_job_id: None,
    })
    .unwrap();
    if images {
        start.image_generation_id = format!("start-race-{}", start.job_id);
        let root = Path::new(job::IMAGES);
        trusted_directory(root).unwrap();
        let generation = root.join(&start.image_generation_id);
        assert!(!generation.exists());
        std::fs::create_dir(&generation).unwrap();
        jobs.images(crate::images::Operation::Remove(
            start.image_generation_id.clone(),
        ))
        .unwrap();
        assert!(!generation.exists());
        assert!(root
            .join(format!(".{}.deleted", start.image_generation_id))
            .exists());
    } else {
        jobs.handle(
            Operation::DeleteWorkspace {
                account: start.account.clone(),
                disk_id: start.workspace_disk_id.clone(),
            },
            peer(),
        )
        .unwrap();
        assert!(!jobs.workspaces.join(&start.workspace_disk_id).exists());
    }
    let error = jobs
        .handle(Operation::Start(start.clone()), peer())
        .err()
        .unwrap();
    assert!(error.0.contains("No such file"), "{error}");
    assert!(jobs.view(&start.job_id).is_none());
    assert!(!jobs
        .records_path
        .join(format!("{}.json", start.job_id))
        .exists());
    if images {
        crate::linux::images::finish_remove(&start.image_generation_id).unwrap();
    }
}

#[cfg(feature = "linux-root-tests")]
#[test]
fn isolated_workspace_removal_before_start_refuses_real_admission() {
    real_removal_before_start(false);
}

#[cfg(feature = "linux-root-tests")]
#[test]
fn isolated_image_removal_before_start_refuses_real_admission() {
    let root = Path::new(job::IMAGES);
    let host_identity = match std::fs::metadata(root) {
        Ok(meta) => Some((meta.dev(), meta.ino())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => panic!("stat host image store: {error}"),
    };
    // Declare scratch before isolation so namespace restoration precedes its
    // cleanup, including when admission or image removal panics.
    let private = cortexkit_test_support::ScratchDir::new("start-removal-images");
    let _isolation = namespace::root_tests::Fixture::new();
    let images = private.join("images");
    std::fs::create_dir(&images).unwrap();
    if host_identity.is_none() {
        // A private mount namespace does not make mkdir private. Hide the
        // closest existing ancestor before creating a missing mount point.
        let ancestor = root.ancestors().skip(1).find(|path| path.exists()).unwrap();
        assert!(
            !private.starts_with(ancestor),
            "image-store ancestor overlay would hide the test scratch root"
        );
        let parents = private.join("parents");
        std::fs::create_dir(&parents).unwrap();
        namespace::root_tests::command(
            "/usr/bin/mount",
            &[
                "--bind",
                parents.to_str().unwrap(),
                ancestor.to_str().unwrap(),
            ],
        );
        let mounted = std::fs::metadata(ancestor).unwrap();
        let scratch = std::fs::metadata(&parents).unwrap();
        assert_eq!(
            (mounted.dev(), mounted.ino()),
            (scratch.dev(), scratch.ino())
        );
        std::fs::create_dir_all(root).unwrap();
    }
    namespace::root_tests::command(
        "/usr/bin/mount",
        &["--bind", images.to_str().unwrap(), job::IMAGES],
    );
    let mounted = std::fs::metadata(root).unwrap();
    let scratch = std::fs::metadata(&images).unwrap();
    assert_eq!(
        (mounted.dev(), mounted.ino()),
        (scratch.dev(), scratch.ino())
    );
    assert_ne!(
        Some((mounted.dev(), mounted.ino())),
        host_identity,
        "image test must not write to the host's real image store"
    );
    real_removal_before_start(true);
}
