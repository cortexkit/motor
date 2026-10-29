//! A host slot remains owned until VM teardown and file cleanup both succeed.
use super::*;
use std::sync::{Arc, Mutex};

pub(super) type Owners = Arc<Mutex<std::collections::BTreeMap<Uuid, String>>>;

pub(super) struct Slot {
    _file: fs::File,
    id: Uuid,
    owners: Owners,
}

impl Slot {
    pub(super) fn acquire(config: &Config, id: Uuid) -> io::Result<Self> {
        use fs2::FileExt;
        let vm = config.windows.as_ref().unwrap();
        #[cfg(windows)]
        let concurrent_jobs = vm.concurrent_jobs;
        #[cfg(not(windows))]
        let concurrent_jobs = 1;
        for index in 0..concurrent_jobs {
            let file = fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(vm.layout_root.join(format!("slot-{index}.lock")))?;
            match file.try_lock_exclusive() {
                Ok(()) => {
                    let owners = config.windows_runtime.slot_owners.clone();
                    owners
                        .lock()
                        .unwrap()
                        .insert(id, "preparing or running".into());
                    return Ok(Self {
                        _file: file,
                        id,
                        owners,
                    });
                }
                Err(error)
                    if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {}
                Err(error) => return Err(error),
            }
        }
        Err(config.windows_runtime.slot_busy())
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.owners.lock().unwrap().remove(&self.id);
    }
}
