//! Operational preparation failures must not leave a partial dependency tree.
use super::{Check, Project, Refusal, RefusalKind, is_node_modules, refuse};
use std::path::Path;

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
        .detail(format!(
            "{check:?} failed at . (Bun install working directory)"
        ))
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
    clean_failed_install(directory, "", refusal)
}

fn clean_failed_install(
    directory: &Path,
    lock_sha256: &str,
    refusal: Refusal,
) -> Result<Refusal, Refusal> {
    if refusal.kind.is_security() {
        return Err(refusal);
    }
    let invalid = || {
        refuse(RefusalKind::BunProjectionInvalid, &refusal.project, "", "")
            .at(Check::InstallCleanup)
    };
    let failed = |path: &Path| {
        invalid().detail(super::path_detail("InstallCleanup failed", directory, path))
    };
    if !directory.is_dir() {
        return Err(failed(directory));
    }
    let roots = super::install_roots::Roots::read(directory, lock_sha256)
        .map_err(|step| invalid().detail(format!("InstallCleanup: {step}")))?;
    // The preflight rejected existing modules in these directories. Only Bun
    // created their node_modules, so cleanup need not touch fixture source.
    for root in roots.existing(directory).map_err(|path| failed(&path))? {
        for entry in std::fs::read_dir(&root).map_err(|_| failed(&root))? {
            let entry = entry.map_err(|_| failed(&root))?;
            if !is_node_modules(&entry.file_name()) {
                continue;
            }
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path).map_err(|_| failed(&path))?;
            if metadata.is_dir() {
                std::fs::remove_dir_all(&path).map_err(|_| failed(&path))?;
            } else {
                std::fs::remove_file(&path).map_err(|_| failed(&path))?;
            }
        }
    }
    Ok(refusal)
}

pub fn finish_install(
    project: &Project,
    directory: &Path,
    result: Result<(), Refusal>,
) -> Result<Option<Refusal>, Refusal> {
    match result.and_then(|()| super::verify_installed(project, directory)) {
        Ok(()) => Ok(None),
        Err(refusal) => clean_failed_install(directory, &project.lock_sha256, refusal).map(Some),
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
        if let Some(owner) = owner {
            warnings.push(super::workspace_member_warning(&project, &owner));
            continue;
        }
        let directory = source.join(project.strip_prefix("m/").unwrap_or(""));
        let invalid = |detail: &str| {
            refuse(RefusalKind::BunProjectionInvalid, &project, "", "").detail(detail)
        };
        let lock = super::read_bounded(&directory.join("bun.lock"), super::MAX_LOCK_BYTES as u64)
            .map_err(|_| invalid("read bun.lock"))?;
        let text = std::str::from_utf8(&lock).map_err(|_| invalid("decode bun.lock as UTF-8"))?;
        let lock = super::text_json(text).map_err(|_| {
            refuse(RefusalKind::BunLockInvalid, &project, "", "").detail("parse bun.lock as JSON")
        })?;
        super::read_lock_value(&project, &lock)?;
        super::install_roots::check_inputs(source, &directory, &lock, &budget)
            .map_err(|detail| invalid(&detail))?;
        // No install ran, and validation already excluded pre-existing modules
        // at each install root. Nothing needs to be removed before continuing.
        let mut refusal = failure.clone();
        refusal.project = project;
        warnings.push(refusal.warning());
    }
    Ok(warnings)
}
