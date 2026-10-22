//! The macOS profile of the shared protocol: its accepted sequence, its
//! refusal of every Windows-only frame family, and its end-of-session rules.

use super::*;

const NONCE: &str = "boot-mac";

fn command() -> Command {
    Command {
        command: "cargo test --offline".into(),
        cwd: "/Users/me/project".into(),
        workspace: "/Users/me/project".into(),
        env: BTreeMap::new(),
        source_commit: "abc123".into(),
    }
}

fn hello() -> Frame {
    Frame::Hello {
        version: MACOS_PROTOCOL_VERSION,
        nonce: NONCE.into(),
    }
}

fn output(stream: Stream) -> Frame {
    Frame::Output {
        stream,
        bytes: vec![0, 255, 10],
    }
}

/// A complete macOS conversation ending in `last`, which is Exit or Signal.
fn transcript(last: Frame) -> Vec<(Peer, Frame)> {
    vec![
        (Peer::Agent, hello()),
        (Peer::Host, Frame::TreeEnd { command: command() }),
        (Peer::Agent, Frame::TreeReceived {}),
        (Peer::Host, Frame::Start {}),
        (Peer::Agent, Frame::Started {}),
        (Peer::Agent, output(Stream::Stdout)),
        (Peer::Agent, output(Stream::Stderr)),
        (Peer::Agent, output(Stream::Stdout)),
        (Peer::Agent, last),
    ]
}

fn protocol() -> Protocol {
    Protocol::with_profile(Profile::Macos, NONCE).unwrap()
}

/// A macOS protocol after the first `count` frames of an Exit transcript.
fn advanced(count: usize) -> Protocol {
    let mut protocol = protocol();
    for (sender, frame) in transcript(Frame::Exit { code: 0 }).into_iter().take(count) {
        protocol.accept(sender, &frame).unwrap();
    }
    protocol
}

const TRANSCRIPT_LEN: usize = 9;

fn other(peer: Peer) -> Peer {
    match peer {
        Peer::Host => Peer::Agent,
        Peer::Agent => Peer::Host,
    }
}

/// Exchange `frames` between a host and an agent session over real framing,
/// checking that both sides track the same state.
fn converse(host: &mut Session, agent: &mut Session, frames: Vec<(Peer, Frame)>) {
    for (sender, frame) in frames {
        let (send, receive) = if sender == Peer::Host {
            (&mut *host, &mut *agent)
        } else {
            (&mut *agent, &mut *host)
        };
        let mut bytes = Vec::new();
        send.send(&mut bytes, &frame).unwrap();
        assert_eq!(receive.receive(&mut &bytes[..]).unwrap(), Some(frame));
        assert_eq!(host.state(), agent.state());
    }
}

fn sessions() -> (Session, Session) {
    (
        Session::with_profile(Peer::Host, Profile::Macos, NONCE).unwrap(),
        Session::with_profile(Peer::Agent, Profile::Macos, NONCE).unwrap(),
    )
}

#[test]
fn profiles_are_versioned_independently() {
    assert_eq!(Profile::Windows.version(), 4);
    assert_eq!(Profile::Macos.version(), 1);
    assert_eq!(Protocol::new("n").unwrap().profile(), Profile::Windows);
    assert_eq!(
        Session::new(Peer::Host, "n").unwrap().profile(),
        Profile::Windows
    );
    assert_eq!(protocol().profile(), Profile::Macos);
}

#[test]
fn exit_conversation_completes_and_then_eof_is_clean() {
    let (mut host, mut agent) = sessions();
    converse(&mut host, &mut agent, transcript(Frame::Exit { code: 7 }));
    assert_eq!(host.state(), State::Complete);
    assert_eq!(host.receive(&mut &[][..]).unwrap(), None);
    assert_eq!(agent.receive(&mut &[][..]).unwrap(), None);
}

#[test]
fn signal_conversation_completes_and_then_eof_is_clean() {
    let (mut host, mut agent) = sessions();
    converse(
        &mut host,
        &mut agent,
        transcript(Frame::Signal { signal: 11 }),
    );
    assert_eq!(host.state(), State::Complete);
    assert_eq!(host.receive(&mut &[][..]).unwrap(), None);
    assert_eq!(agent.receive(&mut &[][..]).unwrap(), None);
}

#[test]
fn command_may_end_without_output() {
    let mut protocol = advanced(5);
    assert_eq!(protocol.state(), State::Running);
    protocol
        .accept(Peer::Agent, &Frame::Signal { signal: 9 })
        .unwrap();
    assert_eq!(protocol.state(), State::Complete);
}

#[test]
fn eof_before_exit_or_signal_is_an_error_in_every_phase() {
    let frames = transcript(Frame::Exit { code: 0 });
    for step in 0..TRANSCRIPT_LEN {
        let (mut host, mut agent) = sessions();
        converse(&mut host, &mut agent, frames[..step].to_vec());
        for session in [&mut host, &mut agent] {
            let error = session.receive(&mut &[][..]).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof, "step {step}");
            assert!(
                error.to_string().contains("before exit or signal"),
                "step {step}: {error}"
            );
            assert_eq!(session.state(), State::Failed, "step {step}");
        }
    }
}

#[test]
fn hello_with_any_other_version_is_rejected() {
    for version in [0, 2, PROTOCOL_VERSION, u32::MAX] {
        let mut protocol = protocol();
        let error = protocol
            .accept(
                Peer::Agent,
                &Frame::Hello {
                    version,
                    nonce: NONCE.into(),
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("version"), "{version}: {error}");
        assert_eq!(protocol.state(), State::Failed);
    }
}

#[test]
fn hello_with_a_different_nonce_is_rejected() {
    let mut protocol = Protocol::with_profile(Profile::Macos, "other-boot").unwrap();
    let error = protocol.accept(Peer::Agent, &hello()).unwrap_err();
    assert!(error.to_string().contains("nonce"), "{error}");
    assert_eq!(protocol.state(), State::Failed);
    assert!(protocol.accept(Peer::Agent, &hello()).is_err());
    assert!(Protocol::with_profile(Profile::Macos, "").is_err());
}

#[test]
fn order_and_direction_are_enforced_at_every_phase() {
    let frames = transcript(Frame::Exit { code: 0 });
    for (step, (sender, frame)) in frames.iter().enumerate() {
        let mut protocol = advanced(step);
        assert!(
            protocol.accept(other(*sender), frame).is_err(),
            "wrong direction accepted at step {step}"
        );
        assert_eq!(protocol.state(), State::Failed);
    }
    for (count, sender, frame) in [
        (0, Peer::Host, Frame::TreeEnd { command: command() }),
        (1, Peer::Host, Frame::Start {}),
        (1, Peer::Agent, hello()),
        (2, Peer::Host, Frame::TreeEnd { command: command() }),
        (3, Peer::Agent, Frame::Started {}),
        (3, Peer::Agent, output(Stream::Stdout)),
        (4, Peer::Agent, Frame::Exit { code: 0 }),
        (4, Peer::Agent, Frame::Signal { signal: 9 }),
        (4, Peer::Agent, output(Stream::Stdout)),
        (5, Peer::Host, Frame::Start {}),
        (TRANSCRIPT_LEN, Peer::Agent, output(Stream::Stdout)),
        (TRANSCRIPT_LEN, Peer::Agent, Frame::Exit { code: 0 }),
        (TRANSCRIPT_LEN, Peer::Agent, Frame::Signal { signal: 9 }),
        (TRANSCRIPT_LEN, Peer::Host, Frame::Start {}),
        (TRANSCRIPT_LEN, Peer::Agent, hello()),
    ] {
        let mut protocol = advanced(count);
        assert!(
            protocol.accept(sender, &frame).is_err(),
            "accepted {frame:?} from {sender:?} after {count} frames"
        );
        assert_eq!(protocol.state(), State::Failed);
    }
}

#[test]
fn cancel_is_accepted_only_while_starting_or_running() {
    for step in 0..=TRANSCRIPT_LEN {
        let mut protocol = advanced(step);
        let before = protocol.state();
        let accepted = protocol.accept(Peer::Host, &Frame::Cancel {}).is_ok();
        let expected = matches!(before, State::AwaitStarted | State::Running);
        assert_eq!(accepted, expected, "cancel in {before:?}");
        if accepted {
            assert_eq!(protocol.state(), before);
        }
        let mut protocol = advanced(step);
        assert!(protocol.accept(Peer::Agent, &Frame::Cancel {}).is_err());
    }
    let mut protocol = advanced(4);
    protocol.accept(Peer::Host, &Frame::Cancel {}).unwrap();
    protocol.accept(Peer::Agent, &Frame::Started {}).unwrap();
    protocol.accept(Peer::Host, &Frame::Cancel {}).unwrap();
    protocol
        .accept(Peer::Agent, &Frame::Signal { signal: 9 })
        .unwrap();
    assert_eq!(protocol.state(), State::Complete);
}

#[test]
fn non_positive_signal_is_rejected() {
    for signal in [0, -1, i32::MIN] {
        let mut protocol = advanced(5);
        assert!(
            protocol
                .accept(Peer::Agent, &Frame::Signal { signal })
                .is_err(),
            "accepted signal {signal}"
        );
        assert_eq!(protocol.state(), State::Failed);
    }
}

#[test]
fn command_paths_follow_posix_rules_not_windows_names() {
    let mut accepted = command();
    accepted.cwd = "/Users/me/aux/con.rs dir.".into();
    let mut protocol = advanced(1);
    protocol
        .accept(Peer::Host, &Frame::TreeEnd { command: accepted })
        .unwrap();
    for (cwd, workspace) in [
        ("relative/project", "/Users/me/project"),
        ("/Users/me/../project", "/Users/me/project"),
        ("/Users/me/./project", "/Users/me/project"),
        ("/Users//me", "/Users/me/project"),
        ("/Users/me/project/", "/Users/me/project"),
        ("/Users/me/project", "/Users/me/pro\0ject"),
    ] {
        let mut command = command();
        command.cwd = cwd.into();
        command.workspace = workspace.into();
        let mut protocol = advanced(1);
        assert!(
            protocol
                .accept(Peer::Host, &Frame::TreeEnd { command })
                .is_err(),
            "accepted cwd {cwd:?} workspace {workspace:?}"
        );
    }
    let mut command = command();
    command.env.insert("A=B".into(), "x".into());
    let mut protocol = advanced(1);
    assert!(
        protocol
            .accept(Peer::Host, &Frame::TreeEnd { command })
            .is_err()
    );
}

#[test]
fn signal_frame_roundtrips_through_shared_framing() {
    let frame = Frame::Signal { signal: 11 };
    let mut bytes = Vec::new();
    write_frame(&mut bytes, &frame).unwrap();
    assert_eq!(read_frame(&mut &bytes[..]).unwrap(), Some(frame));
}

/// `frame` must poison the macOS conversation from either sender, at every
/// point of the sequence including after it has completed.
fn assert_rejected_everywhere(frame: Frame) {
    for step in 0..=TRANSCRIPT_LEN {
        for sender in [Peer::Host, Peer::Agent] {
            let mut protocol = advanced(step);
            let before = protocol.state();
            assert!(
                protocol.accept(sender, &frame).is_err(),
                "macOS profile accepted {frame:?} from {sender:?} in {before:?}"
            );
            assert_eq!(protocol.state(), State::Failed);
        }
    }
}

#[test]
fn macos_rejects_cache_mounted() {
    assert_rejected_everywhere(Frame::CacheMounted {
        timings: CacheTimings::default(),
    });
}

#[test]
fn macos_rejects_cache_progress() {
    assert_rejected_everywhere(Frame::CacheProgress {
        step: cache_steps::CacheStep::DiskLayout,
        elapsed_ms: 1,
    });
}

#[test]
fn macos_rejects_cache_failed() {
    assert_rejected_everywhere(Frame::CacheFailed {
        failure: cache_steps::CacheFailure {
            step: cache_steps::CacheStep::DiskLayout,
            elapsed_ms: 3_000,
            message: "DiskLayout: device not ready (os error 21)".into(),
            completed: vec![],
        },
    });
}

#[test]
fn macos_rejects_receive_tree() {
    assert_rejected_everywhere(Frame::ReceiveTree {
        bytes: vec![0, 255],
    });
}

#[test]
fn macos_rejects_collect() {
    assert_rejected_everywhere(Frame::Collect {});
}

#[test]
fn macos_rejects_collected() {
    assert_rejected_everywhere(Frame::Collected { bytes: vec![1, 2] });
}

#[test]
fn macos_rejects_collect_end() {
    assert_rejected_everywhere(Frame::CollectEnd {
        deleted: vec!["m/Users/me/project/gone.rs".into()],
    });
}

#[test]
fn macos_rejects_dismount() {
    assert_rejected_everywhere(Frame::Dismount {});
}

#[test]
fn macos_rejects_dismounted() {
    for clean in [true, false] {
        assert_rejected_everywhere(Frame::Dismounted { clean });
    }
}

#[test]
fn windows_rejects_signal_in_every_phase() {
    let windows = [
        (
            Peer::Agent,
            Frame::Hello {
                version: PROTOCOL_VERSION,
                nonce: NONCE.into(),
            },
        ),
        (Peer::Host, Frame::ReceiveTree { bytes: vec![1] }),
        (Peer::Host, Frame::TreeEnd { command: command() }),
        (Peer::Agent, Frame::TreeReceived {}),
        (Peer::Host, Frame::Start {}),
        (Peer::Agent, Frame::Started {}),
        (Peer::Agent, output(Stream::Stdout)),
        (Peer::Agent, Frame::Exit { code: 0 }),
        (Peer::Host, Frame::Collect {}),
        (Peer::Agent, Frame::CollectEnd { deleted: vec![] }),
        (Peer::Host, Frame::Dismount {}),
        (Peer::Agent, Frame::Dismounted { clean: true }),
    ];
    for step in 0..=windows.len() {
        for sender in [Peer::Host, Peer::Agent] {
            let mut protocol = Protocol::new(NONCE).unwrap();
            for (peer, frame) in &windows[..step] {
                protocol.accept(*peer, frame).unwrap();
            }
            let before = protocol.state();
            assert!(
                protocol
                    .accept(sender, &Frame::Signal { signal: 11 })
                    .is_err(),
                "Windows profile accepted Signal from {sender:?} in {before:?}"
            );
            assert_eq!(protocol.state(), State::Failed);
        }
    }
}

#[test]
fn windows_session_rejects_a_received_signal() {
    let mut host = Session::new(Peer::Host, NONCE).unwrap();
    let mut agent = Session::new(Peer::Agent, NONCE).unwrap();
    let windows = [
        (
            Peer::Agent,
            Frame::Hello {
                version: PROTOCOL_VERSION,
                nonce: NONCE.into(),
            },
        ),
        (
            Peer::Host,
            Frame::TreeEnd {
                command: Command {
                    cwd: "/work/project".into(),
                    workspace: "/work/project".into(),
                    ..command()
                },
            },
        ),
        (Peer::Agent, Frame::TreeReceived {}),
        (Peer::Host, Frame::Start {}),
        (Peer::Agent, Frame::Started {}),
    ];
    converse(&mut host, &mut agent, windows.to_vec());
    let mut bytes = Vec::new();
    write_frame(&mut bytes, &Frame::Signal { signal: 11 }).unwrap();
    assert!(host.receive(&mut &bytes[..]).is_err());
    assert_eq!(host.state(), State::Failed);
    assert!(
        agent
            .send(&mut Vec::new(), &Frame::Signal { signal: 11 })
            .is_err()
    );
}
