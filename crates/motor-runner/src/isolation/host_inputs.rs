//! Host discovery used by sandbox planning. Tests can supply a complete host
//! layout without changing process-wide environment variables or host files.
use std::{ffi::OsString, path::Path};

pub(super) fn environment(name: &str) -> Option<OsString> {
    #[cfg(test)]
    if let Some(value) = fixture::read(|inputs| {
        inputs
            .environment
            .get(name)
            .unwrap_or_else(|| panic!("unpinned sandbox environment read: {name}"))
            .clone()
    }) {
        return Some(value);
    }
    std::env::var_os(name)
}

pub(super) fn exists(path: &Path) -> bool {
    #[cfg(test)]
    if let Some(value) = fixture::read(|inputs| {
        *inputs
            .existing
            .get(path)
            .unwrap_or_else(|| panic!("unpinned sandbox existence probe: {}", path.display()))
    }) {
        return value;
    }
    path.exists()
}

pub(super) fn is_dir(path: &Path) -> bool {
    #[cfg(test)]
    if let Some(value) = fixture::read(|inputs| {
        *inputs
            .directories
            .get(path)
            .unwrap_or_else(|| panic!("unpinned sandbox directory probe: {}", path.display()))
    }) {
        return value;
    }
    path.is_dir()
}

#[cfg(test)]
pub(super) mod fixture {
    use super::*;
    use std::{cell::RefCell, collections::BTreeMap, path::PathBuf};

    pub(crate) struct Inputs {
        pub environment: BTreeMap<&'static str, OsString>,
        pub existing: BTreeMap<PathBuf, bool>,
        pub directories: BTreeMap<PathBuf, bool>,
    }

    thread_local! {
        static INPUTS: RefCell<Option<Inputs>> = const { RefCell::new(None) };
    }

    pub(super) fn read<T>(f: impl FnOnce(&Inputs) -> T) -> Option<T> {
        INPUTS.with(|slot| slot.borrow().as_ref().map(f))
    }

    pub(crate) fn with<T>(inputs: Inputs, f: impl FnOnce() -> T) -> T {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                INPUTS.with(|slot| *slot.borrow_mut() = None);
            }
        }
        INPUTS.with(|slot| {
            let mut slot = slot.borrow_mut();
            assert!(slot.is_none(), "sandbox host fixtures cannot nest");
            *slot = Some(inputs);
        });
        let _reset = Reset;
        f()
    }
}
