//! Trusted mirror exports and fresh-disk imports for background fills.
#[cfg(target_os = "linux")]
use crate::Runner;
use crate::{
    storage::invalid,
    workspace::{self, Trees},
};
use motor_protocol::{BundleManifest, FillSource, GitDependency, FILL_SOURCE_MAX_BYTES};
#[cfg(target_os = "linux")]
use motor_protocol::{Frame, TenantAgentRequest};
use sha2::{Digest, Sha256};
#[cfg(target_os = "linux")]
use std::path::PathBuf;
use std::{
    fs,
    io::{self, Read},
    path::Path,
};
#[cfg(target_os = "linux")]
use tokio::io::{AsyncRead, AsyncWrite};

pub(crate) fn fill_boot(cmdline: &str) -> bool {
    cmdline
        .split_whitespace()
        .any(|word| word == "motor.fill=1")
}

#[cfg(target_os = "linux")]
pub(crate) fn require_fill_boot() -> io::Result<()> {
    if !fill_boot(&fs::read_to_string("/proc/cmdline")?) {
        return Err(invalid("operation does not match VM boot kind"));
    }
    Ok(())
}

pub(crate) fn run_refusal_for_boot(
    cmdline: &str,
) -> Option<(motor_protocol::RefusalReason, String)> {
    fill_boot(cmdline).then(|| {
        (
            motor_protocol::RefusalReason::RequestRejected,
            "Fill VM refuses job admission".into(),
        )
    })
}

pub(crate) fn import(trees: &Trees, source: &FillSource, directory: &Path) -> io::Result<()> {
    source.validate(FILL_SOURCE_MAX_BYTES)?;
    let repository = crate::caller_path::fill_source(source).caller.text();
    verify_files(source, directory)?;
    trees.fetch_fill(
        directory,
        &source.bundles[0],
        repository,
        &source.integration_tip,
    )?;
    refuse_replacements(&trees.mirror(repository))?;
    let expected: std::collections::BTreeSet<_> =
        crate::writer::fill::locked_git(trees, repository, &source.integration_tip)?
            .into_iter()
            .flat_map(|dependency| {
                dependency
                    .commits
                    .into_iter()
                    .map(move |commit| (dependency.url.clone(), commit))
            })
            .collect();
    let declared: std::collections::BTreeSet<_> = source
        .git_dependencies
        .iter()
        .flat_map(|dependency| {
            dependency
                .commits
                .iter()
                .map(|commit| (dependency.url.clone(), commit.clone()))
        })
        .collect();
    if declared != expected {
        return Err(invalid("fill dependency bundles do not match tip lockfile"));
    }
    trees.validate_dependency_bundles(directory, &source.git_dependencies)?;
    trees.import_dependencies(directory, &source.git_dependencies)?;
    for dependency in &source.git_dependencies {
        refuse_replacements(&trees.dependency_mirror(&dependency.url))?;
    }
    trees.pin(
        repository,
        &source.integration_tip,
        &source.integration_tip,
        &[],
    )
}

fn refuse_replacements(mirror: &Path) -> io::Result<()> {
    if !workspace::git(
        mirror,
        &["for-each-ref", "--format=%(refname)", "refs/replace/"],
    )?
    .is_empty()
    {
        return Err(invalid("fill mirror contains replacement refs"));
    }
    Ok(())
}

pub(crate) fn pins(
    trees: &Trees,
    source: &FillSource,
) -> io::Result<Vec<motor_protocol::MirrorRef>> {
    let mut pins = vec![];
    for reference in [
        format!("refs/snapshots/{}", source.integration_tip),
        format!("refs/tips/{}", source.integration_tip),
    ] {
        let commit = workspace::git(
            &trees.mirror(crate::caller_path::fill_source(source).caller.text()),
            &["rev-parse", &format!("{reference}^{{commit}}")],
        )?;
        if commit != source.integration_tip {
            return Err(invalid("fill pin changed after import"));
        }
        pins.push(motor_protocol::MirrorRef {
            repository: Some(
                crate::caller_path::fill_source(source)
                    .caller
                    .text()
                    .clone(),
            ),
            dependency: None,
            reference,
            commit,
        });
    }
    for dependency in &source.git_dependencies {
        for expected in &dependency.commits {
            let reference = format!("refs/ck-motor/dep/{expected}");
            let commit = workspace::git(
                &trees.dependency_mirror(&dependency.url),
                &["rev-parse", &format!("{reference}^{{commit}}")],
            )?;
            if commit != *expected {
                return Err(invalid("fill dependency pin changed after import"));
            }
            pins.push(motor_protocol::MirrorRef {
                repository: None,
                dependency: Some(dependency.url.clone()),
                reference,
                commit,
            });
        }
    }
    Ok(pins)
}

fn digest(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub(crate) fn verify_files(source: &FillSource, directory: &Path) -> io::Result<()> {
    source.validate(FILL_SOURCE_MAX_BYTES)?;
    for bundle in &source.bundles {
        let path = directory.join(bundle.index.to_string());
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.len() != bundle.length || digest(&path)? != bundle.sha256
        {
            return Err(invalid("fill bundle digest or length mismatch"));
        }
    }
    Ok(())
}

fn bundle(
    mirror: &Path,
    commit: &str,
    reference: &str,
    repository: &str,
    directory: &Path,
    index: u32,
) -> io::Result<BundleManifest> {
    workspace::git(mirror, &["cat-file", "-e", &format!("{commit}^{{commit}}")])?;
    let previous = workspace::git(mirror, &["rev-parse", "--verify", reference]).ok();
    workspace::git(mirror, &["update-ref", reference, commit])?;
    let path = directory.join(index.to_string());
    let result = workspace::git(
        mirror,
        &[
            "bundle",
            "create",
            path.to_str().ok_or_else(|| invalid("bundle path"))?,
            reference,
        ],
    );
    let removed = match previous {
        Some(previous) => workspace::git(mirror, &["update-ref", reference, previous.trim()]),
        None => workspace::git(mirror, &["update-ref", "-d", reference]),
    };
    result?;
    removed?;
    Ok(BundleManifest {
        index,
        repository: repository.into(),
        length: fs::metadata(&path)?.len(),
        sha256: digest(&path)?,
    })
}

#[derive(Debug)]
pub(crate) enum Export {
    Complete(FillSource),
    Incomplete { url: String, commit: String },
}

pub(crate) fn export(
    trees: &Trees,
    root: &str,
    tip: &str,
    directory: &Path,
    cap: u64,
) -> io::Result<Export> {
    let mut source = FillSource {
        repository_root: root.into(),
        integration_tip: tip.into(),
        bundles: vec![],
        git_dependencies: vec![],
    };
    if !matches!(tip.len(), 40 | 64) || !tip.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid("missing integration tip"));
    }
    let dependencies = crate::writer::fill::locked_git(trees, root, tip)?;
    // Jobs receive their snapshot's dependencies, which need not include the
    // default-branch tip's graph. Cargo's online cache is not a trusted mirror.
    // Check the whole graph before creating any export files.
    for dependency in &dependencies {
        let mirror = trees.dependency_mirror(&dependency.url);
        let lock = trees.dependency_lock(&mirror);
        let _guard = lock.lock().unwrap();
        for commit in &dependency.commits {
            if !mirror.try_exists()?
                || !workspace::git(&mirror, &["cat-file", "-t", commit])
                    .is_ok_and(|kind| kind == "commit")
            {
                return Ok(Export::Incomplete {
                    url: dependency.url.clone(),
                    commit: commit.clone(),
                });
            }
        }
    }
    fs::create_dir(directory)?;
    let mirror = trees.mirror(root);
    {
        let lock = trees.mirror_lock(&mirror);
        let _guard = lock.lock().unwrap();
        source.bundles.push(bundle(
            &mirror,
            tip,
            workspace::FILL_BUNDLE_REF,
            root,
            directory,
            0,
        )?);
    }
    let mut total = source.bundles[0].length;
    if total > cap.min(FILL_SOURCE_MAX_BYTES) {
        return Err(invalid("fill source exceeds byte cap"));
    }
    for dependency in dependencies {
        for commit in dependency.commits {
            let mirror = trees.dependency_mirror(&dependency.url);
            let lock = trees.dependency_lock(&mirror);
            let _guard = lock.lock().unwrap();
            let manifest = bundle(
                &mirror,
                &commit,
                &format!("refs/ck-motor/dep/{commit}"),
                &dependency.url,
                directory,
                source.bundles.len() as u32,
            )?;
            total = total
                .checked_add(manifest.length)
                .ok_or_else(|| invalid("fill source size overflow"))?;
            if total > cap.min(FILL_SOURCE_MAX_BYTES) {
                return Err(invalid("fill source exceeds byte cap"));
            }
            source.git_dependencies.push(GitDependency {
                url: dependency.url.clone(),
                commits: vec![commit],
                bundle: Some(manifest.clone()),
            });
            source.bundles.push(manifest);
        }
    }
    source.validate(cap)?;
    Ok(Export::Complete(source))
}

#[cfg(target_os = "linux")]
pub(crate) async fn send<S: AsyncWrite + Unpin>(
    socket: &mut S,
    source: &FillSource,
    directory: &Path,
) -> io::Result<()> {
    use tokio::io::AsyncReadExt;
    for bundle in &source.bundles {
        let mut file = tokio::fs::File::open(directory.join(bundle.index.to_string())).await?;
        let mut offset = 0;
        let mut buffer = vec![0; 65536];
        loop {
            let n = file.read(&mut buffer).await?;
            if n == 0 {
                break;
            }
            motor_protocol::write_frame_async(
                socket,
                &Frame::BundleChunk {
                    index: bundle.index,
                    offset,
                    data: motor_protocol::BytePayload(buffer[..n].to_vec()),
                },
            )
            .await?;
            offset += n as u64;
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
struct Scratch(PathBuf);
#[cfg(target_os = "linux")]
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(target_os = "linux")]
pub(crate) async fn control<S: AsyncRead + AsyncWrite + Unpin>(
    runner: Runner,
    request: TenantAgentRequest,
    socket: &mut S,
) -> io::Result<()> {
    match request {
        TenantAgentRequest::ExportTip { job_id, max_bytes } => {
            let directory = runner
                .config()
                .state_dir()
                .join(format!("fill-export-{}", motor_protocol::Uuid::now_v7()));
            let scratch = Scratch(directory.clone());
            let exported = tokio::task::spawn_blocking(move || {
                runner.export_fill_source(job_id, &directory, max_bytes)
            })
            .await
            .map_err(io::Error::other)??;
            let source = match exported {
                Export::Complete(source) => source,
                Export::Incomplete { url, commit } => {
                    return crate::tenant_agent::write_json(
                        socket,
                        &serde_json::json!({"ok":true,"skipped":{
                            "reason":"fill_source_incomplete","url":url,"commit":commit,
                        }}),
                    )
                    .await;
                }
            };
            tracing::info!(
                tip = source.integration_tip,
                bundle_bytes = source.validate(max_bytes)?,
                dependencies = source.git_dependencies.len(),
                "fill_source_exported"
            );
            crate::tenant_agent::write_json(
                socket,
                &serde_json::json!({"ok":true,"source":source}),
            )
            .await?;
            send(socket, &source, &scratch.0).await
        }
        TenantAgentRequest::Fill { source } => {
            // Refuse before allocating spools or reading chunks on a Job VM.
            require_fill_boot()?;
            source.validate(FILL_SOURCE_MAX_BYTES)?;
            let directory = runner
                .config()
                .state_dir()
                .join(format!("fill-import-{}", motor_protocol::Uuid::now_v7()));
            let mut upload = crate::spool::Upload::new(directory, &source.bundles)?;
            while !upload.complete() {
                let chunk = motor_protocol::read_frame_async(socket)
                    .await?
                    .ok_or_else(|| invalid("incomplete fill source"))?;
                upload.chunk(chunk)?;
            }
            let copy = runner.clone();
            let imported = source.clone();
            let (id, _hold) = tokio::task::spawn_blocking(move || {
                let result = copy.import_fill_source(&imported, &upload.directory);
                drop(upload);
                result
            })
            .await
            .map_err(io::Error::other)??;
            let Some(id) = id else {
                tracing::info!(
                    tip = source.integration_tip,
                    base_published = false,
                    writer_ran = false,
                    "fill_source_completed"
                );
                crate::tenant_agent::write_json(
                    socket,
                    &serde_json::json!({"ok":true,"base_published":false,"have":[]}),
                )
                .await?;
                return Ok(());
            };
            let reply = completion(&runner, id, &source).await?;
            // The import hold remains alive until the reply has been written,
            // including while publication and ownership repair are settling.
            crate::tenant_agent::write_json(socket, &reply).await
        }
        _ => Err(invalid("operation_refused")),
    }
}

#[cfg(target_os = "linux")]
pub(crate) async fn completion(
    runner: &Runner,
    id: motor_protocol::Uuid,
    source: &FillSource,
) -> io::Result<serde_json::Value> {
    loop {
        if let Some(terminal) = runner.job_status(id)?.terminal {
            if !matches!(terminal.outcome, motor_protocol::Outcome::Exit { code: 0 }) {
                return Err(invalid(format!(
                    "fill command failed ({:?}): {}",
                    terminal.outcome,
                    output_tail(runner, id)
                )));
            }
            // A terminal alone does not prove publication and ownership repair
            // finished; powering off early could lose the new base.
            let repository = crate::caller_path::fill_source(source).caller;
            if !runner.fill_settled(id, repository.text())? {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                continue;
            }
            let base = runner
                .config()
                .state_dir()
                .join("build-bases")
                .join(workspace::name(repository.text()))
                .join("current/base.json");
            let published = base.is_file();
            tracing::info!(
                tip = source.integration_tip,
                base_published = published,
                writer_ran = true,
                "fill_source_completed"
            );
            let have = if published {
                runner.fill_source_pins(source)?
            } else {
                vec![]
            };
            let mut reply = serde_json::json!({"ok":true,"base_published":published,"have":have});
            if let Some(samples) = runner.fill_write_samples(id, repository.text()) {
                reply["cache_write_keys"] = serde_json::json!(samples);
            }
            return Ok(reply);
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Recent diagnostics plus the final output, bounded to 2 KiB on one line.
/// Cargo can print progress after its error, and the failed fill VM's disk
/// disappears, so the host's refusal must retain more than the progress tail.
#[cfg(target_os = "linux")]
fn output_tail(runner: &Runner, id: motor_protocol::Uuid) -> String {
    let Ok(frames) = runner.replay(id, 0) else {
        return "output unavailable".into();
    };
    let bytes: Vec<u8> = frames
        .into_iter()
        .filter_map(|frame| match frame {
            Frame::Output(output) => Some(output.bytes.0),
            _ => None,
        })
        .flatten()
        .collect();
    output_summary(&bytes)
}

#[cfg(target_os = "linux")]
fn output_summary(bytes: &[u8]) -> String {
    const MAX_BYTES: usize = 2048;
    const DIAGNOSTIC_BYTES: usize = MAX_BYTES / 2;
    fn prefix(text: &str, limit: usize) -> &str {
        let mut end = text.len().min(limit);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        &text[..end]
    }
    fn tail(text: &str, limit: usize) -> &str {
        let mut start = text.len().saturating_sub(limit);
        while !text.is_char_boundary(start) {
            start += 1;
        }
        &text[start..]
    }
    let text = String::from_utf8_lossy(bytes);
    let lines = text
        .split(['\n', '\r'])
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let mut diagnostics = lines
        .clone()
        .rev()
        .filter(|line| line.starts_with("error") || line.starts_with("warning: build failed"))
        .take(4)
        .collect::<Vec<_>>();
    let output = lines.collect::<Vec<_>>().join(" | ");
    if diagnostics.is_empty() {
        return tail(&output, MAX_BYTES).into();
    }
    diagnostics.reverse();
    let line_bytes = (DIAGNOSTIC_BYTES - (diagnostics.len() - 1) * 3) / diagnostics.len();
    let diagnostics = diagnostics
        .into_iter()
        .map(|line| prefix(line, line_bytes))
        .collect::<Vec<_>>()
        .join(" | ");
    format!(
        "{diagnostics} | {}",
        tail(&output, MAX_BYTES - diagnostics.len() - 3)
    )
}

#[cfg(target_os = "linux")]
pub(crate) async fn serve_control(runner: Runner) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let directory = Path::new("/run/motor/fill-control");
    match fs::create_dir(directory) {
        Ok(()) => fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let listener = crate::tenant_local::bind(directory, "admin.sock", 0)?;
    loop {
        let (socket, _) = listener.accept().await?;
        let runner = runner.clone();
        tokio::spawn(async move {
            if let Err(error) = crate::tenant_local::admin_session(runner, socket).await {
                tracing::warn!(%error, "fill control channel failed");
            }
        });
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::output_summary;

    #[test]
    fn output_summary_is_utf8_bounded_and_retains_recent_diagnostics() {
        let output = format!("error: old failure\r\nerror: recent one\nerror: recent two\n{}\r\nwarning: build failed, waiting\r\n{}\nlast line\n",
            "error: 日本語".repeat(500), "Compiling 日本語\n".repeat(500));
        let summary = output_summary(output.as_bytes());
        assert!(summary.len() <= 2048);
        assert!(!summary.contains("old failure"));
        assert!(summary.contains("error: 日本語"));
        assert!(summary.contains("warning: build failed, waiting"));
        assert!(summary.ends_with("last line"));
        assert!(!summary.contains(['\n', '\r', '\u{fffd}']));
    }

    #[test]
    fn output_without_diagnostics_keeps_the_tail_budget() {
        assert_eq!(output_summary(b"first\r\n\nlast\n"), "first | last");
        let summary = output_summary(format!("{}final line", "progress\n".repeat(500)).as_bytes());
        assert_eq!(summary.len(), 2048);
        assert!(summary.ends_with("final line"));
        assert!(output_summary(b"").is_empty());
    }
}
