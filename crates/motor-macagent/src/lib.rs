//! Single-job guest execution using the shared macOS agent protocol.

pub use motor_macvm::{BootConfig, Mount, read_boot_config};
use std::io::{self, Read};
#[cfg(unix)]
pub mod mounts;

pub const VSOCK_PORT: u32 = 5000;
pub const CARGO_HOME: &str = "/opt/motor/job/cargo-home";
pub const BASH: &str = "/opt/motor/bin/bash";
pub const HOME: &str = "/Users/motorjob";
pub const PATH: &str =
    "/opt/motor/cargo/bin:/opt/motor/bun/bin:/opt/motor/bin:/usr/bin:/bin:/usr/sbin:/sbin";

/// Probe whole raw disks, requiring exactly one v2 boot metadata disk.
/// A matching header with corrupt metadata is an error, not a disk to skip.
pub fn find_boot_config<R: Read>(disks: impl IntoIterator<Item = R>) -> io::Result<BootConfig> {
    let mut found = None;
    for disk in disks {
        let Some(config) = motor_macvm::read_boot_candidate(disk)? else {
            continue;
        };
        if found.replace(config).is_some() {
            return Err(io::Error::other("multiple nonce disks"));
        }
    }
    found.ok_or_else(|| io::Error::other("nonce disk not found"))
}

#[cfg(test)]
mod nonce_tests {
    use super::*;
    const NONCE: &str = "0123456789abcdef0123456789abcdef";
    fn disk() -> Vec<u8> {
        motor_macvm::nonce_bytes(NONCE, &[]).unwrap()
    }
    #[test]
    fn discovery_uses_one_aligned_full_device_read() {
        struct Device(Vec<u8>);
        impl Read for Device {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                assert_eq!(out.len(), motor_macvm::NONCE_DISK_SIZE);
                assert_eq!(out.as_ptr() as usize % 4096, 0);
                out.copy_from_slice(&self.0);
                Ok(out.len())
            }
        }
        assert_eq!(find_boot_config([Device(disk())]).unwrap().nonce, NONCE);
    }
    #[test]
    fn v2_metadata_reader_is_shared_with_host_writer() {
        let bytes = disk();
        assert_eq!(read_boot_config(&bytes[..]).unwrap().nonce, NONCE);
        let mut old = bytes.clone();
        old[b"MOTOR-NONCE-V".len()] = b'1';
        assert!(read_boot_config(&old[..]).is_err());
        let mut corrupt = bytes;
        *corrupt.last_mut().unwrap() = 1;
        assert!(read_boot_config(&corrupt[..]).is_err());
    }
    #[test]
    fn discovers_exactly_one_v2_disk_and_rejects_corrupt_matching_disk() {
        let valid = disk();
        for disks in [
            vec![vec![0; 512], valid.clone()],
            vec![valid.clone(), vec![0; 512]],
        ] {
            assert_eq!(
                find_boot_config(disks.iter().map(|d| &d[..]))
                    .unwrap()
                    .nonce,
                NONCE
            );
        }
        assert!(
            find_boot_config([&[0; 512][..]])
                .unwrap_err()
                .to_string()
                .contains("not found")
        );
        assert!(
            find_boot_config([&valid[..], &valid[..]])
                .unwrap_err()
                .to_string()
                .contains("multiple")
        );
        let mut bad = valid.clone();
        bad[motor_macvm::NONCE_HEADER.len()] = b'g';
        assert!(find_boot_config([&bad[..], &valid[..]]).is_err());
    }
}

#[cfg(unix)]
mod execution {
    use super::*;
    use motor_winagent::{Frame, Peer, Profile, Protocol, Stream};
    use std::collections::BTreeMap;
    use std::os::unix::process::CommandExt;
    use std::os::unix::process::ExitStatusExt;
    use std::path::{Path, PathBuf};
    use std::process::{ExitStatus, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
    use tokio::sync::mpsc;
    use tokio::task::{JoinHandle, JoinSet};

    const PIPE_EOF_LIMIT: Duration = Duration::from_secs(2);
    const OUTPUT_CHUNK: usize = 16 * 1024;
    type SharedProtocol = Arc<Mutex<Protocol>>;

    pub fn job_environment(
        caller: &BTreeMap<String, String>,
        tmpdir: &Path,
        cargo_home: &Path,
    ) -> BTreeMap<String, String> {
        let mut env: BTreeMap<_, _> = caller
            .iter()
            .filter(|(key, _)| motor_protocol::allowed_env(key))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        for (key, value) in [
            ("HOME", HOME.to_owned()),
            ("TMPDIR", tmpdir.to_string_lossy().into_owned()),
            ("PATH", PATH.to_owned()),
            ("RUSTUP_HOME", "/opt/motor/rustup".to_owned()),
            ("CARGO_HOME", cargo_home.to_string_lossy().into_owned()),
        ] {
            env.insert(key.to_owned(), value);
        }
        env.remove("CARGO_TARGET_DIR");
        env
    }

    #[derive(Clone, Copy)]
    struct User {
        uid: libc::uid_t,
        gid: libc::gid_t,
    }
    fn validate_user(user: User) -> io::Result<User> {
        if user.uid == 0 || user.gid == 0 {
            return Err(io::Error::other(
                "motorjob must have a non-root uid and gid",
            ));
        }
        Ok(user)
    }
    fn set_credentials(process: &mut tokio::process::Command, user: User) -> io::Result<()> {
        let user = validate_user(user)?;
        // std applies gid/uid and clears supplementary groups before resolving
        // current_dir; a pre_exec credential drop runs too late for that lookup.
        process.as_std_mut().gid(user.gid).uid(user.uid);
        Ok(())
    }
    /// After clearing, the group list must hold no supplementary group. macOS
    /// keeps the effective gid as the list's first entry even after
    /// setgroups(0, NULL), so a list of exactly [egid] counts as cleared; the
    /// child's own setgid replaces that entry before it runs.
    fn require_empty_groups(
        clear: impl FnOnce() -> io::Result<()>,
        groups: impl FnOnce() -> io::Result<Vec<libc::gid_t>>,
        egid: libc::gid_t,
    ) -> io::Result<()> {
        clear()?;
        let groups = groups()?;
        if !(groups.is_empty() || groups == [egid]) {
            return Err(io::Error::other(format!(
                "guest supplementary groups were not cleared: {groups:?} (egid {egid})"
            )));
        }
        Ok(())
    }

    enum KillScope {
        User(User),
        #[cfg(test)]
        ProcessGroup {
            pid: std::sync::atomic::AtomicI32,
            suppress: std::sync::atomic::AtomicBool,
            killed: tokio::sync::Notify,
        },
    }

    impl KillScope {
        fn kill(&self) -> io::Result<()> {
            match self {
                Self::User(user) => {
                    let user = validate_user(*user)?;
                    // A non-root kill(-1) reaches all processes of the job uid,
                    // including descendants that escaped the shell's process group.
                    // The root agent must never issue that syscall as root.
                    unsafe {
                        let pid = libc::fork();
                        if pid == -1 {
                            return Err(io::Error::last_os_error());
                        }
                        if pid == 0 {
                            if libc::setgroups(0, std::ptr::null()) != 0
                                || libc::setgid(user.gid) != 0
                                || libc::setuid(user.uid) != 0
                            {
                                libc::_exit(1);
                            }
                            let result = libc::kill(-1, libc::SIGKILL);
                            let absent =
                                io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
                            libc::_exit(if result == 0 || absent { 0 } else { 1 });
                        }
                        let mut status = 0;
                        loop {
                            if libc::waitpid(pid, &mut status, 0) != -1 {
                                break;
                            }
                            let error = io::Error::last_os_error();
                            if error.kind() != io::ErrorKind::Interrupted {
                                return Err(error);
                            }
                        }
                        if status != 0 {
                            return Err(io::Error::other("motorjob SIGKILL failed"));
                        }
                    }
                    Ok(())
                }
                #[cfg(test)]
                Self::ProcessGroup {
                    pid,
                    suppress,
                    killed,
                } => {
                    if suppress.load(std::sync::atomic::Ordering::SeqCst) {
                        return Ok(());
                    }
                    let pid = pid.load(std::sync::atomic::Ordering::SeqCst);
                    if pid > 0 && unsafe { libc::kill(-pid, libc::SIGKILL) } != 0 {
                        let error = io::Error::last_os_error();
                        if error.raw_os_error() != Some(libc::ESRCH) {
                            return Err(error);
                        }
                    }
                    killed.notify_one();
                    Ok(())
                }
            }
        }
    }

    struct Cleanup(Arc<KillScope>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.0.kill();
        }
    }

    struct Task<T>(JoinHandle<T>);
    impl<T> Drop for Task<T> {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    struct Settings {
        shell: PathBuf,
        cargo_home: PathBuf,
        temporary_root: PathBuf,
        scope: Arc<KillScope>,
    }

    /// Only the root launch daemon calls this entry point. Tests use a private
    /// process group instead of the real job uid, so they cannot kill other jobs.
    pub async fn serve_guest<S>(stream: S, nonce: String, cargo_home: PathBuf) -> io::Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        if unsafe { libc::geteuid() } != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "agent must run as root",
            ));
        }
        // uid() clears groups in the child before chdir, but std tolerates
        // EPERM from that clearing. Empty the daemon's groups first and fail
        // closed here so no child can inherit a privileged supplementary group.
        require_empty_groups(
            || {
                if unsafe { libc::setgroups(0, std::ptr::null()) } != 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            },
            || {
                let mut groups = vec![0 as libc::gid_t; 64];
                let count = unsafe { libc::getgroups(groups.len() as i32, groups.as_mut_ptr()) };
                if count < 0 {
                    return Err(io::Error::last_os_error());
                }
                groups.truncate(count as usize);
                Ok(groups)
            },
            unsafe { libc::getegid() },
        )?;
        // Copy the passwd fields before another libc lookup can reuse its storage.
        let user = unsafe {
            let entry = libc::getpwnam(c"motorjob".as_ptr());
            if entry.is_null() || (*entry).pw_uid == 0 {
                return Err(io::Error::other("unprivileged motorjob account missing"));
            }
            validate_user(User {
                uid: (*entry).pw_uid,
                gid: (*entry).pw_gid,
            })?
        };
        serve(
            stream,
            nonce,
            Settings {
                shell: BASH.into(),
                cargo_home,
                temporary_root: "/var/tmp".into(),
                scope: Arc::new(KillScope::User(user)),
            },
        )
        .await
    }

    // The async transport processes one bounded frame at a time in each direction;
    // shared protocol code handles JSON decoding and state transitions.
    async fn receive<R: AsyncRead + Unpin>(
        reader: &mut R,
        protocol: &SharedProtocol,
    ) -> io::Result<Frame> {
        let mut header = [0; 4];
        reader.read_exact(&mut header).await?;
        let length = u32::from_be_bytes(header) as usize;
        if length == 0 || length > motor_winagent::MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid frame length",
            ));
        }
        let mut bytes = vec![0; length + 4];
        bytes[..4].copy_from_slice(&header);
        reader.read_exact(&mut bytes[4..]).await?;
        let frame = motor_winagent::read_frame(&mut &bytes[..])?.expect("complete frame");
        protocol.lock().unwrap().accept(Peer::Host, &frame)?;
        Ok(frame)
    }

    async fn send<W: AsyncWrite + Unpin>(
        writer: &mut W,
        protocol: &SharedProtocol,
        frame: Frame,
    ) -> io::Result<()> {
        protocol.lock().unwrap().accept(Peer::Agent, &frame)?;
        let mut bytes = Vec::new();
        motor_winagent::write_frame(&mut bytes, &frame)?;
        writer.write_all(&bytes).await?;
        writer.flush().await
    }

    enum Output {
        Chunk(Stream, usize),
        Terminal(Frame),
    }

    async fn drain<R: AsyncRead + Unpin>(
        mut pipe: R,
        mut spool: tokio::fs::File,
        stream: Stream,
        output: mpsc::UnboundedSender<Output>,
    ) -> io::Result<()> {
        let mut bytes = vec![0; OUTPUT_CHUNK];
        loop {
            let count = pipe.read(&mut bytes).await?;
            if count == 0 {
                return Ok(());
            }
            spool.write_all(&bytes[..count]).await?;
            // Flush the async file adapter before publishing a readable range.
            spool.flush().await?;
            output
                .send(Output::Chunk(stream, count))
                .map_err(|_| io::Error::other("output closed"))?;
        }
    }

    async fn forward<W: AsyncWrite + Unpin>(
        mut writer: W,
        protocol: SharedProtocol,
        mut stdout: tokio::fs::File,
        mut stderr: tokio::fs::File,
        mut output: mpsc::UnboundedReceiver<Output>,
    ) -> io::Result<()> {
        send(&mut writer, &protocol, Frame::Started {}).await?;
        while let Some(event) = output.recv().await {
            match event {
                Output::Chunk(stream, count) => {
                    let mut bytes = vec![0; count];
                    let file = match stream {
                        Stream::Stdout => &mut stdout,
                        Stream::Stderr => &mut stderr,
                    };
                    file.read_exact(&mut bytes).await?;
                    send(&mut writer, &protocol, Frame::Output { stream, bytes }).await?;
                }
                Output::Terminal(frame) => {
                    send(&mut writer, &protocol, frame).await?;
                    return writer.shutdown().await;
                }
            }
        }
        Err(io::Error::other(
            "output_incomplete: missing terminal status",
        ))
    }

    fn termination(status: ExitStatus) -> io::Result<Frame> {
        if let Some(signal) = status.signal() {
            Ok(Frame::Signal { signal })
        } else if let Some(code) = status.code() {
            Ok(Frame::Exit { code: code as u32 })
        } else {
            Err(io::Error::other("unrecognized command status"))
        }
    }

    async fn serve<S>(stream: S, nonce: String, settings: Settings) -> io::Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let protocol = Arc::new(Mutex::new(Protocol::with_profile(Profile::Macos, &nonce)?));
        let (mut reader, mut writer) = tokio::io::split(stream);
        send(
            &mut writer,
            &protocol,
            Frame::Hello {
                version: Profile::Macos.version(),
                nonce,
            },
        )
        .await?;
        let Frame::TreeEnd { command } = receive(&mut reader, &protocol).await? else {
            return Err(io::Error::other("expected command"));
        };
        let tmpdir = tempfile::Builder::new()
            .prefix("motor-job-")
            .tempdir_in(&settings.temporary_root)?;
        // Reopen by name for independent offsets; dup/try_clone would share the
        // writer's seek position. These root-owned files isolate pipe draining
        // from host backpressure without buffering the command's output in RAM.
        let stdout = tempfile::NamedTempFile::new_in(&settings.temporary_root)?;
        let stderr = tempfile::NamedTempFile::new_in(&settings.temporary_root)?;
        let stdout_write = tokio::fs::File::from_std(stdout.reopen()?);
        let stderr_write = tokio::fs::File::from_std(stderr.reopen()?);
        let stdout_read = tokio::fs::File::from_std(stdout.reopen()?);
        let stderr_read = tokio::fs::File::from_std(stderr.reopen()?);
        let mut process = tokio::process::Command::new(&settings.shell);
        process
            .arg("-c")
            .arg(&command.command)
            .current_dir(&command.cwd)
            .env_clear()
            .envs(job_environment(
                &command.env,
                tmpdir.path(),
                &settings.cargo_home,
            ))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        match &*settings.scope {
            KillScope::User(user) => {
                let user = validate_user(*user)?;
                let path = std::ffi::CString::new(tmpdir.path().as_os_str().as_encoded_bytes())?;
                if unsafe { libc::chown(path.as_ptr(), user.uid, user.gid) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                set_credentials(&mut process, user)?;
            }
            #[cfg(test)]
            KillScope::ProcessGroup { .. } => {
                process.as_std_mut().process_group(0);
            }
        }
        let mut child = process.spawn()?;
        #[cfg(test)]
        if let KillScope::ProcessGroup { pid, .. } = &*settings.scope {
            pid.store(
                child.id().unwrap() as i32,
                std::sync::atomic::Ordering::SeqCst,
            );
        }
        let _cleanup = Cleanup(settings.scope.clone());
        let (output_tx, output_rx) = mpsc::unbounded_channel();
        let mut pipes = JoinSet::new();
        pipes.spawn(drain(
            child.stdout.take().unwrap(),
            stdout_write,
            Stream::Stdout,
            output_tx.clone(),
        ));
        pipes.spawn(drain(
            child.stderr.take().unwrap(),
            stderr_write,
            Stream::Stderr,
            output_tx.clone(),
        ));
        let mut forwarding = Task(tokio::spawn(forward(
            writer,
            protocol.clone(),
            stdout_read,
            stderr_read,
            output_rx,
        )));
        let (control_tx, mut control_rx) = mpsc::unbounded_channel();
        let _input = Task(tokio::spawn(async move {
            loop {
                let message = receive(&mut reader, &protocol).await;
                let failed = message.is_err();
                if control_tx.send(message).is_err() || failed {
                    break;
                }
            }
        }));
        let mut status = None;
        let mut eof_deadline = None;
        while status.is_none() || !pipes.is_empty() {
            tokio::select! {
                result = child.wait(), if status.is_none() => {
                    status = Some(result?);
                    settings.scope.kill()?;
                    eof_deadline = Some(tokio::time::Instant::now() + PIPE_EOF_LIMIT);
                }
                result = pipes.join_next(), if !pipes.is_empty() => {
                    result.unwrap().map_err(io::Error::other)??;
                }
                result = &mut forwarding.0 => { return result.map_err(io::Error::other)?; }
                message = control_rx.recv() => { handle_control(message, &settings.scope)?; }
                _ = async { tokio::time::sleep_until(eof_deadline.unwrap()).await }, if eof_deadline.is_some() && !pipes.is_empty() => {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "output_incomplete: pipe did not reach EOF after SIGKILL"));
                }
            }
        }
        output_tx
            .send(Output::Terminal(termination(status.unwrap())?))
            .map_err(|_| io::Error::other("output closed"))?;
        loop {
            tokio::select! {
                result = &mut forwarding.0 => { return result.map_err(io::Error::other)?; }
                message = control_rx.recv() => { handle_control(message, &settings.scope)?; }
            }
        }
    }

    fn handle_control(message: Option<io::Result<Frame>>, scope: &KillScope) -> io::Result<()> {
        match message {
            Some(Ok(Frame::Cancel {})) => scope.kill(),
            Some(Err(error)) => Err(error),
            _ => Err(io::Error::other("unexpected host frame")),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
        use tokio::io::DuplexStream;

        const NONCE: &str = "0123456789abcdef0123456789abcdef";
        #[test]
        fn root_uid_or_primary_gid_is_rejected() {
            for user in [User { uid: 0, gid: 20 }, User { uid: 501, gid: 0 }] {
                assert!(validate_user(user).is_err());
            }
            assert!(validate_user(User { uid: 501, gid: 20 }).is_ok());
        }
        #[test]
        fn supplementary_group_clear_failure_is_fatal() {
            assert!(
                require_empty_groups(
                    || Err(io::Error::from_raw_os_error(libc::EPERM)),
                    || Ok(vec![]),
                    0
                )
                .is_err()
            );
            assert!(require_empty_groups(|| Ok(()), || Ok(vec![]), 0).is_ok());
            // Measured on macOS 27 as root: setgroups(0, NULL) leaves [egid].
            assert!(require_empty_groups(|| Ok(()), || Ok(vec![0]), 0).is_ok());
            assert!(require_empty_groups(|| Ok(()), || Ok(vec![0, 80]), 0).is_err());
            assert!(require_empty_groups(|| Ok(()), || Ok(vec![80]), 0).is_err());
        }
        #[cfg(target_os = "macos")]
        #[tokio::test]
        #[ignore = "operator: root inside disposable guest with motorjob; never on the host"]
        async fn macos_qualification_drop_precedes_cwd_resolution() {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::env::var("MOTOR_MACOS_GUEST_QUALIFICATION").as_deref(),
                Ok("1")
            );
            assert_eq!(unsafe { libc::geteuid() }, 0);
            let entry = unsafe { libc::getpwnam(c"motorjob".as_ptr()) };
            assert!(!entry.is_null());
            let user = validate_user(unsafe {
                User {
                    uid: (*entry).pw_uid,
                    gid: (*entry).pw_gid,
                }
            })
            .unwrap();
            let scratch = cortexkit_test_support::scratch_root();
            std::fs::create_dir_all(&scratch).unwrap();
            let root = tempfile::tempdir_in(scratch).unwrap();
            std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            let mut command = tokio::process::Command::new(BASH);
            command.args(["-c", "exit 0"]).current_dir(root.path());
            set_credentials(&mut command, user).unwrap();
            assert_eq!(
                command.spawn().unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
        }

        struct Fixture {
            host: DuplexStream,
            agent: Task<io::Result<()>>,
            scope: Arc<KillScope>,
            _directory: tempfile::TempDir,
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                if let KillScope::ProcessGroup { suppress, .. } = &*self.scope {
                    suppress.store(false, Ordering::SeqCst);
                }
                let _ = self.scope.kill();
            }
        }

        async fn host_send(host: &mut DuplexStream, frame: Frame) {
            let mut bytes = Vec::new();
            motor_winagent::write_frame(&mut bytes, &frame).unwrap();
            host.write_all(&bytes).await.unwrap();
        }
        async fn host_receive(host: &mut DuplexStream) -> io::Result<Option<Frame>> {
            let mut header = [0; 4];
            if host.read(&mut header[..1]).await? == 0 {
                return Ok(None);
            }
            host.read_exact(&mut header[1..]).await?;
            let length = u32::from_be_bytes(header) as usize;
            assert!(length <= motor_winagent::MAX_FRAME_BYTES);
            let mut bytes = vec![0; 4 + length];
            bytes[..4].copy_from_slice(&header);
            host.read_exact(&mut bytes[4..]).await?;
            motor_winagent::read_frame(&mut &bytes[..])
        }

        async fn start(command: &str, suppress_kill: bool) -> Fixture {
            let root = cortexkit_test_support::scratch_root();
            std::fs::create_dir_all(&root).unwrap();
            let directory = tempfile::tempdir_in(root).unwrap();
            let scope = Arc::new(KillScope::ProcessGroup {
                pid: AtomicI32::new(0),
                suppress: AtomicBool::new(suppress_kill),
                killed: tokio::sync::Notify::new(),
            });
            let settings = Settings {
                shell: "/bin/bash".into(),
                cargo_home: CARGO_HOME.into(),
                temporary_root: directory.path().into(),
                scope: scope.clone(),
            };
            let (mut host, guest) = tokio::io::duplex(1024);
            let agent = Task(tokio::spawn(serve(guest, NONCE.into(), settings)));
            assert_eq!(
                host_receive(&mut host).await.unwrap(),
                Some(Frame::Hello {
                    version: 1,
                    nonce: NONCE.into()
                })
            );
            host_send(
                &mut host,
                Frame::TreeEnd {
                    command: motor_winagent::Command {
                        command: command.into(),
                        cwd: directory
                            .path()
                            .canonicalize()
                            .unwrap()
                            .to_string_lossy()
                            .into_owned(),
                        workspace: directory
                            .path()
                            .canonicalize()
                            .unwrap()
                            .to_string_lossy()
                            .into_owned(),
                        env: BTreeMap::new(),
                        source_commit: String::new(),
                    },
                },
            )
            .await;
            assert_eq!(
                host_receive(&mut host).await.unwrap(),
                Some(Frame::Started {})
            );
            Fixture {
                host,
                agent,
                scope,
                _directory: directory,
            }
        }

        async fn collect(fixture: &mut Fixture) -> (Vec<u8>, Vec<u8>, Frame) {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            loop {
                match host_receive(&mut fixture.host)
                    .await
                    .unwrap()
                    .expect("terminal frame")
                {
                    Frame::Output { stream, bytes } => match stream {
                        Stream::Stdout => stdout.extend(bytes),
                        Stream::Stderr => stderr.extend(bytes),
                    },
                    terminal @ (Frame::Exit { .. } | Frame::Signal { .. }) => {
                        assert!(host_receive(&mut fixture.host).await.unwrap().is_none());
                        (&mut fixture.agent.0).await.unwrap().unwrap();
                        return (stdout, stderr, terminal);
                    }
                    frame => panic!("unexpected frame {frame:?}"),
                }
            }
        }

        async fn wait_for_kill(fixture: &Fixture) {
            let KillScope::ProcessGroup { killed, .. } = &*fixture.scope else {
                unreachable!()
            };
            tokio::time::timeout(Duration::from_secs(3), killed.notified())
                .await
                .unwrap();
        }

        #[test]
        fn pinned_environment_overrides_caller_and_filters_secrets() {
            let mut caller = BTreeMap::new();
            for key in [
                "HOME",
                "TMPDIR",
                "PATH",
                "RUSTUP_HOME",
                "CARGO_HOME",
                "CARGO_TARGET_DIR",
            ] {
                caller.insert(key.to_owned(), "caller-value".to_owned());
            }
            caller.insert("RUST_BACKTRACE".into(), "1".into());
            caller.insert("CARGO_BUILD_JOBS".into(), "4".into());
            caller.insert("CARGO_REGISTRY_TOKEN".into(), "secret".into());
            caller.insert("BASH_ENV".into(), "host-script".into());
            let env = job_environment(&caller, Path::new("/fresh/job/tmp"), Path::new(CARGO_HOME));
            assert_eq!(env["HOME"], "/Users/motorjob");
            assert_eq!(env["TMPDIR"], "/fresh/job/tmp");
            assert_eq!(
                env["PATH"],
                "/opt/motor/cargo/bin:/opt/motor/bun/bin:/opt/motor/bin:/usr/bin:/bin:/usr/sbin:/sbin"
            );
            assert_eq!(env["RUSTUP_HOME"], "/opt/motor/rustup");
            assert_eq!(env["CARGO_HOME"], "/opt/motor/job/cargo-home");
            assert_eq!(env["RUST_BACKTRACE"], "1");
            assert_eq!(env["CARGO_BUILD_JOBS"], "4");
            for key in ["CARGO_TARGET_DIR", "CARGO_REGISTRY_TOKEN", "BASH_ENV"] {
                assert!(!env.contains_key(key));
            }
        }

        #[tokio::test]
        async fn exit_7_is_exit_not_signal() {
            let mut fixture = start("exit 7", false).await;
            assert_eq!(collect(&mut fixture).await.2, Frame::Exit { code: 7 });
        }

        #[tokio::test]
        async fn segv_is_signal_11_not_exit_139() {
            let mut fixture = start("ulimit -c 0; kill -SEGV $$", false).await;
            assert_eq!(collect(&mut fixture).await.2, Frame::Signal { signal: 11 });
        }

        #[tokio::test]
        async fn background_writer_is_killed_before_exit_0_within_two_seconds() {
            let mut fixture = start("sleep 1000 & exit 0", false).await;
            let result = tokio::time::timeout(Duration::from_secs(2), collect(&mut fixture))
                .await
                .unwrap();
            assert_eq!(result.2, Frame::Exit { code: 0 });
        }

        #[tokio::test]
        async fn background_writer_is_killed_before_signal_11_within_two_seconds() {
            let mut fixture = start("ulimit -c 0; sleep 1000 & kill -SEGV $$", false).await;
            let result = tokio::time::timeout(Duration::from_secs(2), collect(&mut fixture))
                .await
                .unwrap();
            assert_eq!(result.2, Frame::Signal { signal: 11 });
        }

        #[tokio::test]
        async fn delayed_reader_gets_all_output_before_status_without_forwarding_deadline() {
            let mut fixture = start("head -c 2097152 /dev/zero; printf 'tail-out\\377\\200'; head -c 2097153 /dev/zero >&2; printf 'tail-err\\377\\200' >&2", false).await;
            wait_for_kill(&fixture).await;
            tokio::time::sleep(Duration::from_millis(2500)).await;
            let (stdout, stderr, terminal) = collect(&mut fixture).await;
            let mut expected_out = vec![0; 2097152];
            expected_out.extend(b"tail-out\xff\x80");
            let mut expected_err = vec![0; 2097153];
            expected_err.extend(b"tail-err\xff\x80");
            assert_eq!(stdout, expected_out);
            assert_eq!(stderr, expected_err);
            assert_eq!(terminal, Frame::Exit { code: 0 });
        }

        #[tokio::test]
        async fn pipe_not_eof_after_kill_closes_session_without_terminal() {
            // Simulate an inherited writer surviving the uid kill. The fixture
            // owns and kills this process group after checking the failure path.
            let mut fixture = start("sleep 1000 & exit 0", true).await;
            let began = tokio::time::Instant::now();
            assert!(
                tokio::time::timeout(Duration::from_secs(3), host_receive(&mut fixture.host))
                    .await
                    .unwrap()
                    .unwrap()
                    .is_none()
            );
            let error = (&mut fixture.agent.0).await.unwrap().unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            assert!(error.to_string().contains("output_incomplete"));
            assert!(began.elapsed() >= Duration::from_secs(2));
        }

        #[tokio::test]
        async fn cancel_kills_shell_and_background_processes() {
            let mut fixture = start("sleep 1000 & wait", false).await;
            host_send(&mut fixture.host, Frame::Cancel {}).await;
            wait_for_kill(&fixture).await;
            let result = tokio::time::timeout(Duration::from_secs(2), collect(&mut fixture))
                .await
                .unwrap();
            assert_eq!(result.2, Frame::Signal { signal: 9 });
        }

        #[tokio::test]
        async fn cancel_is_processed_while_host_output_is_blocked() {
            let mut fixture = start("head -c 2097152 /dev/zero; sleep 1000 & wait", false).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            host_send(&mut fixture.host, Frame::Cancel {}).await;
            wait_for_kill(&fixture).await;
            let result = tokio::time::timeout(Duration::from_secs(2), collect(&mut fixture))
                .await
                .unwrap();
            assert_eq!(result.2, Frame::Signal { signal: 9 });
        }

        #[tokio::test]
        async fn bash_uses_caller_cwd_and_fresh_tmpdir() {
            let mut first = start("printf '%s\\n' \"$PWD\" \"$HOME\" \"$TMPDIR\"; test -d \"$TMPDIR\" && test -w \"$TMPDIR\"", false).await;
            let (stdout, _, status) = collect(&mut first).await;
            assert_eq!(status, Frame::Exit { code: 0 });
            let text = String::from_utf8(stdout).unwrap();
            let lines: Vec<_> = text.lines().collect();
            assert_eq!(
                lines[0],
                first
                    ._directory
                    .path()
                    .canonicalize()
                    .unwrap()
                    .to_str()
                    .unwrap()
            );
            assert_eq!(lines[1], HOME);
            let first_tmp = lines[2].to_owned();
            assert!(!Path::new(&first_tmp).exists());
            let mut second = start("printf '%s' \"$TMPDIR\"", false).await;
            let (stdout, _, _) = collect(&mut second).await;
            assert_ne!(String::from_utf8(stdout).unwrap(), first_tmp);
        }
    }
}

#[cfg(unix)]
pub use execution::{job_environment, serve_guest};
