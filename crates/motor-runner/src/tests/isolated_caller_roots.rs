//! Root-only fixtures for caller-root mapping on an isolated Linux runner: a
//! `/home` job's checkout lives under the runner's host directory H, and the
//! job sees it, and everything about it, at the caller's `/home` path.
use super::*;
use base64::{engine::general_purpose::STANDARD, Engine as _};

const USER: &str = "motor-mapped-fixture";

fn caller_workspace() -> String {
    format!("/home/{USER}/fixture")
}

fn isolated_mapped_config(f: &Fixture) -> Config {
    let mut config = f.config();
    config.isolate_jobs = true;
    config.disable_warm_builds = true;
    let host = f.scratch.join("callers/home");
    fs::create_dir_all(&host).unwrap();
    config.caller_roots = vec![crate::CallerRoot {
        caller_prefix: "/home".into(),
        host_root: host,
    }];
    config
}

fn host_root(config: &Config) -> PathBuf {
    config.caller_roots[0].host_root.clone()
}

/// A Run for `workspace` from the fixture's main repository, without siblings.
fn mapped_run(
    f: &Fixture,
    workspace: &str,
    command: &str,
    returns: &[&str],
) -> (Run, Vec<Vec<u8>>) {
    let (mut request, mut bytes) = f.run(command);
    request.workspace_key = workspace.into();
    request.repository_root = workspace.into();
    request.cwd = workspace.into();
    request.siblings.clear();
    request.bundles.truncate(1);
    request.bundles[0].repository = workspace.into();
    bytes.truncate(1);
    request.return_paths = returns
        .iter()
        .map(|name| format!("{workspace}/{name}"))
        .collect();
    (request, bytes)
}

fn stdout(runner: &Runner, id: Uuid) -> String {
    let bytes: Vec<u8> = runner
        .replay(id, 0)
        .unwrap()
        .iter()
        .filter_map(|frame| match frame {
            Frame::Output(o) if o.stream == OutputStream::Stdout => Some(o.bytes.0.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    String::from_utf8(bytes).unwrap()
}

fn line<'a>(output: &'a str, key: &str) -> &'a str {
    output
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("{key} missing from {output}"))
}

fn return_result(runner: &Runner, id: Uuid) -> ReturnResult {
    runner
        .replay(id, 0)
        .unwrap()
        .into_iter()
        .find_map(|frame| match frame {
            Frame::ReturnResult(result) => Some(result),
            _ => None,
        })
        .expect("return result")
}

/// Every regular file under `directory`, for checking runner-written
/// job-visible metadata.
fn files(directory: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        let metadata = fs::symlink_metadata(&path).unwrap();
        if metadata.is_dir() {
            found.extend(files(&path));
        } else if metadata.is_file() {
            found.push(path);
        }
    }
    found
}

/// The names in the host's real `/home` with the modification times of
/// `/home` and each entry, which a mapped job must leave unchanged.
fn host_home() -> Vec<(String, Option<SystemTime>)> {
    let mut entries: Vec<_> = fs::read_dir("/home")
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().to_string_lossy().into_owned(),
                fs::symlink_metadata(entry.path())
                    .ok()
                    .and_then(|m| m.modified().ok()),
            )
        })
        .collect();
    entries.push((
        "/home".into(),
        fs::metadata("/home").ok().and_then(|m| m.modified().ok()),
    ));
    entries.sort();
    entries
}

/// Unit properties naming paths the job is shown: none may name H.
fn unit_properties(unit: &str) -> String {
    let output = Command::new("systemctl")
        .args([
            "show",
            unit,
            "--property=RootDirectory,WorkingDirectory,BindPaths,BindReadOnlyPaths,Environment,ExecStart",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

const MEASURE: &str = r#"
echo "pwd=$(pwd)"
echo "PWD=$PWD"
echo "toplevel=$(git rev-parse --show-toplevel)"
echo "status=[$(git status --porcelain)]"
echo "head=$(git log -1 --format=%H)"
echo "home=$(ls -a /home | tr '\n' ' ')"
echo "committed=$(cat committed-link)"
printf 'fn main() { let x: u32 = "s"; }\n' > /tmp/mapped-error.rs
mkdir -p compile && cp /tmp/mapped-error.rs compile/error.rs
echo "rustc=$(rustc --edition 2021 "$PWD/compile/error.rs" -o /tmp/mapped-error 2>&1 | grep -m1 -- '-->')"
echo "gitconfig=$(grep -c . "$HOME/.gitconfig")"
cat "$HOME/.gitconfig"
ln -s "$PWD/one" link
echo one > one
touch ready
while [ ! -e release ]; do sleep 0.05; done
"#;

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires host system manager, sudo and motor-job account"]
async fn isolated_mapped_job_sees_caller_paths_and_never_learns_h() {
    let f = Fixture::new();
    let workspace = caller_workspace();
    // An absolute caller-form link committed in the snapshot, beside the one
    // the command creates.
    std::os::unix::fs::symlink(format!("{workspace}/source"), f.main.join("committed-link"))
        .unwrap();
    git(&f.main, &["add", "committed-link"]);
    git(
        &f.main,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "link",
        ],
    );
    let config = isolated_mapped_config(&f);
    let h = host_root(&config);
    let h_text = h.to_str().unwrap().to_owned();
    let home_before = host_home();
    let runner = Runner::open(config.clone()).unwrap();
    let (request, bytes) = mapped_run(&f, &workspace, MEASURE, &["link", "committed-link"]);
    // The fixture repository never names H.
    assert!(bytes.iter().all(|bundle| !bundle
        .windows(h_text.len())
        .any(|window| window == h_text.as_bytes())));
    f.submit(&runner, &request, &bytes);
    let host = h.join(format!("{USER}/fixture"));
    let started = Instant::now();
    while !host.join("ready").exists() {
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "job never got ready"
        );
        let ended = runner.core.inner.lock().unwrap().jobs[&request.job_id]
            .terminal
            .clone();
        assert!(ended.is_none(), "{ended:?}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // Argv: the running unit's paths, environment and command never name H.
    let unit = runner.job_status(request.job_id).unwrap().unit.unwrap();
    let properties = unit_properties(&unit);
    assert!(!properties.contains(&h_text), "{properties}");
    assert!(
        properties.contains(&format!("WorkingDirectory={workspace}")),
        "{properties}"
    );
    assert!(Command::new("/usr/bin/sudo")
        .args(["-n", "-u", "motor-job", "/usr/bin/touch", "--"])
        .arg(host.join("release"))
        .status()
        .unwrap()
        .success());
    let t = terminal(&runner, request.job_id).await;
    assert_eq!(t.outcome, Outcome::Exit { code: 0 });
    let output = stdout(&runner, request.job_id);
    assert!(!output.contains(&h_text), "{output}");
    assert_eq!(line(&output, "pwd"), workspace);
    assert_eq!(line(&output, "PWD"), workspace);
    assert_eq!(line(&output, "toplevel"), workspace);
    assert_eq!(line(&output, "status"), "[]");
    assert_eq!(line(&output, "head"), request.snapshot_commit);
    assert_eq!(line(&output, "committed"), "first");
    assert!(
        line(&output, "rustc").contains(&format!("--> {workspace}/compile/error.rs")),
        "{output}"
    );
    // The job's safe.directory entry is its caller workspace.
    assert!(
        output.contains(&format!("directory = {workspace}\n")),
        "{output}"
    );
    // `ls -a /home` lists only `.`, `..`, the caller's own ancestor and the
    // runner's reserved binds beneath /home; nothing from the real /home.
    let mut allowed = BTreeSet::from([".".to_string(), "..".to_string(), USER.to_string()]);
    for reserved in crate::isolation::reserved_destinations(&config) {
        if let Ok(rest) = reserved.strip_prefix("/home") {
            if let Some(first) = rest.components().next() {
                allowed.insert(first.as_os_str().to_string_lossy().into_owned());
            }
        }
    }
    let listed: BTreeSet<_> = line(&output, "home")
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    assert!(
        listed.is_subset(&allowed),
        "{listed:?} not within {allowed:?}"
    );
    if let Some((hidden, _)) = home_before
        .iter()
        .find(|(name, _)| name != "/home" && !allowed.contains(name))
    {
        assert!(!listed.contains(hidden), "{hidden} leaked into the job");
    }
    assert_eq!(host_home(), home_before);
    // Runner-written job-visible metadata in the checkout never names H.
    for file in files(&host.join(".git")) {
        let bytes = fs::read(&file).unwrap();
        assert!(
            !bytes
                .windows(h_text.len())
                .any(|window| window == h_text.as_bytes()),
            "{} names H",
            file.display()
        );
    }
    // Caller-visible records name caller paths only.
    let record = serde_json::to_string(&t).unwrap();
    assert!(!record.contains(&h_text), "{record}");
    let result = return_result(&runner, request.job_id);
    assert_eq!(result.return_paths_failed, None, "{result:?}");
    let returned = result.returned_files.unwrap();
    assert_eq!(returned[0].path, format!("{workspace}/link"));
    assert_eq!(returned[0].data, STANDARD.encode("one\n"));
    assert_eq!(returned[1].path, format!("{workspace}/committed-link"));
    assert_eq!(returned[1].data, STANDARD.encode("first\n"));
    drop(runner);
    let runner = reopen(config);
    // Replay after a same-configuration restart names no H either.
    let replayed = serde_json::to_string(&runner.replay(request.job_id, 0).unwrap()).unwrap();
    assert!(!replayed.contains(&h_text), "{replayed}");
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires host system manager, sudo and motor-job account"]
async fn isolated_mapped_job_returns_files_and_output_naming_h_byte_for_byte() {
    let f = Fixture::new();
    let config = isolated_mapped_config(&f);
    let h_text = host_root(&config).to_str().unwrap().to_owned();
    let workspace = caller_workspace();
    let runner = Runner::open(config).unwrap();
    // The caller's own command names H; the runner never rewrites it.
    let command = format!("printf '%s' '{h_text}' > named; printf '%s' '{h_text}'");
    let (request, bytes) = mapped_run(&f, &workspace, &command, &["named"]);
    f.submit(&runner, &request, &bytes);
    assert_eq!(
        terminal(&runner, request.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    assert_eq!(stdout(&runner, request.job_id), h_text);
    let returned = return_result(&runner, request.job_id)
        .returned_files
        .unwrap();
    assert_eq!(returned[0].path, format!("{workspace}/named"));
    assert_eq!(returned[0].data, STANDARD.encode(&h_text));
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires host system manager, sudo, motor-job and motor-cache accounts"]
async fn isolated_mapped_cache_fills_run_at_caller_paths_at_depth_two_and_one() {
    for repository in [
        format!("/home/{USER}/repo"),
        "/home/motor-mapped-repo".to_string(),
    ] {
        let f = Fixture::new();
        let mut config = isolated_mapped_config(&f);
        config.disable_warm_builds = false;
        config.base_commands =
            vec!["echo \"pwd=$(pwd)\"; echo \"toplevel=$(git rev-parse --show-toplevel)\"".into()];
        let h_text = host_root(&config).to_str().unwrap().to_owned();
        let runner = Runner::open(config.clone()).unwrap();
        let (request, bytes) = mapped_run(&f, &repository, "true", &[]);
        f.submit(&runner, &request, &bytes);
        assert_eq!(
            terminal(&runner, request.job_id).await.outcome,
            Outcome::Exit { code: 0 }
        );
        base_done(&runner).await;
        let checkout = format!(
            "{}/.motor-base-{}-{}",
            Path::new(&repository).parent().unwrap().display(),
            config.instance,
            workspace::name(&repository)
        );
        let fill = {
            let inner = runner.core.inner.lock().unwrap();
            inner
                .jobs
                .values()
                .find(|job| job.base)
                .cloned()
                .expect("a fill ran")
        };
        let fill_request = fill.request.as_ref().unwrap();
        assert_eq!(fill_request.workspace_key, format!("base:{repository}"));
        assert_eq!(fill_request.cwd, checkout);
        let output = stdout(&runner, fill.id);
        assert_eq!(line(&output, "pwd"), checkout, "{output}");
        assert_eq!(line(&output, "toplevel"), checkout, "{output}");
        assert!(!output.contains(&h_text), "{output}");
        // The fill's request keeps its opaque key, and only the checkout is
        // ever a path: the fill job's record names no H.
        let record = serde_json::to_string(&fill).unwrap();
        assert!(!record.contains(&h_text), "{record}");
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires host system manager, sudo and motor-job account"]
async fn isolated_mapped_siblings_and_aliases_show_caller_toplevels() {
    let f = Fixture::new();
    let config = isolated_mapped_config(&f);
    let h_text = host_root(&config).to_str().unwrap().to_owned();
    let runner = Runner::open(config).unwrap();
    let sibling = format!("/home/{USER}/commons");
    for workspace in [caller_workspace(), f.key.clone()] {
        let alias = Path::new(&workspace)
            .parent()
            .unwrap()
            .join("commons")
            .to_string_lossy()
            .into_owned();
        let command = format!(
            "echo \"sibling=$(git -C {sibling} rev-parse --show-toplevel)\"; \
             echo \"alias=$(git -C ../commons rev-parse --show-toplevel)\"; \
             grep -c . \"$HOME/.gitconfig\""
        );
        let (mut request, bytes) = f.run(&command);
        request.workspace_key = workspace.clone();
        request.repository_root = workspace.clone();
        request.cwd = workspace.clone();
        request.bundles[0].repository = workspace.clone();
        request.siblings[0].canonical_path = sibling.clone();
        request.bundles[1].repository = sibling.clone();
        f.submit(&runner, &request, &bytes);
        let t = terminal(&runner, request.job_id).await;
        assert_eq!(t.outcome, Outcome::Exit { code: 0 }, "{workspace}");
        let output = stdout(&runner, request.job_id);
        assert_eq!(line(&output, "sibling"), sibling, "{output}");
        assert_eq!(line(&output, "alias"), alias, "{output}");
        assert!(!output.contains(&h_text), "{output}");
    }
}

/// Run by `isolated_deploy_reattaches_mapped_job_with_caller_returns` in a
/// child process: start the mapped job, then die as a drained runner's
/// process does at a deploy, leaving the job's unit running.
#[cfg(target_os = "linux")]
#[test]
fn mapped_surviving_unit_child() {
    let Some(root) = std::env::var_os("MOTOR_MAPPED_REATTACH_FIXTURE") else {
        return;
    };
    let root = PathBuf::from(root);
    let request: Run = read_json(&root.join("request.json")).unwrap();
    let config: MappedChild = read_json(&root.join("config.json")).unwrap();
    let runner = Runner::open(config.config()).unwrap();
    runner.accept(request.clone()).unwrap();
    runner
        .fetch_run(&request, &root.join("bundles"), true)
        .unwrap();
    let started = Instant::now();
    while !config.host_workspace.join("marker").exists() {
        let inner = runner.core.inner.lock().unwrap();
        assert!(
            inner.jobs[&request.job_id].terminal.is_none(),
            "launch failed: {:?}",
            inner.jobs[&request.job_id].terminal
        );
        drop(inner);
        assert!(started.elapsed() < Duration::from_secs(60));
        std::thread::park_timeout(Duration::from_millis(25));
    }
    let process = runner.core.inner.lock().unwrap().jobs[&request.job_id]
        .process
        .unwrap();
    unsafe {
        libc::kill(-process.pid, libc::SIGKILL);
    }
    std::process::exit(86);
}

/// The options both runner processes start with: the same binary and the
/// same caller root, as a deploy restarts the unit.
#[derive(serde::Serialize, serde::Deserialize)]
struct MappedChild {
    state: PathBuf,
    owned: PathBuf,
    host_root: PathBuf,
    host_workspace: PathBuf,
}

impl MappedChild {
    fn config(&self) -> Config {
        let mut config = Config::new(&self.state, "e2e");
        config.owned_workspace_roots = vec![self.owned.clone()];
        config.free_space_floor_bytes = 0;
        config.isolate_jobs = true;
        config.disable_warm_builds = true;
        config.caller_roots = vec![crate::CallerRoot {
            caller_prefix: "/home".into(),
            host_root: self.host_root.clone(),
        }];
        config
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires host system manager, sudo and motor-job account"]
async fn isolated_deploy_reattaches_mapped_job_with_caller_returns() {
    let f = Fixture::new();
    let workspace = caller_workspace();
    let host_root = f.scratch.join("callers/home");
    fs::create_dir_all(&host_root).unwrap();
    let child = MappedChild {
        state: f.scratch.join("child-state"),
        owned: f.scratch.join("remote"),
        host_root: host_root.clone(),
        host_workspace: host_root.join(format!("{USER}/fixture")),
    };
    let (request, bytes) = mapped_run(
        &f,
        &workspace,
        "echo once >> marker; while [ ! -f release ]; do sleep 0.01; done; \
         printf returned > out; echo after",
        &["out"],
    );
    atomic_json(&f.scratch.join("request.json"), &request).unwrap();
    atomic_json(&f.scratch.join("config.json"), &child).unwrap();
    fs::create_dir_all(f.scratch.join("bundles")).unwrap();
    for (i, bytes) in bytes.iter().enumerate() {
        fs::write(f.scratch.join("bundles").join(i.to_string()), bytes).unwrap();
    }
    let status = crate::test_process::ChildGuard::spawn(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "engine::tests::isolated_caller_roots::mapped_surviving_unit_child",
                "--nocapture",
            ])
            .env("MOTOR_MAPPED_REATTACH_FIXTURE", f.scratch.path()),
    )
    .unwrap()
    .wait()
    .unwrap();
    assert_eq!(status.code(), Some(86));
    let runner = reopen(child.config());
    let recovered = runner.job_status(request.job_id).unwrap();
    assert_eq!(recovered.state, "running");
    assert_eq!(recovered.unit_alive, Some(true));
    assert!(Command::new("/usr/bin/sudo")
        .args(["-n", "-u", "motor-job", "/usr/bin/touch", "--"])
        .arg(child.host_workspace.join("release"))
        .status()
        .unwrap()
        .success());
    assert_eq!(
        terminal(&runner, request.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    let result = return_result(&runner, request.job_id);
    let returned = result.returned_files.unwrap();
    assert_eq!(returned[0].path, format!("{workspace}/out"));
    assert_eq!(returned[0].data, STANDARD.encode("returned"));
    assert_eq!(
        fs::read_to_string(child.host_workspace.join("marker")).unwrap(),
        "once\n"
    );
    let replay = serde_json::to_string(&runner.replay(request.job_id, 0).unwrap()).unwrap();
    assert!(!replay.contains(host_root.to_str().unwrap()), "{replay}");
}
