//! Real Mac/Linux acceptance. Invoke directly, never through remote bash routing:
//! cargo nextest run -p motor-module --test motor_e2e --run-ignored all --test-threads 1 --no-capture
//!
//! The second invocation runs a test consumer registered as the scratch daemon's
//! reserved `aft` module. The daemon attests its identity, so it can exercise
//! ck-motor's admission rules without borrowing the operator's daemon credentials.
//! Operator inputs (no defaults): CK_MOTOR_E2E_SSH_TARGET, CK_MOTOR_E2E_RUNNER_BINARY,
//! CK_MOTOR_E2E_STATE_ROOT, CK_MOTOR_E2E_SYSTEMD_UNIT and CK_MOTOR_E2E_SYSTEMD_SCOPE
//! (user or system). Paths are absolute on the SSH target; STATE_ROOT is the
//! instance directory containing mirrors and generations.
#![cfg(target_os = "macos")]

use fs2::FileExt;
use motor_protocol::*;
use rand::RngCore;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    process::{Command, Output as ProcessOutput, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};
use subc_protocol::{Flags, Frame as BusFrame, FrameType, Priority};
use subc_transport::{authenticate_client, connection_file, read_frame, write_frame};
use tokio::{net::TcpStream, time::timeout};

const BOUND: Duration = Duration::from_secs(30 * 60);
const CORE_GATE: &str = "cargo nextest run -p prefrontal-core-module --lib --bins";
const STORE_GATE: &str = "cargo nextest run -p prefrontal-core-store";
const STATUSES: &str = " --status-level all --final-status-level none --no-fail-fast --color never";
const BROKEN: &str = "motor_e2e_probe::deliberately_broken";
const CHILD: &str = "CK_MOTOR_E2E_CONSUMER";

fn operator_input(name: &str) -> String {
    let value =
        std::env::var(name).unwrap_or_else(|_| panic!("set {name} for the operator harness"));
    assert!(
        !value.is_empty() && !value.contains(['\n', '\r']),
        "invalid {name}"
    );
    value
}

fn ssh_target() -> String {
    let target = operator_input("CK_MOTOR_E2E_SSH_TARGET");
    assert!(
        !target.starts_with('-') && !target.chars().any(char::is_whitespace),
        "invalid SSH target"
    );
    target
}

fn remote_path(name: &str) -> PathBuf {
    let path = PathBuf::from(operator_input(name));
    assert!(path.is_absolute(), "{name} must be an absolute remote path");
    path
}

fn checked(mut command: Command) -> ProcessOutput {
    command
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_KEY_0")
        .env_remove("GIT_CONFIG_VALUE_0")
        .env_remove("SUBC_LAUNCH_NONCE")
        .env_remove("SUBC_LAUNCH_NONCE_FD");
    let output = command.output().expect("spawn harness helper");
    assert!(
        output.status.success(),
        "helper {command:?}: {}",
        text(&output)
    );
    output
}
fn text(output: &ProcessOutput) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}
fn quote(path: impl AsRef<str>) -> String {
    format!("'{}'", path.as_ref().replace('\'', "'\\''"))
}
fn git(path: &Path, args: &[&str]) -> String {
    let mut command = Command::new("git");
    command
        .args(["--no-optional-locks", "-C"])
        .arg(path)
        .args(args);
    String::from_utf8(checked(command).stdout)
        .unwrap()
        .trim_end()
        .to_owned()
}
fn ssh(line: &str) -> ProcessOutput {
    let mut command = Command::new("ssh");
    command.args([
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "ForwardAgent=no",
        &ssh_target(),
        line,
    ]);
    checked(command)
}
fn shell(cwd: &Path, line: &str) -> ProcessOutput {
    let mut command = Command::new("bash");
    command
        .args(["-lc", line])
        .current_dir(cwd)
        // Only the explicit local baseline needs the operator's toolchain home.
        // The daemon and ck-motor still retain every scratch ambient-root fence.
        .env(
            "HOME",
            std::env::var_os("CK_MOTOR_E2E_LOCAL_HOME").expect("local baseline home"),
        )
        // Unit tests share stable executables under the test-support namespace,
        // outside the daemon's ephemeral ambient-root fence. Both local baseline
        // commands reuse that namespace for the life of this harness process.
        .env(
            "TMPDIR",
            std::env::var_os("CK_MOTOR_E2E_LOCAL_TMPDIR").expect("local baseline TMPDIR"),
        )
        .env("CARGO_TARGET_DIR", cwd.join("target"))
        .env("NO_COLOR", "1");
    // A failed test is an observation, not a helper failure.
    command
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_KEY_0")
        .env_remove("GIT_CONFIG_VALUE_0")
        .env_remove("SUBC_LAUNCH_NONCE")
        .env_remove("SUBC_LAUNCH_NONCE_FD")
        .env_remove("SUBC_MODULE_ID")
        .output()
        .unwrap()
}
fn write_json(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}
fn append(path: &Path, value: &str) {
    use std::io::Write;
    OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(value.as_bytes())
        .unwrap();
}
fn broken_test(path: &Path) {
    append(path, "\n#[cfg(test)] mod motor_e2e_probe { #[test] fn deliberately_broken() { assert_eq!(1, 2, \"motor e2e deliberate assertion\"); } }\n");
}

#[derive(Default)]
struct Ownership {
    workspaces: BTreeSet<PathBuf>,
    jobs: BTreeSet<Uuid>,
}
#[derive(Clone)]
struct Bus {
    connection: PathBuf,
    root: PathBuf,
    nonce: String,
    owned: Arc<Mutex<Ownership>>,
}
struct Call {
    socket: TcpStream,
    channel: u16,
    epoch: u32,
    records: Vec<StreamRecord>,
    raw_terminals: Vec<Vec<u8>>,
}
impl Bus {
    fn remember(&self) {
        let owned = self.owned.lock().unwrap();
        write_json(
            &self.root.parent().unwrap().join("ownership.json"),
            &json!({"jobs":owned.jobs,"workspaces":owned.workspaces}),
        );
    }
    async fn open(&self, op: &str, params: Value) -> Call {
        let conn = connection_file::read(&self.connection).unwrap();
        let endpoint = conn.endpoints.first().unwrap();
        let mut socket = TcpStream::connect((endpoint.host.as_str(), endpoint.port))
            .await
            .unwrap();
        authenticate_client(&mut socket, &conn, Duration::from_secs(5))
            .await
            .unwrap();
        let request = json!({"op":"route.open", "target":{"kind":"management_surface", "module_id":"ck-motor"},
            "identity":{"project_root":self.root, "harness":"motor-e2e", "session":"motor-e2e"},
            "consumer_identity":{"module_id":"aft", "launch_nonce":self.nonce}});
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        let reply = loop {
            send(&mut socket, FrameType::Request, 0, 0, request.clone()).await;
            let reply = tokio::time::timeout_at(deadline, receive(&mut socket))
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "route.open exceeded 60 s: {}",
                        daemon_tail(self.root.parent().unwrap())
                    )
                });
            let body: Value = serde_json::from_slice(&reply.body).unwrap();
            if reply.header.ty == FrameType::Error
                && (matches!(
                    body["code"].as_str(),
                    Some("module_warming" | "module_timeout" | "module_reloading")
                ) || (body["code"] == "target_unavailable"
                    && body["message"].as_str().is_some_and(|message| {
                        message.contains("state=draining")
                            || message.contains("state=starting")
                            || message.contains("state=running")
                    })))
            {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "module not ready: {body}\n{}",
                    daemon_tail(self.root.parent().unwrap())
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            break reply;
        };
        assert_eq!(
            reply.header.ty,
            FrameType::Response,
            "route.open: {}",
            String::from_utf8_lossy(&reply.body)
        );
        let value: Value = serde_json::from_slice(&reply.body).unwrap();
        let channel = value["route_channel"].as_u64().unwrap() as u16;
        let epoch = value["route_epoch"].as_u64().unwrap() as u32;
        send(
            &mut socket,
            FrameType::Request,
            channel,
            epoch,
            json!({"op":op,"params":params}),
        )
        .await;
        Call {
            socket,
            channel,
            epoch,
            records: Vec::new(),
            raw_terminals: Vec::new(),
        }
    }
    async fn rpc(&self, op: &str, params: Value) -> Value {
        let mut call = self.open(op, params).await;
        loop {
            let reply = receive(&mut call.socket).await;
            if reply.header.channel != call.channel {
                continue;
            }
            assert_eq!(
                reply.header.ty,
                FrameType::Response,
                "{op}: {}",
                String::from_utf8_lossy(&reply.body)
            );
            return serde_json::from_slice(&reply.body).unwrap();
        }
    }
    async fn status(&self) -> Value {
        self.rpc("exec.status", json!({})).await
    }
    async fn run(&self, params: Value) -> Call {
        let mut call = self.open("exec.run", params).await;
        let first = call.next().await.expect("first record");
        let id = match first {
            StreamRecord::Accepted(ref a) => a.job_id,
            StreamRecord::Terminal(ref t) => t.job_id,
            _ => panic!("first record must identify the job"),
        };
        self.owned.lock().unwrap().jobs.insert(id);
        self.remember();
        call.records.push(first);
        call
    }
    async fn attach(&self, id: Uuid) -> Call {
        self.open("exec.attach", json!({"job_id":id,"from_seq":0}))
            .await
    }
    async fn cancel(&self, id: Uuid) {
        self.rpc("exec.cancel", json!({"job_id":id})).await;
    }
    async fn prepare(&self, key: &Path, siblings: &[PathBuf]) {
        self.owned.lock().unwrap().workspaces.insert(key.to_owned());
        self.remember();
        let reply = self
            .rpc(
                "workspace.prepare",
                json!({"workspace_key":key,"repository_root":self.root,"base_commit":git(key,&["rev-parse","HEAD"]),"siblings":siblings}),
            )
            .await;
        assert_eq!(reply["outcome"]["type"], "prepared", "prepare: {reply}");
    }
    async fn wait_status(&self, wait: Duration, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + wait;
        loop {
            let status = self.status().await;
            if predicate(&status) {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "exec.status condition timed out: {status}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
    async fn idle(&self) {
        self.wait_status(BOUND, |s| {
            s["queue_depth"] == 0
                && s["running_jobs"].as_array().is_some_and(Vec::is_empty)
                && s["repositories"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|r| r["last_rebuild_result"] != "building")
        })
        .await;
    }
}
async fn send(socket: &mut TcpStream, ty: FrameType, channel: u16, epoch: u32, value: Value) {
    let frame = BusFrame::build(
        ty,
        Flags::new(false, Priority::Interactive, false),
        channel,
        epoch,
        1,
        if ty == FrameType::Cancel {
            Vec::new()
        } else {
            serde_json::to_vec(&value).unwrap()
        },
    )
    .unwrap();
    write_frame(socket, &frame).await.unwrap();
}
async fn receive(socket: &mut TcpStream) -> BusFrame {
    timeout(BOUND, read_frame(socket))
        .await
        .expect("subc frame deadline")
        .unwrap()
        .expect("subc disconnected")
}
impl Call {
    async fn next(&mut self) -> Option<StreamRecord> {
        loop {
            let frame = receive(&mut self.socket).await;
            if frame.header.channel != self.channel {
                continue;
            }
            match frame.header.ty {
                FrameType::StreamData => {
                    let record: StreamRecord =
                        serde_json::from_slice(&frame.body).expect("exec-remote record");
                    if matches!(record, StreamRecord::Terminal(_)) {
                        self.raw_terminals.push(frame.body);
                    }
                    return Some(record);
                }
                FrameType::StreamEnd => return None,
                _ => panic!(
                    "unexpected exec frame {:?}: {}",
                    frame.header.ty,
                    String::from_utf8_lossy(&frame.body)
                ),
            }
        }
    }
    fn id(&self) -> Uuid {
        match self.records.first().unwrap() {
            StreamRecord::Accepted(a) => a.job_id,
            StreamRecord::Terminal(t) => t.job_id,
            _ => unreachable!(),
        }
    }
    async fn cancel(&mut self) {
        send(
            &mut self.socket,
            FrameType::Cancel,
            self.channel,
            self.epoch,
            json!({}),
        )
        .await;
    }
    async fn output_until(&mut self, needle: &str) {
        while !self.output().contains(needle) {
            let record = self.next().await.expect("output before end");
            assert!(
                !matches!(record, StreamRecord::Terminal(_)),
                "job ended before {needle}: {record:?}"
            );
            self.records.push(record);
        }
    }
    async fn finish(mut self) -> Self {
        while let Some(record) = self.next().await {
            self.records.push(record);
        }
        assert_eq!(
            self.records
                .iter()
                .filter(|r| matches!(r, StreamRecord::Terminal(_)))
                .count(),
            1,
            "exactly one terminal record"
        );
        assert!(matches!(
            self.records.last(),
            Some(StreamRecord::Terminal(_))
        ));
        self
    }
    async fn finish_caller_cancelled(mut self) -> Self {
        timeout(Duration::from_secs(5), async {
            loop {
                let frame = receive(&mut self.socket).await;
                if frame.header.channel != self.channel {
                    continue;
                }
                match frame.header.ty {
                    FrameType::StreamData => {
                        let record: StreamRecord = serde_json::from_slice(&frame.body).unwrap();
                        if matches!(record, StreamRecord::Terminal(_)) {
                            self.raw_terminals.push(frame.body);
                        }
                        self.records.push(record);
                    }
                    FrameType::StreamEnd => break,
                    FrameType::Error => {
                        let error: Value = serde_json::from_slice(&frame.body).unwrap();
                        assert_eq!(
                            error["code"], "cancelled",
                            "only capacity-queued cancellation may end with Error: {error}"
                        );
                        break;
                    }
                    other => panic!("unexpected withdrawn stream frame: {other:?}"),
                }
            }
        })
        .await
        .expect("caller-cancelled stream must promptly reach StreamEnd");
        assert!(self.raw_terminals.is_empty(),"caller Cancel withdraws the original stream; the retained terminal is observed by attach");
        self
    }
    fn terminal(&self) -> &TerminalRecord {
        match self.records.last().unwrap() {
            StreamRecord::Terminal(t) => t,
            _ => panic!("missing terminal"),
        }
    }
    fn output(&self) -> String {
        let bytes: Vec<u8> = self
            .records
            .iter()
            .filter_map(|r| match r {
                StreamRecord::Output(o) => Some(o.bytes.0.as_slice()),
                _ => None,
            })
            .flatten()
            .copied()
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
    fn success(&self) {
        assert_eq!(
            self.terminal().outcome,
            Outcome::Exit { code: 0 },
            "{}",
            self.output()
        );
        assert_eq!(self.terminal().ran, Some(Ran::Remote));
    }
    fn refusal(&self, reason: RefusalReason, accepted: bool) {
        assert_eq!(
            self.terminal().outcome,
            Outcome::RefusedBeforeStart { reason }
        );
        assert_eq!(self.terminal().ran, Some(Ran::None));
        assert_eq!(
            matches!(self.records.first(), Some(StreamRecord::Accepted(_))),
            accepted
        );
        assert_eq!(self.records.len(), if accepted { 2 } else { 1 });
    }
}

struct Rig {
    parent: PathBuf,
    root: PathBuf,
    worktrees: PathBuf,
    siblings: Vec<PathBuf>,
    bus: Bus,
    a: String,
    tip: String,
    report: Arc<AcceptanceReport>,
}

struct AcceptanceReport {
    directory: PathBuf,
    latest: PathBuf,
    previous_flakes: BTreeSet<String>,
    data: Mutex<Value>,
}
impl AcceptanceReport {
    fn new() -> Self {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("motor-e2e-evidence");
        fs::create_dir_all(&root).unwrap();
        let latest = root.join("latest-report.json");
        let previous: Value = fs::read(&latest)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or(json!({}));
        let previous_flakes = previous["flakes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|f| f["test"].as_str().map(str::to_owned))
            .collect();
        let directory = root.join(Uuid::now_v7().to_string());
        fs::create_dir(&directory).unwrap();
        let report = Self {
            directory,
            latest,
            previous_flakes,
            data: Mutex::new(json!({"status":"running","runs":[],"flakes":[]})),
        };
        report.save();
        report
    }
    fn save(&self) {
        let data = self
            .data
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        write_json(&self.directory.join("acceptance-report.json"), &data);
        write_json(&self.latest, &data);
    }
    fn raw(&self, side: &str, command: &str, output: &str, code: i32, wall_ms: u128) {
        let mut data = self.data.lock().unwrap();
        let ordinal = data["runs"].as_array().unwrap().len();
        let path = self.directory.join(format!("{ordinal:03}-{side}.log"));
        fs::write(&path, output).unwrap();
        data["runs"].as_array_mut().unwrap().push(json!({"side":side,"command":command,"exit_code":code,"wall_ms":wall_ms,"raw_output":path}));
        drop(data);
        self.save();
    }
    fn flake(&self, side: &str, full: &str, key: &str, rerun: &str) {
        let test = key.split_whitespace().last().unwrap();
        let mut data = self.data.lock().unwrap();
        data["flakes"].as_array_mut().unwrap().push(json!({"side":side,"test":test,"binary_and_test":key,
            "complete_run_result":result_line(full,key),"exact_rerun_result":result_line(rerun,key),
            "failure_message":failure_section(full,key),"consecutive_acceptance_runs":self.previous_flakes.contains(test)}));
        drop(data);
        self.save();
    }
    fn status(&self, status: &str) {
        self.data
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())["status"] = json!(status);
        self.save();
    }
}

fn result_line(output: &str, key: &str) -> String {
    output
        .lines()
        .find(|line| status_lines(line).iter().any(|(_, name)| name == key))
        .expect("per-test result line")
        .to_owned()
}
fn failure_section(output: &str, key: &str) -> String {
    let lines: Vec<_> = output.lines().collect();
    let start = lines
        .iter()
        .position(|line| {
            status_lines(line)
                .iter()
                .any(|(status, name)| status != "PASS" && status != "SKIP" && name == key)
        })
        .expect("full failure section");
    let end = lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find(|(_, line)| {
            !status_lines(line).is_empty() || line.trim_start().starts_with("Summary [")
        })
        .map(|(n, _)| n)
        .unwrap_or(lines.len());
    lines[start..end].join("\n")
}
fn without_flake_diagnostics(output: &str, flakes: &BTreeSet<String>) -> String {
    let mut skipping = false;
    let mut result = Vec::new();
    for line in output.lines() {
        let statuses = status_lines(line);
        if let Some((_, key)) = statuses.first() {
            skipping = flakes.contains(key);
        }
        if line.trim_start().starts_with("Summary [") {
            skipping = false;
        }
        if !skipping {
            result.push(line);
        }
    }
    result.join("\n")
}
fn rerun_is_pass(key: &str, code: i32, output: &str) -> Result<(), String> {
    let statuses: Vec<_> = status_lines(output)
        .into_iter()
        .filter(|(status, _)| status != "SKIP")
        .collect();
    // Exit zero alone is not evidence: nextest can select no tests. Pin the
    // actual binary and full test name, not merely a summary or exit status.
    if code != 0 || statuses != vec![("PASS".into(), key.into())] {
        return Err(format!(
            "exact rerun did not pass exactly one selected test: {key}, exit={code}\n{output}"
        ));
    }
    Ok(())
}
fn apply_exact_rerun(
    key: &str,
    code: i32,
    output: &str,
    statuses: &mut [(String, String)],
    flakes: &mut BTreeSet<String>,
) -> Result<(), String> {
    rerun_is_pass(key, code, output)?;
    for (status, name) in statuses {
        if name == key {
            *status = "PASS".into();
        }
    }
    flakes.insert(key.into());
    Ok(())
}
#[derive(Debug)]
struct NormalGate {
    statuses: Vec<(String, String)>,
    diagnostics: BTreeSet<String>,
}
impl Rig {
    async fn observed_gate(
        &self,
        side: &str,
        key: &Path,
        cwd: &Path,
        command: &str,
    ) -> (i32, String) {
        let start = Instant::now();
        let (code, output) = if side == "mac" {
            let local = shell(cwd, command);
            (
                local.status.code().expect("Mac gate exit code"),
                text(&local),
            )
        } else {
            let mut params = self.request(key, command);
            params["cwd"] = json!(cwd);
            let result = self.bus.run(params).await.finish().await;
            assert!(matches!(
                result.records.first(),
                Some(StreamRecord::Accepted(_))
            ));
            assert_eq!(result.terminal().ran, Some(Ran::Remote));
            let Outcome::Exit { code } = result.terminal().outcome else {
                panic!("remote gate did not exit: {:?}", result.terminal());
            };
            (code, result.output())
        };
        self.report
            .raw(side, command, &output, code, start.elapsed().as_millis());
        (code, output)
    }
    async fn normalized_gate(
        &self,
        side: &str,
        key: &Path,
        cwd: &Path,
        selection: &str,
        roots: &[PathBuf],
        verbatim: bool,
    ) -> NormalGate {
        let command = if verbatim {
            selection.to_owned()
        } else {
            format!("{selection}{STATUSES}")
        };
        let (code, full) = self.observed_gate(side, key, cwd, &command).await;
        let mut statuses = status_lines(&full);
        assert!(
            full.lines()
                .filter(|line| line.trim_start().starts_with("Summary ["))
                .any(|line| {
                    let words: Vec<_> = line.split_whitespace().collect();
                    words
                        .iter()
                        .position(|word| *word == "tests")
                        .is_some_and(|i| i > 0 && words[i - 1].parse::<u64>().is_ok_and(|n| n > 0))
                }),
            "{side} gate must actually run tests: {full}"
        );
        let failures: Vec<_> = statuses
            .iter()
            .filter(|(status, _)| matches!(status.as_str(), "FAIL" | "TIMEOUT" | "XPASS"))
            .map(|(_, key)| key.clone())
            .collect();
        assert!(
            code == 0 || (code == 100 && !failures.is_empty()),
            "{side} gate infrastructure failed, exit={code}: {full}"
        );
        let mut flakes = BTreeSet::new();
        let mut rerun_diagnostics = BTreeSet::new();
        let mut real_failures = Vec::new();
        for test_key in failures {
            let mut parts = test_key.split_whitespace();
            let display_binary = parts.next().unwrap();
            let test = parts.next().unwrap();
            let binary = display_binary
                .split_once("::")
                .map(|(_, name)| name.trim_start_matches("bin/").to_owned())
                .unwrap_or_else(|| display_binary.replace('-', "_"));
            let filter = quote(format!("test(={test}) and binary(={binary})"));
            let rerun_command = format!("{selection} -E {filter} -j 1{STATUSES}");
            let (rerun_code, rerun) = self.observed_gate(side, key, cwd, &rerun_command).await;
            match apply_exact_rerun(&test_key, rerun_code, &rerun, &mut statuses, &mut flakes) {
                Ok(()) => {
                    self.report.flake(side, &full, &test_key, &rerun);
                    rerun_diagnostics.extend(diagnostics(&rerun, roots));
                }
                Err(error) => real_failures.push(error),
            }
        }
        assert!(
            real_failures.is_empty(),
            "{side} real failures after one exact rerun:\n{}",
            real_failures.join("\n")
        );
        statuses.sort();
        let mut normalized_diagnostics =
            diagnostics(&without_flake_diagnostics(&full, &flakes), roots);
        normalized_diagnostics.extend(rerun_diagnostics);
        NormalGate {
            statuses,
            diagnostics: normalized_diagnostics,
        }
    }
    async fn baseline_pair(&self, key: &Path, cwd: &Path, selection: &str, roots: &[PathBuf]) {
        let mut linux = self
            .normalized_gate("linux", key, cwd, selection, roots, false)
            .await;
        eprintln!("motor-e2e step 1: remote gate normalized; running Mac baseline");
        let mut mac = self
            .normalized_gate("mac", key, cwd, selection, roots, false)
            .await;
        platform_exclusions(&mut mac.statuses, &mut linux.statuses);
        assert_eq!(
            mac.statuses, linux.statuses,
            "normalized per-test status multiset after symmetric exact reruns"
        );
        assert_eq!(
            mac.diagnostics, linux.diagnostics,
            "workspace and sibling diagnostics after exact reruns; complete raw output retained"
        );
    }
    fn workspace(&self, name: &str, commit: &str) -> PathBuf {
        let path = self.worktrees.join(name);
        self.bus
            .owned
            .lock()
            .unwrap()
            .workspaces
            .insert(path.clone());
        self.bus.remember();
        git(
            &self.root,
            &[
                "worktree",
                "add",
                "-b",
                &format!("e2e-{name}"),
                path.to_str().unwrap(),
                commit,
            ],
        );
        path
    }
    fn request(&self, key: &Path, command: &str) -> Value {
        json!({"workspace_key":key,"repository_root":self.root,"cwd":key,"command":command,"env":{"NO_COLOR":"1"},"siblings":self.siblings,"queue_wait_limit_s":900})
    }
    async fn run(&self, key: &Path, command: &str) -> Call {
        self.bus
            .run(self.request(key, command))
            .await
            .finish()
            .await
    }
    async fn start(&self, key: &Path, command: &str, weight: u32) -> Call {
        let mut params = self.request(key, command);
        params["weight_hint"] = json!(weight);
        self.bus.run(params).await
    }
    async fn published(&self, commit: &str, result: &str) -> Value {
        self.bus
            .wait_status(BOUND, |s| {
                s["repositories"].as_array().unwrap().iter().any(|r| {
                    r["repository_root"] == self.root.to_str().unwrap()
                        && r["published_commit"] == commit
                        && r["last_rebuild_result"] == result
                })
            })
            .await
    }
    async fn running(&self, id: Uuid) {
        self.bus
            .wait_status(Duration::from_secs(60), |s| {
                s["running_jobs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|j| j["job_id"] == id.to_string())
            })
            .await;
    }
    async fn gone(&self, id: Uuid) {
        self.bus
            .wait_status(Duration::from_secs(5), |s| {
                !s["running_jobs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|j| j["job_id"] == id.to_string())
            })
            .await;
    }
    fn control(&self, name: Option<&str>) {
        let mut config = json!({"runner_instance":"e2e","worktree_roots":[self.worktrees],"ssh_config_override":self.parent.join("ssh-live")});
        match name {
            Some("unreachable") => {
                config["ssh_config_override"] = json!(self.parent.join("ssh-dead"))
            }
            Some(field) => config[field] = json!(true),
            None => {}
        }
        write_json(&self.parent.join("motor.json"), &config);
        let mut command = Command::new("pgrep");
        command.args(["-f", "--"]).arg(format!(
            "--config {}",
            self.parent.join("motor.json").display()
        ));
        let output = checked(command);
        for pid in String::from_utf8(output.stdout).unwrap().split_whitespace() {
            let pid: i32 = pid.parse().unwrap();
            assert_eq!(
                unsafe { libc::kill(pid, libc::SIGKILL) },
                0,
                "kill harness-owned ck-motor"
            );
        }
    }
    async fn daemon_rpc(&self, request: Value) -> Value {
        let connection = connection_file::read(&self.bus.connection).unwrap();
        let endpoint = connection.endpoints.first().unwrap();
        let mut socket = TcpStream::connect((endpoint.host.as_str(), endpoint.port))
            .await
            .unwrap();
        authenticate_client(&mut socket, &connection, Duration::from_secs(5))
            .await
            .unwrap();
        send(&mut socket, FrameType::Request, 0, 0, request).await;
        let reply = timeout(Duration::from_secs(60), receive(&mut socket))
            .await
            .expect("scratch daemon RPC deadline");
        assert_eq!(
            reply.header.ty,
            FrameType::Response,
            "{}",
            String::from_utf8_lossy(&reply.body)
        );
        serde_json::from_slice(&reply.body).unwrap()
    }
    async fn healthy_after_caller_detach(&self, log_offset: usize, id: Uuid) {
        let health = self
            .daemon_rpc(json!({"op":"supervisor.health_probe","module_id":"ck-motor"}))
            .await;
        assert_eq!(
            health["status"], "ok",
            "withdrawn reader must not degrade module health: {health}"
        );
        let log = fs::read(
            self.parent
                .join("daemon/data/cortexkit/run/logs/ck-motor.stderr.log"),
        )
        .expect("supervised ck-motor stderr capture");
        assert!(
            log.len() >= log_offset,
            "module capture must not rotate during this case"
        );
        let delta = String::from_utf8_lossy(&log[log_offset..]);
        let marker = format!("caller_detached job_id={id}");
        assert_eq!(
            delta.lines().filter(|line| line.contains(&marker)).count(),
            1,
            "one detach, no retry storm: {delta}"
        );
        let lower = delta.to_lowercase();
        for bad in ["error", "caller detached", "delivery failed", "retry"] {
            assert!(
                !lower.contains(bad),
                "post-cancel delivery must be quiet: {delta}"
            );
        }
        self.report.data.lock().unwrap()["caller_detach_observation"] = json!({"job_id":id,"health":health,"stderr_delta":delta,
            "stream_end_producer":"subc-client-rs 0.26.1 lib.rs send_handler_outcome(Streamed):1957-1960; late emit drop:1299-1320; Cancel token:1964-1975",
            "status_evidence":"job absent from running_jobs; terminal obtained by exec.attach"});
        self.report.save();
    }
    async fn reconnect(&self) {
        // Killing the module proves automatic recovery, but repeated deliberate
        // faults must not exhaust the scratch daemon's ordinary crash budget.
        // An explicit operator restart re-arms supervision between fault cases.
        tokio::time::sleep(Duration::from_secs(2)).await;
        self.bus.status().await;
        let connection = connection_file::read(&self.bus.connection).unwrap();
        let endpoint = connection.endpoints.first().unwrap();
        let mut socket = TcpStream::connect((endpoint.host.as_str(), endpoint.port))
            .await
            .unwrap();
        authenticate_client(&mut socket, &connection, Duration::from_secs(5))
            .await
            .unwrap();
        send(
            &mut socket,
            FrameType::Request,
            0,
            0,
            json!({"op":"supervisor.restart","module_id":"ck-motor"}),
        )
        .await;
        let reply = timeout(Duration::from_secs(60), receive(&mut socket))
            .await
            .expect("scratch supervisor restart deadline");
        assert_eq!(
            reply.header.ty,
            FrameType::Response,
            "{}",
            String::from_utf8_lossy(&reply.body)
        );
        let reply: Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(reply["op"], "supervisor.ack");
        assert_eq!(reply["applied"], true);
        self.bus.status().await;
    }
}

fn status_lines(output: &str) -> Vec<(String, String)> {
    let mut result = Vec::new();
    for line in output.lines() {
        let words: Vec<_> = line.split_whitespace().collect();
        if !words.first().is_some_and(|w| {
            matches!(
                *w,
                "PASS" | "FAIL" | "SKIP" | "TIMEOUT" | "LEAK" | "XPASS" | "XFAIL"
            )
        }) {
            continue;
        }
        let close = words
            .iter()
            .position(|w| w.ends_with(']'))
            .expect("nextest status timing");
        assert!(
            words.len() > close + 2,
            "status must have binary and test name: {line}"
        );
        let mut name = &words[close + 1..];
        // Nextest also prints a dispatch ordinal, whose denominator differs
        // between platforms. Neither that counter nor timing identifies a test.
        if name.first().is_some_and(|word| word.starts_with('(')) {
            let end = name
                .iter()
                .position(|word| word.ends_with(')'))
                .expect("nextest dispatch ordinal");
            name = &name[end + 1..];
        }
        assert!(name.len() >= 2, "binary and test name: {line}");
        result.push((words[0].into(), name.join(" ")));
    }
    result.sort();
    result
}
fn diagnostics(output: &str, roots: &[PathBuf]) -> BTreeSet<String> {
    output
        .split_whitespace()
        .filter_map(|word| {
            let word = word
                .trim_matches(|c: char| matches!(c, '(' | ')' | ',' | '\'' | '"'))
                .trim_end_matches(':');
            let mut pieces = word.rsplitn(3, ':');
            let last = pieces.next()?;
            let middle = pieces.next()?;
            let first = pieces.next()?;
            if !last.chars().all(|c| c.is_ascii_digit())
                || !middle.chars().all(|c| c.is_ascii_digit())
            {
                return None;
            }
            let path = Path::new(first);
            if path.is_absolute() && !roots.iter().any(|r| path.starts_with(r)) {
                return None;
            }
            if !first.ends_with(".rs") {
                return None;
            }
            Some(format!("{first}:{middle}"))
        })
        .collect()
}
fn platform_exclusions(mac: &mut Vec<(String, String)>, linux: &mut Vec<(String, String)>) {
    for name in include_str!("fixtures/macos_only_tests.txt")
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.starts_with('#'))
    {
        assert_eq!(
            mac.iter()
                .filter(|(_, n)| n.split_whitespace().last() == Some(name))
                .count(),
            1,
            "listed Mac-only test missing: {name}"
        );
        assert!(
            !linux
                .iter()
                .any(|(_, n)| n.split_whitespace().last() == Some(name)),
            "Mac-only test ran on Linux: {name}"
        );
        mac.retain(|(_, n)| n.split_whitespace().last() != Some(name));
    }
    for name in include_str!("fixtures/linux_only_tests.txt")
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.starts_with('#'))
    {
        assert_eq!(
            linux
                .iter()
                .filter(|(_, n)| n.split_whitespace().last() == Some(name))
                .count(),
            1,
            "listed Linux-only test missing: {name}"
        );
        assert!(
            !mac.iter()
                .any(|(_, n)| n.split_whitespace().last() == Some(name)),
            "Linux-only test ran on Mac: {name}"
        );
        linux.retain(|(_, n)| n.split_whitespace().last() != Some(name));
    }
}
fn compare(local: &ProcessOutput, remote: &Call, roots: &[PathBuf], mac_exclusions: bool) {
    assert_eq!(
        remote.terminal().outcome,
        Outcome::Exit {
            code: local.status.code().unwrap()
        },
        "local:\n{}\nremote:\n{}",
        text(local),
        remote.output()
    );
    let mut mac = status_lines(&text(local));
    let mut linux = status_lines(&remote.output());
    assert!(
        !mac.is_empty() && !linux.is_empty(),
        "must actually run tests"
    );
    if mac_exclusions {
        platform_exclusions(&mut mac, &mut linux);
    }
    assert_eq!(mac, linux, "final per-test status multiset");
    assert_eq!(
        diagnostics(&text(local), roots),
        diagnostics(&remote.output(), roots),
        "workspace and sibling file:line diagnostics"
    );
}

async fn acceptance(rig: &mut Rig) {
    let resume = std::env::var("CK_MOTOR_E2E_RESUME").unwrap_or_default();
    assert!(
        matches!(resume.as_str(), "" | "incremental" | "snapshots"),
        "unknown acceptance checkpoint"
    );
    if matches!(resume.as_str(), "incremental" | "snapshots") {
        let prior_path = PathBuf::from(
            std::env::var_os("CK_MOTOR_E2E_PRIOR_REPORT")
                .expect("incremental resume requires retained full-gate evidence"),
        );
        let prior: Value = serde_json::from_slice(&fs::read(&prior_path).unwrap()).unwrap();
        let runs = prior["runs"].as_array().expect("prior full-gate runs");
        assert_eq!(
            runs.len(),
            4,
            "this checkpoint requires four complete, directly passing baseline runs"
        );
        let roots = [
            rig.worktrees.join("parity"),
            rig.siblings[0].clone(),
            rig.siblings[1].clone(),
        ];
        // Use an array-sized chunk to express that this loop compares run pairs.
        for pair in runs.as_chunks::<2>().0 {
            let mut parsed = Vec::new();
            for (run, side) in pair.iter().zip(["linux", "mac"]) {
                assert_eq!(run["side"], side);
                assert_eq!(run["exit_code"], 0);
                assert_eq!(run["command"], format!("{CORE_GATE}{STATUSES}"));
                let raw = fs::read_to_string(run["raw_output"].as_str().unwrap())
                    .expect("retained complete raw output");
                parsed.push((status_lines(&raw), diagnostics(&raw, &roots)));
            }
            let (linux, mac) = parsed.split_at_mut(1);
            platform_exclusions(&mut mac[0].0, &mut linux[0].0);
            assert_eq!(linux[0], mac[0], "retained root/crate-cwd baseline parity");
        }
        rig.report.data.lock().unwrap()["resumed_from"] =
            json!({"step":resume,"prior_full_gate_report":prior_path});
        rig.report.save();
        eprintln!(
            "motor-e2e step 2: resume at incremental transfer with retained full-gate evidence"
        );
        if resume == "snapshots" {
            git(
                &rig.root,
                &["commit", "--allow-empty", "-m", "D: checkpoint restore"],
            );
            rig.tip = git(&rig.root, &["rev-parse", "HEAD"]);
            assert_eq!(
                git(&rig.root, &["rev-parse", "HEAD^{tree}"]),
                git(&rig.root, &["rev-parse", &format!("{}^{{tree}}", rig.a)])
            );
            let probe = rig.workspace("restore-probe", &rig.tip);
            rig.bus.prepare(&probe, &rig.siblings).await;
            rig.published(&rig.tip, "ok").await;
            snapshots_and_env(rig).await;
            fairness(rig).await;
            final_gates(rig).await;
            return;
        }
    } else {
        eprintln!("motor-e2e step 1: remote/local parity and crate cwd");
        let key = rig.workspace("parity", &rig.a);
        let roots = vec![
            key.clone(),
            rig.siblings[0].clone(),
            rig.siblings[1].clone(),
        ];
        rig.baseline_pair(&key, &key, CORE_GATE, &roots).await;
        let file = key.join("crates/prefrontal-core-store/src/lib.rs");
        broken_test(&file);
        let command = format!("{STORE_GATE}{STATUSES}");
        let remote = rig.run(&key, &command).await;
        compare(&shell(&key, &command), &remote, &roots, false);
        assert!(remote.output().contains("motor e2e deliberate assertion"));
        assert!(
            !diagnostics(&remote.output(), &roots).is_empty(),
            "broken-test file:line comparison must actually observe a diagnostic"
        );
        assert!(file.exists());
        assert_one_failure(&remote, BROKEN);
        git(&key, &["restore", "."]);
        let cwd = key.join("crates/prefrontal-core-module");
        rig.baseline_pair(&key, &cwd, CORE_GATE, &roots).await;

        eprintln!("motor-e2e step 2: fresh changes, concurrency, incremental bundles");
        rig.published(&rig.a, "ok").await;
        let fresh = rig.workspace("fresh", &rig.a);
        broken_test(&fresh.join("crates/motor-protocol/src/lib.rs"));
        let result = rig
            .run(
                &fresh,
                &format!("cargo nextest run -p motor-protocol{STATUSES}"),
            )
            .await;
        assert!(result.output().contains("Compiling motor-protocol"));
        assert_one_failure(&result, BROKEN);
        git(&fresh, &["restore", "."]);
        let commons_lib = rig.siblings[0].join("crates/cortexkit-store-types/src/lib.rs");
        broken_test(&commons_lib);
        let result = rig
            .run(
                &fresh,
                &format!(
                    "cargo nextest run --manifest-path {} -p cortexkit-store-types{STATUSES}",
                    quote(rig.siblings[0].join("Cargo.toml").to_str().unwrap())
                ),
            )
            .await;
        assert!(result.output().contains("Compiling cortexkit-store-types"));
        assert_one_failure(&result, BROKEN);
        git(&rig.siblings[0], &["restore", "."]);
        let other = rig.workspace("concurrent", &rig.a);
        for (key, content) in [
            (&fresh, "// Left branch isolation probe.\n"),
            (&other, "// Right branch isolation probe.\n"),
        ] {
            append(&key.join("crates/motor-protocol/src/lib.rs"), content);
            git(key, &["add", "crates/motor-protocol/src/lib.rs"]);
            git(key, &["commit", "-m", "distinct concurrent workspace tree"]);
        }
        let left = rig
            .start(
                &fresh,
                "cargo build -p motor-protocol && mkdir -p target && echo left > target/left-only",
                4,
            )
            .await;
        let right = rig
        .start(
            &other,
            "cargo build -p motor-protocol && mkdir -p target && echo right > target/right-only",
            4,
        )
        .await;
        let (left, right) = tokio::join!(left.finish(), right.finish());
        left.success();
        right.success();
        assert_eq!(
            left.terminal().tree_hash.as_deref(),
            Some(git(&fresh, &["rev-parse", "HEAD^{tree}"]).as_str())
        );
        assert_eq!(
            right.terminal().tree_hash.as_deref(),
            Some(git(&other, &["rev-parse", "HEAD^{tree}"]).as_str())
        );
        assert_ne!(left.terminal().tree_hash, right.terminal().tree_hash);
        rig.run(
            &fresh,
            "test -f target/left-only && test ! -f target/right-only",
        )
        .await
        .success();
        rig.run(
            &other,
            "test -f target/right-only && test ! -f target/left-only",
        )
        .await
        .success();
    }
    let fresh = if resume == "incremental" {
        let fresh = rig.workspace("fresh", &rig.a);
        rig.bus.prepare(&fresh, &rig.siblings).await;
        rig.published(&rig.a, "ok").await;
        fresh
    } else {
        rig.worktrees.join("fresh")
    };
    let mut random = vec![0; 2 * 1024 * 1024];
    rand::thread_rng().fill_bytes(&mut random);
    fs::write(fresh.join("random-untracked.bin"), random).unwrap();
    rig.run(&fresh, "cargo build -p motor-protocol")
        .await
        .success();
    append(
        &fresh.join("crates/motor-protocol/src/lib.rs"),
        "\n// Incremental rebuild probe.\n",
    );
    let result = rig.run(&fresh, "cargo build -p motor-protocol").await;
    result.success();
    assert!(
        result.terminal().bundle_bytes < 1_000_000,
        "incremental bundle bytes: {}",
        result.terminal().bundle_bytes
    );
    rig.report.data.lock().unwrap()["incremental_bundle_bytes"] =
        json!(result.terminal().bundle_bytes);
    rig.report.save();
    assert_eq!(
        compiling(&result.output()),
        BTreeSet::from(["motor-protocol".to_owned()])
    );

    eprintln!("motor-e2e step 3: dependency-cache measurement; A -> B -> C -> D");
    let warm = rig.workspace("warm-measure", &rig.a);
    let start = Instant::now();
    let result = rig
        .run(&warm, "cargo nextest run -p prefrontal-core-store --no-run")
        .await;
    result.success();
    let workspace_names: BTreeSet<String> = serde_json::from_slice::<Value>(
        &shell(&warm, "cargo metadata --no-deps --format-version 1").stdout,
    )
    .unwrap()["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_owned())
        .collect();
    let compiled = compiling(&result.output());
    let report = json!({"workspace_crates":compiled.intersection(&workspace_names).collect::<Vec<_>>(),
        "registry_crates":compiled.difference(&workspace_names).collect::<Vec<_>>(),"wall_ms":start.elapsed().as_millis()});
    let report_root = PathBuf::from(
        std::env::var_os("CARGO_TARGET_TMPDIR").expect("cargo supplies CARGO_TARGET_TMPDIR"),
    );
    fs::create_dir_all(&report_root).unwrap();
    write_json(&report_root.join("warm-base-report.json"), &report);
    eprintln!("warm-base-report.json: {report}");
    let a_status = rig.published(&rig.a, "ok").await;
    let a_age = a_status["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|repository| repository["repository_root"] == rig.root.to_str().unwrap())
        .unwrap()["warm_target_age_s"]
        .as_u64()
        .unwrap();
    let probe = rig.workspace("tip-probe", &rig.a);
    let store = "crates/prefrontal-core-store/src/lib.rs";
    broken_test(&rig.root.join(store));
    git(&rig.root, &["add", store]);
    git(&rig.root, &["commit", "-m", "B: failing store test"]);
    let b = git(&rig.root, &["rev-parse", "HEAD"]);
    let mirror = format!(
        "{}/mirrors/{:x}",
        remote_path("CK_MOTOR_E2E_STATE_ROOT").display(),
        Sha256::digest(rig.root.to_str().unwrap().as_bytes())
    );
    let mirror = quote(mirror);
    ssh(&format!(
        "if git --no-optional-locks -C {mirror} cat-file -e {b} 2>/dev/null; then exit 1; fi"
    ));
    git(&probe, &["reset", "--hard", &b]);
    rig.bus.prepare(&probe, &rig.siblings).await;
    ssh(&format!(
        "git --no-optional-locks -C {mirror} cat-file -e {b}"
    ));
    let b_status = rig.published(&b, "ok").await;
    assert!(
        b_status["repositories"]
            .as_array()
            .unwrap()
            .iter()
            .find(|repository| repository["repository_root"] == rig.root.to_str().unwrap())
            .unwrap()["warm_target_age_s"]
            .as_u64()
            .unwrap()
            >= a_age
    );
    let b_key = rig.workspace("at-b", &b);
    assert_one_failure(
        &rig.run(&b_key, &format!("{STORE_GATE}{STATUSES}")).await,
        BROKEN,
    );
    // The test-only source edit reused the successful dependency descriptor.
    // Changing the warm command now makes the dependency writer run and fail
    // without making foreground builds of the source tree uncompilable.
    let manifest = rig.root.join("Cargo.toml");
    let manifest_text = fs::read_to_string(&manifest).unwrap();
    let command_section = "[workspace.metadata.motor]\nwarm-commands = [\"sleep 20; cargo build --workspace --all-targets --locked --features motor_e2e_missing_dependency_feature\"]\n";
    let failed_manifest = if let Some(start) = manifest_text.find("[workspace.metadata.motor]") {
        let end = manifest_text[start + 1..]
            .find("\n[")
            .map(|offset| start + 1 + offset)
            .unwrap_or(manifest_text.len());
        format!(
            "{}{}{}",
            &manifest_text[..start],
            command_section,
            &manifest_text[end..]
        )
    } else {
        format!("{manifest_text}\n{command_section}")
    };
    fs::write(&manifest, failed_manifest).unwrap();
    git(&rig.root, &["add", "Cargo.toml"]);
    git(
        &rig.root,
        &["commit", "-m", "C: failed dependency fill descriptor"],
    );
    let c = git(&rig.root, &["rev-parse", "HEAD"]);
    git(&probe, &["reset", "--hard", &c]);
    rig.bus.prepare(&probe, &rig.siblings).await;
    rig.bus
        .wait_status(BOUND, |s| {
            s["repositories"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["last_rebuild_result"] == "building")
        })
        .await;
    let during = rig.workspace("b-during-c", &b);
    rig.bus.prepare(&during, &rig.siblings).await;
    rig.published(&b, "failed").await;
    let after = rig.workspace("b-after-c", &b);
    rig.bus.prepare(&after, &rig.siblings).await;
    for key in [&during, &after] {
        assert_one_failure(
            &rig.run(key, &format!("{STORE_GATE}{STATUSES}")).await,
            BROKEN,
        );
    }
    let c_key = rig.workspace("at-c", &c);
    assert_one_failure(
        &rig.run(&c_key, &format!("{STORE_GATE}{STATUSES}")).await,
        BROKEN,
    );
    let status = rig.bus.status().await;
    assert_no_base(&status);
    rig.published(&b, "failed").await;
    git(
        &rig.root,
        &["restore", "--source", &rig.a, "--", store, "Cargo.toml"],
    );
    git(&rig.root, &["add", store, "Cargo.toml"]);
    git(&rig.root, &["commit", "-m", "D: restore integration tree"]);
    rig.tip = git(&rig.root, &["rev-parse", "HEAD"]);
    assert_eq!(
        git(&rig.root, &["rev-parse", "HEAD^{tree}"]),
        git(&rig.root, &["rev-parse", &format!("{}^{{tree}}", rig.a)])
    );
    git(&probe, &["reset", "--hard", &rig.tip]);
    rig.bus.prepare(&probe, &rig.siblings).await;
    rig.published(&rig.tip, "ok").await;
    rig.bus.idle().await;

    cancellation(rig).await;
    disruptions(rig).await;
    refusals(rig).await;
    runner_restart(rig).await;
    snapshots_and_env(rig).await;
    fairness(rig).await;
    final_gates(rig).await;
}
async fn final_gates(rig: &Rig) {
    eprintln!("motor-e2e final step: status shape and verbatim server gates at D");
    let status = rig.bus.status().await;
    serde_json::from_value::<StatusReply>(status.clone()).expect("live contract status");
    assert!(status["server_reachable"].as_bool().unwrap());
    assert!(status["rustc_version"]
        .as_str()
        .unwrap()
        .starts_with("rustc "));
    let key = rig.workspace("final-gates", &rig.tip);
    for command in [CORE_GATE, STORE_GATE] {
        rig.normalized_gate(
            "linux",
            &key,
            &key,
            command,
            &[
                key.clone(),
                rig.siblings[0].clone(),
                rig.siblings[1].clone(),
            ],
            true,
        )
        .await;
    }
}
fn compiling(output: &str) -> BTreeSet<String> {
    output
        .lines()
        .filter_map(|l| {
            l.trim()
                .strip_prefix("Compiling ")
                .and_then(|s| s.split_whitespace().next())
                .map(str::to_owned)
        })
        .collect()
}
fn assert_one_failure(call: &Call, name: &str) {
    assert_eq!(
        call.terminal().outcome,
        Outcome::Exit { code: 100 },
        "{}",
        call.output()
    );
    let failures: Vec<_> = status_lines(&call.output())
        .into_iter()
        .filter(|(s, _)| s != "PASS" && s != "SKIP")
        .collect();
    assert_eq!(failures.len(), 1, "{}", call.output());
    assert_eq!(failures[0].0, "FAIL");
    assert_eq!(failures[0].1.split_whitespace().last(), Some(name));
}
fn assert_no_base(status: &Value) {
    assert!(
        status["repositories"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["last_rebuild_result"] != "building"),
        "{status}"
    );
    assert!(
        status["running_jobs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|j| !j["workspace_key"].as_str().unwrap().starts_with("base:")),
        "{status}"
    );
}

async fn cancellation(rig: &Rig) {
    eprintln!("motor-e2e step 4: cancellation, runtime deadline, queued deadline, drop");
    let key = rig.workspace("cancel", &rig.tip);
    for bus_cancel in [false, true] {
        let log_offset = fs::read(
            rig.parent
                .join("daemon/data/cortexkit/run/logs/ck-motor.stderr.log"),
        )
        .expect("supervised module log exists before cancellation")
        .len();
        let mut call = rig
            .start(
                &key,
                "trap '' TERM; echo started; while :; do sleep 1; done",
                2,
            )
            .await;
        let id = call.id();
        call.output_until("started").await;
        rig.running(id).await;
        let attached = if bus_cancel {
            // Ending the exec.run request is a detach: AFT does it on Esc, a
            // tool timeout or its own restart, and reattaches later. The job
            // must keep running until an explicit exec.cancel.
            call.cancel().await;
            call.finish_caller_cancelled().await;
            rig.running(id).await;
            None
        } else {
            Some(call)
        };
        rig.bus.cancel(id).await;
        rig.gone(id).await;
        let result = match attached {
            Some(call) => call.finish().await,
            None => rig.bus.attach(id).await.finish().await,
        };
        assert_cancel_terminal(result.terminal(), id);
        let retained = rig.bus.attach(id).await.finish().await;
        assert_eq!(
            result.raw_terminals, retained.raw_terminals,
            "retained cancellation terminal must replay byte-for-byte"
        );
        rig.bus.cancel(id).await;
        assert_eq!(
            retained.terminal(),
            rig.bus.attach(id).await.finish().await.terminal()
        );
        if bus_cancel {
            rig.healthy_after_caller_detach(log_offset, id).await;
        }
    }
    let mut params = rig.request(&key, "sleep 60");
    params["timeout"] = json!(1);
    let result = rig.bus.run(params).await.finish().await;
    assert_eq!(result.terminal().killed, Some(Killed::Deadline));
    rig.bus.idle().await;
    let blocker_key = rig.workspace("deadline-blocker", &rig.tip);
    // Snapshotting and SSH handshakes must not consume the entire queue probe.
    // Release the twelve-second sleeper only after the second job is accepted.
    let blocker = rig
        .start(
            &blocker_key,
            "echo started; while test ! -e release-queue-wait; do sleep .05; done; sleep 12",
            28,
        )
        .await;
    rig.running(blocker.id()).await;
    let mut params = rig.request(&key, "sleep 1");
    params["timeout"] = json!(5);
    params["queue_wait_limit_s"] = json!(60);
    let queued = rig.bus.run(params).await;
    assert!(matches!(
        queued.records.first(),
        Some(StreamRecord::Accepted(_))
    ));
    ssh(&format!(
        "touch {}",
        quote(blocker_key.join("release-queue-wait").to_str().unwrap())
    ));
    let result = queued.finish().await;
    result.success();
    assert!(
        result.terminal().queue_wait_ms >= 9_000,
        "deadline starts at dispatch, not acceptance"
    );
    blocker.finish().await.success();
    let dropped = rig.workspace("drop-running", &rig.tip);
    let mut call = rig.start(&dropped, "echo started; sleep 600", 2).await;
    call.output_until("started").await;
    rig.running(call.id()).await;
    let reply = rig
        .bus
        .rpc("workspace.drop", json!({"workspace_key":dropped}))
        .await;
    assert_eq!(reply["dropped"], true);
    assert_eq!(reply["cancelled_jobs"], json!([call.id()]));
    call.finish().await;
    ssh(&format!("test ! -e {}", quote(dropped.to_str().unwrap())));
}
fn assert_cancel_terminal(terminal: &TerminalRecord, id: Uuid) {
    assert_eq!(terminal.job_id, id);
    assert_eq!(terminal.ran, Some(Ran::Remote));
    assert!(terminal.killed == Some(Killed::Cancel) || (terminal.killed.is_none() && matches!(terminal.outcome,Outcome::Exit { .. })),
        "cancellation races normal completion, but must yield a known retained terminal: {terminal:?}");
}

async fn disruptions(rig: &Rig) {
    eprintln!("motor-e2e step 5: transport loss and supervised module restart");
    let key = rig.workspace("reattach", &rig.tip);
    for kill_module in [false, true] {
        let command =
            "echo once >> execution-count; for i in $(seq 1 90); do echo $i; sleep 1; done";
        let mut call = rig.start(&key, command, 2).await;
        let id = call.id();
        call.output_until("5\n").await;
        rig.running(id).await;
        let result = if kill_module {
            rig.control(None);
            drop(call);
            rig.reconnect().await;
            rig.bus.attach(id).await.finish().await
        } else {
            let mut command = Command::new("pgrep");
            command.args(["-f", "--"]).arg(format!("--job {id}"));
            for pid in String::from_utf8(checked(command).stdout)
                .unwrap()
                .split_whitespace()
            {
                assert_eq!(
                    unsafe { libc::kill(pid.parse().unwrap(), libc::SIGKILL) },
                    0
                );
            }
            call.finish().await
        };
        result.success();
        assert_eq!(
            result.output().lines().collect::<Vec<_>>(),
            (1..=90).map(|n| n.to_string()).collect::<Vec<_>>()
        );
        let seqs: Vec<_> = result
            .records
            .iter()
            .filter_map(|r| {
                if let StreamRecord::Output(o) = r {
                    Some(o.seq)
                } else {
                    None
                }
            })
            .collect();
        assert!(seqs.windows(2).all(|p| p[1] == p[0] + 1));
        ssh(&format!(
            "test \"$(wc -l < {}/execution-count)\" -eq 1",
            quote(key.to_str().unwrap())
        ));
    }
    let mut call = rig.start(&key, "echo start; sleep 600", 2).await;
    let id = call.id();
    call.output_until("start").await;
    rig.running(id).await;
    rig.control(Some("unreachable"));
    drop(call);
    rig.reconnect().await;
    let mut attached = rig.bus.attach(id).await;
    attached.cancel().await;
    // The cancellation has to reach the restarted module before killing it.
    tokio::time::sleep(Duration::from_secs(2)).await;
    rig.control(None);
    drop(attached);
    rig.reconnect().await;
    let result = rig.bus.attach(id).await.finish().await;
    assert_cancel_terminal(result.terminal(), id);
    rig.gone(id).await;
}

async fn marker_absent(rig: &Rig, key: &Path, marker: &str) {
    assert!(!key.join(marker).exists(), "submitted command ran on Mac");
    ssh(&format!(
        "test ! -e {}",
        quote(key.join(marker).to_str().unwrap())
    ));
    let status = rig.bus.status().await;
    assert_no_base(&status);
}
async fn refusals(rig: &Rig) {
    eprintln!("motor-e2e step 6: before-start refusal proofs");
    rig.bus.idle().await;
    let key = rig.workspace("refusals", &rig.tip);
    for (control, reason, accepted) in [
        ("unreachable", RefusalReason::Unreachable, false),
        (
            "inject_snapshot_failure",
            RefusalReason::SnapshotFailed,
            false,
        ),
        (
            "inject_tree_hash_mismatch",
            RefusalReason::TreeHashMismatch,
            true,
        ),
    ] {
        rig.control(Some(control));
        rig.reconnect().await;
        let before = Instant::now();
        let result = rig
            .run(&key, &format!("echo forbidden > {control}-marker"))
            .await;
        result.refusal(reason, accepted);
        assert!(result.terminal().tree_hash.is_some());
        if control == "unreachable" {
            assert!(before.elapsed() < Duration::from_secs(30));
        }
        assert!(git(&key, &["for-each-ref", "refs/ck-motor"]).is_empty());
        rig.control(None);
        rig.reconnect().await;
        marker_absent(rig, &key, &format!("{control}-marker")).await;
    }
    let foreign = rig.workspace("foreign-root", &rig.tip);
    for primary in [true, false] {
        let mut params = rig.request(
            if primary { &rig.root } else { &foreign },
            "echo forbidden > invalid-marker",
        );
        if !primary {
            params["repository_root"] = json!(rig.siblings[0]);
        }
        let result = rig.bus.run(params).await.finish().await;
        result.refusal(RefusalReason::WorkspaceKeyRejected, false);
        assert!(
            result.terminal().tree_hash.is_none() && result.terminal().workspace_changes.is_none()
        );
        marker_absent(
            rig,
            if primary { &rig.root } else { &foreign },
            "invalid-marker",
        )
        .await;
    }
    let saturation = rig.workspace("saturation", &rig.tip);
    let blocker = rig
        .start(&saturation, "echo saturated; sleep 600", 28)
        .await;
    rig.running(blocker.id()).await;
    assert_no_base(&rig.bus.status().await);
    let mut params = rig.request(&key, "echo forbidden > queue-marker");
    params["queue_wait_limit_s"] = json!(0);
    let result = rig.bus.run(params).await.finish().await;
    let Outcome::RefusedBeforeStart {
        reason: RefusalReason::Unknown(detail),
    } = &result.terminal().outcome
    else {
        panic!("missing queue wait diagnostic")
    };
    assert!(detail.starts_with("queue_wait_exceeded:"), "{detail}");
    assert!(detail.contains("held_permits=28/28"), "{detail}");
    result.refusal(RefusalReason::Unknown(detail.clone()), true);
    let replay = rig.bus.attach(result.id()).await.finish().await;
    assert_eq!(replay.terminal(), result.terminal());
    marker_absent(rig, &key, "queue-marker").await;
    let mut queued = Vec::new();
    let queue_limit = motor_module::module::RUN_ADMISSION_LIMIT - 1;
    for batch in (0..queue_limit).step_by(8) {
        let mut jobs = tokio::task::JoinSet::new();
        for _ in batch..(batch + 8).min(queue_limit) {
            let bus = rig.bus.clone();
            let params = rig.request(&saturation, "sleep 600");
            jobs.spawn(async move { bus.run(params).await });
        }
        while let Some(job) = jobs.join_next().await {
            queued.push(job.unwrap());
        }
    }
    let full_status = timeout(Duration::from_secs(30), rig.bus.status())
        .await
        .expect("status_while_full must retain SDK control headroom");
    assert_eq!(full_status["queue_depth"], json!(queue_limit));
    assert_no_base(&full_status);
    let health = rig
        .daemon_rpc(json!({"op":"supervisor.health_probe","module_id":"ck-motor"}))
        .await;
    assert_eq!(
        health["status"], "ok",
        "full runner queue must not saturate provider dispatch: {health}"
    );
    rig.report.data.lock().unwrap()["admission_capacity"] = json!({"sdk":motor_module::module::SDK_HANDLER_CAPACITY,
        "reserved_controls":motor_module::module::CONTROL_HEADROOM,"runs":motor_module::module::RUN_ADMISSION_LIMIT,"full_status":full_status,"health":health});
    rig.report.save();
    let result = rig.run(&key, "echo forbidden > full-marker").await;
    result.refusal(RefusalReason::RunnerFull, false);
    marker_absent(rig, &key, "full-marker").await;
    for call in queued {
        timeout(Duration::from_secs(30), rig.bus.cancel(call.id()))
            .await
            .expect("cancel_while_full must retain SDK control headroom");
        call.finish().await;
    }
    rig.bus.cancel(blocker.id()).await;
    blocker.finish().await;
    rig.bus.idle().await;
}

async fn runner_restart(rig: &Rig) {
    eprintln!("motor-e2e step 7: runner restart never dispatches twice");
    rig.bus.idle().await;
    let key = rig.workspace("runner-restart", &rig.tip);
    let other = rig.workspace("runner-queued", &rig.tip);
    let completed = rig.run(&other, "echo retained-output").await;
    completed.success();
    let mut first = rig
        .start(
            &key,
            "echo once >> restart-count; echo started; sleep 600",
            26,
        )
        .await;
    first.output_until("started").await;
    rig.running(first.id()).await;
    let second = rig
        .start(&other, "echo forbidden > never-dispatched", 16)
        .await;
    rig.bus
        .wait_status(Duration::from_secs(30), |s| s["queue_depth"] == 1)
        .await;
    let scope = match operator_input("CK_MOTOR_E2E_SYSTEMD_SCOPE").as_str() {
        "user" => "--user",
        "system" => "--system",
        _ => panic!("CK_MOTOR_E2E_SYSTEMD_SCOPE must be user or system"),
    };
    ssh(&format!(
        "systemctl {scope} restart -- {}",
        quote(operator_input("CK_MOTOR_E2E_SYSTEMD_UNIT"))
    ));
    for id in [first.id(), second.id()] {
        let attached = rig.bus.attach(id).await.finish().await;
        assert_eq!(attached.terminal().outcome, Outcome::OutcomeUnknown);
    }
    let retained = rig.bus.attach(completed.id()).await.finish().await;
    assert_eq!(retained.output(), completed.output());
    assert_eq!(retained.terminal(), completed.terminal());
    ssh(&format!(
        "test \"$(wc -l < {}/restart-count)\" -eq 1 && test ! -e {}/never-dispatched",
        quote(key.to_str().unwrap()),
        quote(other.to_str().unwrap())
    ));
    rig.bus.idle().await;
}

fn checkout_state(key: &Path) -> (String, String, SystemTime, String) {
    let index = git(
        key,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    );
    (
        git(key, &["rev-parse", "HEAD"]),
        git(key, &["status", "--porcelain"]),
        fs::metadata(index).unwrap().modified().unwrap(),
        git(key, &["for-each-ref", "refs/ck-motor"]),
    )
}
async fn snapshots_and_env(rig: &Rig) {
    eprintln!("motor-e2e step 8: inline prepare, sibling snapshots, checkout and env fences");
    let key = rig.workspace("snapshots", &rig.tip);
    git(&key, &["reset", "--hard", &rig.tip]);
    fs::write(
        key.join("tracked-delete-probe"),
        "tracked deletion fixture\n",
    )
    .unwrap();
    git(&key, &["add", "tracked-delete-probe"]);
    git(&key, &["commit", "-m", "tracked deletion fixture"]);
    rig.run(&key, "echo inline").await.success();
    let sibling_file = rig.siblings[0].join("crates/cortexkit-store-types/src/lib.rs");
    append(
        &sibling_file,
        "\ncompile_error!(\"motor sibling snapshot error\");\n",
    );
    // The repository now consumes published store crates. Select the mirrored
    // sibling explicitly so this probe still compiles the sibling snapshot.
    let failed = rig
        .run(
            &key,
            &format!(
                "cargo check --manifest-path {} -p cortexkit-store-types",
                quote(rig.siblings[0].join("Cargo.toml").to_str().unwrap())
            ),
        )
        .await;
    assert!(
        failed.output().contains("motor sibling snapshot error"),
        "{}",
        failed.output()
    );
    let sibling_diagnostics = diagnostics(&failed.output(), &[rig.siblings[0].clone()]);
    assert!(
        sibling_diagnostics.iter().any(|diagnostic| {
            let path = Path::new(diagnostic.rsplit_once(':').unwrap().0);
            let resolved = if path.is_absolute() {
                path.to_owned()
            } else {
                rig.siblings[0].join(path)
            };
            resolved == sibling_file && resolved.is_file()
        }),
        "compiler diagnostic must resolve to the same existing Mac sibling source: {}",
        failed.output()
    );
    assert!(
        failed.output().contains(
            rig.siblings[0]
                .join("crates/cortexkit-store-types")
                .to_str()
                .unwrap()
        ),
        "Cargo identifies the canonical sibling package root"
    );
    git(&rig.siblings[0], &["restore", "."]);
    let other = rig.workspace("sibling-conflict", &rig.tip);
    fs::write(rig.siblings[0].join("sibling-content"), "first\n").unwrap();
    let command = format!(
        "cat {}; echo start; sleep 3; echo end",
        quote(rig.siblings[0].join("sibling-content").to_str().unwrap())
    );
    let held_command = format!(
        "cat {}; echo start; while test ! -e target/release-sibling; do sleep .05; done; echo end",
        quote(rig.siblings[0].join("sibling-content").to_str().unwrap())
    );
    let mut first = rig.start(&key, &held_command, 2).await;
    first.output_until("start").await;
    rig.running(first.id()).await;
    fs::write(rig.siblings[0].join("sibling-content"), "second\n").unwrap();
    let second = rig.start(&other, &command, 2).await;
    let status = rig.bus.status().await;
    assert!(
        !status["running_jobs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|j| j["job_id"] == second.id().to_string()),
        "conflicting sibling waits"
    );
    // Hold the first snapshot across SSH setup of the second request, then
    // release it only after the runner has proved the conflict cannot dispatch.
    ssh(&format!(
        "touch {}",
        quote(key.join("target/release-sibling").to_str().unwrap())
    ));
    let (first, second) = tokio::join!(first.finish(), second.finish());
    first.success();
    second.success();
    assert_eq!(first.output(), "first\nstart\nend\n");
    assert_eq!(second.output(), "second\nstart\nend\n");
    fs::remove_file(rig.siblings[0].join("sibling-content")).unwrap();
    let file = key.join("crates/motor-protocol/src/lib.rs");
    append(&file, "\n// Staged snapshot edit.\n");
    git(&key, &["add", "crates/motor-protocol/src/lib.rs"]);
    append(&file, "// Unstaged snapshot edit.\n");
    fs::remove_file(key.join("tracked-delete-probe")).unwrap();
    fs::write(key.join("untracked.bin"), [0, 255, 128, 0]).unwrap();
    fs::write(key.join("deleted-after-first"), "present").unwrap();
    let before = checkout_state(&key);
    assert!(before.3.is_empty());
    rig.run(
        &key,
        "test ! -e tracked-delete-probe && test -e untracked.bin && test -e deleted-after-first",
    )
    .await
    .success();
    assert_eq!(before, checkout_state(&key));
    rig.control(Some("inject_snapshot_failure"));
    rig.reconnect().await;
    rig.run(&key, "echo forbidden > snapshot-failure-marker")
        .await
        .refusal(RefusalReason::SnapshotFailed, false);
    assert_eq!(before, checkout_state(&key));
    rig.control(None);
    rig.reconnect().await;
    marker_absent(rig, &key, "snapshot-failure-marker").await;
    fs::remove_file(key.join("deleted-after-first")).unwrap();
    let result = rig
        .run(
            &key,
            "test ! -e deleted-after-first && echo remote > remote-write",
        )
        .await;
    result.success();
    assert!(result
        .terminal()
        .workspace_changes
        .as_ref()
        .unwrap()
        .contains(&"remote-write".to_owned()));
    assert!(!key.join("remote-write").exists());
    fs::create_dir_all(key.join("target")).unwrap();
    fs::write(key.join("target/ignored-secret"), "not transferred").unwrap();
    rig.run(&key, "test ! -e target/ignored-secret")
        .await
        .success();
    let mut request = rig.request(&key, &format!("env | sort; {STORE_GATE}"));
    request["env"] = json!({"RUST_BACKTRACE":"1","SUBC_LAUNCH_NONCE":"dummy-nonce","SSH_AUTH_SOCK":"dummy-agent","ANTHROPIC_API_KEY":"dummy-key",
        "CARGO_TARGET_DIR":"/nope","PATH":"/nope","HOME":"/nope","TMPDIR":"/var/folders/x","CARGO_HOME":"/nope"});
    let result = rig.bus.run(request).await.finish().await;
    result.success();
    let output = result.output();
    assert!(output.lines().any(|l| l == "RUST_BACKTRACE=1"));
    for name in ["SUBC_LAUNCH_NONCE", "SSH_AUTH_SOCK", "ANTHROPIC_API_KEY"] {
        assert!(!output.contains(&format!("{name}=")));
    }
    for name in ["PATH", "HOME", "TMPDIR", "CARGO_HOME", "CARGO_TARGET_DIR"] {
        assert!(
            output.lines().any(|l| l.starts_with(&format!("{name}="))),
            "runner owns {name}"
        );
    }
    assert!(!output.contains("/nope") && !output.contains("/var/folders/x"));
}
async fn fairness(rig: &Rig) {
    eprintln!("motor-e2e step 9: round-robin workspace fairness");
    rig.bus.idle().await;
    assert_no_base(&rig.bus.status().await);
    let a = rig.workspace("fair-a", &rig.tip);
    let b = rig.workspace("fair-b", &rig.tip);
    let first = rig
        .start(
            &a,
            "echo started; while test ! -e target/release-fairness; do sleep .05; done; sleep 20",
            16,
        )
        .await;
    rig.running(first.id()).await;
    let mut rest = Vec::new();
    for _ in 1..10 {
        rest.push(rig.start(&a, "sleep 20", 16).await);
    }
    let b_job = rig.start(&b, "echo started; sleep 20", 16).await;
    ssh(&format!(
        "touch {}",
        quote(a.join("target/release-fairness").to_str().unwrap())
    ));
    first.finish().await.success();
    rig.running(b_job.id()).await;
    let status = rig.bus.status().await;
    assert_no_base(&status);
    assert!(!status["running_jobs"]
        .as_array()
        .unwrap()
        .iter()
        .any(|j| j["workspace_key"] == a.to_str().unwrap()));
    for job in rest {
        rig.bus.cancel(job.id()).await;
        job.finish().await;
    }
    rig.bus.cancel(b_job.id()).await;
    b_job.finish().await;
    rig.bus.idle().await;
}

async fn runner_rpc(frame: Frame) -> Frame {
    let connect = format!(
        "{} connect --instance e2e",
        quote(remote_path("CK_MOTOR_E2E_RUNNER_BINARY").to_str().unwrap())
    );
    let mut child = tokio::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPath=none",
            &ssh_target(),
            &connect,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    motor_protocol::write_frame_async(
        &mut stdin,
        &Frame::Hello {
            protocol_version: motor_protocol::PROTOCOL_VERSION,
            started: true,
            default_branch_tip: false,
            gate_jobs: false,
            platforms: Vec::new(),
        },
    )
    .await
    .unwrap();
    let hello = timeout(Duration::from_secs(30), async {
        loop {
            let frame = motor_protocol::read_frame_async(&mut stdout)
                .await
                .unwrap()
                .unwrap();
            if !matches!(frame, Frame::Heartbeat { .. }) {
                break frame;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        hello,
        Frame::Hello {
            protocol_version: motor_protocol::PROTOCOL_VERSION,
            started: true,
            default_branch_tip: false,
            gate_jobs: false,
            platforms: Vec::new(),
        }
    );
    motor_protocol::write_frame_async(&mut stdin, &frame)
        .await
        .unwrap();
    loop {
        let response = timeout(
            Duration::from_secs(60),
            motor_protocol::read_frame_async(&mut stdout),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        if !matches!(response, Frame::Heartbeat { .. }) {
            return response;
        }
    }
}

struct Cleanup {
    parent: PathBuf,
    daemon: Option<cortexkit_test_support::TestDaemon>,
    armed: bool,
}
impl Cleanup {
    async fn finish(&mut self) {
        // Stop the supervised consumer first so it cannot submit more jobs.
        if let Some(child) = self.daemon.as_mut() {
            child.stop();
        }
        if self.parent.join("ownership.json").exists() {
            let owned: Value =
                serde_json::from_slice(&fs::read(self.parent.join("ownership.json")).unwrap())
                    .unwrap();
            for batch in owned["jobs"].as_array().unwrap().chunks(2) {
                let mut cancels = tokio::task::JoinSet::new();
                for id in batch {
                    let id: Uuid = serde_json::from_value(id.clone()).unwrap();
                    cancels.spawn(async move {
                        runner_rpc(Frame::Cancel(
                            serde_json::from_value(json!({"job_id":id})).unwrap(),
                        ))
                        .await
                    });
                }
                while let Some(reply) = cancels.join_next().await {
                    assert!(matches!(reply.unwrap(), Frame::CancelReply(_)));
                }
            }
            for key in owned["workspaces"].as_array().unwrap() {
                let key = key.as_str().unwrap();
                assert!(Path::new(key).starts_with(self.parent.join("worktrees")));
                assert!(matches!(
                    runner_rpc(Frame::Drop(
                        serde_json::from_value(json!({"workspace_key":key})).unwrap()
                    ))
                    .await,
                    Frame::DropReply(_)
                ));
                // A failed warm clone may leave a partial directory before the
                // runner can publish a workspace catalog entry. Drop has still
                // been called; remove only this explicitly recorded scratch key.
                ssh(&format!("rm -rf -- {}", quote(key)));
                ssh(&format!("test ! -e {}", quote(key)));
            }
        }
        let cleanup_deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let Frame::StatusReply(status) = runner_rpc(Frame::Status).await else {
                panic!("cleanup status");
            };
            for job in &status.running_jobs {
                assert_eq!(
                    job.workspace_key,
                    format!("base:{}", self.parent.join("prefrontal").display()),
                    "unowned e2e job during teardown"
                );
                runner_rpc(Frame::Cancel(
                    serde_json::from_value(json!({"job_id":job.job_id})).unwrap(),
                ))
                .await;
            }
            if status.running_jobs.is_empty() && status.queue_depth == 0 {
                break;
            }
            assert!(
                Instant::now() < cleanup_deadline,
                "teardown jobs did not stop"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        // Restrict cleanup to these three deterministic mirror names. The e2e
        // instance's mirrors and published generation deliberately persist.
        let roots = [
            self.parent.join("prefrontal"),
            self.parent.join("commons"),
            self.parent.join("claustrum"),
        ];
        let mirror_names: Vec<_> = roots
            .iter()
            .map(|p| format!("{:x}", Sha256::digest(p.to_str().unwrap().as_bytes())))
            .collect();
        ssh(&format!(
            "rm -rf -- {}",
            quote(
                self.parent
                    .join("worktrees")
                    .join(format!(".motor-base-e2e-{}", mirror_names[0]))
                    .to_str()
                    .unwrap()
            )
        ));
        for name in &mirror_names {
            let mirror = remote_path("CK_MOTOR_E2E_STATE_ROOT")
                .join("mirrors")
                .join(name);
            ssh(&format!("m={}; if test -d \"$m\"; then git --no-optional-locks -C \"$m\" for-each-ref --format='delete %(refname)' refs/snapshots refs/tips | git --no-optional-locks -C \"$m\" update-ref --stdin; fi", quote(mirror.to_str().unwrap())));
        }
        for sibling in [
            &roots[1],
            &roots[2],
            &self.parent.join("worktrees/commons"),
            &self.parent.join("worktrees/claustrum"),
        ] {
            ssh(&format!(
                "rm -rf -- {}; test ! -e {}",
                quote(sibling.to_str().unwrap()),
                quote(sibling.to_str().unwrap())
            ));
        }
        // Published base checkouts are runner-owned under its state root, not
        // task workspaces. No scratch absolute path may remain on the server.
        ssh(&format!("p={}; test ! -d \"$p/worktrees\" || test -z \"$(find \"$p/worktrees\" -mindepth 1 -maxdepth 1 -print)\"; rm -rf -- \"$p\"", quote(self.parent.to_str().unwrap())));
        if self.parent.exists() {
            for entry in fs::read_dir(&self.parent).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() && !path.is_symlink() {
                    fs::remove_dir_all(path).unwrap();
                } else {
                    fs::remove_file(path).unwrap();
                }
            }
        }
        assert_eq!(fs::read_dir(&self.parent).unwrap().count(), 0);
        self.armed = false;
    }
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        // Kill locally before remote cleanup: a broken SSH teardown must never
        // leave the daemon (or its supervised consumer) running.
        if let Some(child) = self.daemon.as_mut() {
            child.stop();
        }
        if !self.armed {
            return;
        }
        // An assertion may unwind through the async harness. A separate runtime
        // ensures teardown still awaits cancel/drop rather than abandoning them.
        let parent = self.parent.clone();
        let daemon = self.daemon.take();
        let cleanup = std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let mut cleanup = Cleanup {
                parent,
                daemon,
                armed: false,
            };
            runtime.block_on(async {
                timeout(Duration::from_secs(420), cleanup.finish())
                    .await
                    .expect("failure teardown exceeded its overall deadline");
            });
        })
        .join();
        if cleanup.is_err() {
            eprintln!(
                "motor-e2e teardown failed; retain the scratch path for manual recovery: {}",
                self.parent.display()
            );
        }
    }
}

async fn consumer(parent: &Path) {
    // A supervised child may be respawned after an assertion. Preserve the first
    // run's raw evidence and ownership ledger until the parent finishes teardown.
    if parent.join("failed").exists() {
        std::future::pending::<()>().await;
    }
    let report = Arc::new(AcceptanceReport::new());
    let failed_path = parent.join("failed");
    let previous_hook = std::panic::take_hook();
    let failed_report = report.clone();
    std::panic::set_hook(Box::new(move |info| {
        let _ = fs::write(&failed_path, info.to_string());
        failed_report.status("failed");
        previous_hook(info);
    }));
    let nonce = subc_os::launch_nonce()
        .unwrap()
        .expect("supervised AFT nonce")
        .value()
        .to_owned();
    let root = parent.join("prefrontal");
    let connection = parent.join("daemon/runtime/subc-connection.json");
    let bus = Bus {
        connection,
        root: root.clone(),
        nonce,
        owned: Arc::new(Mutex::new(Ownership::default())),
    };
    let mut rig = Rig {
        parent: parent.to_owned(),
        root: root.clone(),
        worktrees: parent.join("worktrees"),
        siblings: vec![parent.join("commons"), parent.join("claustrum")],
        a: git(&root, &["rev-parse", "HEAD"]),
        tip: String::new(),
        bus,
        report: report.clone(),
    };
    // A supervised consumer presents its nonce on route.open; it does not need
    // to serve any provider surface of its own.
    rig.bus.status().await;
    fs::write(
        parent.join("consumer-ready"),
        "reserved consumer connected\n",
    )
    .unwrap();
    acceptance(&mut rig).await;
    report.status("passed");
    eprintln!(
        "motor-e2e acceptance-report: {}",
        report.directory.join("acceptance-report.json").display()
    );
    fs::write(
        parent.join("passed"),
        "all ordered motor acceptance steps passed\n",
    )
    .unwrap();
    // Keep the supervised child alive until the parent stops the scratch daemon.
    std::future::pending::<()>().await;
}

fn clone_into(source: &Path, destination: &Path) {
    let mut command = Command::new("git");
    command
        .args(["--no-optional-locks", "clone", "--no-hardlinks"])
        .arg(source)
        .arg(destination);
    checked(command);
    git(destination, &["config", "user.email", "motor-e2e@invalid"]);
    git(destination, &["config", "user.name", "Motor E2E"]);
    git(destination, &["config", "commit.gpgsign", "false"]);
    git(destination, &["config", "core.hooksPath", "/dev/null"]);
}
fn ssh_configs(parent: &Path, home: &Path) {
    let mut command = Command::new("ssh");
    command.args(["-G", &ssh_target()]);
    let settings = String::from_utf8(checked(command).stdout).unwrap();
    let field = |name: &str| {
        settings
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{name} ")))
            .unwrap()
            .to_owned()
    };
    // Resolve the explicit operator target, then map it to ck-motor's default
    // `alfonso-runner` alias in the isolated config without changing production routing.
    let identity =
        PathBuf::from(field("identityfile").replacen('~', &home.display().to_string(), 1));
    fs::write(parent.join("ssh-live"), format!("Host alfonso-runner\n HostName {}\n User {}\n Port {}\n IdentityFile {}\n UserKnownHostsFile {}\n ControlPath none\n ForwardAgent no\n IdentitiesOnly yes\n", field("hostname"), field("user"), field("port"), identity.display(), home.join(".ssh/known_hosts").display())).unwrap();
    fs::write(
        parent.join("ssh-dead"),
        "Host alfonso-runner\n HostName 127.0.0.1\n Port 1\n ControlPath none\n ForwardAgent no\n",
    )
    .unwrap();
}

#[tokio::test]
#[ignore = "requires an explicitly configured operator e2e instance on the Mac"]
async fn motor_e2e() {
    assert_eq!(status_lines("PASS [ 0.01s] (   1/20) package tests::one\nSLOW [ 1s] package tests::one\nPASS [ 2s] (2/20) package tests::two"),
        vec![("PASS".into(), "package tests::one".into()), ("PASS".into(), "package tests::two".into())]);
    assert_eq!(
        diagnostics("at crates/example.rs:12:3:", &[]),
        BTreeSet::from(["crates/example.rs:12".into()])
    );
    if std::env::var_os("CK_MOTOR_E2E_COMPARISON_CHECK").is_some() {
        return;
    }
    if let Some(parent) = std::env::var_os(CHILD) {
        consumer(Path::new(&parent)).await;
        return;
    }
    ssh_target();
    remote_path("CK_MOTOR_E2E_RUNNER_BINARY");
    remote_path("CK_MOTOR_E2E_STATE_ROOT");
    operator_input("CK_MOTOR_E2E_SYSTEMD_UNIT");
    assert!(matches!(
        operator_input("CK_MOTOR_E2E_SYSTEMD_SCOPE").as_str(),
        "user" | "system"
    ));
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(cortexkit_test_support::shared_scratch_root().join("motor-e2e.lock"))
        .unwrap();
    lock.try_lock_exclusive()
        .expect("another motor e2e harness holds the lock");
    let parent = cortexkit_test_support::shared_scratch_root().join("motor-e2e");
    let status = runner_rpc(Frame::Status).await;
    let Frame::StatusReply(status) = status else {
        panic!("e2e status reply");
    };
    assert_eq!(status.queue_depth, 0, "e2e instance is already busy");
    assert!(
        status.running_jobs.is_empty(),
        "e2e instance is already busy"
    );
    fs::create_dir_all(&parent).unwrap();
    let metadata = fs::symlink_metadata(&parent).unwrap();
    assert!(
        metadata.is_dir() && !metadata.is_symlink(),
        "motor e2e scratch must be a real directory"
    );
    if std::env::var_os("CK_MOTOR_E2E_RECOVER").is_some() {
        assert!(
            parent.join("motor.json").is_file(),
            "only recover a previous harness-owned scratch parent"
        );
        let config: Value =
            serde_json::from_slice(&fs::read(parent.join("motor.json")).unwrap()).unwrap();
        assert_eq!(config["runner_instance"], "e2e");
        assert_eq!(config["worktree_roots"], json!([parent.join("worktrees")]));
        Cleanup {
            parent: parent.clone(),
            daemon: None,
            armed: false,
        }
        .finish()
        .await;
    }
    assert_eq!(
        fs::read_dir(&parent).unwrap().count(),
        0,
        "scratch parent must be empty; never erase an unknown checkout"
    );
    let mut cleanup = Cleanup {
        parent: parent.clone(),
        daemon: None,
        armed: true,
    };
    clone_into(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap(),
        &parent.join("prefrontal"),
    );
    for name in ["commons", "claustrum"] {
        clone_into(
            &cortexkit_test_support::sibling_checkout(name).expect("Cargo sibling checkout"),
            &parent.join(name),
        );
    }
    let worktrees = parent.join("worktrees");
    fs::create_dir(&worktrees).unwrap();
    for name in ["commons", "claustrum"] {
        std::os::unix::fs::symlink(parent.join(name), worktrees.join(name)).unwrap();
    }
    ssh_configs(&parent, &home);
    write_json(
        &parent.join("motor.json"),
        &json!({"runner_instance":"e2e","worktree_roots":[worktrees],"ssh_config_override":parent.join("ssh-live")}),
    );
    let daemon_root = parent.join("daemon");
    fs::create_dir_all(daemon_root.join("config/cortexkit")).unwrap();
    // subc appends --subc FILE. Consume those daemon arguments before execing
    // libtest, which accepts only its own test-runner flags.
    let wrapper = parent.join("consumer.sh");
    fs::write(&wrapper, format!("#!/bin/sh\nwhile [ \"$#\" -gt 0 ]; do\n case \"$1\" in\n  --subc) export CK_MOTOR_E2E_CONNECTION=\"$2\"; shift 2;;\n  *) echo \"unexpected consumer argument: $1\" >&2; exit 2;;\n esac\ndone\nexec {} --exact motor_e2e --ignored --nocapture\n", quote(std::env::current_exe().unwrap().to_str().unwrap()))).unwrap();
    let config = json!({"version":1,"storage":{"backend":"sqlite","data_home":daemon_root.join("data")},"modules":{
        "ck-motor":{"program":cortexkit_test_support::ckdev_binary(Path::new(env!("CARGO_BIN_EXE_ck-motor"))),"args":["--config",parent.join("motor.json")]},
        "aft":{"reserved":true,"program":"/bin/sh","args":[wrapper],
            "env":{CHILD:parent,"CARGO_TARGET_TMPDIR":env!("CARGO_TARGET_TMPDIR"),"CK_MOTOR_E2E_LOCAL_HOME":home,
                "CK_MOTOR_E2E_LOCAL_TMPDIR":cortexkit_test_support::scratch_root(),
                "CK_MOTOR_E2E_SSH_TARGET":ssh_target(),
                "CK_MOTOR_E2E_RUNNER_BINARY":remote_path("CK_MOTOR_E2E_RUNNER_BINARY"),
                "CK_MOTOR_E2E_STATE_ROOT":remote_path("CK_MOTOR_E2E_STATE_ROOT"),
                "CK_MOTOR_E2E_SYSTEMD_UNIT":operator_input("CK_MOTOR_E2E_SYSTEMD_UNIT"),
                "CK_MOTOR_E2E_SYSTEMD_SCOPE":operator_input("CK_MOTOR_E2E_SYSTEMD_SCOPE"),
                "CK_MOTOR_E2E_RESUME":std::env::var("CK_MOTOR_E2E_RESUME").unwrap_or_default(),
                "CK_MOTOR_E2E_PRIOR_REPORT":std::env::var("CK_MOTOR_E2E_PRIOR_REPORT").unwrap_or_default()}}
    }});
    write_json(&daemon_root.join("config/cortexkit/subc.jsonc"), &config);
    let daemon_binary = cortexkit_test_support::SubcDaemonBinary::build(&["commons"]);
    let child = daemon_binary
        .command(&daemon_root)
        .env("SUBC_PORT", "0")
        .stdout(Stdio::null())
        .stderr(File::create(parent.join("daemon.stderr.log")).unwrap())
        .spawn()
        .unwrap();
    cleanup.daemon = Some(child);
    let startup = Instant::now() + Duration::from_secs(60);
    let deadline = Instant::now() + Duration::from_secs(6 * 60 * 60);
    let mut reported_stage = String::new();
    while !parent.join("passed").exists() {
        assert!(
            !parent.join("failed").exists(),
            "supervised consumer failed: {}\n{}",
            fs::read_to_string(parent.join("failed")).unwrap_or_default(),
            daemon_tail(&parent)
        );
        assert!(
            Instant::now() < deadline,
            "ordered harness deadline:\n{}",
            daemon_tail(&parent)
        );
        assert!(
            parent.join("consumer-ready").exists() || Instant::now() < startup,
            "scratch consumer startup exceeded 60 s:\n{}",
            daemon_tail(&parent)
        );
        if parent.join("daemon/data/cortexkit/run/logs").exists() {
            for entry in fs::read_dir(parent.join("daemon/data/cortexkit/run/logs")).unwrap() {
                let path = entry.unwrap().path();
                if path.is_file() {
                    let log = fs::read_to_string(&path).unwrap_or_default();
                    if let Some(stage) = log.lines().rev().find(|line| {
                        line.contains("motor-e2e step") || line.contains("warm-base-report.json:")
                    }) {
                        if stage != reported_stage {
                            eprintln!("{stage}");
                            reported_stage = stage.to_owned();
                        }
                    }
                    assert!(
                        !log.contains("test result: FAILED")
                            && !log.contains("thread 'motor_e2e' panicked"),
                        "consumer failed:\n{log}"
                    );
                }
            }
        }
        assert!(
            cleanup
                .daemon
                .as_mut()
                .unwrap()
                .try_wait()
                .unwrap()
                .is_none(),
            "scratch daemon died:\n{}",
            daemon_tail(&parent)
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    eprintln!("{}", daemon_tail(&parent));
    cleanup.finish().await;
    ssh(&format!("python3 - {} <<'PY'\nimport os, sys\np = sys.argv[1]\nassert len(os.listdir(p + '/mirrors')) == 3, os.listdir(p + '/mirrors')\nassert len(os.listdir(p + '/generations')) == 1, os.listdir(p + '/generations')\nprint('motor-e2e teardown: 0 workspaces, 3 mirrors, 1 generation')\nPY", quote(remote_path("CK_MOTOR_E2E_STATE_ROOT").to_str().unwrap())));
    drop(lock);
}

fn daemon_tail(parent: &Path) -> String {
    let mut output = String::new();
    let log_root = parent.join("daemon/data/cortexkit/run/logs");
    if let Ok(entries) = fs::read_dir(log_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "aft.stderr.log" || (name.starts_with("subc.") && name.ends_with(".log")) {
                let log = fs::read_to_string(&path).unwrap_or_default();
                output.push_str(&format!(
                    "{}:\n{}\n",
                    path.display(),
                    log.lines()
                        .rev()
                        .take(40)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect::<Vec<_>>()
                        .join("\n")
                ));
            }
        }
    }
    output
}

struct StubbedGateSide {
    replies: std::collections::VecDeque<(i32, String)>,
    calls: usize,
}
impl StubbedGateSide {
    fn run(&mut self) -> (i32, String) {
        self.calls += 1;
        self.replies
            .pop_front()
            .expect("only one full run and one rerun")
    }
}
const STUB_KEY: &str = "package tests::load_probe";
const STUB_FAIL: &str = "FAIL [ 0.25s] (1/1) package tests::load_probe\n  stderr ───\n  panicked at crates/probe.rs:12:3:\n  assertion failed: load-sensitive probe\nSummary [ 0.25s] 1 tests run: 0 passed, 1 failed\n";
const STUB_PASS: &str =
    "PASS [ 0.03s] (1/1) package tests::load_probe\nSummary [ 0.03s] 1 tests run: 1 passed\n";

#[test]
fn load_flake_passes_on_one_exact_rerun() {
    for _side in ["mac", "linux"] {
        let mut stub = StubbedGateSide {
            replies: [(100, STUB_FAIL.into()), (0, STUB_PASS.into())].into(),
            calls: 0,
        };
        let (_, full) = stub.run();
        let mut statuses = status_lines(&full);
        let mut flakes = BTreeSet::new();
        let (code, rerun) = stub.run();
        apply_exact_rerun(STUB_KEY, code, &rerun, &mut statuses, &mut flakes).unwrap();
        assert_eq!(stub.calls, 2);
        assert!(stub.replies.is_empty());
        assert_eq!(statuses, vec![("PASS".into(), STUB_KEY.into())]);
        assert_eq!(flakes, BTreeSet::from([STUB_KEY.into()]));
        assert!(result_line(&full, STUB_KEY).contains("0.25s"));
        assert!(result_line(&rerun, STUB_KEY).contains("0.03s"));
        assert!(failure_section(&full, STUB_KEY).contains("assertion failed: load-sensitive probe"));
        assert!(diagnostics(&without_flake_diagnostics(&full, &flakes), &[]).is_empty());
    }
}
#[test]
fn repeated_failure_or_zero_test_rerun_blocks_acceptance() {
    for (code, rerun) in [
        (100, STUB_FAIL),
        (0, "Summary [ 0.00s] 0 tests run: 0 passed\n"),
    ] {
        let mut stub = StubbedGateSide {
            replies: [(100, STUB_FAIL.into()), (code, rerun.into())].into(),
            calls: 0,
        };
        let (_, full) = stub.run();
        let mut statuses = status_lines(&full);
        let mut flakes = BTreeSet::new();
        let (code, rerun) = stub.run();
        assert!(apply_exact_rerun(STUB_KEY, code, &rerun, &mut statuses, &mut flakes).is_err());
        assert_eq!(stub.calls, 2);
        assert!(stub.replies.is_empty());
        assert_eq!(statuses, vec![("FAIL".into(), STUB_KEY.into())]);
        assert!(flakes.is_empty());
    }
}
