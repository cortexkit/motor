//! Mirror-only operator client. Each invocation performs one operation using
//! motor-module's production transport, rather than reproducing its wire codec.
#[cfg(unix)]
use motor_module::{
    benchmark::{dependencies, Client},
    Connector, Duplex,
};
#[cfg(unix)]
use std::{future::Future, io, path::PathBuf, pin::Pin, sync::Arc};
#[cfg(unix)]
use uuid::Uuid;

#[cfg(unix)]
struct UnixConnector(PathBuf);

#[cfg(unix)]
impl Connector for UnixConnector {
    fn connect(
        &self,
        _id: Uuid,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
        Box::pin(async move {
            Ok(Box::new(tokio::net::UnixStream::connect(&self.0).await?) as Box<dyn Duplex>)
        })
    }
}

#[cfg(unix)]
fn json_rows(rows: &[motor_protocol::Frame], output: &mut impl io::Write) -> io::Result<()> {
    for row in rows {
        serde_json::to_writer(&mut *output, row)?;
        writeln!(output)?;
    }
    Ok(())
}

#[cfg(unix)]
fn operation(name: &str, input: impl io::Read) -> io::Result<motor_protocol::Frame> {
    use motor_protocol::Frame;
    #[derive(serde::Deserialize)]
    struct Have {
        repositories: Vec<motor_protocol::RepositoryCandidates>,
        #[serde(default)]
        git_dependencies: Vec<String>,
    }
    Ok(match name {
        "have" => {
            let have: Have = serde_json::from_reader(input)?;
            Frame::Have {
                repositories: have.repositories,
                git_dependencies: have.git_dependencies,
            }
        }
        "prepare" => Frame::Prepare(serde_json::from_reader(input)?),
        "status" => Frame::Status,
        "run" => Frame::Run(serde_json::from_reader(input)?),
        "drop" => Frame::Drop(serde_json::from_reader(input)?),
        _ => return Err(io::Error::other("unsupported benchmark operation")),
    })
}

#[cfg(unix)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() != 3 {
        return Err(
            "usage: motor-bench-client-dev <runner.sock> <dependencies|have|prepare|status|run|drop>".into(),
        );
    }
    if args[2] == "dependencies" {
        #[derive(serde::Deserialize)]
        struct Plan {
            mirror: PathBuf,
            commits: Vec<String>,
        }
        let plan: Plan = serde_json::from_reader(std::io::stdin().lock())?;
        println!(
            "{}",
            serde_json::json!({
                "git_dependencies": dependencies(&plan.mirror, &plan.commits)?,
            })
        );
        return Ok(());
    }
    let request = operation(
        args[2].to_str().ok_or("invalid operation name")?,
        std::io::stdin().lock(),
    )?;
    let client = Client::new(Arc::new(UnixConnector(args[1].clone().into())));
    let rows = client.execute(request).await?;
    json_rows(&rows, &mut std::io::stdout().lock())?;
    Ok(())
}

#[cfg(not(unix))]
fn main() {
    eprintln!("the operator benchmark requires a Unix runner socket");
    std::process::exit(1);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine};
    use cortexkit_test_support::{scratch_root, ScratchDir};
    use motor_protocol::{Frame, Prepare, RepositoryCandidates, Run};
    use motor_runner::{Config, Runner};
    use sha2::{Digest, Sha256};
    use std::{collections::BTreeMap, fs, path::Path, process::Command, time::Duration};

    fn git(path: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_CONFIG_KEY_0")
            .env_remove("GIT_CONFIG_VALUE_0")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().into()
    }

    async fn invoke(client: &Client, name: &str, payload: impl serde::Serialize) -> Vec<Frame> {
        let payload = serde_json::to_vec(&payload).unwrap();
        client
            .execute(operation(name, payload.as_slice()).unwrap())
            .await
            .unwrap()
    }

    struct Server {
        task: tokio::task::JoinHandle<io::Result<()>>,
        socket: PathBuf,
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
            let _ = fs::remove_file(&self.socket);
        }
    }

    fn git_dependency(scratch: &ScratchDir) -> (PathBuf, String) {
        let source = scratch.join("git-source");
        fs::create_dir_all(&source).unwrap();
        git(&source, &["init", "--quiet"]);
        fs::write(source.join("lib.rs"), "pub fn dependency() {}\n").unwrap();
        git(&source, &["add", "."]);
        git(
            &source,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "dependency",
            ],
        );
        let commit = git(&source, &["rev-parse", "HEAD"]);
        (source, commit)
    }

    fn lockfile(url: &str, commit: &str) -> String {
        format!("version = 3\n[[package]]\nname = \"bench-dependency\"\nversion = \"0.1.0\"\nsource = \"git+{url}?rev={commit}#{commit}\"\n")
    }

    #[test]
    fn bench_dependencies_merge_committed_base_and_tip() {
        let scratch = ScratchDir::new("bench-lockfiles");
        git(&scratch, &["init", "--quiet"]);
        let url = "https://example.invalid/bench-dependency";
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        fs::write(scratch.join("Cargo.lock"), lockfile(url, &a)).unwrap();
        git(&scratch, &["add", "."]);
        git(
            &scratch,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "base",
            ],
        );
        let base = git(&scratch, &["rev-parse", "HEAD"]);
        fs::write(scratch.join("Cargo.lock"), lockfile(url, &b)).unwrap();
        git(&scratch, &["add", "."]);
        git(
            &scratch,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "tip",
            ],
        );
        let tip = git(&scratch, &["rev-parse", "HEAD"]);
        fs::write(scratch.join("Cargo.lock"), "uncommitted invalid lockfile").unwrap();
        let prepare = dependencies(&scratch, &[base, tip.clone()]).unwrap();
        assert_eq!(prepare.len(), 1);
        assert_eq!(prepare[0].url, url);
        assert_eq!(prepare[0].commits, vec![a, b.clone()]);
        assert!(prepare[0].bundle.is_none());
        let reader = dependencies(&scratch, &[tip]).unwrap();
        assert_eq!(reader[0].commits, vec![b]);
    }

    #[test]
    fn bench_operation_rejects_raw_hello() {
        assert!(operation("hello", &b"{}"[..]).is_err());
    }

    #[tokio::test]
    async fn bench_client_rejects_uploads_before_connecting() {
        let client = Client::new(Arc::new(UnixConnector(PathBuf::from(
            "/missing-bench.sock",
        ))));
        let request = Frame::Prepare(Prepare {
            transfer_id: Uuid::now_v7(),
            workspace_key: "/reader".into(),
            workspace_kind: None,
            repository_root: "/repo".into(),
            base_commit: "a".repeat(40),
            integration_tip: "a".repeat(40),
            siblings: vec![],
            bundles: vec![motor_protocol::BundleManifest {
                index: 0,
                repository: "/repo".into(),
                length: 1,
                sha256: "0".repeat(64),
            }],
            git_dependencies: vec![],
        });
        let error = client.execute(request).await.unwrap_err();
        assert!(
            error.to_string().contains("mirror-only operation"),
            "{error}"
        );
    }

    struct ObservedStream {
        inner: Box<dyn Duplex>,
        written: Arc<std::sync::Mutex<Vec<u8>>>,
    }
    impl tokio::io::AsyncRead for ObservedStream {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            Pin::new(&mut *self.inner).poll_read(cx, buf)
        }
    }
    impl tokio::io::AsyncWrite for ObservedStream {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            let result = Pin::new(&mut *self.inner).poll_write(cx, buf);
            if let std::task::Poll::Ready(Ok(count)) = &result {
                self.written
                    .lock()
                    .unwrap()
                    .extend_from_slice(&buf[..*count]);
            }
            result
        }
        fn poll_flush(
            mut self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            Pin::new(&mut *self.inner).poll_flush(cx)
        }
        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            Pin::new(&mut *self.inner).poll_shutdown(cx)
        }
    }

    struct CountedConnector {
        inner: UnixConnector,
        connections: Arc<std::sync::atomic::AtomicUsize>,
        written: Arc<std::sync::Mutex<Vec<u8>>>,
    }
    impl Connector for CountedConnector {
        fn connect(
            &self,
            id: Uuid,
        ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
            self.connections
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                let inner = self.inner.connect(id).await?;
                Ok(Box::new(ObservedStream {
                    inner,
                    written: self.written.clone(),
                }) as Box<dyn Duplex>)
            })
        }
    }

    #[tokio::test]
    async fn bench_missing_git_dependency_aborts_before_prepare() {
        let scratch = ScratchDir::new("bench-missing-dependency");
        let (_, dependency_commit) = git_dependency(&scratch);
        let url = "https://example.invalid/bench-dependency";
        let repo = scratch.join("repo");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "--quiet"]);
        fs::write(repo.join("Cargo.lock"), lockfile(url, &dependency_commit)).unwrap();
        git(&repo, &["add", "."]);
        git(
            &repo,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        );
        let commit = git(&repo, &["rev-parse", "HEAD"]);
        let declared = dependencies(&repo, std::slice::from_ref(&commit)).unwrap();
        let mut config = Config::new(scratch.join("state"), "bench");
        config.free_space_floor_bytes = 0;
        config.base_commands = vec!["true".into()];
        let runner = Runner::open(config).unwrap();
        let mirror = runner.config().state_dir().join("mirrors").join(format!(
            "{:x}",
            Sha256::digest(repo.to_str().unwrap().as_bytes())
        ));
        git(
            &repo,
            &[
                "clone",
                "--quiet",
                "--mirror",
                repo.to_str().unwrap(),
                mirror.to_str().unwrap(),
            ],
        );
        let socket = scratch_root().parent().unwrap().join(format!(
            "bm-{}-{}",
            std::process::id(),
            &Uuid::now_v7().simple().to_string()[..12]
        ));
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let _server = Server {
            task: tokio::spawn(motor_runner::serve(runner.clone(), listener)),
            socket: socket.clone(),
        };
        let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let written = Arc::new(std::sync::Mutex::new(Vec::new()));
        let client = Client::new(Arc::new(CountedConnector {
            inner: UnixConnector(socket),
            connections: connections.clone(),
            written: written.clone(),
        }));
        let workspace = scratch.join("reader");
        let request = Prepare {
            transfer_id: Uuid::now_v7(),
            workspace_key: workspace.to_str().unwrap().into(),
            workspace_kind: None,
            repository_root: repo.to_str().unwrap().into(),
            base_commit: commit.clone(),
            integration_tip: commit,
            siblings: vec![],
            bundles: vec![],
            git_dependencies: declared,
        };
        let payload = serde_json::to_vec(&request).unwrap();
        let error = client
            .execute(operation("prepare", payload.as_slice()).unwrap())
            .await
            .unwrap_err();
        assert!(error.to_string().contains(url), "{error}");
        assert!(error.to_string().contains(&dependency_commit), "{error}");
        // Only Have connected: no Prepare session or writer request was submitted.
        assert_eq!(connections.load(std::sync::atomic::Ordering::SeqCst), 1);
        let mut bytes = std::io::Cursor::new(written.lock().unwrap().clone());
        let mut operations = Vec::new();
        while let Some(frame) = motor_protocol::read_frame(&mut bytes).unwrap() {
            if !matches!(frame, Frame::Hello { .. } | Frame::Heartbeat { .. }) {
                operations.push(frame);
            }
        }
        assert!(
            matches!(&operations[..], [Frame::Have { git_dependencies, .. }]
            if git_dependencies == &[url])
        );
        assert!(!workspace.exists());
        assert!(runner.status().repositories.is_empty());
    }

    #[tokio::test]
    async fn bench_client_prepare_status_and_run_reconnect() {
        tokio::time::timeout(Duration::from_secs(30), async {
            let scratch = ScratchDir::new("bench-client");
            let repo = scratch.join("repo");
            fs::create_dir_all(&repo).unwrap();
            git(&repo, &["init", "--quiet"]);
            fs::write(repo.join("source"), "benchmark fixture\n").unwrap();
            let (dependency_source, dependency_commit) = git_dependency(&scratch);
            let dependency_url = "https://example.invalid/bench-dependency";
            fs::write(repo.join("Cargo.lock"), lockfile(dependency_url, &dependency_commit)).unwrap();
            git(&repo, &["add", "."]);
            git(&repo, &["-c", "user.name=Test", "-c", "user.email=test@example.invalid",
                "commit", "--quiet", "-m", "fixture"]);
            let commit = git(&repo, &["rev-parse", "HEAD"]);
            let tree = git(&repo, &["rev-parse", "HEAD^{tree}"]);
            let repository = repo.to_str().unwrap().to_owned();
            let mut config = Config::new(scratch.join("state"), "bench");
            config.free_space_floor_bytes = 0;
            config.base_commands = vec!["true".into()];
            let runner = Runner::open(config).unwrap();
            let mirror = runner.config().state_dir().join("mirrors")
                .join(format!("{:x}", Sha256::digest(repository.as_bytes())));
            git(&repo, &["clone", "--quiet", "--mirror", &repository, mirror.to_str().unwrap()]);
            git(&mirror, &["update-ref", "refs/tips/bench", &commit]);
            // Keep sun_path short even when the test-support scratch root is long.
            let socket = scratch_root().parent().unwrap().join(format!(
                "bc-{}-{}", std::process::id(), &Uuid::now_v7().simple().to_string()[..12]));
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let dependency_mirror = runner.config().state_dir().join("git-deps")
                .join(format!("{:x}.git", Sha256::digest(dependency_url.as_bytes())));
            fs::create_dir_all(dependency_mirror.parent().unwrap()).unwrap();
            git(&repo, &["clone", "--quiet", "--mirror", dependency_source.to_str().unwrap(),
                dependency_mirror.to_str().unwrap()]);
            git(&dependency_mirror, &["update-ref", &format!("refs/ck-motor/dep/{dependency_commit}"),
                &dependency_commit]);
            let declared = dependencies(&mirror, std::slice::from_ref(&commit)).unwrap();
            assert_eq!(declared[0].commits, vec![dependency_commit.clone()]);
            let _server = Server {
                task: tokio::spawn(motor_runner::serve(runner, listener)),
                socket: socket.clone(),
            };
            let client = Client::new(Arc::new(UnixConnector(socket)));
            let held = invoke(&client, "have", serde_json::json!({
                "repositories": [RepositoryCandidates {
                    repository: repository.clone(), commits: vec![commit.clone()],
                }],
                "git_dependencies": [dependency_url],
            })).await;
            assert!(matches!(&held[0], Frame::HaveReply { repositories, .. }
                if repositories[0].commits.contains(&commit)));
            assert!(matches!(&held[0], Frame::HaveReply { git_dependencies: Some(candidates), .. }
                if candidates.iter().any(|candidate| candidate.url == dependency_url
                    && candidate.commits == vec![dependency_commit.clone()])));
            let workspace = scratch.join("reader").to_str().unwrap().to_owned();
            let transfer = Uuid::now_v7();
            let mut rows = invoke(&client, "prepare", Prepare {
                transfer_id: transfer, workspace_key: workspace.clone(), workspace_kind: None,
                repository_root: repository.clone(), base_commit: commit.clone(),
                integration_tip: commit.clone(), siblings: vec![], bundles: vec![],
                git_dependencies: declared.clone(),
            }).await;
            assert!(matches!(rows.last(), Some(Frame::Prepared { transfer_id }) if *transfer_id == transfer));
            // Prepare's server session has completed. Polling must reconnect
            // rather than writing to that closed operation's channel.
            loop {
                let status = invoke(&client, "status", serde_json::json!({})).await;
                let json = serde_json::to_value(&status[0]).unwrap();
                let fill = json["repositories"].as_array().unwrap().iter()
                    .find(|repo| repo["repository_root"] == repository).unwrap();
                assert_ne!(fill["last_rebuild_result"], "failed", "{fill}");
                let ready = fill["last_rebuild_result"] == "ok" && fill["published_commit"] == commit;
                rows.extend(status);
                if ready { break; }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let job = Uuid::now_v7();
            rows.extend(invoke(&client, "run", Run {
                job_id: job, platform: None, workspace_key: workspace.clone(), workspace_kind: None,
                repository_root: repository, cwd: workspace,
                command: "printf 'MOTOR_SCCACHE_BENCH={\"case\":\"first\",\"pair\":1}\\n'".into(),
                env: BTreeMap::new(), weight_hint: Some(2), timeout: Some(10),
                queue_wait_limit_s: 10, snapshot_commit: commit, tree_hash: tree,
                integration_tip: String::new(), siblings: vec![], bundles: vec![],
                git_dependencies: declared, base_commit: None,
            }).await);
            let mut output = Vec::new();
            json_rows(&rows, &mut output).unwrap();
            let json: Vec<serde_json::Value> = std::str::from_utf8(&output).unwrap().lines()
                .map(|line| serde_json::from_str(line).unwrap()).collect();
            assert!(json.iter().any(|row| row["type"] == "prepared"));
            assert!(json.iter().any(|row| row["type"] == "status_reply" && row["server_reachable"] == true));
            assert!(json.iter().any(|row| row["type"] == "started"));
            let terminal = json.iter().find(|row| row["type"] == "terminal").unwrap();
            assert_eq!(terminal["outcome"], serde_json::json!({"type": "exit", "code": 0}));
            let bytes: Vec<u8> = json.iter().filter(|row| row["type"] == "output")
                .flat_map(|row| STANDARD.decode(row["bytes"].as_str().unwrap()).unwrap()).collect();
            assert_eq!(String::from_utf8(bytes).unwrap(),
                "MOTOR_SCCACHE_BENCH={\"case\":\"first\",\"pair\":1}\n");
        }).await.expect("benchmark protocol sequence hung");
    }
}
