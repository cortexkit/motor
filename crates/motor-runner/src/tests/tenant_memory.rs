use super::*;

#[tokio::test]
#[ignore = "requires root operator, motor-job account and real systemd cgroups"]
async fn isolated_per_job_terminal_reports_memory_and_measures_normal_accounting_wait() {
    assert_eq!(unsafe { libc::geteuid() }, 0, "run as the root operator");
    for burst in [false, true] {
        let f = Fixture::new();
        let mut config = f.config();
        config.isolate_jobs = true;
        config.tenant_vm = true;
        config.per_job = true;
        config.cpu_topology = None;
        config.manual_tick = true;
        config.disable_warm_builds = true;
        config.slice_budget_probe = Some(Arc::new(|| Ok((64 << 30, None))));
        let runner = Runner::open(config).unwrap();
        let allocation = if burst { 300 } else { 64 };
        let (mut request, bundles) = f.run(&format!(
            "python3 -c 'import time; b = bytearray({allocation} * 2**20); time.sleep(3)'"
        ));
        request.weight_hint = Some(1);
        let id = request.job_id;
        let mut verified = lease(id, LeasePurpose::Run);
        verified.vcpus = 1;
        verified.mem_mib = Some(256);
        verified.mem_burst_mib = burst.then_some(128);
        let (mut client, task) = connect(&runner, verified).await;
        write_frame_async(&mut client, &Frame::Run(request))
            .await
            .unwrap();
        assert!(matches!(frame(&mut client).await, Some(Frame::Accepted(_))));
        for (index, bytes) in bundles.into_iter().enumerate() {
            write_frame_async(
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
        assert_eq!(
            terminal(&runner, id).await.outcome,
            Outcome::Exit { code: 0 }
        );
        let observed_terminal = std::time::Instant::now();
        let frame =
            tokio::time::timeout(Duration::from_secs(10), per_job::raw_terminal(&mut client))
                .await
                .unwrap();
        eprintln!(
            "normal exit burst={burst} terminal_side_wait_ms={}",
            observed_terminal.elapsed().as_millis()
        );
        let limit = if burst { 384 } else { 256 };
        assert!((allocation..=limit)
            .contains(&frame["mem_peak_mib"].as_u64().expect("job cgroup peak")));
        assert!((allocation..=limit).contains(
            &frame["mem_working_peak_mib"]
                .as_u64()
                .expect("sampled working peak")
        ));
        if burst {
            assert!(frame["mem_over_mib_ms"]
                .as_u64()
                .is_some_and(|over| over > 0));
        } else {
            assert!(frame.get("mem_over_mib_ms").is_none());
        }
        write_frame_async(&mut client, &Frame::TerminalStored { jti: id })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

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
