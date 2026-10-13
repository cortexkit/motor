//! `ck motor run`: submit one remote job through the subc daemon, stream its
//! output and exit with its exit code. It lets a script with no agent behind
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
    CancelRequest, Killed, Outcome, OutputStream, RunRequest, StreamRecord, TerminalRecord, Uuid,
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
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
/// has one, because a caller that loses its connection can't cancel its job.
pub const DEFAULT_TIMEOUT_SECS: u64 = 3600;

/// The line `ck` shows for this domain in its help. `ck` requires exactly one
/// line on stdout and exit status 0 from `ck-motor --ck-domain`.
pub const HEADLINE: &str = "run a command on the remote build server (ck motor run --help)";

/// What `ck motor`, `ck motor --help` and `ck motor help` print.
pub const TOP_HELP: &str = "\
usage: ck motor run [OPTIONS] -- COMMAND...

Runs COMMAND on the remote build server and streams its output here.
See `ck motor run --help` for the options and exit statuses.";

pub const HELP: &str = "\
usage: ck motor run [--vcpus N] [--sibling PATH]... [--env NAME=VALUE]...
                    [--timeout SECONDS] [--cwd PATH] -- COMMAND...

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

The words after -- are joined with single spaces into one command line, which
bash runs on the runner (bash -c). Quote shell syntax as one argument:
  ck motor run -- 'cargo test 2>&1 | tail -n 20'

Exit status: the command's own exit code; 128+N when a signal N killed it;
200 refused before start (nothing ran); 201 outcome unknown or connection lost
(the job ID is printed); 202 timeout; 203 cancelled; 204 bad arguments.

Ctrl-C or SIGTERM cancels the job and waits for its outcome; a second one stops
waiting. If the connection drops, the job keeps running until it ends or its
timeout stops it: a new connection can't attach to it or cancel it. The job ID
is printed so an operator can check it. Keep one connection per job, and run
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
        let (name, inline) = match word.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value.to_string())),
            _ => (word.as_str(), None),
        };
        if !matches!(
            name,
            "--vcpus" | "--sibling" | "--env" | "--timeout" | "--cwd"
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
    /// Send `exec.cancel`. Its reply is only an acknowledgement; the job's
    /// terminal still arrives on its run stream.
    async fn cancel(&self, job_id: Uuid) -> Result<(), String>;
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
    let mut job_id = None;
    let mut received = false;
    let mut cancel_wanted = false;
    let mut cancel_sent = false;
    loop {
        let next = tokio::select! {
            next = stream.next() => next,
            Some(()) = interrupts.recv() => {
                if cancel_wanted {
                    let _ = writeln!(err, "ck motor run: stopped waiting; a cancel was requested. {}", job_hint(job_id));
                    return EXIT_UNKNOWN;
                }
                cancel_wanted = true;
                let _ = writeln!(err, "ck motor run: cancelling; waiting for the job to stop (interrupt again to stop waiting)");
                if let Some(id) = job_id {
                    cancel_sent = true;
                    send_cancel(client, id, err).await;
                }
                continue;
            }
        };
        let bytes = match next {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                let _ = writeln!(
                    err,
                    "ck motor run: outcome unknown: the stream ended without an outcome. {}",
                    job_hint(job_id)
                );
                return EXIT_UNKNOWN;
            }
            Err(failure) => {
                if !received
                    && failure
                        .code
                        .as_deref()
                        .is_some_and(|code| REFUSED_BEFORE_JOB.contains(&code))
                {
                    let _ = writeln!(
                        err,
                        "ck motor run: refused before start: {}",
                        failure.message
                    );
                    return EXIT_REFUSED;
                }
                let _ = writeln!(
                    err,
                    "ck motor run: outcome unknown: {}. {}",
                    failure.message,
                    job_hint(job_id)
                );
                return EXIT_UNKNOWN;
            }
        };
        received = true;
        let record: StreamRecord = match serde_json::from_slice(&bytes) {
            Ok(record) => record,
            Err(error) => {
                let _ = writeln!(err, "ck motor run: skipped an unreadable record: {error}");
                continue;
            }
        };
        match record {
            StreamRecord::Accepted(accepted) => {
                job_id = Some(accepted.job_id);
                if let Some(names) = accepted.env_not_forwarded.filter(|n| !n.is_empty()) {
                    let _ = writeln!(
                        err,
                        "ck motor run: not forwarded to the job: {}",
                        names.join(", ")
                    );
                }
                if cancel_wanted && !cancel_sent {
                    cancel_sent = true;
                    send_cancel(client, accepted.job_id, err).await;
                }
            }
            StreamRecord::Output(output) => {
                let bytes = &output.bytes.0;
                let _ = match output.stream {
                    OutputStream::Stderr => err.write_all(bytes).and_then(|()| err.flush()),
                    _ => out.write_all(bytes).and_then(|()| out.flush()),
                };
            }
            StreamRecord::Terminal(terminal) => return settle(&terminal, err),
            _ => {}
        }
    }
}

async fn send_cancel<C: ExecClient + ?Sized>(client: &C, id: Uuid, err: &mut dyn Write) {
    if let Err(error) = client.cancel(id).await {
        let _ = writeln!(
            err,
            "ck motor run: cancel request for job_id={id} failed: {error}"
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
        let _ = writeln!(err, "ck motor run: {message}");
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
}

impl SubcClient {
    pub async fn connect(workspace: &Path) -> Result<Self, String> {
        let consumer = SubcConsumer::connect_default(ConsumerOptions::default())
            .await
            .map_err(|e| format!("connect to the subc daemon: {e}"))?;
        let module_id = consumer
            .resolve_provider(crate::module::CAPABILITY)
            .await
            .map_err(|e| format!("find ck-motor: {e}"))?;
        let identity = BindIdentity::new(
            workspace,
            "ck-motor-run",
            format!("ck-motor-run-{}", Uuid::now_v7()),
        );
        let route = consumer
            .open_route(
                RouteTarget::ManagementSurface { module_id },
                identity,
                CallOptions::default(),
            )
            .await
            .map_err(|e| format!("open a route to ck-motor: {e}"))?;
        Ok(Self { consumer, route })
    }
}

fn envelope(op: &str, params: impl serde::Serialize) -> Vec<u8> {
    serde_json::to_vec(&json!({ "op": op, "params": params })).expect("serializable request")
}

#[async_trait]
impl ExecClient for SubcClient {
    async fn run(&self, request: &RunRequest) -> Result<Box<dyn RecordStream>, SubmitError> {
        let mut subscription = self
            .consumer
            .subscribe_route(
                &self.route,
                envelope("exec.run", request),
                SubscribeOptions::default(),
            )
            .await
            .map_err(|error| match error {
                CallError::NotSent(_) | CallError::StaleRouteHandle(_) => {
                    SubmitError::NotSent(error.to_string())
                }
                _ => SubmitError::Unknown(error.to_string()),
            })?;
        // The SDK ends a subscription whose bounded event channel fills up. A
        // slow reader of this command's stdout must not cost the job's
        // outcome, so a task drains the events into an unbounded queue.
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

    async fn cancel(&self, job_id: Uuid) -> Result<(), String> {
        self.consumer
            .request(
                &self.route,
                envelope("exec.cancel", CancelRequest::new(job_id)),
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
    let client = match client {
        Ok(client) => client,
        Err(message) => {
            eprintln!("ck motor run: refused before start: {message}");
            return EXIT_REFUSED;
        }
    };
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    drive(&client, &request, &mut out, &mut err, &mut interrupts).await
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
        assert!(args.env.is_empty() && args.siblings.is_empty());
        // Everything after -- belongs to the command, options included.
        let Ok(Invocation::Run(args)) = parse_args(&words("-- ls --help")) else {
            panic!("expected a run")
        };
        assert_eq!(command_line(&args.command), "ls --help");
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
