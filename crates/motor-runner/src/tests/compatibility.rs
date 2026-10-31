//! Pins serialized records and wire payloads that keep caller paths unchanged.
use super::*;
use crate::compatibility_config::{assert_identity, configure, modes};
use crate::storage::{
    BaseRequest, Catalog, LegacyCheckout, Repository, RetainedTransfer, Workspace,
};

const ROOT: &str = "/Users/u/repo";
const SIBLING: &str = "/Users/u/library";
const ID: &str = "00000000-0000-4000-8000-000000000001";

fn request() -> Run {
    Run {
        job_id: Uuid::parse_str(ID).unwrap(),
        workspace_key: ROOT.into(),
        repository_root: ROOT.into(),
        cwd: format!("{ROOT}/src"),
        command: "cargo test".into(),
        env: BTreeMap::from([("RUST_LOG".into(), "info".into())]),
        weight_hint: None,
        timeout: None,
        queue_wait_limit_s: 300,
        snapshot_commit: "1111111111111111111111111111111111111111".into(),
        tree_hash: "2222222222222222222222222222222222222222".into(),
        integration_tip: "3333333333333333333333333333333333333333".into(),
        siblings: vec![SiblingSnapshot {
            canonical_path: SIBLING.into(),
            commit: "4444444444444444444444444444444444444444".into(),
            tree_hash: "5555555555555555555555555555555555555555".into(),
            base_commit: None,
        }],
        bundles: Vec::new(),
        git_dependencies: Vec::new(),
        base_commit: None,
        return_paths: vec![format!("{ROOT}/out")],
        return_max_bytes: motor_protocol::RETURN_MAX_BYTES,
        return_chunked: false,
        gate_job: false,
        network: None,
        platform: None,
        workspace_kind: None,
    }
}

fn assert_bytes(actual: &[u8], expected: &[u8]) {
    assert_eq!(
        actual,
        expected,
        "identity bytes changed:\n{}",
        String::from_utf8_lossy(actual)
    );
}

#[test]
fn users_catalog_matches_pre_mapping_bytes() {
    for &enabled in modes() {
        users_catalog_case(enabled);
    }
}

fn users_catalog_case(enabled: bool) {
    let scratch = ScratchDir::new("motor-compat-catalog");
    let mut config = Config::new(scratch.join("state"), "main");
    configure(&mut config, &scratch, enabled);
    let run = request();
    let mut catalog = Catalog::default();
    catalog.workspaces.insert(
        ROOT.into(),
        Workspace {
            repository: ROOT.into(),
            snapshot: run.snapshot_commit.clone(),
            siblings: run.siblings.clone(),
            used_ms: 100,
            ..Workspace::default()
        },
    );
    catalog.repositories.insert(
        ROOT.into(),
        Repository {
            desired: Some(BaseRequest {
                tip: run.integration_tip.clone(),
                workspace: ROOT.into(),
                siblings: run.siblings.clone(),
                git_dependencies: Vec::new(),
                command: Some("cargo check".into()),
            }),
            ..Repository::default()
        },
    );
    catalog.legacy_checkouts.insert(
        "/Users/u/old-repo".into(),
        LegacyCheckout {
            repository: ROOT.into(),
            tip: Some(run.integration_tip.clone()),
        },
    );
    catalog
        .sibling_commits
        .insert(SIBLING.into(), run.siblings[0].commit.clone());
    catalog.transferred.insert(
        run.job_id,
        RetainedTransfer {
            repository: ROOT.into(),
            snapshot: run.snapshot_commit,
            tip: run.integration_tip,
            siblings: run.siblings,
            finished_ms: 200,
        },
    );
    catalog.mirror_used_ms.insert("mirror".into(), 150);
    catalog.compile_cache_used_ms = Some(200);
    for plan in crate::caller_path::keys(&catalog.workspaces)
        .chain(crate::caller_path::keys(&catalog.repositories))
        .chain(crate::caller_path::keys(&catalog.legacy_checkouts))
        .chain(crate::caller_path::keys(&catalog.sibling_commits))
    {
        assert_identity(&config, plan);
    }
    let path = scratch.join("catalog.json");
    atomic_json(&path, &catalog).unwrap();
    let bytes = fs::read(&path).unwrap();
    assert_bytes(
        &bytes,
        include_bytes!("../../test-fixtures/compatibility/catalog.json"),
    );
    let restored: Catalog = read_json(&path).unwrap();
    assert_bytes(&serde_json::to_vec(&restored).unwrap(), &bytes);
}

#[test]
fn users_job_record_matches_pre_mapping_bytes() {
    for &enabled in modes() {
        users_job_record_case(enabled);
    }
}

fn users_job_record_case(enabled: bool) {
    let scratch = ScratchDir::new("motor-compat-job");
    let mut config = Config::new(scratch.join("state"), "main");
    configure(&mut config, &scratch, enabled);
    let run = request();
    assert_identity(&config, crate::caller_path::request(&run).workspace_path());
    assert_identity(&config, crate::caller_path::request(&run).repository_path());
    assert_identity(&config, crate::caller_path::identity(&run.cwd));
    let mut job = Job::new(run.job_id, Some(run), 100, 1, 2);
    job.queued_ms = Some(110);
    job.setup_ms = Some(120);
    job.started_ms = Some(125);
    job.started = Some(Started::new(0, 10, 125));
    job.next_seq = 1;
    job.cpus = vec![1, 2];
    job.terminal_ended(Outcome::Exit { code: 0 }, 200);
    fs::create_dir_all(scratch.join("jobs")).unwrap();
    let path = scratch.join("jobs").join(ID).join("record.json");
    crate::storage::atomic_job_json(&path, &job).unwrap();
    let bytes = fs::read(&path).unwrap();
    assert_bytes(
        &bytes,
        include_bytes!("../../test-fixtures/compatibility/job-record.json"),
    );
    let restored: Job = read_json(&path).unwrap();
    assert_bytes(&serde_json::to_vec(&restored).unwrap(), &bytes);
}

#[test]
fn users_identity_spellings_keep_pre_mapping_validation() {
    for path in [ROOT, "/Users/u/./repo", "/Users//u/repo", "/Users/u/repo/"] {
        assert!(workspace::absolute(path), "{path}");
        workspace::validate(path, path, &[], false).unwrap();
    }
}

#[test]
fn macos_home_refusal_keeps_pre_mapping_detail() {
    for field in ["workspace", "repository", "cwd", "sibling"] {
        let mut run = request();
        match field {
            "workspace" => run.workspace_key = "/home/u/repo".into(),
            "repository" => run.repository_root = "/home/u/repo".into(),
            "cwd" => run.cwd = "/home/u/repo".into(),
            "sibling" => run.siblings[0].canonical_path = "/home/u/repo".into(),
            _ => unreachable!(),
        }
        assert_eq!(
            crate::macos::validate_paths(&run).unwrap_err().to_string(),
            "macos_path_invalid: /home/u/repo",
            "{field}"
        );
    }
}

#[tokio::test]
async fn users_runner_hello_matches_pre_mapping_bytes() {
    for (snapshot_siblings, caller_roots) in modes().iter().flat_map(|&roots| {
        [false, true]
            .into_iter()
            .map(move |snapshots| (snapshots, roots))
    }) {
        let f = Fixture::new();
        let mut config = if snapshot_siblings {
            super::sibling_snapshots::snapshot_config(&f).0
        } else {
            f.config()
        };
        config.manual_tick = true;
        config.disable_warm_builds = true;
        configure(&mut config, &f.scratch, caller_roots);
        // Hello does not expose compiler versions. Supply a fixed version via
        // the existing toolchain-config seam to skip the startup rustc probe;
        // the backend remains Host, so all Hello capabilities stay unchanged.
        let unused_tool = || crate::macos::Tool {
            path: "/unused-tool".into(),
            sha256: "0".repeat(64),
        };
        config.macos = Some(Arc::new(crate::macos::VmConfig {
            image: "/unused-image".into(),
            helper: "/unused-helper".into(),
            tools: crate::macos::Tools {
                cargo: unused_tool(),
                rustc: unused_tool(),
                bun: unused_tool(),
                git: unused_tool(),
            },
            toolchain_version: "compatibility-rustc".into(),
        }));
        let runner = Runner::open(config).unwrap();
        let (socket, server) = listener(&f, &runner);
        let mut stream = UnixStream::connect(&*socket).await.unwrap();
        let hello = Frame::Hello {
            protocol_version: PROTOCOL_VERSION,
            started: true,
            default_branch_tip: true,
            gate_jobs: true,
            return_paths: true,
            return_chunked_max_bytes: None,
            return_chunked: true,
            sibling_paths: true,
            platforms: Vec::new(),
        };
        write_frame_async(&mut stream, &hello).await.unwrap();
        let response = read_next(&mut stream).await;
        let expected: &[u8] = if snapshot_siblings {
            include_bytes!("../../test-fixtures/compatibility/hello-snapshot.json")
        } else {
            include_bytes!("../../test-fixtures/compatibility/hello.json")
        };
        assert_bytes(&serde_json::to_vec(&response).unwrap(), expected);
        drop(stream);
        server.abort();
    }
}

#[test]
fn identity_non_chunked_return_name_matches_pre_mapping_bytes() {
    for &enabled in modes() {
        identity_non_chunked_return_case(enabled);
    }
}

fn identity_non_chunked_return_case(enabled: bool) {
    let scratch = ScratchDir::new("motor-compat-return");
    let mut config = Config::new(scratch.join("state"), "main");
    configure(&mut config, &scratch, enabled);
    fs::write(scratch.join("out"), b"x").unwrap();
    let mut run = request();
    run.workspace_key = scratch.to_str().unwrap().into();
    run.return_paths = vec![scratch.join("out").to_str().unwrap().into()];
    assert_identity(&config, crate::caller_path::request(&run).workspace_path());
    let result = crate::return_paths::collect(&run, 1);
    let rendered = serde_json::to_string(&Frame::ReturnResult(result))
        .unwrap()
        .replace(scratch.to_str().unwrap(), ROOT);
    assert_bytes(
        rendered.as_bytes(),
        include_bytes!("../../test-fixtures/compatibility/return-result.json"),
    );
}
