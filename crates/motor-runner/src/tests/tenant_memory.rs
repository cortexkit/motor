use super::*;

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires root operator, motor-job account and systemd cgroup OOM support"]
async fn isolated_tenant_stated_memory_kills_above_limit_and_allows_below() {
    assert_eq!(unsafe { libc::geteuid() }, 0, "run as the root operator");
    let f = Fixture::new();
    let mut config = f.config();
    config.isolate_jobs = true;
    config.tenant_vm = true;
    config.cpu_topology = None;
    config.manual_tick = true;
    // Admission must not depend on the operator's live capacity.json. The
    // actual systemd slice still contains these units and keeps its own limit.
    config.slice_budget_probe = Some(Arc::new(|| Ok((64 << 30, None))));
    let runner = Runner::open(config).unwrap();
    for above in [false, true] {
        let python = if above {
            "import time; time.sleep(2); b = bytearray(3 * 2**30)"
        } else {
            "import time; b = bytearray(64 * 2**20); time.sleep(2)"
        };
        let command = format!(
            "group=$(awk -F: '$1 == \"0\" {{print $3}}' /proc/self/cgroup); cat \"/sys/fs/cgroup${{group}}/memory.max\"; python3 -c '{python}'"
        );
        let (mut request, bundles) = f.run(&command);
        request.weight_hint = Some(1);
        request.timeout = Some(60);
        let id = request.job_id;
        let mut verified = lease(id, LeasePurpose::Run);
        verified.vcpus = 1;
        verified.mem_mib = Some(2048);
        verified.max_s = 60;
        let (mut client, task) = connect(&runner, verified).await;
        motor_protocol::write_frame_async(&mut client, &Frame::Run(request))
            .await
            .unwrap();
        assert!(matches!(frame(&mut client).await, Some(Frame::Accepted(_))));
        for (index, bytes) in bundles.into_iter().enumerate() {
            motor_protocol::write_frame_async(
                &mut client,
                &Frame::BundleChunk {
                    index: index as u32,
                    offset: 0,
                    data: BytePayload(bytes),
                },
            )
            .await
            .unwrap();
        }
        until(|| runner.job_status(id).unwrap().state == "queued").await;
        runner.tick().unwrap();
        let result = terminal(&runner, id).await;
        assert_eq!(result.memory_stated_mib(), Some(2048));
        assert!(result.memory_peak_working_mib().is_some());
        assert!(job_output(&runner, id).contains("2147483648\n"));
        if above {
            assert_ne!(result.outcome, Outcome::Exit { code: 0 });
            assert_eq!(
                result.oom_kill().map(|kill| kill.cause()),
                Some("job_limit")
            );
        } else {
            assert_eq!(result.outcome, Outcome::Exit { code: 0 });
            assert_eq!(result.oom_kill(), None);
        }
        loop {
            if matches!(frame(&mut client).await, Some(Frame::Terminal(_))) {
                break;
            }
        }
        task.await.unwrap().unwrap();
    }
}
