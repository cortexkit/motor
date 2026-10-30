//! The systemd-run argv the runner builds for job units, pinned byte for byte.
//!
//! Boat sandboxes on systemd < 257 build a different unit (see `pid_init`).
//! The argv of every unit variant is pinned: this test renders each kind with
//! fixed inputs and compares the SHA-256 of
//! the rendered text to a pinned value. A failure prints the full rendered
//! argv; an intended change to one of these units updates the pin in the same
//! commit and says why.
use super::*;

fn render(name: &str, sandbox: &Sandbox, f: &Fixture, durable: bool) -> String {
    let env = [
        ("RUST_LOG".to_string(), "info".to_string()),
        ("SCCACHE_DIR".to_string(), "/motor-cache".to_string()),
    ];
    let command = sandbox
        .command("cargo test", &f.workspace, &env, durable, &[1, 2], 2)
        .unwrap();
    let scratch = f.scratch.to_str().unwrap();
    let id = sandbox.id.to_string();
    let mut text = format!("== {name}\n{}\n", command.get_program().to_str().unwrap());
    for arg in command.get_args() {
        let arg = arg
            .to_str()
            .unwrap()
            .replace(scratch, "<SCRATCH>")
            .replace(&id, "<ID>");
        text.push_str(&arg);
        text.push('\n');
    }
    text
}

/// SHA-256 of the rendered argv of every case below.
// Jobs and repository-building writer fills need a 65536 open-file soft limit.
// Every case adds LimitNOFILE=65536:524288, preserving systemd's hard ceiling;
// the remaining argv is unchanged.
const PINNED_SHA256: &str = "419cbab8b13f1cb32378ff65947f208f0026909abb5474b80b505e0b77e5fc97";

#[test]
fn job_unit_commands_match_the_pinned_argv() {
    let f = Fixture::new();
    let bound = |mut sandbox: Sandbox| {
        sandbox.read_only = vec!["/usr".into(), "/etc/passwd".into()];
        sandbox.read_write = vec![f.workspace.to_str().unwrap().into()];
        sandbox
    };
    let mut writer = bound(f.command_sandbox());
    writer.writer = true;
    writer.user = WRITER_USER;
    let cases = [
        ("build-server", bound(f.command_sandbox()), true),
        ("build-server-pipe", bound(f.command_sandbox()), false),
        (
            "build-server-retained",
            bound(f.command_sandbox()).retaining_failed_units(),
            true,
        ),
        ("writer", writer, true),
        (
            "tenant",
            bound(f.command_sandbox()).in_tenant_vm(true),
            true,
        ),
        (
            "boat-private-pids",
            bound(f.command_sandbox())
                .with_boat_profile(true, true, Some(14 << 30))
                .without_provider_environment(&["BOAT_FUTURE".to_owned()]),
            true,
        ),
        (
            "boat-tenant-private-pids",
            bound(f.command_sandbox())
                .in_tenant_vm(true)
                .with_boat_profile(true, true, Some(14 << 30)),
            true,
        ),
    ];
    let rendered: String = cases
        .iter()
        .map(|(name, sandbox, durable)| render(name, sandbox, &f, *durable))
        .collect();
    let digest = format!("{:x}", sha2::Sha256::digest(rendered.as_bytes()));
    assert!(
        digest == PINNED_SHA256,
        "job unit argv changed (sha256 {digest}); if intended, update PINNED_SHA256. Rendered argv:\n{rendered}"
    );
}
