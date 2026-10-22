//! Exercise the executable's SSH byte bridge, not the in-process runner server.
use super::process::ChildGuard;
use cortexkit_test_support::{ckdev_binary, ScratchDir};
use motor_protocol::{read_frame, write_frame, Frame, PROTOCOL_VERSION};
use std::{
    fs,
    io::{Read, Write},
    net::Shutdown,
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    process::{Command, Stdio},
    sync::{mpsc, OnceLock},
    thread,
    time::Duration,
};

pub(super) struct SocketScratch {
    _owner: ScratchDir,
    path: std::path::PathBuf,
}
impl SocketScratch {
    pub(super) fn new() -> Self {
        let owner = ScratchDir::new("mc");
        // Keep the socket below macOS SUN_LEN without abandoning the shared
        // scratch root's ownership and cleanup rules.
        let path = loop {
            let id = motor_protocol::Uuid::now_v7().simple().to_string();
            let path = owner
                .path()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join(format!("c{}", &id[26..]));
            match std::os::unix::fs::symlink(owner.path(), &path) {
                Ok(()) => break path,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create short socket alias: {error}"),
            }
        };
        Self {
            _owner: owner,
            path,
        }
    }
    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}
impl Drop for SocketScratch {
    fn drop(&mut self) {
        fs::remove_file(&self.path).unwrap();
    }
}

fn shim(root: &Path) -> ChildGuard {
    static BINARY: OnceLock<std::path::PathBuf> = OnceLock::new();
    ChildGuard::spawn(
        Command::new(
            BINARY.get_or_init(|| ckdev_binary(Path::new(env!("CARGO_BIN_EXE_motor-runner")))),
        )
        .args(["connect", "--instance", "p", "--state-root"])
        .arg(root)
        .env("HOME", root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped()),
    )
    .unwrap()
}

fn listener(root: &Path) -> UnixListener {
    fs::create_dir_all(root.join("p")).unwrap();
    UnixListener::bind(root.join("p/runner.sock")).unwrap()
}

fn peer(listener: &UnixListener) -> UnixStream {
    let socket = listener.accept().unwrap().0;
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    socket
}

#[test]
fn connect_forwards_short_frames_before_socket_eof() {
    let scratch = SocketScratch::new();
    let listener = listener(scratch.path());
    let hello = Frame::Hello {
        return_paths: false,
        return_chunked: false,
        sibling_paths: false,
        protocol_version: PROTOCOL_VERSION,
        started: true,
        default_branch_tip: false,
        gate_jobs: false,
        platforms: Vec::new(),
    };
    let mut channels = Vec::new();
    // Several real shims, with no runner lock or worktree preparation involved.
    for _ in 0..4 {
        let mut child = shim(scratch.path());
        let mut socket = peer(&listener);
        let mut input = child.stdin.take().unwrap();
        write_frame(&mut input, &hello).unwrap();
        assert_eq!(read_frame(&mut socket).unwrap(), Some(hello.clone()));
        write_frame(&mut socket, &hello).unwrap();
        channels.push((child, socket, input));
    }
    // Let the bridge wait for its next socket read before reading stdout. The
    // Linux splice fast path can otherwise win the race and hide the defect.
    thread::sleep(Duration::from_millis(50));
    let mut readers = Vec::new();
    for (child, _, _) in &mut channels {
        let mut output = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            for _ in 0..2 {
                let _ = tx.send(read_frame(&mut output));
            }
        });
        readers.push((rx, reader));
    }
    let mut stalled = Vec::new();
    for (i, ((_, socket, input), (rx, _))) in channels.iter_mut().zip(&readers).enumerate() {
        for exchange in 0..2 {
            if exchange == 1 {
                write_frame(input, &Frame::Status).unwrap();
                assert_eq!(read_frame(socket).unwrap(), Some(Frame::Status));
                write_frame(socket, &hello).unwrap();
            }
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(Ok(Some(frame))) if frame == hello => {}
                other => {
                    stalled.push(format!("channel {i}, exchange {exchange}: {other:?}"));
                    break;
                }
            }
        }
    }
    // Always unblock readers before asserting: a regression must fail, not hang
    // the test process with a reader stuck behind the bridge's next splice.
    for (mut child, socket, input) in channels {
        let _ = socket.shutdown(Shutdown::Both);
        drop(input);
        let _ = child.kill();
        let _ = child.wait();
    }
    for (_, reader) in readers {
        reader.join().unwrap();
    }
    assert!(
        stalled.is_empty(),
        "short runner frames waited for later socket data or EOF: {stalled:?}"
    );
}

#[test]
fn connect_preserves_large_binary_payload_and_eof() {
    let scratch = SocketScratch::new();
    let listener = listener(scratch.path());
    let mut child = shim(scratch.path());
    let mut socket = peer(&listener);
    let payload: Vec<u8> = (0..65_537).map(|i| (i % 256) as u8).collect();
    let mut output = child.stdout.take().unwrap();
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        output.read_to_end(&mut bytes).unwrap();
        bytes
    });
    socket.write_all(&payload).unwrap();
    socket.shutdown(Shutdown::Write).unwrap();
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
    assert_eq!(reader.join().unwrap(), payload);
}

#[test]
fn connect_exits_quietly_when_stdout_pipe_closes() {
    let scratch = SocketScratch::new();
    let listener = listener(scratch.path());
    let mut child = shim(scratch.path());
    let mut socket = peer(&listener);
    drop(child.stdout.take());
    write_frame(
        &mut socket,
        &Frame::Heartbeat {
            queue_wait_ms: None,
        },
    )
    .unwrap();
    drop(child.stdin.take());
    // With its stdout gone the child may already have exited and closed the
    // socket. macOS then refuses the half-close with ENOTCONN (Linux accepts
    // it); either way the peer is gone, and the exit status below is the claim.
    match socket.shutdown(Shutdown::Write) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotConnected => {}
        Err(error) => panic!("half-close the runner socket: {error}"),
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "normal SSH detach: {output:?}");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("Error:"));
}

#[test]
fn drain_executable_sends_bounded_hold_request_and_reports_deadline() {
    for timed_out in [false, true] {
        let scratch = SocketScratch::new();
        let listener = listener(scratch.path());
        let child = ChildGuard::spawn(
            Command::new(ckdev_binary(Path::new(env!("CARGO_BIN_EXE_motor-runner"))))
                .args(["drain", "--instance", "p", "--state-root"])
                .arg(scratch.path())
                .args(["--drain-timeout-s", "2", "--exit-after-drain", "false"])
                .env("HOME", scratch.path())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
        )
        .unwrap();
        let mut socket = peer(&listener);
        assert_eq!(
            read_frame(&mut socket).unwrap(),
            Some(Frame::Hello {
                return_paths: false,
                return_chunked: false,
                sibling_paths: false,
                protocol_version: PROTOCOL_VERSION,
                started: true,
                default_branch_tip: false,
                gate_jobs: false,
                platforms: Vec::new(),
            })
        );
        write_frame(
            &mut socket,
            &Frame::Hello {
                return_paths: false,
                return_chunked: false,
                sibling_paths: false,
                protocol_version: PROTOCOL_VERSION,
                started: true,
                default_branch_tip: false,
                gate_jobs: false,
                platforms: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(
            read_frame(&mut socket).unwrap(),
            Some(Frame::Drain {
                deadline_s: 2,
                exit_after: false
            })
        );
        write_frame(
            &mut socket,
            &Frame::DrainReply {
                timed_out,
                restart_recoverable_jobs: Some(2),
            },
        )
        .unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.success(), !timed_out);
        if timed_out {
            assert!(String::from_utf8_lossy(&output.stderr).contains("drain deadline exceeded"));
        } else {
            assert!(String::from_utf8_lossy(&output.stdout)
                .contains("runner drain ended without a deadline timeout"));
            assert!(String::from_utf8_lossy(&output.stdout)
                .contains("leaving 2 restart-recoverable jobs to reattach"));
        }
    }
}

#[test]
fn drain_executable_refuses_an_old_server_instead_of_hanging_at_eof() {
    let scratch = SocketScratch::new();
    let listener = listener(scratch.path());
    let child = ChildGuard::spawn(
        Command::new(ckdev_binary(Path::new(env!("CARGO_BIN_EXE_motor-runner"))))
            .args(["drain", "--instance", "p", "--state-root"])
            .arg(scratch.path())
            .env("HOME", scratch.path())
            .stderr(Stdio::piped()),
    )
    .unwrap();
    let mut socket = peer(&listener);
    assert!(matches!(
        read_frame(&mut socket).unwrap(),
        Some(Frame::Hello { .. })
    ));
    socket.shutdown(Shutdown::Both).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("did not negotiate drain protocol"));
}

#[test]
fn resume_executable_negotiates_admin_frame_and_rejects_legacy_eof() {
    for supported in [true, false] {
        let scratch = SocketScratch::new();
        let listener = listener(scratch.path());
        let child = ChildGuard::spawn(
            Command::new(ckdev_binary(Path::new(env!("CARGO_BIN_EXE_motor-runner"))))
                .args(["resume", "--instance", "p", "--state-root"])
                .arg(scratch.path())
                .env("HOME", scratch.path())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
        )
        .unwrap();
        let mut socket = peer(&listener);
        assert!(matches!(
            read_frame(&mut socket).unwrap(),
            Some(Frame::Hello { .. })
        ));
        write_frame(
            &mut socket,
            &Frame::Hello {
                return_paths: false,
                return_chunked: false,
                sibling_paths: false,
                protocol_version: PROTOCOL_VERSION,
                started: true,
                default_branch_tip: false,
                gate_jobs: false,
                platforms: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(read_frame(&mut socket).unwrap(), Some(Frame::Resume));
        if supported {
            write_frame(&mut socket, &Frame::ResumeReply { resumed: true }).unwrap();
        } else {
            socket.shutdown(Shutdown::Both).unwrap();
        }
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.success(), supported);
        if !supported {
            assert!(String::from_utf8_lossy(&output.stderr)
                .contains("older daemons may not support Resume"));
        }
    }
}
