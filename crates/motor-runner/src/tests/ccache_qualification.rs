//! Real ccache against the production HTTP adapter and forwarder. The portable
//! runner backend supplies fill scheduling/publication, not Linux isolation;
//! its private cache/output paths are translated to fixture paths explicitly.
use super::*;
use crate::remote_cache::ccache;
use crate::remote_cache::transport::tests::{
    forwarder_binary, free_port, http, start_forwarder, FakeCache,
};
use crate::remote_cache::transport::{Forwarder, Role};
use std::process::Command;

fn compile(workspace: &Path, env: &BTreeMap<String, String>, cached: bool) -> Vec<i32> {
    [
        ("cc", "unit.c", "unit.o"),
        ("c++", "unit.cpp", "unit-cpp.o"),
    ]
    .iter()
    .map(|(compiler, source, object)| {
        let mut command = Command::new(if cached { "ccache" } else { compiler });
        if cached {
            command.arg(compiler);
        }
        let output = command
            .arg("-g")
            .arg(format!("-fdebug-prefix-map={}=.", workspace.display()))
            .args(["-c", source, "-o", object])
            .current_dir(workspace)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .envs(env)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.status.code().unwrap()
    })
    .collect()
}

fn stats(env: &BTreeMap<String, String>) -> Vec<u8> {
    let output = Command::new("ccache")
        .arg("--print-stats")
        .envs(env)
        .output()
        .unwrap();
    assert!(output.status.success());
    output.stdout
}

#[test]
#[ignore = "requires pinned ccache 4.12.3, sccache and native C/C++ compilers on the build server"]
fn pinned_ccache_no_cargo_fill_publishes_and_private_reader_hits_http_remote() {
    // The cache owns a separate Tokio runtime, which must outlive block_on and
    // be dropped in this synchronous context, including on an assertion panic.
    let cache = FakeCache::new();
    tokio::runtime::Runtime::new().unwrap().block_on(async {
    assert!(ccache::installed(
        crate::remote_cache::provisioning::HostRole::BuildServer
    ));
    assert!(Command::new("sccache")
        .arg("--version")
        .status()
        .unwrap()
        .success());
    let f = Fixture::new();
    fs::write(f.main.join("unit.c"), "int answer(void) { return 42; }\n").unwrap();
    fs::write(f.main.join("unit.cpp"), "int cpp_answer() { return 43; }\n").unwrap();
    let fill_port = free_port();
    let transport = cache.transport(&[(Tool::Ccache, fill_port)]);
    let sockets = transport
        .open(&f.scratch.join("ccache-fill"), Role::Fill)
        .unwrap();
    let binary = forwarder_binary(&f.scratch);
    let (status, _fill_forwarder) = start_forwarder(
        &binary,
        &f.scratch.join("ccache-fill"),
        fill_port,
        "ccache.sock",
    );
    assert!(status.success());
    let mut env = ccache::environment(&Forwarder::new(Tool::Ccache, fill_port), true, "/unused");
    env.insert(
        "CCACHE_DIR".into(),
        f.scratch
            .join("fill-private-cache")
            .to_string_lossy()
            .into(),
    );
    env.remove("CCACHE_BASEDIR");
    let exports = env
        .iter()
        .map(|(k, v)| format!("export {k}='{}'; ", v.replace('\'', "'\\''")))
        .collect::<String>();
    let collected = f.scratch.join("fill-collected");
    let captured = f.scratch.join("fill-ccache-stats");
    let command = format!(
        "{}; /bin/bash -c '{}' || exit $?; {exports} export CCACHE_BASEDIR=\"$PWD\"; ccache cc -g -fdebug-prefix-map=\"$PWD\"=. -c unit.c -o unit.o && ccache c++ -g -fdebug-prefix-map=\"$PWD\"=. -c unit.cpp -o unit-cpp.o; code=$?; ccache --print-stats >'{}.tmp' && mv '{}.tmp' '{}'; while [ ! -e '{}' ]; do sleep 0.01; done; exit \"$code\"",
        controlled_command(&f),
        crate::writer::fill::CARGO_CHECK.replace('\'', "'\\''"),
        captured.display(), captured.display(), captured.display(), collected.display(),
    );
    // No root Cargo.toml: the declaration, not Cargo's default warm command,
    // must queue and execute the writer. Native sources are descriptor inputs.
    declare(&f, &command);
    fs::write(f.main.join(".motor/cache.toml"), format!("[[fill]]\ntool = \"ccache\"\ncommands = [{}]\ndescriptor-inputs = [\"unit.c\", \"unit.cpp\"]\n", serde_json::to_string(&command).unwrap())).unwrap();
    commit(&f.main, "C/C++ ccache declaration\n");
    assert!(!f.main.join("Cargo.toml").exists());
    let runner = controlled_runner(&f);
    submit_reader(&f, &runner).await;
    let job = running_fill(&runner, 0).await;
    let ledgers = sockets.ledgers();
    assert_eq!(ledgers.len(), 1);
    assert_eq!(ledgers[0].tool(), Tool::Ccache);
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .fill_ledgers
        .insert(job.id, ledgers);
    fs::write(release_path(&f), "").unwrap();
    until(|| captured.is_file()).await;
    let bytes = fs::read(&captured).unwrap();
    let fill_stats = ccache::statistics(&bytes).unwrap();
    assert!(fill_stats["remote_storage_write"] > 0);
    assert_eq!(fill_stats[ccache::REMOTE_HIT], 0);
    assert_eq!(fill_stats[ccache::REMOTE_ERROR], 0);
    assert_eq!(fill_stats[ccache::REMOTE_TIMEOUT], 0);
    let output = runner
        .core
        .trees
        .state
        .join("sandboxes")
        .join(job.id.to_string())
        .join("output");
    fs::create_dir_all(&output).unwrap();
    fs::write(output.join(ccache::STATS_FILE), &bytes).unwrap();
    // The installed but inactive sccache must not decide ccache publication.
    fs::write(
        output.join("cache-stats"),
        "inactive sccache: no statistics",
    )
    .unwrap();
    fs::write(&collected, "").unwrap();
    settle(&f, &runner, &job).await;
    let record = repository(&f, &runner);
    let publication = &record.publications[job.writer_descriptor.as_ref().unwrap()];
    assert!(!publication.keys.is_empty());
    assert!(publication.keys.iter().all(|key| key.tool == "ccache"));
    assert!(record.publish_failures.is_empty());

    let refresh = f.scratch.join("refresh-workspace");
    fs::create_dir_all(&refresh).unwrap();
    for name in ["unit.c", "unit.cpp"] {
        fs::copy(f.main.join(name), refresh.join(name)).unwrap();
    }
    let mut refresh_env = ccache::environment(
        &Forwarder::new(Tool::Ccache, fill_port),
        true,
        refresh.to_str().unwrap(),
    );
    refresh_env.insert(
        "CCACHE_DIR".into(),
        f.scratch
            .join("refresh-private-cache")
            .to_string_lossy()
            .into(),
    );
    compile(&refresh, &refresh_env, true);
    let refresh_stats = ccache::statistics(&stats(&refresh_env)).unwrap();
        // A fill must regenerate and upload entries even when remote storage already has them.
    assert_eq!(refresh_stats[ccache::REMOTE_HIT], 0);
    assert_eq!(refresh_stats["recache"], 2);
    assert!(refresh_stats["remote_storage_write"] >= 2);
    assert_eq!(refresh_stats[ccache::REMOTE_ERROR], 0);
    assert_eq!(refresh_stats[ccache::REMOTE_TIMEOUT], 0);

    let reader = f.scratch.join("caller-workspace");
    let uncached = f.scratch.join("uncached-workspace");
    for workspace in [&reader, &uncached] {
        fs::create_dir_all(workspace).unwrap();
        for name in ["unit.c", "unit.cpp"] {
            fs::copy(f.main.join(name), workspace.join(name)).unwrap();
        }
    }
    let reader_port = free_port();
    let read_transport = cache.transport(&[(Tool::Ccache, reader_port)]);
    let _read_sockets = read_transport
        .open(&f.scratch.join("ccache-job"), Role::Job("reader"))
        .unwrap();
    let (status, _read_forwarder) = start_forwarder(
        &binary,
        &f.scratch.join("ccache-job"),
        reader_port,
        "ccache.sock",
    );
    assert!(status.success());
    let mut reader_env = ccache::environment(
        &Forwarder::new(Tool::Ccache, reader_port),
        false,
        reader.to_str().unwrap(),
    );
    let private_cache = f.scratch.join("reader-private-cache");
    fs::create_dir(&private_cache).unwrap();
    assert_eq!(fs::read_dir(&private_cache).unwrap().count(), 0);
    reader_env.insert("CCACHE_DIR".into(), private_cache.to_string_lossy().into());
    let plain = compile(&uncached, &BTreeMap::new(), false);
    let cached = compile(&reader, &reader_env, true);
    assert_eq!(cached, plain);
    for object in ["unit.o", "unit-cpp.o"] {
        assert_eq!(
            fs::read(reader.join(object)).unwrap(),
            fs::read(uncached.join(object)).unwrap()
        );
    }
    let bytes = stats(&reader_env);
    fs::write(output.join(ccache::STATS_FILE), &bytes).unwrap();
    let reader_stats =
        crate::writer::fill::ccache_statistics(&runner.core.trees.state, job.id).unwrap();
    assert!(reader_stats[ccache::REMOTE_HIT] >= 2, "{reader_stats:?}");
    assert_eq!(reader_stats[ccache::REMOTE_ERROR], 0);
    assert_eq!(reader_stats[ccache::REMOTE_TIMEOUT], 0);
    assert_eq!(reader_stats["local_storage_hit"], 0);
    assert_eq!(reader_stats["local_storage_write"], 0);
    assert_eq!(reader_stats["remote_storage_write"], 0);
    assert_eq!(
        http(reader_port, "PUT", "/job-write", b"refused")
            .unwrap()
            .0,
        403
    );
    eprintln!("ccache {}: published {} keys; reader remote_storage_hit={}, remote_storage_error=0, remote_storage_timeout=0; exit codes {cached:?}", ccache::VERSION, publication.keys.len(), reader_stats[ccache::REMOTE_HIT]);
    });
}
