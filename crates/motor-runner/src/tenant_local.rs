//! Private Unix equivalents of the host-CID-only tenant channels. These are
//! trusted admission inputs, never an alternative unleased job protocol.

use crate::Runner;
use motor_protocol::{TenantAgentRequest, MAX_FRAME_BYTES};
use serde_json::{json, Value};
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

fn private_directory(directory: &Path, owner: u32) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(directory)?;
    if !metadata.is_dir() || metadata.uid() != owner || metadata.mode() & 0o7777 != 0o700 {
        return Err(io::Error::other(
            "tenant socket directory must be root-owned 0700",
        ));
    }
    Ok(())
}

fn trusted_ancestors(directory: &Path) -> io::Result<()> {
    if !directory.is_absolute() {
        return Err(io::Error::other("tenant socket directory must be absolute"));
    }
    for path in directory.ancestors() {
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(io::Error::other(
                "tenant socket ancestors must be root-owned and not writable by jobs",
            ));
        }
    }
    Ok(())
}

fn bind(directory: &Path, name: &str, owner: u32) -> io::Result<UnixListener> {
    private_directory(directory, owner)?;
    let path = directory.join(name);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if !metadata.file_type().is_socket() || metadata.uid() != owner {
                return Err(io::Error::other("expected an owned stale socket node"));
            }
            match std::os::unix::net::UnixStream::connect(&path) {
                Ok(_) => return Err(io::Error::other("socket still has a live listener")),
                Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {}
                Err(error) => return Err(error),
            }
            std::fs::remove_file(&path)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

pub async fn serve_tenant_local(runner: Runner, directory: &Path) -> io::Result<()> {
    if !runner.config().tenant_vm || runner.config().tenant_socket_dir.as_deref() != Some(directory)
    {
        return Err(io::Error::other("local tenant channels are not configured"));
    }
    trusted_ancestors(directory)?;
    let jobs = bind(directory, "jobs.sock", 0)?;
    let events = bind(directory, "events.sock", 0)?;
    let admin = bind(directory, "admin.sock", 0)?;
    serve_listeners(runner, jobs, events, admin).await
}

async fn serve_listeners(
    runner: Runner,
    jobs: UnixListener,
    events: UnixListener,
    admin: UnixListener,
) -> io::Result<()> {
    let event_runner = runner.clone();
    let lifecycle = async move {
        loop {
            let (socket, _) = events.accept().await?;
            // As on vsock, only one event consumer can advance the ack cursor.
            if let Err(error) = crate::tenant::event_session(&event_runner, socket).await {
                tracing::warn!(%error, "local tenant event channel detached");
            }
        }
        #[allow(unreachable_code)]
        Ok::<(), io::Error>(())
    };
    let connections = async move {
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            let runner = runner.clone();
            tokio::select! {
                socket = jobs.accept() => {
                    let (socket, _) = socket?;
                    tasks.spawn(async move {
                        if let Err(error) = crate::server::tenant_channel(runner, socket).await {
                            tracing::warn!(%error, "local tenant job channel detached");
                        }
                    });
                }
                socket = admin.accept() => {
                    let (socket, _) = socket?;
                    tasks.spawn(async move {
                        if let Err(error) = admin_session(runner, socket).await {
                            tracing::warn!(%error, "local tenant admin channel detached");
                        }
                    });
                }
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
                _ = tokio::time::sleep(Duration::from_millis(25)) => {
                    if runner.drain_should_exit() { return Ok::<(), io::Error>(()); }
                }
            }
        }
    };
    tokio::select! { result = lifecycle => result, result = connections => result }
}

async fn admin_session(runner: Runner, mut socket: UnixStream) -> io::Result<()> {
    let len = socket.read_u32().await? as usize;
    if len == 0 || len > MAX_FRAME_BYTES {
        return Err(io::Error::other("invalid admin frame length"));
    }
    let mut bytes = vec![0; len];
    socket.read_exact(&mut bytes).await?;
    let request: TenantAgentRequest = serde_json::from_slice(&bytes)?;
    let reply = dispatch(runner, request).await?;
    let bytes = serde_json::to_vec(&reply)?;
    socket.write_u32(bytes.len() as u32).await?;
    socket.write_all(&bytes).await?;
    socket.flush().await
}

async fn dispatch(runner: Runner, request: TenantAgentRequest) -> io::Result<Value> {
    match request {
        TenantAgentRequest::Cancel { job_id } => tokio::task::spawn_blocking(move || {
            runner.cancel(job_id)?;
            Ok(json!({"ok":true}))
        })
        .await
        .map_err(io::Error::other)?,
        TenantAgentRequest::Status => Ok(json!({"ok":true,"status":runner.runner_status()})),
        TenantAgentRequest::Resume => {
            runner.resume();
            Ok(json!({"ok":true}))
        }
        TenantAgentRequest::Drain {
            deadline_s,
            exit_after,
        } => {
            let waiter =
                runner.begin_drain_request(Duration::from_secs(deadline_s.min(86400)), exit_after);
            loop {
                if let Some((timed_out, jobs)) = runner.drain_reply_for(&waiter) {
                    return Ok(
                        json!({"ok":true,"timed_out":timed_out,"restart_recoverable_jobs":jobs}),
                    );
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
        _ => Err(io::Error::other("operation_refused")),
    }
}

#[cfg(test)]
mod tests;
