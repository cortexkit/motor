use super::{placement::Lease, Error};
use crate::transport::Duplex;
use ed25519_dalek::SigningKey;
use std::{
    fs::{self, DirBuilder, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::PathBuf,
    pin::Pin,
    process::Stdio,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf},
    net::UnixStream,
    process::{Child, Command},
};

struct Runtime {
    root: PathBuf,
    child: Option<Child>,
}
impl Drop for Runtime {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}
impl Runtime {
    fn new(key: &SigningKey) -> io::Result<Self> {
        let mut nonce = [0; 12];
        getrandom::fill(&mut nonce).map_err(io::Error::other)?;
        // A short path is required by macOS's Unix-domain socket limit. The
        // unpredictable, exclusively-created directory is never shared.
        let root =
            PathBuf::from("/tmp").join(format!("motor-boat-{}", motor_lease::base64url(&nonce)));
        DirBuilder::new().mode(0o700).create(&root)?;
        let runtime = Self { root, child: None };
        let pair = ssh_key::private::Ed25519Keypair::from_seed(&key.to_bytes());
        let private = ssh_key::PrivateKey::from(pair);
        runtime.write(
            "identity",
            private
                .to_openssh(ssh_key::LineEnding::LF)
                .map_err(io::Error::other)?
                .as_bytes(),
        )?;
        Ok(runtime)
    }
    fn write(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.root.join(name))?
            .write_all(bytes)
    }
}

struct Pipe {
    stream: UnixStream,
    _runtime: Runtime,
}
impl AsyncRead for Pipe {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}
impl AsyncWrite for Pipe {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

pub(super) async fn connect(lease: &Lease, key: &SigningKey) -> Result<Box<dyn Duplex>, Error> {
    let runtime = Runtime::new(key).map_err(|_| Error::Connect)?;
    connect_runtime(lease, runtime)
        .await
        .map(|pipe| Box::new(pipe) as Box<dyn Duplex>)
}
async fn connect_runtime(lease: &Lease, mut runtime: Runtime) -> Result<Pipe, Error> {
    let ssh = lease.ssh.as_ref().ok_or(Error::Auth)?;
    let endpoint: std::net::SocketAddrV4 = ssh.endpoint.parse().map_err(|_| Error::Auth)?;
    let known = format!("[{}]:{} {}\n", endpoint.ip(), endpoint.port(), ssh.host_key);
    runtime
        .write("known_hosts", known.as_bytes())
        .map_err(|_| Error::Connect)?;
    let socket = runtime.root.join("forward");
    let mut command = Command::new("ssh");
    command
        .args([
            "-F",
            "/dev/null",
            "-N",
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "GlobalKnownHostsFile=/dev/null",
            "-o",
            "IdentityAgent=none",
            "-o",
            "ForwardAgent=no",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "PasswordAuthentication=no",
            "-o",
            "KbdInteractiveAuthentication=no",
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPath=none",
            "-o",
            "ExitOnForwardFailure=yes",
            "-o",
            "ConnectTimeout=20",
            "-o",
            "ServerAliveInterval=10",
            "-o",
            "ServerAliveCountMax=2",
        ])
        .arg("-o")
        .arg(format!(
            "UserKnownHostsFile={}",
            runtime.root.join("known_hosts").display()
        ))
        .arg("-i")
        .arg(runtime.root.join("identity"))
        .arg("-L")
        .arg(format!(
            "{}:127.0.0.1:{}",
            socket.display(),
            lease.runner_port.ok_or(Error::Auth)?
        ))
        .arg("-p")
        .arg(endpoint.port().to_string())
        .arg("-l")
        .arg(&ssh.user)
        .arg(endpoint.ip().to_string())
        .env_remove("SSH_AUTH_SOCK")
        .env_remove("SSH_AGENT_PID")
        .env_remove("SUBC_LAUNCH_NONCE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    runtime.child = Some(command.spawn().map_err(|_| Error::Connect)?);
    let stream = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let child = runtime.child.as_mut().unwrap();
            if child.try_wait().map_err(|_| Error::Connect)?.is_some() {
                let mut stderr = String::new();
                if let Some(stderr_pipe) = child.stderr.take() {
                    let _ = stderr_pipe.take(8192).read_to_string(&mut stderr).await;
                }
                return Err(
                    if stderr.contains("Host key verification failed")
                        || stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
                    {
                        Error::SshKey
                    } else {
                        Error::Connect
                    },
                );
            }
            if let Ok(stream) = UnixStream::connect(&socket).await {
                return Ok(stream);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| Error::Connect)??;
    Ok(Pipe {
        stream,
        _runtime: runtime,
    })
}

#[cfg(test)]
pub(super) mod tests;
