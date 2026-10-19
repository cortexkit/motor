//! `ck motor run` / `attach`: submit or reattach through the subc daemon, stream
//! output and exit with the job's exit code. It lets a script with no agent behind
//! it (a deploy gate, for example) use the build server.
//!
//! The `ck` launcher reaches this as `ck-motor run ...`: it finds `ck-motor`
//! in the CortexKit bin folder, checks that `ck-motor --ck-domain` prints one
//! headline line, and runs it with the words after `ck motor`. The daemon
//! attests this process as `Principal::Direct`, which ck-motor admits only
//! while its `admit_direct_exec` switch is on. Remote job exit codes are passed
//! through to the caller; submission or transport failures exit unsuccessfully.
use crate::{config::DEFAULT_DIRECT_EXEC_MAX_TIMEOUT_SECS, module::direct_timeout_refusal};
use motor_protocol::{
    AttachRequest, CancelRequest, JobStatusReply, Killed, Outcome, OutputStream, RunRequest,
    StreamRecord, TerminalRecord, Uuid,
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};
use subc_client_rs::{
    async_trait, CallError, CallOptions, ConsumerOptions, RouteHandle, SubcConsumer,
    SubscribeOptions,
};
use subc_protocol::{BindIdentity, RouteTarget};
use tokio::sync::mpsc;

/// Nothing ran: the request was refused before the command started (by
/// ck-motor, the runner or the daemon), or it could not be submitted.
pub const EXIT_REFUSED: u8 = 200;
/// The job may have run, but its outcome never arrived: the connection was
/// lost, or the stream ended without a terminal record. The job ID is printed.
pub const EXIT_UNKNOWN: u8 = 201;
/// The runner stopped the command at its timeout.
pub const EXIT_TIMEOUT: u8 = 202;
/// The job was cancelled, by Ctrl-C or SIGTERM here or by another caller.
pub const EXIT_CANCELLED: u8 = 203;
/// The arguments were malformed; nothing was submitted.
pub const EXIT_USAGE: u8 = 204;

/// The timeout sent when `--timeout` is omitted. Every job from this command
/// has one, so a disconnected caller cannot leave an unbounded job behind.
pub const DEFAULT_TIMEOUT_SECS: u64 = 3600;

/// The line `ck` shows for this domain in its help. `ck` requires exactly one
/// line on stdout and exit status 0 from `ck-motor --ck-domain`.
pub const HEADLINE: &str = "run a command on the remote build server (ck motor run --help)";

/// What `ck motor`, `ck motor --help` and `ck motor help` print.
pub const TOP_HELP: &str = "\
usage: ck motor run [OPTIONS] -- COMMAND...
       ck motor attach JOB_ID [--secret-file PATH] [--status]
       ck motor hosted-check

Runs COMMAND on the remote build server and streams its output here.
See `ck motor run --help` for the options and exit statuses.";

pub const HELP: &str = "\
usage: ck motor run [--vcpus N] [--sibling PATH]... [--env NAME=VALUE]...
                    [--timeout SECONDS] [--cwd PATH] [--secret-file PATH] -- COMMAND...

Runs COMMAND on the remote build server against a snapshot of the git checkout
that contains --cwd (default: the current directory), and streams its stdout
and stderr here as they arrive.

  --vcpus N           vCPUs to reserve (the runner's default when omitted)
  --sibling PATH      also send this sibling repository (repeatable)
  --env NAME=VALUE    set an environment variable for the job (repeatable).
                      Nothing is inherited from this shell; ck-motor forwards
                      only build variables (RUST_*, CARGO_*, NEXTEST_*, CI,
                      NO_COLOR, ...) and reports any it drops.
  --timeout SECONDS   stop the command after this long (default 3600, at most
                      7200; ck-motor may be configured with a lower cap)
  --cwd PATH          directory to run in; its git checkout is the workspace
  --return-path PATH  return this absolute workspace file (repeatable, at most 64)
  --return-max-bytes N  cap total returned raw bytes (default and maximum 262144)
  --secret-file PATH  save the reattach secret in a new 0600 file instead of stderr
  --allow-spill       allow this gate job to spill to hosted capacity when configured

The words after -- are joined with single spaces into one command line, which
bash runs on the runner (bash -c). Quote shell syntax as one argument:
  ck motor run -- 'cargo test 2>&1 | tail -n 20'

Exit status: the command's own exit code; 128+N when a signal N killed it;
200 refused before start (nothing ran); 201 outcome unknown or connection lost
(the job ID is printed); 202 timeout; 203 cancelled; 204 bad arguments.

Ctrl-C or SIGTERM cancels the job and waits for its outcome; a second one stops
waiting. If the connection drops, the job keeps running until it ends or its
timeout stops it. Save the one-time job ID and secret printed on stderr, or use
--secret-file. Reconnect with `ck motor attach JOB_ID`, reading the secret from
stdin, or `ck motor attach JOB_ID --secret-file PATH`. Keep one connection per job, and run
parallel jobs from separate checkouts: the runner runs one job per workspace at
a time.";

/// What `ck motor run` was asked to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Invocation {
    Help,
    Run(RunArgs),
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct RunArgs {
    pub vcpus: Option<u32>,
    pub siblings: Vec<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub timeout: u64,
    pub cwd: Option<PathBuf>,
    pub command: Vec<String>,
    pub secret_file: Option<PathBuf>,
    pub return_paths: Vec<String>,
    pub return_max_bytes: Option<u64>,
    pub allow_spill: bool,
}

/// Parse the words after `ck motor run`. An option takes its value as the
/// next word or after `=`. The command starts after a mandatory `--`.
pub fn parse_args(args: &[String]) -> Result<Invocation, String> {
    let mut parsed = RunArgs {
        timeout: DEFAULT_TIMEOUT_SECS,
        ..RunArgs::default()
    };
    let mut words = args.iter();
    while let Some(word) = words.next() {
        if word == "--" {
            parsed.command = words.cloned().collect();
            if parsed.command.iter().all(|word| word.trim().is_empty()) {
                return Err("no command after --".into());
            }
            return Ok(Invocation::Run(parsed));
        }
        if word == "-h" || word == "--help" {
            return Ok(Invocation::Help);
        }
        if word == "--allow-spill" {
            parsed.allow_spill = true;
            continue;
        }
        let (name, inline) = match word.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value.to_string())),
            _ => (word.as_str(), None),
        };
        if !matches!(
            name,
            "--vcpus"
                | "--sibling"
                | "--env"
                | "--timeout"
                | "--cwd"
                | "--secret-file"
                | "--return-path"
                | "--return-max-bytes"
        ) {
            return Err(if word.starts_with('-') {
                format!("unknown option {word}")
            } else {
                format!("unexpected {word:?}; put the command after --")
            });
        }
        let value = match inline {
            Some(value) => value,
            None => words
                .next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))?,
        };
        match name {
            "--vcpus" => {
                parsed.vcpus = Some(
                    value
                        .parse()
                        .ok()
                        .filter(|vcpus| *vcpus > 0)
                        .ok_or_else(|| {
                            format!("--vcpus needs a positive integer, got {value:?}")
                        })?,
                )
            }
            "--timeout" => {
                parsed.timeout = value
                    .parse()
                    .ok()
                    .filter(|seconds| *seconds > 0)
                    .ok_or_else(|| {
                        format!("--timeout needs a positive number of seconds, got {value:?}")
                    })?
            }
            "--sibling" => parsed.siblings.push(PathBuf::from(value)),
            "--cwd" => parsed.cwd = Some(PathBuf::from(value)),
            "--secret-file" => parsed.secret_file = Some(PathBuf::from(value)),
            "--return-path" => parsed.return_paths.push(value),
            "--return-max-bytes" => {
                parsed.return_max_bytes = Some(value.parse().map_err(|_| {
                    format!("--return-max-bytes needs a nonnegative integer, got {value:?}")
                })?)
            }
            _ => {
                let (key, val) = value
                    .split_once('=')
                    .filter(|(key, _)| !key.is_empty())
                    .ok_or_else(|| format!("--env needs NAME=VALUE, got {value:?}"))?;
                parsed.env.insert(key.into(), val.into());
            }
        }
    }
    Err("missing -- before the command".into())
}

pub const ATTACH_HELP: &str = "\
usage: ck motor attach JOB_ID [--secret-file PATH] [--status]

Reads the job's secret from one line of stdin, or from --secret-file PATH.
Secrets are never accepted as arguments or from environment variables.
Replays output from the start, then streams until the job ends. --status prints
the current state and outcome as JSON without streaming (exit 0 while running).
The terminal exit statuses and interrupt behavior are the same as `ck motor run`.";

#[derive(Debug, PartialEq, Eq)]
pub struct AttachArgs {
    pub job_id: Uuid,
    pub secret_file: Option<PathBuf>,
    pub status: bool,
}

pub fn parse_attach_args(args: &[String]) -> Result<Option<AttachArgs>, String> {
    if args.len() == 1 && matches!(args[0].as_str(), "--help" | "-h") {
        return Ok(None);
    }
    let mut parsed = AttachArgs {
        job_id: args
            .first()
            .and_then(|id| id.parse().ok())
            .ok_or("attach needs a job ID")?,
        secret_file: None,
        status: false,
    };
    let mut words = args[1..].iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "--status" if !parsed.status => parsed.status = true,
            "--secret-file" if parsed.secret_file.is_none() => {
                parsed.secret_file = Some(words.next().ok_or("--secret-file needs a path")?.into());
            }
            _ if word.starts_with("--secret-file=") && parsed.secret_file.is_none() => {
                parsed.secret_file = Some(word[14..].into());
            }
            // Do not echo an unrecognized word: it might be a misplaced secret.
            _ => return Err("attach accepts only JOB_ID, --secret-file PATH and --status".into()),
        }
    }
    Ok(Some(parsed))
}

pub struct SecretFile {
    file: fs::File,
    path: PathBuf,
}

impl SecretFile {
    pub fn create(path: &Path) -> std::io::Result<Self> {
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        Ok(Self {
            file,
            path: path.into(),
        })
    }

    fn save(&mut self, secret: &str) -> std::io::Result<()> {
        writeln!(self.file, "{secret}")?;
        self.file.sync_all()
    }
}

fn read_secret(reader: impl Read) -> String {
    let mut bytes = Vec::new();
    let mut reader = std::io::BufReader::new(reader.take(67));
    if reader.read_until(b'\n', &mut bytes).is_err() {
        return String::new();
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    String::from_utf8(bytes).unwrap_or_default()
}

/// The command line the runner hands to `bash -c`: the words joined by single
/// spaces, exactly as AFT sends a command. No word is quoted or escaped.
pub fn command_line(words: &[String]) -> String {
    words.join(" ")
}

/// Build the `exec.run` request. The workspace is the git checkout that
/// contains the working directory and the repository root is that checkout's
/// main checkout, as AFT keys them; ck-motor validates both again.
pub fn build_request(args: &RunArgs) -> Result<RunRequest, String> {
    let cwd = match &args.cwd {
        Some(cwd) => cwd.clone(),
        None => std::env::current_dir().map_err(|e| format!("current directory: {e}"))?,
    };
    let cwd = canonical(&cwd, "--cwd")?;
    let git_path = |path: &Path, flag: &str| {
        crate::snapshot::git(path, &["rev-parse", "--path-format=absolute", flag])
            .map(PathBuf::from)
            .map_err(|e| {
                format!(
                    "{} is not in a git checkout: {}",
                    path.display(),
                    e.to_string().trim()
                )
            })
    };
    let workspace = canonical(&git_path(&cwd, "--show-toplevel")?, "workspace")?;
    let common = canonical(&git_path(&workspace, "--git-common-dir")?, "git common dir")?;
    let repository_root = common
        .parent()
        .ok_or_else(|| format!("{} has no main checkout", common.display()))?
        .to_path_buf();
    let mut request = RunRequest::new(
        text(&workspace)?,
        text(&repository_root)?,
        text(&cwd)?,
        command_line(&args.command),
    )
    .with_env(args.env.clone())
    .with_timeout(args.timeout)
    .with_siblings(
        args.siblings
            .iter()
            .map(|path| canonical(path, "--sibling").and_then(|path| text(&path)))
            .collect::<Result<_, _>>()?,
    );
    if let Some(vcpus) = args.vcpus {
        request = request.with_weight_hint(vcpus);
    }
    Ok(request)
}

fn canonical(path: &Path, what: &str) -> Result<PathBuf, String> {
    fs::canonicalize(path).map_err(|e| format!("{what} {}: {e}", path.display()))
}

fn text(path: &Path) -> Result<String, String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{} is not UTF-8", path.display()))
}

/// Why a submitted stream ended before its terminal record arrived.
#[derive(Debug)]
pub struct StreamFailure {
    /// The module's error code, when ck-motor answered with an error.
    pub code: Option<String>,
    pub message: String,
}

/// Why a request could not be submitted, or might have been.
#[derive(Debug)]
pub enum SubmitError {
    /// Nothing reached ck-motor, or ck-motor refused it before creating a job.
    NotSent(String),
    /// The request may have reached ck-motor; its fate is unknown.
    Unknown(String),
}

/// The calls `ck motor run` makes on its ck-motor route. The production one
/// is [`SubcClient`]; tests drive the same [`drive`] loop over another.
// async_trait marks each boxed future #[must_use] on top of the future's own.
#[allow(clippy::double_must_use)]
#[async_trait]
pub trait ExecClient: Send + Sync {
    /// Start `exec.run`. Records arrive as the serialized `StreamRecord`
    /// bodies ck-motor emits.
    async fn run(&self, request: &RunRequest) -> Result<Box<dyn RecordStream>, SubmitError>;
    async fn attach(
        &self,
        job_id: Uuid,
        secret: &str,
    ) -> Result<Box<dyn RecordStream>, SubmitError>;
    async fn status(&self, job_id: Uuid, secret: &str) -> Result<JobStatusReply, SubmitError>;
    /// Send `exec.cancel`. Its reply is only an acknowledgement; the job's
    /// terminal still arrives on its run stream.
    async fn cancel(&self, job_id: Uuid, secret: Option<&str>) -> Result<(), String>;
}

// async_trait marks each boxed future #[must_use] on top of the future's own.
#[allow(clippy::double_must_use)]
#[async_trait]
pub trait RecordStream: Send {
    /// The next record body; `Ok(None)` when the stream ended cleanly.
    async fn next(&mut self) -> Result<Option<Vec<u8>>, StreamFailure>;
}

/// ck-motor answers these before it creates a job, so seeing one before any
/// record proves nothing ran.
const REFUSED_BEFORE_JOB: &[&str] = &[
    "motor_route_not_admitted",
    "motor_op_not_permitted_for_principal",
];

/// Run one job to its end: write its output to `out`/`err` as it arrives and
/// return the exit status described in [`HELP`]. Each message on `interrupts`
/// is a Ctrl-C or SIGTERM: the first cancels the job (as soon as its ID is
/// known) and keeps waiting for the terminal, the second stops waiting.
pub async fn drive<C: ExecClient + ?Sized>(
    client: &C,
    request: &RunRequest,
    out: &mut dyn Write,
    err: &mut dyn Write,
    interrupts: &mut mpsc::Receiver<()>,
) -> u8 {
    drive_run(client, request, None, out, err, interrupts).await
}

pub async fn drive_run<C: ExecClient + ?Sized>(
    client: &C,
    request: &RunRequest,
    secret_file: Option<SecretFile>,
    out: &mut dyn Write,
    err: &mut dyn Write,
    interrupts: &mut mpsc::Receiver<()>,
) -> u8 {
    let mut stream = match client.run(request).await {
        Ok(stream) => stream,
        Err(SubmitError::NotSent(message)) => {
            let _ = writeln!(err, "ck motor run: refused before start: {message}");
            return EXIT_REFUSED;
        }
        Err(SubmitError::Unknown(message)) => {
            let _ = writeln!(
                err,
                "ck motor run: outcome unknown: {message}; the job ID was never received"
            );
            return EXIT_UNKNOWN;
        }
    };
    consume(
        client,
        stream.as_mut(),
        Session {
            job_id: None,
            secret: None,
            secret_file,
        },
        out,
        err,
        interrupts,
    )
    .await
}

struct Session {
    job_id: Option<Uuid>,
    secret: Option<String>,
    secret_file: Option<SecretFile>,
}

pub async fn drive_attach<C: ExecClient + ?Sized>(
    client: &C,
    args: &AttachArgs,
    secret: &str,
    out: &mut dyn Write,
    err: &mut dyn Write,
    interrupts: &mut mpsc::Receiver<()>,
) -> u8 {
    if args.status {
        return match client.status(args.job_id, secret).await {
            Ok(status) => {
                let _ = writeln!(
                    out,
                    "{}",
                    serde_json::to_string(&status).expect("serializable status")
                );
                status.terminal.as_ref().map_or(0, exit_code)
            }
            Err(failure) => attach_failure(failure, err),
        };
    }
    let mut stream = match client.attach(args.job_id, secret).await {
        Ok(stream) => stream,
        Err(failure) => return attach_failure(failure, err),
    };
    consume(
        client,
        stream.as_mut(),
        Session {
            job_id: Some(args.job_id),
            secret: Some(secret.into()),
            secret_file: None,
        },
        out,
        err,
        interrupts,
    )
    .await
}

fn attach_failure(failure: SubmitError, err: &mut dyn Write) -> u8 {
    let (message, code) = match failure {
        SubmitError::NotSent(message) => (message, EXIT_REFUSED),
        SubmitError::Unknown(message) => (message, EXIT_UNKNOWN),
    };
    let _ = writeln!(err, "ck motor attach: {message}");
    code
}

async fn consume<C: ExecClient + ?Sized>(
    client: &C,
    stream: &mut dyn RecordStream,
    mut session: Session,
    out: &mut dyn Write,
    err: &mut dyn Write,
    interrupts: &mut mpsc::Receiver<()>,
) -> u8 {
    let name = if session.job_id.is_some() {
        "attach"
    } else {
        "run"
    };
    let mut job_id = session.job_id;
    let mut received = false;
    let mut cancel_wanted = false;
    let mut cancel_sent = false;
    loop {
        let next = tokio::select! {
            next = stream.next() => next,
            Some(()) = interrupts.recv() => {
                if cancel_wanted {
                    let _ = writeln!(err, "ck motor {name}: stopped waiting; a cancel was requested. {}", job_hint(job_id));
                    return EXIT_UNKNOWN;
                }
                cancel_wanted = true;
                let _ = writeln!(err, "ck motor {name}: cancelling; waiting for the job to stop (interrupt again to stop waiting)");
                if let Some(id) = job_id {
                    cancel_sent = true;
                    send_cancel(client, id, session.secret.as_deref(), name, err).await;
                }
                continue;
            }
        };
        let bytes = match next {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                let _ = writeln!(
                    err,
                    "ck motor {name}: outcome unknown: the stream ended without an outcome. {}",
                    job_hint(job_id)
                );
                return EXIT_UNKNOWN;
            }
            Err(failure) => {
                if !received && failure.code.as_deref() == Some("motor_job_not_found") {
                    return attach_failure(SubmitError::NotSent(failure.message), err);
                }
                if !received
                    && failure
                        .code
                        .as_deref()
                        .is_some_and(|code| REFUSED_BEFORE_JOB.contains(&code))
                {
                    let _ = writeln!(
                        err,
                        "ck motor {name}: refused before start: {}",
                        failure.message
                    );
                    return EXIT_REFUSED;
                }
                let _ = writeln!(
                    err,
                    "ck motor {name}: outcome unknown: {}. {}",
                    failure.message,
                    job_hint(job_id)
                );
                return EXIT_UNKNOWN;
            }
        };
        received = true;
        let mut value: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(_) => {
                let _ = writeln!(err, "ck motor: skipped an unreadable record");
                continue;
            }
        };
        if let Some(direct_job) = value
            .as_object_mut()
            .and_then(|record| record.remove("direct_job"))
        {
            if session.secret.is_none() {
                if let (Some(id), Some(secret)) = (
                    direct_job["job_id"].as_str().and_then(|id| id.parse().ok()),
                    direct_job["secret"].as_str(),
                ) {
                    job_id = Some(id);
                    session.secret = Some(secret.into());
                    if let Some(file) = session.secret_file.as_mut() {
                        if file.save(secret).is_err() {
                            let _ = writeln!(
                                err,
                                "ck motor run: could not save the reattach secret for job_id={id}"
                            );
                            return EXIT_UNKNOWN;
                        }
                        let _ = writeln!(
                            err,
                            "ck motor run: job_id={id} secret-file={}",
                            file.path.display()
                        );
                    } else {
                        let _ = writeln!(err, "ck motor run: job_id={id} secret={secret}");
                    }
                }
            }
        }
        let hosted = value
            .get("hosted")
            .cloned()
            .and_then(|v| serde_json::from_value::<crate::HostedJobState>(v).ok());
        let record: StreamRecord = match serde_json::from_value(value) {
            Ok(record) => record,
            Err(error) => {
                let _ = writeln!(
                    err,
                    "ck motor {name}: skipped an unreadable record: {error}"
                );
                continue;
            }
        };
        match record {
            StreamRecord::Accepted(accepted) => {
                if let Some(state) = &hosted {
                    let placement = state
                        .placement
                        .as_ref()
                        .map(|p| p.host.as_str())
                        .unwrap_or("unknown");
                    let _ = writeln!(
                        err,
                        "ck motor {name}: placement: first_party/{placement}, max runtime: {} s",
                        state.max_runtime_s.unwrap_or_default()
                    );
                }
                job_id = Some(accepted.job_id);
                if let Some(names) = accepted.env_not_forwarded.filter(|n| !n.is_empty()) {
                    let _ = writeln!(
                        err,
                        "ck motor {name}: not forwarded to the job: {}",
                        names.join(", ")
                    );
                }
                if cancel_wanted && !cancel_sent {
                    cancel_sent = true;
                    send_cancel(
                        client,
                        accepted.job_id,
                        session.secret.as_deref(),
                        name,
                        err,
                    )
                    .await;
                }
            }
            StreamRecord::Output(output) => {
                let bytes = &output.bytes.0;
                let _ = match output.stream {
                    OutputStream::Stderr => err.write_all(bytes).and_then(|()| err.flush()),
                    _ => out.write_all(bytes).and_then(|()| out.flush()),
                };
            }
            StreamRecord::Terminal(terminal) => {
                if let Some(state) = &hosted {
                    let _ = writeln!(
                        err,
                        "ck motor {name}: job_id={} {}",
                        terminal.job_id,
                        state.billed_line()
                    );
                }
                return settle_named(&terminal, name, err);
            }
            _ => {}
        }
    }
}

async fn send_cancel<C: ExecClient + ?Sized>(
    client: &C,
    id: Uuid,
    secret: Option<&str>,
    name: &str,
    err: &mut dyn Write,
) {
    if let Err(error) = client.cancel(id, secret).await {
        let _ = writeln!(
            err,
            "ck motor {name}: cancel request for job_id={id} failed: {error}"
        );
    }
}

fn job_hint(job_id: Option<Uuid>) -> String {
    match job_id {
        Some(id) => format!(
            "job_id={id}; the job may still be running until it ends or reaches its timeout"
        ),
        None => "the job ID was never received".into(),
    }
}

/// The exit status for a terminal record, after reporting any outcome that is
/// not the command's own exit to `err`.
pub fn settle(terminal: &TerminalRecord, err: &mut dyn Write) -> u8 {
    settle_named(terminal, "run", err)
}

fn settle_named(terminal: &TerminalRecord, name: &str, err: &mut dyn Write) -> u8 {
    let id = terminal.job_id;
    let message = match (&terminal.killed, &terminal.outcome) {
        (_, Outcome::RefusedBeforeStart { reason }) => {
            let reason: String = reason.clone().into();
            let detail = terminal
                .refusal_detail
                .as_deref()
                .map(|detail| format!(": {detail}"))
                .unwrap_or_default();
            let retry = terminal
                .retry_after_ms
                .map(|ms| format!(" (retry after {ms} ms)"))
                .unwrap_or_default();
            Some(format!(
                "refused before start: {reason}{detail}{retry} (job_id={id})"
            ))
        }
        (Some(Killed::Deadline), _) => {
            Some(format!("job_id={id} reached its timeout and was stopped"))
        }
        (Some(Killed::Cancel), _) | (_, Outcome::Cancelled) => {
            Some(format!("job_id={id} was cancelled"))
        }
        (_, Outcome::Exit { .. } | Outcome::WindowsExit { .. } | Outcome::Signal { .. }) => None,
        (_, outcome) => Some(format!("outcome unknown ({outcome:?}) for job_id={id}")),
    };
    if let Some(message) = message {
        let _ = writeln!(err, "ck motor {name}: {message}");
    }
    exit_code(terminal)
}

/// The documented exit status for a terminal record.
pub fn exit_code(terminal: &TerminalRecord) -> u8 {
    // A runner kill is reported with whatever status the killed process left,
    // so the kill reason decides before the outcome does.
    match terminal.killed {
        Some(Killed::Deadline) => return EXIT_TIMEOUT,
        Some(Killed::Cancel) => return EXIT_CANCELLED,
        _ => {}
    }
    match &terminal.outcome {
        Outcome::Exit { code } => u8::try_from(*code).unwrap_or(1),
        Outcome::WindowsExit { code } => u8::try_from(*code).unwrap_or(1),
        Outcome::Signal { signal } => u8::try_from(128 + i64::from(*signal)).unwrap_or(255),
        Outcome::Cancelled => EXIT_CANCELLED,
        Outcome::RefusedBeforeStart { .. } => EXIT_REFUSED,
        _ => EXIT_UNKNOWN,
    }
}

/// The ck-motor route over the subc daemon, opened the same way AFT (the agent
/// tool that runs `runon` commands) opens its own: ask the daemon which module
/// provides the `exec-remote/v1` capability, which is ck-motor, and open a
/// route to it. This process presents no module identity, so the daemon
/// attests it as `Principal::Direct`.
pub struct SubcClient {
    consumer: SubcConsumer,
    route: RouteHandle,
    module_id: String,
    returns: crate::ReturnOptions,
    allow_spill: bool,
}

impl SubcClient {
    pub async fn connect(workspace: &Path) -> Result<Self, String> {
        Self::connect_classified(workspace)
            .await
            .map_err(|failure| match failure {
                SubmitError::NotSent(message) | SubmitError::Unknown(message) => message,
            })
    }

    async fn connect_classified(workspace: &Path) -> Result<Self, SubmitError> {
        let consumer = SubcConsumer::connect_default(ConsumerOptions::default())
            .await
            .map_err(|e| SubmitError::Unknown(format!("connect to the subc daemon: {e}")))?;
        let module_id = consumer
            .resolve_provider(crate::module::CAPABILITY)
            .await
            .map_err(connection_error)?;
        let identity = BindIdentity::new(
            workspace,
            "ck-motor-run",
            format!("ck-motor-run-{}", Uuid::now_v7()),
        );
        let route = consumer
            .open_route(
                RouteTarget::ManagementSurface {
                    module_id: module_id.clone(),
                },
                identity,
                CallOptions::default(),
            )
            .await
            .map_err(connection_error)?;
        Ok(Self {
            consumer,
            route,
            module_id,
            returns: crate::ReturnOptions::default(),
            allow_spill: false,
        })
    }

    async fn subscribe(
        &self,
        op: &str,
        params: impl serde::Serialize,
        secret: Option<&str>,
    ) -> Result<Box<dyn RecordStream>, SubmitError> {
        let mut subscription = self
            .consumer
            .subscribe_route(
                &self.route,
                direct_envelope(op, params, secret),
                SubscribeOptions::default(),
            )
            .await
            .map_err(|error| submit_error(error, &self.module_id))?;
        // Drain the SDK's bounded channel even when the terminal's stdout is slow.
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(bytes) = subscription.events().recv().await {
                if tx.send(Ok(bytes)).is_err() {
                    return;
                }
            }
            if let Err(error) = subscription.closed().await {
                let _ = tx.send(Err(StreamFailure {
                    code: error.code().map(str::to_owned),
                    message: error.to_string(),
                }));
            }
        });
        Ok(Box::new(QueuedStream(rx)))
    }
}

fn direct_envelope(op: &str, params: impl serde::Serialize, secret: Option<&str>) -> Vec<u8> {
    let mut value = json!({"op":op,"params":params});
    if let Some(secret) = secret {
        value["direct_secret"] = json!(secret);
    }
    serde_json::to_vec(&value).expect("serializable request")
}

fn submit_error(error: CallError, module_id: &str) -> SubmitError {
    match error {
        CallError::NotSent(_) | CallError::StaleRouteHandle(_) => {
            SubmitError::NotSent(error.to_string())
        }
        _ if error.code() == Some("motor_job_not_found") => {
            SubmitError::NotSent(format!("{module_id} has no such job for this caller"))
        }
        _ => SubmitError::Unknown(error.to_string()),
    }
}

fn connection_error(error: CallError) -> SubmitError {
    if error.code().is_some() {
        SubmitError::NotSent(error.to_string())
    } else {
        SubmitError::Unknown(error.to_string())
    }
}

#[async_trait]
impl ExecClient for SubcClient {
    async fn run(&self, request: &RunRequest) -> Result<Box<dyn RecordStream>, SubmitError> {
        let mut params = self.returns.params(request);
        if self.allow_spill {
            params["allow_spill"] = json!(true);
        }
        self.subscribe("exec.run", params, None).await
    }

    async fn attach(
        &self,
        job_id: Uuid,
        secret: &str,
    ) -> Result<Box<dyn RecordStream>, SubmitError> {
        self.subscribe("exec.attach", AttachRequest::new(job_id, 0), Some(secret))
            .await
    }

    async fn status(&self, job_id: Uuid, secret: &str) -> Result<JobStatusReply, SubmitError> {
        let bytes = self
            .consumer
            .request(
                &self.route,
                direct_envelope("exec.status", json!({"job_id":job_id}), Some(secret)),
                CallOptions::default(),
            )
            .await
            .map_err(|error| submit_error(error, &self.module_id))?;
        serde_json::from_slice(&bytes)
            .map_err(|_| SubmitError::Unknown("unreadable job status".into()))
    }

    async fn cancel(&self, job_id: Uuid, secret: Option<&str>) -> Result<(), String> {
        self.consumer
            .request(
                &self.route,
                direct_envelope("exec.cancel", CancelRequest::new(job_id), secret),
                CallOptions::default(),
            )
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

pub(crate) struct QueuedStream(pub(crate) mpsc::UnboundedReceiver<Result<Vec<u8>, StreamFailure>>);

#[async_trait]
impl RecordStream for QueuedStream {
    async fn next(&mut self) -> Result<Option<Vec<u8>>, StreamFailure> {
        self.0.recv().await.transpose()
    }
}

/// `ck-motor run ...`: parse, connect, run, and return the exit status.
pub async fn main(args: &[String]) -> u8 {
    let args = match parse_args(args) {
        Ok(Invocation::Help) => {
            println!("{HELP}");
            return 0;
        }
        Ok(Invocation::Run(args)) => args,
        Err(message) => {
            eprintln!("ck motor run: {message}\n\n{HELP}");
            return EXIT_USAGE;
        }
    };
    // ck-motor enforces its configured cap; this local check uses the default
    // cap so an over-long timeout fails before anything is snapshotted.
    if let Some(message) =
        direct_timeout_refusal(Some(args.timeout), DEFAULT_DIRECT_EXEC_MAX_TIMEOUT_SECS)
    {
        eprintln!("ck motor run: refused before start: {message}");
        return EXIT_REFUSED;
    }
    let request = match build_request(&args) {
        Ok(request) => request,
        Err(message) => {
            eprintln!("ck motor run: refused before start: {message}");
            return EXIT_REFUSED;
        }
    };
    let secret_file = match args
        .secret_file
        .as_deref()
        .map(SecretFile::create)
        .transpose()
    {
        Ok(file) => file,
        Err(error) => {
            eprintln!("ck motor run: cannot create secret file: {error}");
            return EXIT_USAGE;
        }
    };
    let mut interrupts = match interrupt_signals() {
        Ok(interrupts) => interrupts,
        Err(error) => {
            eprintln!("ck motor run: refused before start: signal handlers: {error}");
            return EXIT_REFUSED;
        }
    };
    let client = tokio::select! {
        client = SubcClient::connect(Path::new(&request.workspace_key)) => client,
        _ = interrupts.recv() => {
            eprintln!("ck motor run: interrupted before submission; nothing ran");
            return EXIT_CANCELLED;
        }
    };
    let mut client = match client {
        Ok(client) => client,
        Err(message) => {
            eprintln!("ck motor run: refused before start: {message}");
            return EXIT_REFUSED;
        }
    };
    client.returns = crate::ReturnOptions {
        return_paths: args.return_paths.clone(),
        return_max_bytes: args
            .return_max_bytes
            .unwrap_or(motor_protocol::RETURN_MAX_BYTES),
    };
    client.allow_spill = args.allow_spill;
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    drive_run(
        &client,
        &request,
        secret_file,
        &mut out,
        &mut err,
        &mut interrupts,
    )
    .await
}

/// Ask the running module to mint a vault token, without enabling hosted jobs.
pub async fn hosted_check_main(args: &[String]) -> u8 {
    if !args.is_empty() {
        eprintln!("usage: ck motor hosted-check");
        return EXIT_USAGE;
    }
    let workspace = match std::env::current_dir() {
        Ok(path) => path,
        Err(_) => return EXIT_REFUSED,
    };
    let client = match SubcClient::connect(&workspace).await {
        Ok(client) => client,
        Err(_) => {
            eprintln!("hosted-check: module unavailable");
            return EXIT_REFUSED;
        }
    };
    let result = client
        .consumer
        .request(
            &client.route,
            direct_envelope("hosted.check", json!({}), None),
            CallOptions::default(),
        )
        .await;
    match result
        .ok()
        .and_then(|bytes| serde_json::from_slice::<crate::hosted::check::Check>(&bytes).ok())
    {
        Some(check) => {
            let ok = check.status == "ok";
            println!(
                "{}",
                serde_json::to_string(&check).expect("serializable hosted check")
            );
            if ok {
                0
            } else {
                EXIT_REFUSED
            }
        }
        None => {
            eprintln!("hosted-check: module refused or unavailable");
            EXIT_REFUSED
        }
    }
}

pub async fn attach_main(args: &[String]) -> u8 {
    let args = match parse_attach_args(args) {
        Ok(Some(args)) => args,
        Ok(None) => {
            println!("{ATTACH_HELP}");
            return 0;
        }
        Err(message) => {
            eprintln!("ck motor attach: {message}\n\n{ATTACH_HELP}");
            return EXIT_USAGE;
        }
    };
    let secret = match &args.secret_file {
        Some(path) => fs::File::open(path).map(read_secret).unwrap_or_default(),
        None => read_secret(std::io::stdin().lock()),
    };
    let workspace = match std::env::current_dir() {
        Ok(path) => path,
        Err(_) => {
            eprintln!("ck motor attach: current directory unavailable");
            return EXIT_USAGE;
        }
    };
    let mut interrupts = match interrupt_signals() {
        Ok(interrupts) => interrupts,
        Err(_) => {
            eprintln!("ck motor attach: signal handlers unavailable");
            return EXIT_UNKNOWN;
        }
    };
    let client = match SubcClient::connect_classified(&workspace).await {
        Ok(client) => client,
        Err(failure) => return attach_failure(failure, &mut std::io::stderr()),
    };
    drive_attach(
        &client,
        &args,
        &secret,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
        &mut interrupts,
    )
    .await
}

/// One message per SIGINT or SIGTERM. Installing the handlers replaces the
/// default termination, so a signal cancels the job instead of abandoning it.
fn interrupt_signals() -> std::io::Result<mpsc::Receiver<()>> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let (tx, rx) = mpsc::channel(4);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(()) = interrupt.recv() => {}
                Some(()) = terminate.recv() => {}
                else => return,
            }
            if tx.send(()).await.is_err() {
                return;
            }
        }
    });
    Ok(rx)
}

#[cfg(test)]
mod tests {
    #[test]
    fn secret_file_is_private_new_and_never_overwrites() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = cortexkit_test_support::ScratchDir::new("motor-cli-secret");
        let path = scratch.join("secret");
        let mut file = super::SecretFile::create(&path).unwrap();
        file.save(&"a".repeat(64)).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            super::SecretFile::create(&path).err().unwrap().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{}\n", "a".repeat(64))
        );
        let link = scratch.join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(super::SecretFile::create(&link).is_err());
    }

    #[test]
    fn attach_never_accepts_an_argv_secret_or_echoes_it() {
        let id = motor_protocol::Uuid::now_v7().to_string();
        let secret = "a".repeat(64);
        for args in [
            vec![id.clone(), secret.clone()],
            vec![id.clone(), "--secret".into(), secret.clone()],
            vec![id.clone(), format!("--secret={secret}")],
        ] {
            let error = super::parse_attach_args(&args).unwrap_err();
            assert!(!error.contains(&secret));
        }
        let args = vec![id, "--status".into(), "--secret-file=key".into()];
        let args = super::parse_attach_args(&args).unwrap().unwrap();
        assert!(args.status);
        assert_eq!(args.secret_file.unwrap(), std::path::Path::new("key"));
        assert_eq!(super::read_secret(format!("{secret}\n").as_bytes()), secret);
        assert_eq!(super::read_secret(&b""[..]), "");
    }
    #[test]
    fn run_accepts_secret_file_without_putting_secret_in_arguments() {
        let words: Vec<String> = ["--secret-file", "job.key", "--", "true"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert!(super::parse_args(&words).is_ok());
    }
    use super::*;
    use motor_protocol::RefusalReason;

    fn words(line: &str) -> Vec<String> {
        line.split(' ').map(str::to_owned).collect()
    }

    #[test]
    fn arguments_map_onto_the_run_request_fields() {
        let Ok(Invocation::Run(args)) = parse_args(&words(
            "--vcpus 4 --sibling /a --sibling=/b --env CI=1 --env RUSTFLAGS=-D=warnings --timeout=90 --cwd /w -- cargo test -p x",
        )) else {
            panic!("expected a run")
        };
        assert_eq!(
            args,
            RunArgs {
                vcpus: Some(4),
                siblings: vec!["/a".into(), "/b".into()],
                env: [("CI", "1"), ("RUSTFLAGS", "-D=warnings")]
                    .into_iter()
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect(),
                timeout: 90,
                cwd: Some("/w".into()),
                command: words("cargo test -p x"),
                secret_file: None,
                return_paths: Vec::new(),
                return_max_bytes: None,
                allow_spill: false,
            }
        );
        assert_eq!(command_line(&args.command), "cargo test -p x");
    }

    #[test]
    fn defaults_send_a_timeout_and_leave_vcpus_to_the_runner() {
        let Ok(Invocation::Run(args)) = parse_args(&words("-- true")) else {
            panic!("expected a run")
        };
        assert_eq!(args.timeout, DEFAULT_TIMEOUT_SECS);
        assert_eq!(args.vcpus, None);
        assert!(!args.allow_spill);
        let Ok(Invocation::Run(args)) = parse_args(&words("--allow-spill -- true")) else {
            panic!("expected a run")
        };
        assert!(args.allow_spill);
        assert!(args.env.is_empty() && args.siblings.is_empty());
        // Everything after -- belongs to the command, options included.
        let Ok(Invocation::Run(args)) = parse_args(&words("-- ls --help")) else {
            panic!("expected a run")
        };
        assert_eq!(command_line(&args.command), "ls --help");
    }

    #[test]
    fn return_paths_cli_options_are_repeatable_and_merge_into_direct_request() {
        let Ok(Invocation::Run(args)) = parse_args(&words(
            "--return-path /w/one --return-path=/w/two --return-max-bytes 1 -- true",
        )) else {
            panic!("expected run")
        };
        assert_eq!(args.return_paths, vec!["/w/one", "/w/two"]);
        assert_eq!(args.return_max_bytes, Some(1));
        let request = RunRequest::new("/w", "/w", "/w", "true");
        let params = crate::ReturnOptions {
            return_paths: args.return_paths,
            return_max_bytes: args.return_max_bytes.unwrap(),
        }
        .params(&request);
        assert_eq!(params["return_paths"], json!(["/w/one", "/w/two"]));
        assert_eq!(params["return_max_bytes"], 1);
        assert_eq!(params["command"], "true");
        let empty = crate::ReturnOptions::default().params(&request);
        assert!(empty.get("return_paths").is_none());
        assert!(empty.get("return_max_bytes").is_none());
        assert!(parse_args(&words("--return-max-bytes -1 -- true")).is_err());
    }

    #[test]
    fn malformed_arguments_are_usage_errors() {
        for line in [
            "true",
            "--vcpus 0 -- true",
            "--vcpus x -- true",
            "--timeout 0 -- true",
            "--env NOEQUALS -- true",
            "--env =v -- true",
            "--bogus -- true",
            "--",
            "--timeout",
        ] {
            assert!(parse_args(&words(line)).is_err(), "{line:?}");
        }
        assert_eq!(parse_args(&words("--help")), Ok(Invocation::Help));
    }

    #[test]
    fn the_local_timeout_check_uses_the_modules_message() {
        assert_eq!(
            direct_timeout_refusal(
                Some(DEFAULT_TIMEOUT_SECS),
                DEFAULT_DIRECT_EXEC_MAX_TIMEOUT_SECS
            ),
            None
        );
        let message = direct_timeout_refusal(Some(7201), DEFAULT_DIRECT_EXEC_MAX_TIMEOUT_SECS)
            .expect("over the cap");
        assert!(
            message.starts_with("timeout 7201 seconds exceeds the direct-job cap of 7200 seconds")
        );
    }

    #[test]
    fn exit_codes_follow_the_documented_table() {
        let id = Uuid::now_v7();
        let terminal = |outcome| TerminalRecord::new(id, outcome, 0, 0, 0);
        let killed = |outcome, how| {
            let mut t = terminal(outcome);
            t.killed = Some(how);
            t
        };
        for (record, code) in [
            (terminal(Outcome::Exit { code: 0 }), 0),
            (terminal(Outcome::Exit { code: 3 }), 3),
            (terminal(Outcome::Signal { signal: 9 }), 137),
            (
                killed(Outcome::Signal { signal: 15 }, Killed::Deadline),
                EXIT_TIMEOUT,
            ),
            (
                killed(Outcome::Exit { code: 0 }, Killed::Deadline),
                EXIT_TIMEOUT,
            ),
            (
                killed(Outcome::Signal { signal: 15 }, Killed::Cancel),
                EXIT_CANCELLED,
            ),
            (terminal(Outcome::Cancelled), EXIT_CANCELLED),
            (
                terminal(Outcome::RefusedBeforeStart {
                    reason: RefusalReason::RunnerFull,
                }),
                EXIT_REFUSED,
            ),
            (terminal(Outcome::OutcomeUnknown), EXIT_UNKNOWN),
            (terminal(Outcome::HistoryExpired), EXIT_UNKNOWN),
        ] {
            assert_eq!(exit_code(&record), code, "{record:?}");
        }
        let codes = [
            EXIT_REFUSED,
            EXIT_UNKNOWN,
            EXIT_TIMEOUT,
            EXIT_CANCELLED,
            EXIT_USAGE,
        ];
        // Distinct, nonzero, and above every 128+signal status.
        assert!(codes.iter().all(|code| *code > 128 + 64));
        assert_eq!(
            codes
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            codes.len()
        );
    }

    #[test]
    fn the_command_exiting_with_a_reserved_code_is_not_reported_as_one() {
        let mut err = Vec::new();
        let terminal = TerminalRecord::new(
            Uuid::now_v7(),
            Outcome::Exit {
                code: EXIT_TIMEOUT.into(),
            },
            0,
            0,
            0,
        );
        assert_eq!(settle(&terminal, &mut err), EXIT_TIMEOUT);
        assert!(err.is_empty(), "{}", String::from_utf8_lossy(&err));
    }

    #[test]
    fn the_workspace_is_the_checkout_containing_cwd() {
        let scratch = cortexkit_test_support::ScratchDir::new("cli-workspace");
        let main = fs::canonicalize(scratch.path()).unwrap().join("main");
        let nested = main.join("crates/x");
        fs::create_dir_all(&nested).unwrap();
        crate::snapshot::git(&main, &["init", "--quiet"]).unwrap();
        let args = RunArgs {
            cwd: Some(nested.clone()),
            timeout: 60,
            command: words("echo hi"),
            ..RunArgs::default()
        };
        let request = build_request(&args).unwrap();
        assert_eq!(request.workspace_key, main.to_str().unwrap());
        assert_eq!(request.repository_root, main.to_str().unwrap());
        assert_eq!(request.cwd, nested.to_str().unwrap());
        assert_eq!(request.command, "echo hi");
        assert_eq!(request.timeout, Some(60));
        assert!(request.env.is_empty(), "nothing is inherited");
        // A linked worktree is its own workspace, and its repository root is
        // the main checkout: ck-motor refuses a run whose repository root is
        // not the checkout that owns the workspace's git common directory.
        let git = |args: &[&str]| crate::snapshot::git(&main, args).unwrap();
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "initial",
        ]);
        let linked = main.parent().unwrap().join("linked");
        git(&[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "task",
            linked.to_str().unwrap(),
        ]);
        let request = build_request(&RunArgs {
            cwd: Some(linked.clone()),
            ..RunArgs::default()
        })
        .unwrap();
        assert_eq!(request.workspace_key, linked.to_str().unwrap());
        assert_eq!(request.repository_root, main.to_str().unwrap());
        let outside = RunArgs {
            cwd: Some(fs::canonicalize(scratch.path()).unwrap()),
            ..args
        };
        // The scratch root itself may sit inside some checkout on a developer
        // machine; only assert that the result is not the nested repository.
        if let Ok(request) = build_request(&outside) {
            assert_ne!(request.workspace_key, main.to_str().unwrap());
        }
    }
}

#[cfg(test)]
mod hosted_cli_tests {
    use super::*;
    use std::collections::VecDeque;

    struct Records(VecDeque<Vec<u8>>);
    #[async_trait]
    impl RecordStream for Records {
        async fn next(&mut self) -> Result<Option<Vec<u8>>, StreamFailure> {
            Ok(self.0.pop_front())
        }
    }
    struct UnusedClient;
    #[async_trait]
    impl ExecClient for UnusedClient {
        async fn run(&self, _: &RunRequest) -> Result<Box<dyn RecordStream>, SubmitError> {
            unreachable!()
        }
        async fn attach(&self, _: Uuid, _: &str) -> Result<Box<dyn RecordStream>, SubmitError> {
            unreachable!()
        }
        async fn status(&self, _: Uuid, _: &str) -> Result<JobStatusReply, SubmitError> {
            unreachable!()
        }
        async fn cancel(&self, _: Uuid, _: Option<&str>) -> Result<(), String> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn hosted_cli_completion_prints_billing_and_keeps_real_exit_code() {
        for (billed, billing, expected) in [
            (
                Some(6001),
                crate::BillingStatus::Available,
                "billed: 6.001 vCPU-s",
            ),
            (None, crate::BillingStatus::Pending, "billed: pending"),
            (
                None,
                crate::BillingStatus::Unavailable,
                "billed: unavailable",
            ),
        ] {
            let id = Uuid::now_v7();
            let terminal = TerminalRecord::new(id, Outcome::Exit { code: 23 }, 0, 0, 0);
            let mut record = serde_json::to_value(StreamRecord::Terminal(terminal)).unwrap();
            record["hosted"] = serde_json::to_value(crate::HostedJobState {
                billed_vcpu_ms: billed,
                billing,
                ..crate::HostedJobState::default()
            })
            .unwrap();
            let mut records = Records(VecDeque::from([serde_json::to_vec(&record).unwrap()]));
            let (_, mut interrupts) = mpsc::channel(1);
            let mut out = Vec::new();
            let mut err = Vec::new();
            let code = consume(
                &UnusedClient,
                &mut records,
                Session {
                    job_id: Some(id),
                    secret: None,
                    secret_file: None,
                },
                &mut out,
                &mut err,
                &mut interrupts,
            )
            .await;
            assert_eq!(code, 23);
            assert!(String::from_utf8(err).unwrap().contains(expected));
        }
    }
}
