use super::scratch;
use motor_host::helper::{
    control_read_deadline, exchange_control, read_json, write_json, Control, Helper,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};

#[test]
fn fresh_attach_read_deadline_outlasts_guest_runner_readiness_wait() {
    let attach = control_read_deadline(&json!({"op":"attach_tenant"}));
    assert_eq!(attach, motor_protocol::TENANT_ATTACH_READ_DEADLINE);
    assert!(
        attach > motor_protocol::TENANT_RUNNER_READINESS_WAIT,
        "host attach read deadline {attach:?} must exceed the guest readiness wait"
    );
    assert!(
        control_read_deadline(&json!({"op":"attach_tenant","recovery_required":true})) > attach
    );
    assert_eq!(
        control_read_deadline(&json!({"op":"status"})),
        std::time::Duration::from_secs(30)
    );
}

#[test]
fn delayed_guest_control_reply_succeeds_without_retry() {
    let (host, mut guest) = UnixStream::pair().unwrap();
    let peer = std::thread::spawn(move || {
        assert_eq!(read_json(&mut guest).unwrap()["op"], "attach_tenant");
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_json(&mut guest, &json!({"ok":true})).unwrap();
    });
    assert_eq!(
        exchange_control(
            host,
            "tenant",
            &json!({"op":"attach_tenant"}),
            std::time::Duration::from_secs(1)
        )
        .unwrap(),
        json!({"ok":true})
    );
    peer.join().unwrap();
}

#[test]
fn guest_control_read_deadline_names_initialization_and_header_operation() {
    let (host, mut guest) = UnixStream::pair().unwrap();
    let peer = std::thread::spawn(move || {
        read_json(&mut guest).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(150));
    });
    let error = exchange_control(
        host,
        "tenant",
        &json!({"op":"attach_tenant"}),
        std::time::Duration::from_millis(30),
    )
    .unwrap_err();
    let detail = format!("{error:#}");
    assert!(
        detail.contains("tenant initialization timed out waiting for runner readiness"),
        "{detail}"
    );
    assert!(detail.contains("operation=attach_tenant"), "{detail}");
    assert!(detail.contains("read guest JSON frame header"), "{detail}");
    peer.join().unwrap();
}

#[test]
fn unexpected_nonblocking_guest_control_is_not_mislabeled_timeout() {
    let (host, _guest) = UnixStream::pair().unwrap();
    host.set_nonblocking(true).unwrap();
    let error = exchange_control(
        host,
        "tenant",
        &json!({"op":"attach_tenant"}),
        std::time::Duration::from_secs(1),
    )
    .unwrap_err();
    let detail = format!("{error:#}");
    assert!(detail.contains("nonblocking=true"), "{detail}");
    assert!(!detail.contains("timed out waiting"), "{detail}");
}

fn descriptor_reply(stream: &UnixStream, fd: &UnixStream) {
    let bytes = b"{\"result\":\"relay\",\"fd_count\":1}\n";
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    let mut control = [0usize; 8];
    // SAFETY: the message and ancillary buffer live through sendmsg; the peer
    // receives a duplicate descriptor, so both local streams retain ownership.
    unsafe {
        let mut message: libc::msghdr = std::mem::zeroed();
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as _) as _;
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as _) as _;
        std::ptr::write(
            libc::CMSG_DATA(header).cast::<libc::c_int>(),
            fd.as_raw_fd(),
        );
        assert_eq!(
            libc::sendmsg(stream.as_raw_fd(), &message, 0),
            bytes.len() as isize
        );
    }
}
#[test]
fn helper_initialization_holds_one_vcpu_until_attach_or_post_restore_reply() {
    for restored in [false, true] {
        let directory = scratch();
        let path = directory.path().join("helper.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let worker = std::thread::spawn(move || {
            for (op, reply) in [
                (
                    "adopt",
                    json!({"result":"tenant", "running":false, "restored":false, "generation":1}),
                ),
                (
                    "ensure",
                    json!({"result":"tenant", "running":true, "restored":restored, "generation":2}),
                ),
                (
                    "set_cpu_max",
                    json!({"result":"tenant", "running":true, "restored":restored, "generation":2}),
                ),
                ("open_relay", json!(null)),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["op"], op);
                assert_eq!(request["tenant"], "tenant");
                if op == "set_cpu_max" {
                    assert_eq!(request["vcpus"], 1);
                }
                if op == "open_relay" {
                    assert_eq!(request["port"], 54);
                    let (fd, mut guest) = UnixStream::pair().unwrap();
                    descriptor_reply(&stream, &fd);
                    let init = read_json(&mut guest).unwrap();
                    assert_eq!(
                        init["op"],
                        if restored {
                            "post_restore"
                        } else {
                            "attach_tenant"
                        }
                    );
                    assert_eq!(init["seed"].as_str().unwrap().len(), 43);
                    assert!(init["unix_ms"].as_u64().unwrap() > 0);
                    if !restored {
                        assert_eq!(init["generation"], 2);
                    }
                    write_json(&mut guest, &json!({"ok":true})).unwrap();
                } else {
                    stream.write_all(format!("{reply}\n").as_bytes()).unwrap();
                }
            }
        });
        let helper = Helper {
            socket: path,
            storage: directory.path().to_path_buf(),
            guest_init: BTreeMap::from([(
                "tenant".into(),
                json!({"address":"10.200.1.2/30","gateway":"10.200.1.1","resolvers":["1.1.1.1"],"hostname":"motor-1","machine_id":"12345678901234567890123456789012"}),
            )]),
        };
        assert_eq!(helper.wake("tenant", None).unwrap(), 2);
        worker.join().unwrap();
    }
}

#[test]
fn recovery_guest_pass_clears_flag_but_failure_or_unreachable_guest_keeps_tenant_fenced() {
    for result in ["pass", "fail", "unreachable"] {
        let directory = scratch();
        let socket = directory.path().join("helper.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let worker = std::thread::spawn(move || {
            for op in ["adopt", "ensure", "set_cpu_max", "open_relay"] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["op"], op);
                assert_eq!(request["tenant"], "tenant");
                if op == "open_relay" {
                    let (fd, mut guest) = UnixStream::pair().unwrap();
                    descriptor_reply(&stream, &fd);
                    let init = read_json(&mut guest).unwrap();
                    assert_eq!(init["recovery_required"], true);
                    assert_eq!(init["op"], "attach_tenant");
                    if result == "pass" {
                        write_json(&mut guest, &json!({"ok":true})).unwrap();
                    }
                    if result == "fail" {
                        write_json(&mut guest,&json!({"ok":false,"error":"tenant_recovery_failed","detail":"xfs_check failed"})).unwrap();
                    }
                } else {
                    writeln!(stream,"{}",json!({"result":"tenant","running":op!="adopt","generation":2,"recovery_required":true})).unwrap();
                }
            }
            if result == "pass" {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                assert_eq!(
                    serde_json::from_str::<Value>(&line).unwrap()["op"],
                    "recovery_complete"
                );
                writeln!(
                    stream,
                    "{}",
                    json!({"result":"tenant","running":true,"generation":2})
                )
                .unwrap();
            }
        });
        let helper = Helper {
            socket,
            storage: directory.path().into(),
            guest_init: BTreeMap::from([("tenant".into(), json!({}))]),
        };
        let response = helper.wake("tenant", None);
        assert_eq!(response.is_ok(), result == "pass");
        if result != "pass" {
            assert!(matches!(
                response
                    .unwrap_err()
                    .downcast_ref::<motor_host::state::Error>(),
                Some(motor_host::state::Error::Refused(
                    motor_host::state::Refusal::InternalFailure
                ))
            ));
        }
        worker.join().unwrap();
    }
}
