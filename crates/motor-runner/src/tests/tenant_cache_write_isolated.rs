//! Real tenant admission, sandbox and pinned compiler-cache client. Only the
//! upstream object store is in memory; identity, permissions and socket binding
//! use the same checks as the runner service.
use super::super::remote_cache_rollout::{provision, provision_generation};
use super::*;
use crate::remote_cache::gateway::tests::{FakeConnector, FakeStore, BUCKET, TENANT};
use crate::remote_cache::gateway::{Gateway, Limits, Tool};
use crate::remote_cache::provisioning::{HostRole, KeyRole, Layout, Watch};
use crate::remote_cache::runtime::Runtime;
use motor_protocol::RemoteCacheTotals;

struct TenantCache {
    runner: Runner,
    cache: Arc<Runtime>,
    store: Arc<FakeStore>,
    layout: Layout,
    fixture: Fixture,
}

impl TenantCache {
    fn new() -> Self {
        assert_eq!(unsafe { libc::geteuid() }, 0, "requires the root operator");
        let version = std::process::Command::new("sccache")
            .arg("--version")
            .output()
            .expect("pinned sccache must be installed");
        assert!(version.status.success(), "sccache --version failed");
        assert_eq!(
            String::from_utf8(version.stdout).unwrap().trim(),
            "sccache 0.18.0",
            "requires the image's pinned sccache"
        );
        let f = Fixture::new();
        for name in ["dep", "denied", "budget", "blocked"] {
            fs::write(
                f.main.join(format!("{name}.rs")),
                format!("pub fn {name}() -> u32 {{ 42 }}\n"),
            )
            .unwrap();
        }
        // Incompressible data keeps the real sccache upload larger than every
        // transport buffer, so cancellation cannot race an already sent body.
        let mut payload = vec![0; 2 * 1024 * 1024];
        rand::RngCore::fill_bytes(&mut StdRng::seed_from_u64(817), &mut payload);
        fs::write(f.main.join("payload.bin"), payload).unwrap();
        fs::write(
            f.main.join("large.rs"),
            "pub static PAYLOAD: &[u8] = include_bytes!(\"payload.bin\");\n",
        )
        .unwrap();
        git(&f.main, &["add", "."]);
        git(
            &f.main,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-m",
                "cacheable compiler inputs",
            ],
        );
        let layout = Layout::new(f.scratch.join("provisioning"), HostRole::TenantVm);
        layout.prepare().unwrap();
        provision(&layout, true);
        let watch = Watch::new(layout.clone());
        watch.reread();
        let store = Arc::new(FakeStore::default());
        let gateway = Gateway::with_connector(
            Limits {
                max_job_put_objects: 1,
                ..Limits::default()
            },
            Arc::new(watch.clone()),
            Arc::new(FakeConnector(store.clone())),
        );
        let cache = Runtime::with_gateway(watch, gateway, vec![(Tool::Sccache, 3893)]).unwrap();
        let mut config = f.config();
        config.manual_tick = true;
        config.isolate_jobs = true;
        config.tenant_vm = true;
        config.disable_warm_builds = true;
        config.account_subject = Some(TENANT.into());
        config.remote_cache = true;
        config.remote_cache_runtime = Some(cache.clone());
        let runner = Runner::open(config).expect("real tenant isolation prerequisites");
        Self {
            runner,
            cache,
            store,
            layout,
            fixture: f,
        }
    }

    async fn queue(&self, command: &str, claim: bool, account: &str) -> Run {
        let (mut run, bundles) = self.fixture.run(command);
        run.siblings.clear();
        run.bundles.truncate(1);
        run.weight_hint = Some(2);
        run.timeout = Some(90);
        let mut token = lease(run.job_id, LeasePurpose::Run);
        token.sub = account.into();
        token.cache_write = claim;
        token.max_s = 90;
        let (mut client, task) = connect(&self.runner, token).await;
        motor_protocol::write_frame_async(&mut client, &Frame::Run(run.clone()))
            .await
            .unwrap();
        assert!(matches!(frame(&mut client).await, Some(Frame::Accepted(_))));
        for (offset, bytes) in bundles[0].chunks(64 * 1024).enumerate() {
            motor_protocol::write_frame_async(
                &mut client,
                &Frame::BundleChunk {
                    index: 0,
                    offset: (offset * 64 * 1024) as u64,
                    data: BytePayload(bytes.to_vec()),
                },
            )
            .await
            .unwrap();
        }
        until_with_diagnostics(
            || self.runner.job_status(run.job_id).unwrap().state == "queued",
            || runner_diagnostics(&self.runner),
        )
        .await;
        drop(client);
        task.await.unwrap().unwrap();
        run
    }

    async fn later_tokens(&self, run: &Run, admitted: bool, account: &str) {
        for purpose in [LeasePurpose::Run, LeasePurpose::Attach] {
            for claim in [false, true] {
                let mut token = lease(run.job_id, purpose);
                token.sub = account.into();
                token.cache_write = claim;
                token.max_s = 90;
                let (mut client, task) = connect(&self.runner, token).await;
                let request = match purpose {
                    LeasePurpose::Run => Frame::Run(run.clone()),
                    LeasePurpose::Attach => Frame::Attach(AttachRequest::new(run.job_id, 0)),
                };
                motor_protocol::write_frame_async(&mut client, &request)
                    .await
                    .unwrap();
                if purpose == LeasePurpose::Run {
                    assert!(
                        frame(&mut client).await.is_none(),
                        "duplicate Run must close"
                    );
                }
                drop(client);
                task.await.unwrap().unwrap();
                assert_eq!(
                    self.runner.core.inner.lock().unwrap().jobs[&run.job_id].cache_write,
                    admitted
                );
            }
        }
    }

    async fn finish(&self, run: &Run) -> RemoteCacheTotals {
        self.runner.tick().unwrap();
        let terminal = terminal(&self.runner, run.job_id).await;
        let output = job_output(&self.runner, run.job_id);
        eprintln!("job {}: {output}", run.job_id);
        assert_eq!(terminal.outcome, Outcome::Exit { code: 0 }, "{output}");
        let job: Job = read_json(&self.runner.record_path(run.job_id)).unwrap();
        job.remote_cache_totals.expect("real compiler cache totals")
    }
}

fn compile(name: &str, writable: bool, wait_write: bool) -> String {
    format!(
        r#"set -eu
test "$SCCACHE_S3_RW_MODE" = {mode}
test -z "${{SCCACHE_RECACHE+x}}"
test -z "${{SCCACHE_MULTILEVEL_WRITE_ERROR_POLICY+x}}"
test -z "${{SCCACHE_MULTILEVEL_CHAIN+x}}"
test -z "${{SCCACHE_DIR+x}}"
mkdir -p out
rm -f out/lib{name}.rlib
"$RUSTC_WRAPPER" "$(rustc --print sysroot)/bin/rustc" --crate-type=rlib --crate-name {name} --emit=link --out-dir out {name}.rs
test -s out/lib{name}.rlib
{wait}
sccache --show-stats
"#,
        mode = if writable { "READ_WRITE" } else { "READ_ONLY" },
        wait = if wait_write {
            r#"for i in $(seq 300); do
    sccache --show-stats --stats-format=json > stats.json
    if python3 -c 'import json; s=json.load(open("stats.json"))["stats"]; exit(not (s["cache_writes"] + s["cache_write_errors"]))'; then break; fi
    sleep 0.05
done
python3 -c 'import json; s=json.load(open("stats.json"))["stats"]; assert s["cache_writes"] + s["cache_write_errors"] >= 1, s'"#
        } else {
            ""
        },
    )
}

fn put(key: &str, expected: u16, duplicate: bool) -> String {
    format!(
        r#"python3 - <<'PY'
import http.client, os, urllib.parse
url = urllib.parse.urlsplit(os.environ['SCCACHE_ENDPOINT'])
c = http.client.HTTPConnection(url.hostname, url.port, timeout=10)
path = '/{BUCKET}/{key}'
body = b'job upload'
if {duplicate}:
    c.request('GET', path)
    r = c.getresponse()
    assert r.status == 200, r.status
    body = r.read()
c.request('PUT', path, body)
r = c.getresponse()
assert r.status == {expected}, r.status
r.read()
c.close()
PY
"#,
        duplicate = if duplicate { "True" } else { "False" }
    )
}

#[tokio::test]
#[ignore = "requires root, systemd, motor-job accounts, reflink scratch and pinned sccache 0.18.0"]
async fn isolated_tenant_cache_writes_use_admitted_claims() {
    let h = TenantCache::new();
    let creator = h.queue(&compile("dep", true, true), true, TENANT).await;
    h.later_tokens(&creator, true, TENANT).await;
    let totals = h.finish(&creator).await;
    assert_eq!((totals.hits, totals.misses), (0, 1));
    let writes: Vec<_> = h
        .store
        .seen()
        .into_iter()
        .filter(|r| r.method == "PUT" && !r.path.ends_with("/.sccache_check"))
        .collect();
    assert_eq!(
        writes.len(),
        1,
        "one finished compiler output, not a folder copy"
    );
    let entry = &writes[0];
    assert!(!entry.body.is_empty());
    assert_eq!(entry.header("if-none-match"), Some("*"));
    assert!(entry
        .header("authorization")
        .unwrap()
        .contains("if-none-match"));
    assert!(totals.puts_accepted >= 1);
    assert!(totals.put_bytes_accepted >= entry.body.len() as u64);
    let retained = h.store.objects();

    let reader = h.queue(&compile("dep", true, false), true, TENANT).await;
    let totals = h.finish(&reader).await;
    assert_eq!(
        (totals.hits, totals.misses),
        (1, 0),
        "a new job must hit S3, not a local tier"
    );
    assert_eq!(h.store.objects(), retained);

    let prefix = crate::remote_cache::gateway::tests::stored(Tool::Sccache, "");
    let key = entry
        .path
        .strip_prefix(&prefix)
        .expect("account-bound sccache key");
    let duplicate = h
        .queue(&format!("set -eu\n{}", put(key, 200, true)), true, TENANT)
        .await;
    let totals = h.finish(&duplicate).await;
    assert_eq!(
        (
            totals.puts_duplicate,
            totals.puts_accepted,
            totals.put_bytes_accepted
        ),
        (1, 0, 0)
    );
    assert_eq!(
        h.store.objects(),
        retained,
        "a duplicate cannot replace compiler bytes"
    );

    let denied = h
        .queue(
            &format!(
                "{}{}",
                compile("denied", false, false),
                put("denied", 403, false)
            ),
            false,
            TENANT,
        )
        .await;
    h.later_tokens(&denied, false, TENANT).await;
    let totals = h.finish(&denied).await;
    assert_eq!(
        (totals.hits, totals.misses, totals.puts_accepted),
        (0, 1, 0)
    );
    assert_eq!(
        h.store.objects(),
        retained,
        "false admission stays read-only after reconnect/attach"
    );

    // Spend this job's single object reservation before invoking the compiler.
    let budget = h
        .queue(
            &format!(
                "set -eu\n{}{}",
                put("budget-reservation", 200, false),
                compile("budget", true, true)
            ),
            true,
            TENANT,
        )
        .await;
    let totals = h.finish(&budget).await;
    assert_eq!((totals.hits, totals.misses), (0, 1));
    assert!(
        totals.puts_refused >= 1,
        "compile upload must meet the exhausted budget"
    );
    assert!(h.store.seen().iter().all(|r| r.method != "PUT"
        || r.path == entry.path
        || r.path.ends_with("/.sccache_check")
        || r.path.ends_with("/budget-reservation")));

    // Block after admission: the fixed write-enabled socket must deny writes,
    // rather than this case merely selecting read-only at admission.
    let blocked = h.queue(&compile("blocked", true, true), true, TENANT).await;
    h.cache.gateway.set_job_write_block(true).await;
    let before_block = h.store.objects();
    let totals = h.finish(&blocked).await;
    assert_eq!((totals.hits, totals.misses), (0, 1));
    assert!(totals.puts_refused >= 1);
    assert_eq!(h.store.objects(), before_block);
    h.cache.gateway.set_job_write_block(false).await;

    h.store.slow_uploads(4096, Duration::from_millis(5));
    let heads = h.store.upload_heads().len();
    let cancelling = h
        .queue(
            &format!(
                "{}\nwhile :; do sleep 1; done",
                compile("large", true, false)
            ),
            true,
            TENANT,
        )
        .await;
    h.runner.tick().unwrap();
    until_with_diagnostics(
        || {
            h.store.upload_heads()[heads..]
                .iter()
                .any(|p| !p.ends_with("/.sccache_check"))
        },
        || {
            format!(
                "no compiler upload: {}",
                job_output(&h.runner, cancelling.job_id)
            )
        },
    )
    .await;
    let pending = h.store.upload_heads()[heads..]
        .iter()
        .find(|p| !p.ends_with("/.sccache_check"))
        .unwrap()
        .clone();
    assert!(
        !h.store.objects().contains_key(&pending),
        "cancel must interrupt a streaming body"
    );
    h.runner.cancel(cancelling.job_id).unwrap();
    let terminal = terminal(&h.runner, cancelling.job_id).await;
    assert_eq!(terminal.killed, Some(motor_protocol::Killed::Cancel));
    until(|| h.store.interrupted_uploads().contains(&pending)).await;
    assert_eq!(
        h.store.objects(),
        before_block,
        "no partial object; prior invocations survive"
    );
}

#[tokio::test]
#[ignore = "requires root, systemd, motor-job accounts, reflink scratch and pinned sccache 0.18.0"]
async fn isolated_tenant_cache_denials_survive_later_credentials_and_attach() {
    let h = TenantCache::new();
    let id = Uuid::now_v7();
    let mut token = lease(id, LeasePurpose::Attach);
    token.sub = TENANT.into();
    token.cache_write = true;
    let (mut client, task) = connect(&h.runner, token).await;
    motor_protocol::write_frame_async(&mut client, &Frame::Attach(AttachRequest::new(id, 0)))
        .await
        .unwrap();
    // An unknown attach has no execution to authorize, even with the claim.
    while frame(&mut client).await.is_some() {}
    task.await.unwrap().unwrap();
    assert!(!h.runner.has_job_record(id));

    for (index, cause) in ["keyless", "blocked", "wrong-account", "rotation"]
        .into_iter()
        .enumerate()
    {
        let other = "acct:01KXJJHK4V6DB42W1YAN2XD9QP";
        if cause == "keyless" {
            fs::remove_file(h.layout.key_path(KeyRole::Write)).unwrap();
            h.cache.watch.reread();
        }
        if cause == "blocked" {
            h.cache.gateway.set_job_write_block(true).await;
        }
        let account = if cause == "wrong-account" {
            other
        } else {
            TENANT
        };
        let run = h
            .queue(
                &format!(
                    "{}{}",
                    compile("denied", false, false),
                    put(cause, 403, false)
                ),
                true,
                account,
            )
            .await;
        let admitted = cause == "rotation";
        assert_eq!(
            h.runner.core.inner.lock().unwrap().jobs[&run.job_id].cache_write,
            admitted,
            "{cause}"
        );
        if cause == "rotation" {
            provision_generation(&h.layout, true, other, &format!("rotated-{index}"));
        } else {
            provision_generation(&h.layout, true, TENANT, &format!("restored-{index}"));
        }
        h.cache.watch.reread();
        h.cache.gateway.set_job_write_block(false).await;
        h.later_tokens(&run, admitted, account).await;
        let totals = h.finish(&run).await;
        assert_eq!(
            (totals.hits, totals.misses, totals.puts_accepted),
            (0, 1, 0),
            "{cause}"
        );
        assert!(
            h.store.seen().iter().all(|r| r.method != "PUT"),
            "{cause}: real job PUT reached store"
        );
        provision_generation(&h.layout, true, TENANT, &format!("next-{index}"));
        h.cache.watch.reread();
    }
}
