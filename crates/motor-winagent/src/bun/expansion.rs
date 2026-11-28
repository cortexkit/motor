//! One shared budget covers every package and every decompression pass in a job.
use std::{
    cell::RefCell,
    io::{self, Read},
    rc::Rc,
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct ExpansionBudget(Rc<RefCell<State>>);
struct State {
    bytes: u64,
    entries: u64,
    metadata: u64,
    names: usize,
    components: u64,
    max_bytes: u64,
    max_entries: u64,
    deadline: Instant,
}
impl Default for ExpansionBudget {
    fn default() -> Self {
        Self::new(
            4 * 1024 * 1024 * 1024,
            250_000,
            Instant::now() + Duration::from_secs(300),
        )
    }
}
impl ExpansionBudget {
    pub fn new(max_bytes: u64, max_entries: u64, deadline: Instant) -> Self {
        Self(Rc::new(RefCell::new(State {
            bytes: 0,
            entries: 0,
            metadata: 0,
            names: 0,
            components: 0,
            max_bytes,
            max_entries,
            deadline,
        })))
    }
    pub fn check(&self) -> io::Result<()> {
        let now = Instant::now();
        let deadline = self.0.borrow().deadline;
        if now >= deadline {
            Err(io::Error::other(format!(
                "Bun preparation deadline reached ({} ms past deadline)",
                now.duration_since(deadline).as_millis()
            )))
        } else {
            Ok(())
        }
    }
    pub(super) fn entry(&self, metadata: Option<u64>) -> io::Result<()> {
        self.check()?;
        let mut state = self.0.borrow_mut();
        state.entries += 1;
        if let Some(bytes) = metadata {
            state.metadata = state.metadata.saturating_add(bytes);
            if bytes > 32 * 1024 {
                return Err(over_limit("Bun tar metadata", bytes, 32 * 1024));
            }
            if state.metadata > 8 * 1024 * 1024 {
                return Err(over_limit(
                    "Bun job tar metadata",
                    state.metadata,
                    8 * 1024 * 1024,
                ));
            }
        }
        if state.entries > state.max_entries {
            return Err(over_limit(
                "Bun job entry",
                state.entries,
                state.max_entries,
            ));
        }
        Ok(())
    }
    pub(super) fn path(&self, size: usize) -> io::Result<()> {
        let mut state = self.0.borrow_mut();
        state.names = state.names.saturating_add(size);
        state.components += 1;
        if state.names > 32 * 1024 * 1024 {
            return Err(over_limit(
                "Bun path index bytes",
                state.names as u64,
                32 * 1024 * 1024,
            ));
        }
        if state.components > state.max_entries {
            return Err(over_limit(
                "Bun path index entries",
                state.components,
                state.max_entries,
            ));
        }
        Ok(())
    }
    pub(super) fn reader<R: Read>(&self, reader: R, charge: bool) -> Reader<R> {
        Reader {
            reader,
            budget: self.clone(),
            charge,
        }
    }
}

fn over_limit(what: &str, used: u64, limit: u64) -> io::Error {
    io::Error::other(format!("{what} limit exceeded ({used} > {limit})"))
}

pub(super) struct Reader<R> {
    reader: R,
    budget: ExpansionBudget,
    charge: bool,
}
impl<R: Read> Read for Reader<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.budget.check()?;
        let cap = if self.charge {
            let state = self.budget.0.borrow();
            output.len().min(
                usize::try_from(
                    state
                        .max_bytes
                        .saturating_sub(state.bytes)
                        .saturating_add(1),
                )
                .unwrap_or(usize::MAX),
            )
        } else {
            output.len()
        };
        let count = self.reader.read(&mut output[..cap])?;
        if self.charge {
            let mut state = self.budget.0.borrow_mut();
            state.bytes = state.bytes.saturating_add(count as u64);
            if state.bytes > state.max_bytes {
                return Err(over_limit(
                    "Bun job expanded-byte",
                    state.bytes,
                    state.max_bytes,
                ));
            }
        }
        Ok(count)
    }
}
