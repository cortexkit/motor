use super::{fs::Dir, Control};
use crate::storage::invalid;
use std::{
    io::{self, Read, Seek},
    os::unix::process::CommandExt,
    path::Path,
    process::{Command, ExitStatus, Stdio},
    time::Duration,
};

/// A process group's lifetime ends even when its leader exits before descendants.
struct Group(std::process::Child);
impl Drop for Group {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-(self.0.id() as libc::pid_t), libc::SIGKILL);
        }
        let _ = self.0.wait();
    }
}
fn wait(group: &mut Group, control: &Control) -> io::Result<ExitStatus> {
    loop {
        control.check()?;
        if let Some(status) = group.0.try_wait()? {
            return Ok(status);
        }
        std::thread::park_timeout(Duration::from_millis(10));
    }
}
#[cfg(test)]
pub(super) fn wait_after_ready(
    cmd: &mut Command,
    ready: impl FnOnce() -> io::Result<Control>,
) -> io::Result<()> {
    cmd.process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut group = Group(cmd.spawn()?);
    let control = ready()?;
    wait(&mut group, &control).map(|_| ())
}
pub(super) fn run(cmd: &mut Command, logs: &Path, control: &Control) -> io::Result<()> {
    output(cmd, logs, control).map(|_| ())
}
pub(super) fn output(cmd: &mut Command, logs: &Path, control: &Control) -> io::Result<String> {
    control.check()?;
    let dir = Dir::open(logs, false)?;
    let name = format!("process-{}.log", uuid::Uuid::now_v7());
    let mut log = dir.file(name.as_ref(), true)?;
    cmd.process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log.try_clone()?));
    let result = (|| {
        let mut group = Group(cmd.spawn()?);
        let status = wait(&mut group, control)?;
        log.rewind()?;
        let mut output = String::new();
        log.take(65536).read_to_string(&mut output)?;
        if status.success() {
            return Ok(output);
        }
        Err(invalid(format!(
            "dependency preparation failed ({status}): {output}"
        )))
    })();
    dir.remove(name.as_ref())?;
    result
}
