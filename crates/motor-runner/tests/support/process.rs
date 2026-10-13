use std::{
    io,
    ops::{Deref, DerefMut},
    os::unix::process::CommandExt,
    process::{Child, Command, Output},
};

pub struct GroupGuard(pub i32);

impl Drop for GroupGuard {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-self.0, libc::SIGKILL);
        }
    }
}

pub struct ChildGuard {
    group: Option<GroupGuard>,
    child: Option<Child>,
}

impl ChildGuard {
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        let child = command.process_group(0).spawn()?;
        Ok(Self {
            group: Some(GroupGuard(child.id() as i32)),
            child: Some(child),
        })
    }
    pub fn wait_with_output(mut self) -> io::Result<Output> {
        self.child.take().unwrap().wait_with_output()
    }
}
impl Deref for ChildGuard {
    type Target = Child;
    fn deref(&self) -> &Child {
        self.child.as_ref().unwrap()
    }
}
impl DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Child {
        self.child.as_mut().unwrap()
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        drop(self.group.take());
        if let Some(child) = &mut self.child {
            let _ = child.wait();
        }
    }
}
