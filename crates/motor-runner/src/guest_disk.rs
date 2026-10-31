//! Cached tenant workspace observations; no filesystem probe runs on a job path.
use super::Runner;
use motor_protocol::GuestDisk;
use std::{
    ffi::CString,
    io,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const INTERVAL: Duration = Duration::from_secs(15);
const FRESH: Duration = Duration::from_secs(60);
type Cache = Mutex<Option<(Instant, GuestDisk)>>;

pub(super) fn start(runner: &Runner) {
    // Job targets are bound from a workspace root into /run/motor/target.
    // Startup requires every workspace root to share the state filesystem.
    let path = runner.core.config.caller_roots.first().map_or_else(
        || runner.core.trees.state.clone(),
        |root| root.host_root.clone(),
    );
    let weak = Arc::downgrade(&runner.core);
    let result = std::thread::Builder::new()
        .name("guest-disk".into())
        .spawn(move || {
            let mut sampler = Sampler::new(path);
            loop {
                if weak.strong_count() == 0 {
                    break;
                }
                let sample = sampler.sample(Instant::now(), probe);
                if let Some(core) = weak.upgrade() {
                    if let Some(sample) = sample {
                        *core.guest_disk.lock().unwrap() = sample;
                    }
                } else {
                    break;
                }
                std::thread::sleep(INTERVAL);
            }
        });
    if let Err(error) = result {
        tracing::warn!(%error, "guest disk sampler unavailable");
    }
}

pub(super) fn report(cache: &Cache) -> Option<GuestDisk> {
    report_at(cache, Instant::now())
}

fn report_at(cache: &Cache, now: Instant) -> Option<GuestDisk> {
    let cache = cache.lock().unwrap();
    let (at, sample) = cache.as_ref()?;
    let age = now.checked_duration_since(*at)?;
    (age <= FRESH).then(|| GuestDisk {
        age_ms: age.as_millis() as u64,
        ..sample.clone()
    })
}

struct Sampler {
    path: PathBuf,
    last_attempt: Option<Instant>,
}

impl Sampler {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            last_attempt: None,
        }
    }

    // None means not due; Some(None) clears an observation after a failed probe.
    fn sample(
        &mut self,
        now: Instant,
        probe: impl FnOnce(&Path) -> io::Result<(u64, u64)>,
    ) -> Option<Option<(Instant, GuestDisk)>> {
        if self
            .last_attempt
            .is_some_and(|at| now.duration_since(at) < INTERVAL)
        {
            return None;
        }
        self.last_attempt = Some(now);
        Some(probe(&self.path).ok().map(|(free_bytes, total_bytes)| {
            (
                now,
                GuestDisk {
                    free_bytes,
                    total_bytes,
                    age_ms: 0,
                },
            )
        }))
    }
}

fn probe(path: &Path) -> io::Result<(u64, u64)> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    let bytes = |blocks: u64| {
        blocks
            .checked_mul(stat.f_frsize)
            .ok_or_else(|| io::Error::other("statvfs overflow"))
    };
    Ok((bytes(stat.f_bavail)?, bytes(stat.f_blocks)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_disk_samples_workspace_path_at_most_every_fifteen_seconds() {
        let path = PathBuf::from("workspace-filesystem-seam");
        let mut sampler = Sampler::new(path.clone());
        let now = Instant::now();
        let sample = sampler
            .sample(now, |seen| {
                assert_eq!(seen, path);
                Ok((17, 31))
            })
            .unwrap()
            .unwrap();
        assert_eq!((sample.1.free_bytes, sample.1.total_bytes), (17, 31));
        assert!(sampler
            .sample(now + INTERVAL - Duration::from_millis(1), |_| panic!(
                "too early"
            ))
            .is_none());
        assert!(sampler
            .sample(now + INTERVAL, |_| Err(io::Error::other("unavailable")))
            .unwrap()
            .is_none());
        let cache = Mutex::new(Some(sample));
        assert_eq!(report_at(&cache, now + FRESH).unwrap().age_ms, 60_000);
        assert!(report_at(&cache, now + FRESH + Duration::from_millis(1)).is_none());
    }

    #[test]
    fn guest_disk_statvfs_uses_the_selected_filesystem_and_available_blocks() {
        let scratch = cortexkit_test_support::scratch_root()
            .join(format!("guest-disk-{}", motor_protocol::Uuid::now_v7()));
        std::fs::create_dir_all(&scratch).unwrap();
        let (free, total) = probe(&scratch).unwrap();
        assert!(total > 0);
        assert!(free <= total);
        assert!(probe(&scratch.join("missing")).is_err());
        std::fs::remove_dir(scratch).unwrap();
    }
}
