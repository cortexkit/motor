use super::*;
use std::{fs::File, os::fd::AsRawFd, path::Path, sync::mpsc, time::Duration};

fn acquire(path: &Path) -> Result<lock::Guard> {
    lock::acquire_at(path, unsafe { libc::geteuid() })
}

fn render(lock_path: &Path, list: impl FnOnce() -> Result<Vec<Tap>>) -> String {
    with_job_taps(
        || acquire(lock_path),
        list,
        |jobs| policy_rules("eno1", &jobs?, None, &Default::default()),
    )
    .unwrap()
}

#[test]
fn allocation_provider_runs_under_network_lock_and_rereads_after_release() {
    let root = cortexkit_test_support::ScratchDir::new("network-render");
    let path = root.join("network");
    let record = root.join("tap");
    drop(acquire(&path).unwrap());
    std::fs::write(&record, "17").unwrap();
    let list = || {
        let other = File::open(path.join("lock")).unwrap();
        assert_ne!(
            unsafe { libc::flock(other.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0,
            "allocation provider ran without the network lock"
        );
        assert_eq!(
            std::io::Error::last_os_error().kind(),
            std::io::ErrorKind::WouldBlock
        );
        Ok(std::fs::read_to_string(&record)
            .unwrap()
            .trim()
            .parse()
            .ok()
            .map(|tap_index| Tap {
                tap_index,
                uid: 500000,
            })
            .into_iter()
            .collect())
    };
    assert!(render(&path, list).contains("tenant_mtap17_egress"));
    {
        let _guard = acquire(&path).unwrap();
        std::fs::write(&record, "").unwrap();
    }
    assert!(!render(&path, list).contains("mtap17"));
}

#[test]
fn render_waiting_for_release_cannot_restore_freed_tap() {
    let root = cortexkit_test_support::ScratchDir::new("network-render-release");
    let path = root.join("network");
    let record = root.join("tap");
    std::fs::write(&record, "17").unwrap();
    let guard = acquire(&path).unwrap();
    let (waiting, ready) = mpsc::channel();
    let (provided, provider_called) = mpsc::channel();
    let worker_path = path.clone();
    let worker_record = record.clone();
    let worker = std::thread::spawn(move || {
        with_job_taps(
            || {
                waiting.send(()).unwrap();
                acquire(&worker_path)
            },
            || {
                provided.send(()).unwrap();
                Ok(std::fs::read_to_string(worker_record)
                    .unwrap()
                    .trim()
                    .parse()
                    .ok()
                    .map(|tap_index| Tap {
                        tap_index,
                        uid: 500000,
                    })
                    .into_iter()
                    .collect())
            },
            |jobs| policy_rules("eno1", &jobs?, None, &Default::default()),
        )
        .unwrap()
    });
    ready.recv_timeout(Duration::from_secs(2)).unwrap();
    let called_early = provider_called
        .recv_timeout(Duration::from_millis(50))
        .is_ok();
    std::fs::write(&record, "").unwrap();
    drop(guard);
    let text = worker.join().unwrap();
    assert!(
        !called_early,
        "provider read the allocation before acquiring the network lock"
    );
    assert!(!text.contains("mtap17"));
}
