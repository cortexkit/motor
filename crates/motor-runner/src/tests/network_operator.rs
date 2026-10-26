use super::*;

fn config(f: &Fixture) -> Config {
    let mut config = f.config();
    config.isolate_jobs = true;
    config.job_network = Some(JobNetwork::Veth);
    config.disable_warm_builds = true;
    config
}

fn restart_config(root: &Path) -> Config {
    let mut config = Config::new(root.join("state"), "e2e");
    config.workspace_roots = vec![root.join("remote/worktrees")];
    config.owned_workspace_roots = vec![root.join("remote")];
    config.free_space_floor_bytes = 0;
    config.isolate_jobs = true;
    config.job_network = Some(JobNetwork::Veth);
    config.disable_warm_builds = true;
    config
}

#[test]
fn restart_child_configuration_admits_fixture_destinations() {
    let f = Fixture::new();
    let mut config = restart_config(f.scratch.path());
    config.isolate_jobs = false;
    config.job_network = None;
    config.cpu_topology = f.config().cpu_topology;
    config.test_processes = f.config().test_processes;
    let runner = Runner::open(config).unwrap();
    let (request, _) = f.run("true");
    assert_run_accepted(&runner, &request);
}

fn install_probe(f: &Fixture) {
    fs::write(
        f.main.join("network-check.py"),
        include_str!("../../../../script/runner/network-check.py"),
    )
    .unwrap();
    fs::write(
        f.main.join("network-bun-package.fixture"),
        include_str!("../../../../script/runner/network-fixture/package.json"),
    )
    .unwrap();
    fs::write(
        f.main.join("network-bun-lock.fixture"),
        include_str!("../../../../script/runner/network-fixture/bun.lock"),
    )
    .unwrap();
    // Non-manifest names keep trusted preparation from preinstalling the fixture;
    // only the measured job copies them to package.json/bun.lock and downloads.
    git(
        &f.main,
        &[
            "add",
            "network-check.py",
            "network-bun-package.fixture",
            "network-bun-lock.fixture",
        ],
    );
    git(&f.main, &["commit", "-m", "network probe"]);
}

async fn finished(runner: &Runner, id: Uuid) -> TerminalRecord {
    tokio::time::timeout(Duration::from_secs(150), async {
        loop {
            let status = runner.job_status(id).unwrap();
            if let Some(terminal) = status.terminal {
                return terminal;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "network operator job {id} did not terminate; {}",
            runner_diagnostics(runner)
        )
    })
}

fn job_output(runner: &Runner, id: Uuid) -> String {
    let bytes: Vec<_> = runner
        .replay(id, 0)
        .unwrap()
        .into_iter()
        .filter_map(|frame| match frame {
            Frame::Output(output) => Some(output.bytes.0),
            _ => None,
        })
        .flatten()
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

async fn assert_success(runner: &Runner, request: &Run) {
    let terminal = finished(runner, request.job_id).await;
    let output = job_output(runner, request.job_id);
    println!("{output}");
    assert_eq!(
        terminal.outcome,
        Outcome::Exit { code: 0 },
        "{terminal:?}\n{output}"
    );
}

pub(crate) async fn case(mode: &str) {
    let f = Fixture::new();
    let (_generation, _) = crate::network::linux::validated_generation(f.scratch.path());
    install_probe(&f);
    let runner = Runner::open(config(&f)).unwrap();
    assert_eq!(
        crate::network::unsupported(runner.config()),
        None,
        "operator fixture requires a currently valid generation and net admission"
    );
    if mode == "forward" {
        forward_drops(&f, &runner).await;
        return;
    }
    if matches!(mode, "input" | "forbidden") {
        // Discover addresses in the host namespace, not the isolated job's routes.
        let output = Command::new("python3")
            .args(["network-check.py", "host-addresses"])
            .current_dir(&f.main)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "host address discovery: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        fs::write(f.main.join("network-host-addresses.fixture"), output.stdout).unwrap();
        git(&f.main, &["add", "network-host-addresses.fixture"]);
        git(&f.main, &["commit", "-m", "host address probe targets"]);
    }
    if mode == "offline" {
        // Use the same enabled runner for both requests. This exercises the
        // production per-request opt-in decision, not a manually offline sandbox.
        let (mut net, bytes) = f.run("python3 network-check.py public");
        net.network = Some(Network::Outbound);
        f.submit(&runner, &net, &bytes);
        assert_success(&runner, &net).await;
    }
    let name = format!("motor-net-test-{}", Uuid::now_v7());
    let _abstract_listener = if mode == "environment" {
        use std::os::{
            linux::net::SocketAddrExt,
            unix::net::{SocketAddr, UnixListener},
        };
        Some(
            UnixListener::bind_addr(&SocketAddr::from_abstract_name(name.as_bytes()).unwrap())
                .unwrap(),
        )
    } else {
        None
    };
    let (mut request, bytes) = f.run(&format!("python3 network-check.py {mode} {name}"));
    request.network = if mode == "offline" {
        None
    } else {
        Some(Network::Outbound)
    };
    f.submit(&runner, &request, &bytes);
    assert_success(&runner, &request).await;
    if mode == "measure" {
        if let Some(destination) = std::env::var_os("MOTOR_NET_RESULTS") {
            let destination = PathBuf::from(destination);
            fs::create_dir_all(&destination).unwrap();
            fs::copy(
                Path::new(&request.workspace_key).join("bun-public-fixture/bun.lock"),
                destination.join("bun.lock"),
            )
            .unwrap();
            fs::write(
                destination.join("bun-install-output.txt"),
                job_output(&runner, request.job_id),
            )
            .unwrap();
        }
    }
    if mode != "offline" {
        let job: Job = read_json(&runner.record_path(request.job_id)).unwrap();
        let unit = job.process.unwrap().unit.unwrap();
        let name = crate::network::linux::interface(unit);
        let started = Instant::now();
        loop {
            let output = Command::new("/usr/bin/sudo")
                .args(["-n", "/usr/sbin/ip", "link", "show", "dev", &name])
                .output()
                .unwrap();
            if !output.status.success() {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "veth/address not cleaned after job: {name}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let rules = Command::new("/usr/bin/sudo")
            .args(["-n", "/usr/sbin/nft", "list", "table", "inet", "motor_net"])
            .output()
            .unwrap();
        assert!(rules.status.success());
        assert!(
            !String::from_utf8_lossy(&rules.stdout).contains(&name),
            "per-job firewall rule leaked"
        );
    }
}

fn generation_root_step(script: &str) -> Command {
    let mut command = Command::new("/usr/bin/sudo");
    command.args(["-n", "/usr/bin/python3", "-I", "-c", script]);
    command
}

struct GenerationProducerCleanup(PathBuf);
impl Drop for GenerationProducerCleanup {
    fn drop(&mut self) {
        let result = generation_root_step("import pathlib,shutil,sys; p=pathlib.Path(sys.argv[1])/'lockdir'; shutil.rmtree(p) if p.exists() else None")
            .arg(&self.0).output();
        if !matches!(result, Ok(ref output) if output.status.success()) {
            eprintln!("private generation lock cleanup failed: {result:?}");
        }
    }
}

#[test]
fn generation_privileged_steps_use_noninteractive_sudo_not_a_root_test_process() {
    let command = generation_root_step("publish-private-generation");
    assert_eq!(command.get_program(), "/usr/bin/sudo");
    let args: Vec<_> = command.get_args().collect();
    assert_eq!(
        args,
        [
            "-n",
            "/usr/bin/python3",
            "-I",
            "-c",
            "publish-private-generation"
        ]
    );
}

pub(crate) async fn generation(mode: &str) {
    assert_ne!(
        unsafe { libc::geteuid() },
        0,
        "run generation tests as the operator account; only private producer steps use sudo -n"
    );
    let f = Fixture::new();
    install_probe(&f);
    let producer = f.scratch.path().join("generation-producer");
    fs::create_dir_all(&producer).unwrap();
    // Keep the parent operator-owned so ordinary fixture teardown can unlink
    // root-owned leaf files. Only the loader's private lock directory needs sudo.
    let _producer_cleanup = GenerationProducerCleanup(producer.clone());
    let marker = producer.join("ready");
    // Exercise real root-owned publication and consumer reads without withdrawing
    // the production host's generation. Kernel policy and sandbox setup are real.
    let _generation = crate::network::linux::operator_generation_path(marker.clone());
    fs::write(
        producer.join("motor_network_ready.py"),
        include_str!("../../../../script/runner/motor_network_ready.py"),
    )
    .unwrap();
    fs::write(
        producer.join("motor_network_policy.py"),
        include_str!("../../../../script/runner/motor_network_policy.py"),
    )
    .unwrap();
    fs::write(
        producer.join("template.nft"),
        include_str!("../../../../script/runner/motor-net.nft"),
    )
    .unwrap();
    fs::write(
        producer.join("stale.nft"),
        "# stale source intentionally disagrees with the settings\n",
    )
    .unwrap();
    fs::write(
        producer.join("settings.json"),
        r#"{"uplink":"enp6s0","public_ipv4":"8.8.8.8"}"#,
    )
    .unwrap();
    let prepare = generation_root_step(r#"import os,pathlib,stat,sys
p=pathlib.Path(sys.argv[1])
for name in ('motor_network_ready.py','motor_network_policy.py','template.nft','stale.nft','settings.json'):
 f=p/name
 if not stat.S_ISREG(f.lstat().st_mode): raise RuntimeError('fixture input must be a regular file')
 os.chown(f,0,0); os.chmod(f,0o644)
"#).arg(&producer).output().unwrap();
    assert!(
        prepare.status.success(),
        "root-owned private fixture inputs: {}",
        String::from_utf8_lossy(&prepare.stderr)
    );
    let publish = || {
        let result = generation_root_step(r#"import pathlib,runpy,sys
ns=runpy.run_path(sys.argv[1]); ns['publish'].__globals__['READY']=pathlib.Path(sys.argv[2]); ns['publish']()"#)
            .arg(producer.join("motor_network_ready.py")).arg(&marker).output().unwrap();
        assert!(
            result.status.success(),
            "publisher: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    };
    if mode == "generation-reload" {
        publish();
    }
    let runner = Runner::open(config(&f)).unwrap();
    let refuse = |runner: &Runner| {
        let (mut request, _) = f.run("touch generation-job-must-not-run");
        request.network = Some(Network::Outbound);
        let admission = runner.accept(request.clone()).unwrap();
        assert!(
            matches!(
                admission,
                Admission::Refused(RefusalReason::NetworkUnsupported)
            ),
            "missing generation must refuse this net request before upload"
        );
        assert!(!runner.spool_dir(request.job_id).exists());
    };
    if mode == "generation-reload" {
        let (mut first, bytes) = f.run("python3 network-check.py public");
        first.network = Some(Network::Outbound);
        f.submit(&runner, &first, &bytes);
        assert_success(&runner, &first).await;
        let result = generation_root_step(r#"import pathlib,runpy,shutil,sys
p=pathlib.Path(sys.argv[1]); ns=runpy.run_path(str(p/'motor_network_policy.py')); g=ns['load'].__globals__
g['SETTINGS']=p/'settings.json'; g['TEMPLATE']=p/'template.nft'; g['SOURCE']=p/'stale.nft'; g['LOCK']=p/'lockdir'/'lock'
g['readiness'].READY=p/'ready'
try: ns['load']()
finally:
 if (p/'lockdir').exists(): shutil.rmtree(p/'lockdir')"#).arg(&producer).output().unwrap();
        assert!(
            !result.status.success(),
            "the stale-source reload must fail"
        );
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("source does not match uplink"),
            "reload must reach the binding guard, not fail fixture permissions: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(!marker.exists());
        assert!(
            crate::privilege::command("/usr/sbin/nft")
                .args(["list", "table", "inet", "motor_net"])
                .status()
                .unwrap()
                .success(),
            "old kernel table must still exist in the failed-reload regression"
        );
    }
    refuse(&runner);
    let (offline, bytes) = f.run("true");
    f.submit(&runner, &offline, &bytes);
    assert_success(&runner, &offline).await;
    publish();
    let (mut recovered, bytes) = f.run("python3 network-check.py public");
    recovered.network = Some(Network::Outbound);
    f.submit(&runner, &recovered, &bytes);
    assert_success(&runner, &recovered).await;
}

#[test]
#[ignore = "child process for isolated_net_restart_survives_and_cleans_up"]
fn restart_child() {
    let root =
        PathBuf::from(std::env::var_os("MOTOR_NET_RESTART_FIXTURE").expect("restart fixture"));
    let _generation = crate::network::linux::operator_generation_path(PathBuf::from(
        std::env::var_os("MOTOR_NET_TEST_GENERATION").expect("validated operator generation"),
    ));
    let request: Run = read_json(&root.join("request.json")).unwrap();
    let runner = Runner::open(restart_config(&root)).unwrap();
    assert_run_accepted(&runner, &request);
    runner
        .fetch_run(&request, &root.join("bundles"), true)
        .unwrap();
    let started = Instant::now();
    while !Path::new(&request.workspace_key).join("marker").exists() {
        let status = runner.job_status(request.job_id).unwrap();
        assert!(status.terminal.is_none(), "{status:?}");
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "waiting for network restart marker; {}",
            runner_diagnostics(&runner)
        );
        std::thread::park_timeout(Duration::from_millis(25));
    }
    let process = runner.core.inner.lock().unwrap().jobs[&request.job_id]
        .process
        .unwrap();
    // Kill the bridge as a daemon deployment does; the system job unit is not
    // part of that process group and keeps both its namespace and network lease.
    unsafe {
        libc::kill(-process.pid, libc::SIGKILL);
    }
    std::process::exit(86);
}

pub(crate) async fn restart() {
    let f = Fixture::new();
    let (_generation, marker) = crate::network::linux::validated_generation(f.scratch.path());
    install_probe(&f);
    let (mut request, bytes) = f.run("set -eu; python3 network-check.py public; echo ready > marker; while [ ! -f release ]; do sleep 0.05; done; python3 network-check.py public");
    request.network = Some(Network::Outbound);
    atomic_json(&f.scratch.join("request.json"), &request).unwrap();
    fs::create_dir_all(f.scratch.join("bundles")).unwrap();
    for (index, bytes) in bytes.iter().enumerate() {
        fs::write(f.scratch.join("bundles").join(index.to_string()), bytes).unwrap();
    }
    let status = crate::test_process::ChildGuard::spawn(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "engine::tests::network::operator::restart_child",
                "--ignored",
                "--nocapture",
            ])
            .env("MOTOR_NET_RESTART_FIXTURE", f.scratch.path())
            .env("MOTOR_NET_TEST_GENERATION", &marker),
    )
    .unwrap()
    .wait()
    .unwrap();
    assert_eq!(
        status.code(),
        Some(86),
        "network restart child failed for job {} at {}; request={request:?}",
        request.job_id,
        f.key
    );
    let old: Job = read_json(
        &config(&f)
            .state_dir()
            .join("jobs")
            .join(request.job_id.to_string())
            .join("record.json"),
    )
    .unwrap();
    assert_eq!(platform::unit_alive(old.process).unwrap(), Some(true));
    let runner = reopen(config(&f));
    assert_eq!(runner.job_status(request.job_id).unwrap().state, "running");
    assert!(Command::new("/usr/bin/sudo")
        .args(["-n", "-u", "motor-job", "/usr/bin/touch", "--"])
        .arg(Path::new(&f.key).join("release"))
        .status()
        .unwrap()
        .success());
    assert_success(&runner, &request).await;
    let name = crate::network::linux::interface(old.process.unwrap().unit.unwrap());
    let started = Instant::now();
    loop {
        if !Command::new("/usr/bin/sudo")
            .args(["-n", "/usr/sbin/ip", "link", "show", "dev", &name])
            .status()
            .unwrap()
            .success()
        {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "restart leaked {name}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn host_ip(args: &[&str]) {
    let output = Command::new("/usr/bin/sudo")
        .args(["-n", "/usr/sbin/ip"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "ip {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

struct OutsideNamespace {
    name: String,
    interface: String,
}
impl Drop for OutsideNamespace {
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/sudo")
            .args(["-n", "/usr/sbin/ip", "netns", "delete", &self.name])
            .output();
        let _ = Command::new("/usr/bin/sudo")
            .args([
                "-n",
                "/usr/sbin/ip",
                "link",
                "delete",
                "dev",
                &self.interface,
            ])
            .output();
    }
}

fn outside_namespace() -> OutsideNamespace {
    let short = Uuid::now_v7().simple().to_string()[22..].to_owned();
    let outside = OutsideNamespace {
        name: format!("motor-probe-{short}"),
        interface: format!("mpub-{short}"),
    };
    let peer = format!("mpp-{short}");
    host_ip(&["netns", "add", &outside.name]);
    host_ip(&[
        "link",
        "add",
        &outside.interface,
        "type",
        "veth",
        "peer",
        "name",
        &peer,
    ]);
    host_ip(&["link", "set", "dev", &peer, "netns", &outside.name]);
    host_ip(&[
        "addr",
        "add",
        "198.18.255.249/30",
        "dev",
        &outside.interface,
    ]);
    host_ip(&["link", "set", "dev", &outside.interface, "up"]);
    host_ip(&[
        "-n",
        &outside.name,
        "addr",
        "add",
        "198.18.255.250/30",
        "dev",
        &peer,
    ]);
    host_ip(&["-n", &outside.name, "link", "set", "dev", &peer, "up"]);
    host_ip(&["-n", &outside.name, "link", "set", "dev", "lo", "up"]);
    host_ip(&[
        "-n",
        &outside.name,
        "route",
        "add",
        "default",
        "via",
        "198.18.255.249",
    ]);
    outside
}

fn forward_drop_packets(inbound: bool) -> u64 {
    let output = Command::new("/usr/bin/sudo")
        .args([
            "-n",
            "/usr/sbin/nft",
            "-j",
            "list",
            "chain",
            "inet",
            "motor_net",
            "forward",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rules: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    for entry in rules["nftables"].as_array().unwrap() {
        let Some(expressions) = entry["rule"]["expr"].as_array() else {
            continue;
        };
        if expressions
            .last()
            .is_none_or(|expression| expression.get("drop").is_none())
        {
            continue;
        }
        let matcher = &expressions[0]["match"];
        let (key, op) = if inbound {
            ("oifname", "==")
        } else {
            ("iifname", "!=")
        };
        if matcher["left"]["meta"]["key"] == key
            && matcher["op"] == op
            && matcher["right"] == "mnet-*"
        {
            return expressions
                .iter()
                .find_map(|expression| expression["counter"]["packets"].as_u64())
                .expect("forward drop rule needs a packet counter");
        }
    }
    panic!("required motor_net forward drop rule is missing (inbound={inbound})");
}

async fn forward_drops(f: &Fixture, runner: &Runner) {
    let (mut request, bytes) = f.run("python3 network-check.py inbound-listener");
    request.network = Some(Network::Outbound);
    f.submit(runner, &request, &bytes);
    until(|| Path::new(&f.key).join("inbound-ready").exists()).await;
    let job: Job = read_json(&runner.record_path(request.job_id)).unwrap();
    let name = crate::network::linux::interface(job.process.unwrap().unit.unwrap());
    let output = Command::new("/usr/bin/sudo")
        .args([
            "-n",
            "/usr/sbin/ip",
            "-j",
            "-4",
            "addr",
            "show",
            "dev",
            &name,
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let links: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let gateway: std::net::Ipv4Addr = links[0]["addr_info"][0]["local"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let address = std::net::Ipv4Addr::from(u32::from(gateway) + 1).to_string();
    let outside = outside_namespace();
    for (inbound, destination, port) in
        [(true, address.as_str(), "18080"), (false, "1.1.1.1", "443")]
    {
        let before = forward_drop_packets(inbound);
        // A routed, separate namespace stands in for an outside sender. Counter
        // increments prove motor_net itself drops the SYN before ufw sees it;
        // a timeout alone would not establish which firewall stopped the packet.
        let output = Command::new("/usr/bin/sudo").args(["-n", "/usr/sbin/ip", "netns", "exec", &outside.name, "/usr/bin/python3", "-c",
            "import socket,sys\ns=socket.socket(); s.settimeout(0.4)\ntry: s.connect((sys.argv[1], int(sys.argv[2])))\nexcept socket.timeout: print('silent drop')\nelse: raise AssertionError('unexpected inbound/forwarded TCP connection')", destination, port]).output().unwrap();
        assert!(
            output.status.success(),
            "outside probe {destination}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let after = forward_drop_packets(inbound);
        assert!(
            after > before,
            "motor_net did not drop the actual forwarded packet (inbound={inbound})"
        );
        println!("PASS forwarded SYN to {destination}:{port}: motor_net drop counter {before} -> {after}");
    }
    assert_success(runner, &request).await;
}
