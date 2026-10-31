use motor_winagent::{
    returned::{self, Failure, Outcome, Request},
    Frame, Peer, Protocol,
};
use std::{fs, io, path::PathBuf};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = cortexkit_test_support::scratch_root().join(format!(
            "windows-returns-{}",
            motor_protocol::Uuid::now_v7()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn request(paths: &[&str], max_bytes: u64) -> Request {
    Request {
        paths: paths.iter().map(|p| (*p).to_owned()).collect(),
        max_bytes,
    }
}

#[test]
fn returned_paths_refuse_traversal_and_absolute_names() {
    for path in [
        "../secret",
        "a/../../secret",
        "/secret",
        "//server/share",
        "C:/secret",
        "a\\secret",
        "a//secret",
        "./secret",
    ] {
        assert_eq!(
            returned::validate_relative(path),
            Err(Failure::Escape),
            "{path}"
        );
    }
    assert!(returned::validate_relative("out/result.bin").is_ok());
}

#[test]
fn returned_paths_refuse_alternate_data_streams() {
    for path in ["file:secret", "dir/file::$DATA", "C:secret"] {
        assert_eq!(
            returned::validate_relative(path),
            Err(Failure::Escape),
            "{path}"
        );
    }
}

#[test]
fn returned_paths_refuse_windows_device_names() {
    for path in [
        "CON",
        "out/nul.txt",
        "aux.log",
        "COM1",
        "LPT9.dat",
        "conout$",
        "com¹.txt",
    ] {
        assert_eq!(
            returned::validate_relative(path),
            Err(Failure::Escape),
            "{path}"
        );
    }
}

fn outcomes(
    scratch: &Scratch,
    paths: &[&str],
    cap: u64,
    before: impl FnMut(usize),
) -> (Vec<Outcome>, u64) {
    #[cfg(windows)]
    let _identity = impersonate_self();
    let mut results = Vec::new();
    let mut sent = 0;
    returned::collect_before_read(
        &scratch.0,
        "",
        &request(paths, cap),
        &mut |frame| {
            match frame {
                Frame::ReturnChunk { bytes } => {
                    assert!(bytes.len() <= returned::CHUNK_BYTES);
                    sent += bytes.len() as u64;
                }
                Frame::ReturnFileEnd { outcome } => results.push(outcome),
                Frame::ReturnEnd {} => (),
                _ => panic!("unexpected collection frame"),
            }
            Ok(())
        },
        before,
    )
    .unwrap();
    (results, sent)
}

#[cfg(unix)]
#[test]
fn returned_files_count_reads_and_refuse_per_file_over_cap_missing_and_changed() {
    let scratch = Scratch::new();
    fs::write(scratch.0.join("large"), [1; 9]).unwrap();
    fs::write(scratch.0.join("small"), [2; 2]).unwrap();
    fs::write(scratch.0.join("total"), [3; 3]).unwrap();
    fs::write(scratch.0.join("changed"), [4; 1]).unwrap();
    fs::write(scratch.0.join("vanished"), [5; 1]).unwrap();
    let (results, _) = outcomes(
        &scratch,
        &["large", "small", "total", "missing", "changed", "vanished"],
        4,
        |index| {
            if index == 4 {
                fs::write(scratch.0.join("changed"), [9; 2]).unwrap();
            }
            if index == 5 {
                fs::remove_file(scratch.0.join("vanished")).unwrap();
            }
        },
    );
    assert!(matches!(
        results[0],
        Outcome::Failed {
            cause: Failure::OverCap,
            size: Some(5)
        }
    ));
    assert!(matches!(results[1], Outcome::Returned { size: 2, .. }));
    assert!(matches!(
        results[2],
        Outcome::Failed {
            cause: Failure::OverCap,
            ..
        }
    ));
    assert!(matches!(
        results[3],
        Outcome::Failed {
            cause: Failure::Missing,
            ..
        }
    ));
    assert!(matches!(
        results[4],
        Outcome::Failed {
            cause: Failure::ReadError,
            ..
        }
    ));
    assert!(matches!(
        results[5],
        Outcome::Failed {
            cause: Failure::ReadError,
            ..
        }
    ));
}

#[cfg(unix)]
#[test]
fn returned_file_growth_past_cap_is_a_changed_file_read_error() {
    let scratch = Scratch::new();
    fs::write(scratch.0.join("growing"), [1]).unwrap();
    let (results, sent) = outcomes(&scratch, &["growing"], 4, |_| {
        fs::write(scratch.0.join("growing"), [2; 8]).unwrap();
    });
    assert_eq!(sent, 4);
    assert!(matches!(
        results[0],
        Outcome::Failed {
            cause: Failure::ReadError,
            ..
        }
    ));
}

#[cfg(unix)]
#[test]
fn returned_paths_refuse_reparse_files_and_ancestors_without_reading_targets() {
    let scratch = Scratch::new();
    let outside = Scratch::new();
    fs::write(outside.0.join("secret"), "never return").unwrap();
    std::os::unix::fs::symlink(outside.0.join("secret"), scratch.0.join("link")).unwrap();
    std::os::unix::fs::symlink(&outside.0, scratch.0.join("junction")).unwrap();
    let (results, sent) = outcomes(&scratch, &["link", "junction/secret"], 128, |_| {});
    assert_eq!(sent, 0);
    for result in results {
        assert!(matches!(
            result,
            Outcome::Failed {
                cause: Failure::Escape,
                ..
            }
        ));
    }
    let mut sent = 0;
    returned::collect(
        &scratch.0,
        "junction",
        &request(&["secret"], 128),
        |frame| {
            if let Frame::ReturnChunk { bytes } = frame {
                sent += bytes.len();
            }
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(sent, 0, "workspace ancestors must also be pinned");
}

#[cfg(windows)]
#[test]
fn returned_paths_refuse_native_windows_junction() {
    let scratch = Scratch::new();
    let outside = Scratch::new();
    fs::write(outside.0.join("secret"), "never return").unwrap();
    let status = std::process::Command::new("cmd.exe")
        .args(["/c", "mklink", "/J"])
        .arg(scratch.0.join("junction"))
        .arg(&outside.0)
        .status()
        .unwrap();
    assert!(status.success());
    let (results, sent) = outcomes(&scratch, &["junction/secret"], 128, |_| {});
    assert_eq!(sent, 0);
    assert!(matches!(
        results[0],
        Outcome::Failed {
            cause: Failure::Escape,
            ..
        }
    ));
    fs::remove_dir(scratch.0.join("junction")).unwrap();
}

#[test]
fn returned_stream_propagates_transport_failure_without_reading_more_files() {
    #[cfg(windows)]
    let _identity = impersonate_self();
    let scratch = Scratch::new();
    fs::write(scratch.0.join("file"), [1; 16]).unwrap();
    let mut calls = 0;
    let error = returned::collect(&scratch.0, "", &request(&["file", "file"], 128), |_| {
        calls += 1;
        Err(io::Error::other("transport gone"))
    })
    .unwrap_err();
    assert_eq!(error.to_string(), "transport gone");
    assert_eq!(calls, 1);
}

#[cfg(windows)]
pub(crate) struct Identity;
#[cfg(windows)]
pub(crate) fn impersonate_self() -> Identity {
    assert_ne!(
        unsafe {
            windows_sys::Win32::Security::ImpersonateSelf(
                windows_sys::Win32::Security::SecurityImpersonation,
            )
        },
        0
    );
    Identity
}
#[cfg(windows)]
impl Drop for Identity {
    fn drop(&mut self) {
        assert_ne!(unsafe { windows_sys::Win32::Security::RevertToSelf() }, 0);
    }
}

#[cfg(windows)]
#[test]
fn returned_collection_refuses_service_identity_without_impersonation() {
    let scratch = Scratch::new();
    fs::write(scratch.0.join("secret"), "not readable as SYSTEM").unwrap();
    let mut sent = false;
    let error = returned::collect(&scratch.0, "", &request(&["secret"], 128), |_| {
        sent = true;
        Ok(())
    })
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("requires job-user impersonation"));
    assert!(!sent);
}

#[test]
fn returned_protocol_refuses_version_five_before_start_and_names_version() {
    let mut protocol = Protocol::new("nonce").unwrap();
    let error = protocol
        .accept(
            Peer::Agent,
            &Frame::Hello {
                version: 5,
                nonce: "nonce".into(),
            },
        )
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("unsupported agent protocol version 5"));
    assert!(protocol.accept(Peer::Host, &Frame::Start {}).is_err());
}
