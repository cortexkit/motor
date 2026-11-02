//! Operational preparation failures must not leave a partial dependency tree.
use super::{Check, Project, Refusal, RefusalKind, is_node_modules, refuse};
use std::{io, path::Path};

pub enum InstallEvent {
    Stderr(Vec<u8>),
    Output,
    PipeClosed,
}

/// Pipe EOF is independent of process signalling. Once both readers finish,
/// keep polling the process instead of interpreting channel disconnect as exit.
pub fn wait_install(
    project: &Project,
    mut poll: impl FnMut() -> Result<Option<u32>, Check>,
    mut terminate: impl FnMut(u32) -> Result<(), Check>,
    mut receive: impl FnMut() -> Result<Option<InstallEvent>, Check>,
    diagnostics: &mut DiagnosticTail,
) -> Result<(), Refusal> {
    let fail = |check| {
        refuse(
            RefusalKind::BunOfflineInstallFailed,
            &project.path,
            &project.name,
            &project.version,
        )
        .at(check)
    };
    let (mut code, mut closed) = (None, 0);
    loop {
        if let Some(exit) = poll().map_err(fail)?
            && code.is_none()
        {
            code = Some(exit);
            terminate(exit).map_err(fail)?;
        }
        if closed == 2 {
            if let Some(code) = code {
                return if code == 0 {
                    Ok(())
                } else {
                    Err(fail(Check::NonzeroExit))
                };
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            continue;
        }
        match receive().map_err(fail)? {
            Some(InstallEvent::Stderr(bytes)) => diagnostics.push(&bytes),
            Some(InstallEvent::Output) | None => (),
            Some(InstallEvent::PipeClosed) => closed += 1,
        }
    }
}

/// Keep only a small stderr tail. Package diagnostics are never service logs.
#[derive(Default)]
pub struct DiagnosticTail(Vec<u8>);

impl DiagnosticTail {
    pub fn push(&mut self, bytes: &[u8]) {
        const LIMIT: usize = 2048;
        self.0
            .extend_from_slice(&bytes[bytes.len().saturating_sub(LIMIT)..]);
        self.0.drain(..self.0.len().saturating_sub(LIMIT));
    }

    pub fn line(&self) -> String {
        let mut line = String::from_utf8_lossy(&self.0)
            .chars()
            .map(|c| {
                if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
                    ' '
                } else {
                    c
                }
            })
            .collect::<String>()
            .trim()
            .to_owned();
        while line.len() > 2048 {
            line.pop();
        }
        line
    }
}

pub fn skip_project(directory: &Path, refusal: Refusal) -> Result<Refusal, Refusal> {
    if refusal.kind.is_security() {
        return Err(refusal);
    }
    fn clean(directory: &Path) -> io::Result<()> {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let metadata = std::fs::symlink_metadata(entry.path())?;
            if is_node_modules(&entry.file_name()) {
                if metadata.is_dir() {
                    std::fs::remove_dir_all(entry.path())?;
                } else {
                    std::fs::remove_file(entry.path())?;
                }
            } else if metadata.is_dir() && !crate::is_reparse(&metadata) {
                clean(&entry.path())?;
            }
        }
        Ok(())
    }
    // A failed cleanup cannot safely hand an incomplete tree to the command.
    clean(directory)
        .map_err(|_| refuse(RefusalKind::BunProjectionInvalid, &refusal.project, "", ""))?;
    Ok(refusal)
}

pub fn finish_install(
    project: &Project,
    directory: &Path,
    result: Result<(), Refusal>,
) -> Result<Option<Refusal>, Refusal> {
    match result.and_then(|()| super::verify_installed(project, directory)) {
        Ok(()) => Ok(None),
        Err(refusal) => skip_project(directory, refusal).map(Some),
    }
}

pub fn skip_unprepared(source: &Path, failure: Refusal) -> Result<Vec<String>, Refusal> {
    if failure.kind.is_security() {
        return Err(failure);
    }
    let budget = super::ExpansionBudget::new(
        4 * 1024 * 1024 * 1024,
        250_000,
        std::time::Instant::now() + std::time::Duration::from_secs(300),
    );
    let mut warnings = Vec::new();
    for (project, owner) in super::source_projects(source, &budget)? {
        let directory = source.join(project.strip_prefix("m/").unwrap_or(""));
        let invalid = || refuse(RefusalKind::BunProjectionInvalid, &project, "", "");
        super::clean_install_inputs(&directory, &budget).map_err(|_| invalid())?;
        let lock = super::read_bounded(&directory.join("bun.lock"), super::MAX_LOCK_BYTES as u64)
            .map_err(|_| invalid())?;
        super::read_lock(&project, std::str::from_utf8(&lock).map_err(|_| invalid())?)?;
        if let Some(owner) = owner {
            // Bun installs this directory as part of the enclosing workspace and
            // never reads its own lockfile. Say so instead of preparing a tree
            // Bun will not build.
            warnings.push(super::workspace_member_warning(&project, &owner));
            continue;
        }
        let mut refusal = failure.clone();
        refusal.project = project;
        warnings.push(skip_project(&directory, refusal)?.warning());
    }
    Ok(warnings)
}
