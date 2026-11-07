use super::*;
use motor_host::billing::{sign_request, Metrics, OUTBOX_MAX_AGE_MS};
use motor_host::server::LifecycleStream;
use motor_host::state::LeaseState;
use motor_lease::{JobState, UsageOutcome};
use serde_json::{json, Value};

const MS: u64 = NOW * 1000;

#[test]
fn late_runner_terminal_clears_only_its_restart_claim_without_rebilling() {
    for lifecycle in [true, false] {
        let dir = scratch();
        let mut h = disk_host(dir.path());
        started(&mut h, JOB, 1);
        started(&mut h, JOB2, 3);
        h.billing_tick(MS + 330_000).unwrap();
        assert!(h.snapshot.vms["tenant"].stop_pending);
        h.billing_tick(MS + 390_000).unwrap();
        let usage = serde_json::to_vec(&h.snapshot.billing[JOB].usage).unwrap();
        let terminal_before = h.snapshot.billing[JOB].terminal.clone();
        let outbox = serde_json::to_vec(&h.snapshot.outbox).unwrap();
        let logs = Logs::default();
        let captured = logs.0.clone();
        tracing::subscriber::with_default(logs, || {
            if lifecycle {
                h.event(
                    "tenant",
                    &event(5, TenantEventKind::Terminal, Some(JOB), None),
                    MS + 401_000,
                )
                .unwrap();
            } else {
                h.receive_terminal(JOB, terminal(JOB, "exit"), MS + 401_000)
                    .unwrap();
            }
            assert!(
                h.snapshot.vms["tenant"].stop_pending,
                "another lost job still needs a restart"
            );
            h.receive_terminal(JOB2, terminal(JOB2, "exit"), MS + 402_000)
                .unwrap();
            assert!(
                !h.snapshot.vms["tenant"].stop_pending,
                "all lost jobs supplied proof of ending"
            );
        });
        h.billing_tick(MS + 403_000).unwrap();
        assert!(h.stops_due().is_empty());
        assert_eq!(
            serde_json::to_vec(&h.snapshot.billing[JOB].usage).unwrap(),
            usage
        );
        assert_eq!(h.snapshot.billing[JOB].terminal, terminal_before);
        assert_eq!(serde_json::to_vec(&h.snapshot.outbox).unwrap(), outbox);
        let captured = captured.lock().unwrap();
        assert!(
            captured.iter().any(|line| line.starts_with("WARN ")
                && line.contains(JOB)
                && line.contains("delay_since_cap_ms=371000")),
            "{captured:?}"
        );
        drop(h);
        assert!(!disk_host(dir.path()).snapshot.vms["tenant"].stop_pending);
    }
}

#[test]
fn runtime_cap_waits_five_minutes_for_runner_terminal_before_settling_lost() {
    let mut h = host();
    started(&mut h, JOB, 1);
    let cap = MS + 30_000;
    h.billing_tick(cap + 30_000).unwrap();
    assert!(h.snapshot.pending_cancels.contains_key(JOB));
    for delay in [60_000, 101_000, 299_999] {
        h.billing_tick(cap + delay).unwrap();
        assert!(
            !h.snapshot.billing[JOB].host_settled,
            "settled after only {delay}ms"
        );
        assert!(!h.snapshot.vms["tenant"].stop_pending);
    }
    h.billing_tick(cap + 300_000).unwrap();
    assert!(h.snapshot.billing[JOB].host_settled);
    let mut normal = host();
    started(&mut normal, JOB, 1);
    normal.billing_tick(cap + 30_000).unwrap();
    end(&mut normal, JOB, 3, cap + 101_000, terminal(JOB, "exit"));
    normal.billing_tick(cap + 300_000).unwrap();
    assert!(!normal.snapshot.billing[JOB].host_settled);
    assert_eq!(
        normal.snapshot.billing[JOB].usage.as_ref().unwrap().outcome,
        UsageOutcome::Timeout
    );
    assert!(!normal.snapshot.vms["tenant"].stop_pending);
}

#[test]
fn pending_restart_cancels_admitted_without_start_but_keeps_started_lease() {
    let mut h = host();
    h.tenants.get_mut("vm").unwrap().max_weight = Some(4);
    started(&mut h, JOB, 1);
    admitted(&mut h, JOB2, 3);
    started(&mut h, JOB3, 4);
    h.snapshot.leases.get_mut(JOB3).unwrap().claims.max_s = 600;
    h.billing_tick(MS + 330_000).unwrap();
    let cancels = h.expire(MS + 330_000).unwrap();
    assert!(
        cancels.contains(&("tenant".into(), JOB2.into())),
        "queued lease was not cancelled: {cancels:?}"
    );
    assert!(h.stops_due().is_empty(), "Started work must retain its VM");
    end(&mut h, JOB3, 6, MS + 331_000, terminal(JOB3, "exit"));
    assert!(
        h.stops_due().is_empty(),
        "withdrawal still needs a no-start proof"
    );
    h.restart_withdrawn(JOB2, MS + 331_001).unwrap();
    assert_eq!(h.stops_due(), vec!["tenant"]);
    let frames = h.host_settled_frames(JOB2, 0).unwrap();
    assert_eq!(frames[0]["ran"], "none");
    assert_eq!(frames[0]["outcome"]["type"], "refused_before_start");
    assert_eq!(frames[0]["outcome"]["reason"], "queue_wait_exceeded");
    assert_eq!(
        h.snapshot.zero_usage[JOB2].started_at_ms,
        h.snapshot.zero_usage[JOB2].ended_at_ms
    );
    assert!(!h.snapshot.pending_cancels.contains_key(JOB2));
}

#[test]
fn fresh_counter_baselines_and_unexpected_resets_never_bill_negative_or_wrapped_usage() {
    for (baseline, last, expected) in [(0, 9, 9), (100, 0, 0)] {
        let mut h = host();
        admitted(&mut h, JOB, 1);
        h.event(
            "tenant",
            &event(2, TenantEventKind::Started, Some(JOB), None),
            MS,
        )
        .unwrap();
        for bytes in [baseline, last] {
            h.sample(
                "tenant",
                Metrics {
                    memory_bytes: 0,
                    egress_bytes: bytes,
                },
            )
            .unwrap();
        }
        h.event(
            "tenant",
            &event(3, TenantEventKind::Terminal, Some(JOB), None),
            MS + 1000,
        )
        .unwrap();
        h.receive_terminal(JOB, terminal(JOB, "exit"), MS + 1000)
            .unwrap();
        assert_eq!(
            h.snapshot.billing[JOB].usage.as_ref().unwrap().egress_bytes,
            Some(expected)
        );
    }
}
fn admitted(h: &mut Host, job: &str, seq: u64) {
    let (id, _) = h.claim(claims(job), MS).unwrap();
    h.delivered(id).unwrap();
    h.event(
        "tenant",
        &event(seq, TenantEventKind::Admitted, Some(job), None),
        MS,
    )
    .unwrap();
}
pub(super) fn started(h: &mut Host, job: &str, seq: u64) {
    admitted(h, job, seq);
    h.event(
        "tenant",
        &event(seq + 1, TenantEventKind::Started, Some(job), None),
        MS,
    )
    .unwrap();
    h.sample(
        "tenant",
        Metrics {
            memory_bytes: 10 * 1_048_576,
            egress_bytes: 100,
        },
    )
    .unwrap();
}
pub(super) fn terminal(job: &str, outcome: &str) -> Value {
    json!({"type":"terminal","job_id":job,"outcome":{"type":outcome,"code":7,"signal":9},
        "wall_ms":999999999,"started_at_ms":1,"ended_at_ms":999999999,"queue_wait_ms":0,"bundle_bytes":0,"ran":"remote","tree_hash":null,"workspace_changes":null})
}
pub(super) fn end(h: &mut Host, job: &str, seq: u64, at: u64, frame: Value) {
    h.event(
        "tenant",
        &event(seq, TenantEventKind::Terminal, Some(job), None),
        at,
    )
    .unwrap();
    h.sample(
        "tenant",
        Metrics {
            memory_bytes: 13 * 1_048_576,
            egress_bytes: 145,
        },
    )
    .unwrap();
    h.receive_terminal(job, frame, at).unwrap();
}

#[test]
fn vm_memory_is_never_a_job_peak_when_the_runner_has_no_peak() {
    let mut h = host();
    started(&mut h, JOB, 1);
    h.sample(
        "tenant",
        Metrics {
            memory_bytes: 109_600 * 1_048_576,
            egress_bytes: 100,
        },
    )
    .unwrap();
    end(&mut h, JOB, 3, MS + 3000, terminal(JOB, "exit"));
    let usage = h.snapshot.billing[JOB].usage.as_ref().unwrap();
    assert_eq!(usage.mem_peak_mib, None);
    assert!(serde_json::to_value(usage).unwrap()["mem_peak_mib"].is_null());
}

#[test]
fn terminal_lifecycle_settles_the_job_peak_even_with_another_job_in_the_vm() {
    for overlapping in [false, true] {
        let mut h = host();
        started(&mut h, JOB, 1);
        if overlapping {
            admitted(&mut h, JOB2, 3);
        }
        let mut ended = event(4, TenantEventKind::Terminal, Some(JOB), None);
        ended.mem_peak_mib = Some(481);
        h.event("tenant", &ended, MS + 3000).unwrap();
        h.sample(
            "tenant",
            Metrics {
                memory_bytes: 109_600 * 1_048_576,
                egress_bytes: 145,
            },
        )
        .unwrap();
        h.receive_terminal(JOB, terminal(JOB, "exit"), MS + 3000)
            .unwrap();
        h.terminal_delivered(JOB, MS + 3000).unwrap();
        assert_eq!(
            h.snapshot.billing[JOB].usage.as_ref().unwrap().mem_peak_mib,
            Some(481)
        );
        let request = h
            .snapshot
            .outbox
            .iter()
            .find(|r| r.path.ends_with("/usage"))
            .unwrap();
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["mem_peak_mib"], 481);
    }
}

#[test]
fn an_unstarted_job_cannot_report_a_memory_peak() {
    let mut h = host();
    admitted(&mut h, JOB, 1);
    let mut ended = event(2, TenantEventKind::Terminal, Some(JOB), None);
    ended.mem_peak_mib = Some(481);
    h.event("tenant", &ended, MS + 3000).unwrap();
    h.receive_terminal(JOB, terminal(JOB, "refused_before_start"), MS + 3000)
        .unwrap();
    let usage = h.snapshot.billing[JOB].usage.as_ref().unwrap();
    assert_eq!(usage.mem_peak_mib, None);
}

#[test]
fn guest_working_peak_is_reported_without_changing_billing() {
    for peak in [None, Some(513), Some(1 << 53)] {
        let mut h = host();
        started(&mut h, JOB, 1);
        let mut ended = event(3, TenantEventKind::Terminal, Some(JOB), None);
        ended.mem_peak_mib = Some(1600);
        ended.mem_working_peak_mib = peak;
        h.event("tenant", &ended, MS + 3000).unwrap();
        h.receive_terminal(JOB, terminal(JOB, "exit"), MS + 3000)
            .unwrap();
        h.terminal_delivered(JOB, MS + 3000).unwrap();
        let expected = peak.filter(|value| *value < (1 << 53));
        let local = &h.snapshot.billing[JOB];
        assert_eq!(local.mem_working_peak_mib, expected);
        let json = serde_json::to_value(local).unwrap();
        assert_eq!(
            json.get("mem_working_peak_mib").is_some(),
            expected.is_some()
        );
        if let Some(expected) = expected {
            assert_eq!(json["mem_working_peak_mib"], expected);
        }
        let restored: motor_host::billing::JobBilling = serde_json::from_value(json).unwrap();
        assert_eq!(restored.mem_working_peak_mib, expected);
        let request = h
            .snapshot
            .outbox
            .iter()
            .find(|r| r.path.ends_with("/usage"))
            .unwrap();
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(
            body["mem_working_peak_mib"],
            serde_json::to_value(expected).unwrap()
        );
        assert_eq!(
            body.get("mem_working_peak_mib").is_some(),
            expected.is_some()
        );
        assert_eq!(
            local.usage.as_ref().unwrap().mem_working_peak_mib,
            expected.map(Some)
        );
        assert_eq!(body["mem_peak_mib"], 1600);
        assert_eq!(body["vcpus"], 1);
        assert_eq!(body["started_at_ms"], MS);
        assert_eq!(body["ended_at_ms"], MS + 3000);
        assert_eq!(
            local.hosted_terminal.as_ref().unwrap()["billed_vcpu_ms"],
            3000
        );
        assert!(serde_json::from_slice::<motor_lease::Usage>(&request.body).is_ok());
    }
}

#[test]
fn guest_cache_totals_are_logged_as_integers_and_never_sent_to_broker() {
    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let buffer = Buffer::default();
    let capture = tracing::Dispatch::new(motor_protocol::logging::LineSubscriber::new(
        "[host]",
        buffer.clone(),
    ));
    // Two live subscribers disable tracing's single-subscriber interest shortcut.
    let _spare = tracing::Dispatch::new(motor_protocol::logging::LineSubscriber::new(
        "[host]",
        std::io::sink(),
    ));
    for totals in [
        None,
        Some(motor_protocol::RemoteCacheTotals {
            hits: 7,
            misses: 3,
            errors: u64::MAX,
        }),
    ] {
        buffer.0.lock().unwrap().clear();
        let mut h = host();
        started(&mut h, JOB, 1);
        let mut ended = event(3, TenantEventKind::Terminal, Some(JOB), None);
        ended.remote_cache_totals = totals;
        tracing::dispatcher::with_default(&capture, || {
            tracing::callsite::rebuild_interest_cache();
            h.event("tenant", &ended, MS + 3000).unwrap();
            h.event("tenant", &ended, MS + 3000).unwrap();
        });
        let log = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<_> = log
            .lines()
            .filter(|line| line.contains("job remote cache totals (guest-reported)"))
            .collect();
        assert_eq!(lines.len(), usize::from(totals.is_some()), "{log}");
        if totals.is_some() {
            let line = lines[0];
            for field in [
                format!("job_id={JOB}"),
                "hits=7".into(),
                "misses=3".into(),
                format!("errors={}", u64::MAX),
            ] {
                assert!(line.contains(&field), "{line}");
            }
            assert!(!line.contains("Some("), "{line}");
        }
        h.receive_terminal(JOB, terminal(JOB, "exit"), MS + 3000)
            .unwrap();
        h.terminal_delivered(JOB, MS + 3000).unwrap();
        let request = h
            .snapshot
            .outbox
            .iter()
            .find(|r| r.path.ends_with("/usage"))
            .unwrap();
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(
            body,
            json!({
                "job_id": JOB, "vcpus": 1, "started_at_ms": MS, "ended_at_ms": MS + 3000,
                "mem_peak_mib": null, "egress_bytes": 0, "outcome": "exit"
            })
        );
        assert!(serde_json::from_slice::<motor_lease::Usage>(&request.body).is_ok());
        assert!(serde_json::to_value(&h.snapshot.billing[JOB])
            .unwrap()
            .get("remote_cache_totals")
            .is_none());
    }
}

#[test]
fn observed_usage_ignores_guest_clocks_caps_and_maps_all_outcomes() {
    for (outcome, killed, expected) in [
        ("exit", None, UsageOutcome::Exit),
        ("signal", None, UsageOutcome::Signal),
        ("signal", Some("deadline"), UsageOutcome::Timeout),
        ("signal", Some("cancel"), UsageOutcome::Cancelled),
        ("cancelled", None, UsageOutcome::Cancelled),
    ] {
        let mut h = host();
        started(&mut h, JOB, 1);
        let mut frame = terminal(JOB, outcome);
        if let Some(killed) = killed {
            frame["killed"] = json!(killed);
        }
        end(&mut h, JOB, 3, MS + 3000, frame);
        let b = &h.snapshot.billing[JOB];
        let usage = b.usage.as_ref().unwrap();
        assert_eq!((usage.started_at_ms, usage.ended_at_ms), (MS, MS + 3000));
        assert_eq!(usage.outcome, expected);
        assert_eq!((usage.mem_peak_mib, usage.egress_bytes), (None, Some(45)));
        assert_eq!(b.hosted_terminal.as_ref().unwrap()["billed_vcpu_ms"], 3000);
    }
    let mut h = host();
    admitted(&mut h, JOB, 1);
    end(
        &mut h,
        JOB,
        2,
        MS + 3000,
        terminal(JOB, "refused_before_start"),
    );
    let usage = h.snapshot.billing[JOB].usage.as_ref().unwrap();
    assert_eq!(
        (usage.started_at_ms, usage.ended_at_ms, usage.outcome),
        (MS + 3000, MS + 3000, UsageOutcome::Cancelled)
    );
}

#[test]
fn replay_boundary_distinguishes_outage_events_from_new_live_observations_and_old_runners_bill_zero(
) {
    for boundary in [true, false] {
        let mut h = host();
        admitted(&mut h, JOB, 1);
        admitted(&mut h, JOB2, 2);
        h.snapshot.last_alive = MS + 1000;
        h.start_billing(MS + 5000).unwrap();
        let mut stream = LifecycleStream::default();
        if boundary {
            stream
                .boundary(&json!({"type":"event_replay","through_seq":4}))
                .unwrap();
        }
        for (seq, job, kind, now) in [
            (3, JOB, TenantEventKind::Started, MS + 5100),
            (4, JOB, TenantEventKind::Terminal, MS + 5200),
            (5, JOB2, TenantEventKind::Started, MS + 6000),
            (6, JOB2, TenantEventKind::Terminal, MS + 9000),
        ] {
            h.event_observed(
                "tenant",
                &event(seq, kind, Some(job), None),
                now,
                stream.is_live(seq),
            )
            .unwrap();
        }
        h.sample(
            "tenant",
            Metrics {
                memory_bytes: 1_048_576,
                egress_bytes: 0,
            },
        )
        .unwrap();
        h.receive_terminal_observed(JOB, terminal(JOB, "exit"), MS + 10000, false)
            .unwrap();
        h.receive_terminal_observed(JOB2, terminal(JOB2, "exit"), MS + 10000, false)
            .unwrap();
        let replay = h.snapshot.billing[JOB].usage.as_ref().unwrap();
        assert_eq!(
            (replay.started_at_ms, replay.ended_at_ms),
            (MS + 5000, MS + 5000)
        );
        let live = h.snapshot.billing[JOB2].usage.as_ref().unwrap();
        assert_eq!(
            live.ended_at_ms - live.started_at_ms,
            if boundary { 3000 } else { 0 }
        );
        if boundary {
            assert_eq!(live.started_at_ms, MS + 6000);
        }
    }
}

#[test]
fn backstop_cancel_waits_thirty_seconds_after_runner_cap() {
    for provider_managed in [false, true] {
        let mut h = host();
        started(&mut h, JOB, 1);
        h.provider_managed = provider_managed;
        let cap = MS + 30_000;
        for now in [cap, cap + 29_999] {
            h.billing_tick(now).unwrap();
            assert!(
                !h.snapshot.pending_cancels.contains_key(JOB),
                "cancel queued at {now}"
            );
            assert!(!h.snapshot.billing[JOB].cancelled_at_cap);
            assert!(!h.snapshot.billing[JOB].host_settled);
        }
        h.billing_tick(cap + 30_000).unwrap();
        assert_eq!(
            h.snapshot.pending_cancels.get(JOB).map(String::as_str),
            Some("tenant")
        );
        assert!(!h.snapshot.billing[JOB].host_settled);
        if !provider_managed {
            h.billing_tick(cap + 299_999).unwrap();
            assert!(!h.snapshot.billing[JOB].host_settled);
            h.billing_tick(cap + 300_000).unwrap();
            assert!(h.snapshot.billing[JOB].host_settled);
        }
    }
}

#[test]
fn watchdog_cancels_drops_share_across_restart_settles_once_and_defers_stop_for_healthy_job() {
    let dir = scratch();
    let mut h = disk_host(dir.path());
    started(&mut h, JOB, 1);
    started(&mut h, JOB2, 3);
    // The second command starts later and remains inside its own runtime cap.
    h.snapshot.leases.get_mut(JOB2).unwrap().started_at_ms = Some(MS + 20_000);
    h.snapshot.leases.get_mut(JOB2).unwrap().claims.max_s = 600;
    h.billing_tick(MS + 30_000).unwrap();
    assert_eq!(h.share("tenant", MS + 30_000), 1);
    assert!(!h.snapshot.pending_cancels.contains_key(JOB));
    h.billing_tick(MS + 60_000).unwrap();
    assert!(h.snapshot.pending_cancels.contains_key(JOB));
    h.cancel_delivered(JOB).unwrap();
    drop(h);
    let mut h = disk_host(dir.path());
    assert_eq!(h.share("tenant", MS + 31_000), 1);
    h.billing_tick(MS + 329_999).unwrap();
    assert!(!h.snapshot.billing[JOB].host_settled);
    h.billing_tick(MS + 330_000).unwrap();
    assert!(h.snapshot.billing[JOB].host_settled);
    assert!(h.snapshot.vms["tenant"].stop_pending);
    assert!(h.stops_due().is_empty());
    let mut new_claim = claims(JOB3);
    new_claim.iat += 300;
    new_claim.exp += 300;
    assert!(matches!(
        h.claim(new_claim, MS + 330_000),
        Err(motor_host::state::Error::Refused(
            motor_host::state::Refusal::Capacity
        ))
    ));
    end(&mut h, JOB2, 5, MS + 331_000, terminal(JOB2, "exit"));
    assert_eq!(
        h.snapshot.billing[JOB2].usage.as_ref().unwrap().outcome,
        UsageOutcome::Exit
    );
    assert_eq!(h.stops_due(), vec!["tenant"]);
    h.stopped("tenant").unwrap();
    assert!(h.stops_due().is_empty());
    h.billing_tick(MS + 390_000).unwrap();
    let original = serde_json::to_vec(&h.snapshot.billing[JOB].usage).unwrap();
    h.event(
        "tenant",
        &event(6, TenantEventKind::Terminal, Some(JOB), None),
        MS + 400_000,
    )
    .unwrap();
    h.receive_terminal(JOB, terminal(JOB, "exit"), MS + 400_000)
        .unwrap();
    assert_eq!(
        serde_json::to_vec(&h.snapshot.billing[JOB].usage).unwrap(),
        original
    );
    let usage = h.snapshot.billing[JOB].usage.as_ref().unwrap();
    assert_eq!(
        (usage.outcome, usage.ended_at_ms - usage.started_at_ms),
        (UsageOutcome::Lost, 30_000)
    );
    assert_eq!(
        h.snapshot
            .outbox
            .iter()
            .filter(|r| r.job.as_deref() == Some(JOB))
            .count(),
        1
    );
}

#[test]
fn port52_terminal_disarms_watchdog_and_preserves_real_result_with_port53_down() {
    for receipt in [61_000, 89_000] {
        let mut h = host();
        started(&mut h, JOB, 1);
        h.billing_tick(MS + 60_000).unwrap();
        h.receive_terminal(JOB, terminal(JOB, "exit"), MS + receipt)
            .unwrap();
        h.billing_tick(MS + 90_000).unwrap();
        h.billing_tick(MS + receipt + 30_000).unwrap();
        let b = &h.snapshot.billing[JOB];
        assert!(!b.host_settled);
        assert_eq!(b.terminal.as_ref().unwrap()["outcome"]["type"], "exit");
        let usage = b.usage.as_ref().unwrap();
        assert_eq!(
            (usage.outcome, usage.ended_at_ms - usage.started_at_ms),
            (UsageOutcome::Timeout, 30_000)
        );
    }
}

#[test]
fn synthesized_frames_are_durable_identical_unsequenced_and_cursor_filtered() {
    let dir = scratch();
    let mut h = disk_host(dir.path());
    started(&mut h, JOB, 1);
    h.watermark(JOB, 7).unwrap();
    h.lose_tenant("tenant", MS + 3000).unwrap();
    let expected = h.host_settled_frames(JOB, 0).unwrap();
    h.watermark(JOB, 100).unwrap();
    assert_eq!(h.host_settled_frames(JOB, 0).unwrap(), expected);
    assert_eq!(expected[0]["seq"], 8);
    assert_eq!(expected[0]["truncated_before_seq"], 8);
    assert!(expected[1].get("seq").is_none());
    assert!(expected[2].get("seq").is_none());
    // Use the actual protocol decoder, not just a JSON proxy for a terminal.
    for frame in &expected {
        motor_protocol::decode_frame(&serde_json::to_vec(frame).unwrap()).unwrap();
    }
    drop(h);
    let h = disk_host(dir.path());
    for _ in 0..2 {
        assert_eq!(h.host_settled_frames(JOB, 0).unwrap(), expected);
        assert_eq!(h.host_settled_frames(JOB, 8).unwrap(), expected);
        assert_eq!(h.host_settled_frames(JOB, 9).unwrap(), expected[1..]);
    }
}

#[test]
fn hosted_terminal_delivery_or_sixty_second_grace_enqueues_usage_once() {
    for delivered in [true, false] {
        let mut h = host();
        started(&mut h, JOB, 1);
        end(&mut h, JOB, 3, MS + 3000, terminal(JOB, "exit"));
        h.billing_tick(MS + 62_999).unwrap();
        assert!(h.snapshot.outbox.is_empty());
        if delivered {
            h.terminal_delivered(JOB, MS + 62_999).unwrap();
        } else {
            h.billing_tick(MS + 63_000).unwrap();
        }
        assert_eq!(h.snapshot.outbox.len(), 1);
        h.terminal_delivered(JOB, MS + 70_000).unwrap();
        assert_eq!(h.snapshot.outbox.len(), 1);
        let figure = h.snapshot.billing[JOB].hosted_terminal.clone();
        let request = h.snapshot.outbox[0].clone();
        assert!(h
            .finish_request(
                &request,
                200,
                &json!({"job_id":JOB,"settled_vcpu_ms":2000,"released_vcpu_ms":0})
            )
            .unwrap());
        assert!(h.snapshot.outbox.is_empty());
        assert_eq!(h.snapshot.billing[JOB].hosted_terminal, figure);
    }
}

#[test]
fn overlap_makes_both_metrics_null_and_missing_metrics_are_not_fabricated_zero() {
    let mut h = host();
    started(&mut h, JOB, 1);
    admitted(&mut h, JOB2, 3);
    end(&mut h, JOB, 4, MS + 3000, terminal(JOB, "exit"));
    let usage = h.snapshot.billing[JOB].usage.as_ref().unwrap();
    assert_eq!((usage.mem_peak_mib, usage.egress_bytes), (None, None));
    let mut h = host();
    admitted(&mut h, JOB, 1);
    h.event(
        "tenant",
        &event(2, TenantEventKind::Started, Some(JOB), None),
        MS,
    )
    .unwrap();
    h.event(
        "tenant",
        &event(3, TenantEventKind::Terminal, Some(JOB), None),
        MS + 3000,
    )
    .unwrap();
    h.receive_terminal(JOB, terminal(JOB, "exit"), MS + 3000)
        .unwrap();
    assert!(h.snapshot.billing[JOB].usage.is_none());
}

/// A tenant's own cache fill produces no usage record of its own. A job that
/// runs beside its tenant's fill, whether the fill started before or after it,
/// keeps its egress in settlement. Memory is unknown without a job-unit peak:
/// the VM sample also includes the fill and cannot be attributed to the job.
#[test]
fn a_job_beside_its_tenants_fill_keeps_egress_but_not_vm_memory() {
    let mut h = host();
    h.record_fill("tenant", MS, 4);
    h.billing_tick(MS + 120_000).unwrap();
    assert!(h.snapshot.billing.is_empty());
    assert!(h.snapshot.outbox.is_empty());

    for fill_first in [true, false] {
        let mut h = host();
        if fill_first {
            h.record_fill("tenant", MS, 4);
        }
        started(&mut h, JOB, 1);
        if !fill_first {
            h.record_fill("tenant", MS + 1000, 4);
        }
        end(&mut h, JOB, 3, MS + 3000, terminal(JOB, "exit"));
        let b = &h.snapshot.billing[JOB];
        assert!(!b.overlapping, "{fill_first}");
        let usage = b.usage.as_ref().unwrap();
        assert_eq!(usage.vcpus, claims(JOB).vcpus, "{fill_first}");
        assert_eq!(
            (usage.mem_peak_mib, usage.egress_bytes),
            (None, Some(45)),
            "{fill_first}"
        );
        assert_eq!(
            b.hosted_terminal.as_ref().unwrap()["billed_vcpu_ms"],
            u64::from(claims(JOB).vcpus) * 3000
        );
    }
}
/// The guest is root-controlled, so its fill report is tenant input: it may
/// only lower what the host offers that tenant. A report of u32::MAX fill
/// permits leaves the VM's CPU quota and every billing figure exactly as an
/// honest runner's, and lowers only that tenant's advertised `free_vcpus`, to
/// no lower than 0.
#[test]
fn a_forged_fill_report_grants_no_cpu_waives_no_usage_and_lowers_only_its_own_capacity() {
    let run = |report: Option<u32>| {
        let mut h = host();
        h.guest_ready("tenant", 1).unwrap();
        started(&mut h, JOB, 1);
        if let Some(permits) = report {
            h.record_fill("tenant", MS + 1000, permits);
        }
        let quota = h.quota("tenant", MS + 1000);
        let free = h.heartbeat(MS + 1000, 0, true).free_vcpus;
        end(&mut h, JOB, 3, MS + 3000, terminal(JOB, "exit"));
        let b = &h.snapshot.billing[JOB];
        (
            quota,
            free,
            b.usage.clone().unwrap(),
            b.hosted_terminal.clone().unwrap(),
        )
    };
    let (honest_quota, honest_free, honest_usage, honest_terminal) = run(None);
    let (quota, free, usage, terminal) = run(Some(u32::MAX));
    assert_eq!((honest_quota, honest_free), (1, 1));
    assert_eq!(quota, honest_quota);
    assert_eq!(usage, honest_usage);
    assert_eq!((usage.mem_peak_mib, usage.egress_bytes), (None, Some(45)));
    assert_eq!(terminal, honest_terminal);
    assert_eq!(free, 0);

    // Another tenant on the same host keeps its whole advertised capacity.
    let mut tenants = tenants();
    let mut other = tenants["vm"].clone();
    other.tenant = "other".into();
    other.sub = "acct:01ARZ3NDEKTSV4RRFFQ69G5FAW".into();
    tenants.insert("vm-2".into(), other);
    let mut h = Host::new(
        tenants,
        Snapshot::default(),
        Box::<MemoryJournal>::default(),
    );
    h.guest_ready("tenant", 1).unwrap();
    h.record_fill("tenant", MS, u32::MAX);
    assert_eq!(h.heartbeat(MS, 0, true).free_vcpus, 2);
    assert_eq!(h.quota("tenant", MS), 1);
}

#[test]
fn heartbeat_uses_pinned_interim_values_and_released_is_reserved() {
    let mut h = host();
    let (id, _) = h.claim(claims(JOB), MS).unwrap();
    h.guest_ready("tenant", 1).unwrap();
    let hb = h.heartbeat(MS, 123, true);
    assert_eq!((hb.free_vcpus, hb.free_mem_mib, hb.queued), (1, 123, 1));
    // Admission alone does not establish that the guest holds a workspace.
    assert!(hb.warm.is_empty());
    h.close(id).unwrap();
    let hb = h.heartbeat(MS, 123, true);
    assert_eq!(
        (hb.running[0].state, hb.free_vcpus),
        (JobState::Reserved, 2)
    );
}

#[test]
fn stop_pending_tenant_advertises_no_capacity_warm_workspace_or_disk() {
    let mut h = host();
    h.guest_ready("tenant", 1).unwrap();
    admitted(&mut h, JOB, 1);
    h.event(
        "tenant",
        &event(2, TenantEventKind::Terminal, Some(JOB), None),
        MS,
    )
    .unwrap();
    h.record_warm(
        "tenant",
        MS,
        vec![motor_protocol::HeldWorkspace {
            job_id: JOB.parse().unwrap(),
            used_ms: MS,
        }],
    );
    h.record_disk("tenant", MS, 0, 10, 50);
    let before = h.heartbeat(MS, 0, true);
    assert!(before.free_vcpus > 0);
    assert_eq!(before.warm.len(), 1);
    assert!(!before.warm[0].busy);
    assert_eq!(before.tenant_disk.len(), 1);
    h.set_stop_pending("tenant", true).unwrap();
    let blocked = h.heartbeat(MS, 0, true);
    assert_eq!(blocked.free_vcpus, 0);
    assert!(blocked.warm.is_empty());
    assert!(blocked.tenant_disk.is_empty());
    assert!(!h.memory_probe_due("tenant", MS, true));
    h.stopped("tenant").unwrap();
    assert!(h.heartbeat(MS, 0, true).free_vcpus > 0);
}

#[test]
fn warm_workspace_busy_tracks_active_leases_on_its_tenant_and_hash() {
    use motor_host::state::LeaseState;
    let hash = claims(JOB).workspace_key_hash;
    for (state, active_hash, tenant, expected) in [
        (LeaseState::Reserved, hash.clone(), "tenant", false),
        (LeaseState::Released, hash.clone(), "tenant", false),
        (LeaseState::Ended, hash.clone(), "tenant", false),
        (LeaseState::Admitted, hash.clone(), "tenant", true),
        (LeaseState::Started, hash.clone(), "tenant", true),
        (
            LeaseState::Admitted,
            Some(motor_lease::base64url(&[6; 32])),
            "tenant",
            false,
        ),
        (LeaseState::Started, None, "tenant", false),
        (LeaseState::Admitted, hash.clone(), "other", false),
        (LeaseState::Started, hash, "other", false),
    ] {
        let mut h = host();
        h.guest_ready("tenant", 1).unwrap();
        admitted(&mut h, JOB, 1);
        h.event(
            "tenant",
            &event(2, TenantEventKind::Terminal, Some(JOB), None),
            MS,
        )
        .unwrap();
        let mut other = h.tenants["vm"].clone();
        other.tenant = "other".into();
        other.sub = "acct:01ARZ3NDEKTSV4RRFFQ69G5FAW".into();
        h.tenants.insert("other-vm".into(), other);
        let mut c = claims(JOB2);
        c.workspace_key_hash = active_hash;
        if tenant == "other" {
            c.sub = h.tenants["other-vm"].sub.clone();
            c.placement.vm = Some("other-vm".into());
        }
        let (id, _) = h.claim(c, MS).unwrap();
        if state == LeaseState::Released {
            h.close(id).unwrap();
        } else if state != LeaseState::Reserved {
            h.delivered(id).unwrap();
            h.event(
                tenant,
                &event(3, TenantEventKind::Admitted, Some(JOB2), None),
                MS,
            )
            .unwrap();
            if let Some(kind) = match state {
                LeaseState::Started => Some(TenantEventKind::Started),
                LeaseState::Ended => Some(TenantEventKind::Terminal),
                _ => None,
            } {
                h.event(tenant, &event(4, kind, Some(JOB2), None), MS)
                    .unwrap();
            }
        }
        h.record_warm(
            "tenant",
            MS,
            vec![motor_protocol::HeldWorkspace {
                job_id: JOB.parse().unwrap(),
                used_ms: MS,
            }],
        );
        let heartbeat = h.heartbeat(MS, 0, true);
        assert_eq!(heartbeat.warm.len(), 1);
        assert_eq!(heartbeat.warm[0].busy, expected, "{state:?} on {tenant}");
        // The wire carries `busy` only when it is true.
        assert_eq!(
            serde_json::to_value(&heartbeat).unwrap()["warm"][0].get("busy"),
            expected.then_some(&serde_json::Value::Bool(true)),
            "{state:?} on {tenant}"
        );
    }
}

#[test]
fn warm_workspaces_join_admitted_hashes_order_cap_and_forget_evictions() {
    use motor_protocol::HeldWorkspace;
    let mut h = host();
    h.guest_ready("tenant", 1).unwrap();
    let mut held = Vec::new();
    for i in 0..71_u64 {
        let job = format!("00000000-0000-7000-8000-{:012x}", i + 100);
        let mut c = claims(&job);
        c.workspace_key_hash = (i < 70).then(|| motor_lease::base64url(&[i as u8; 32]));
        let (connection, _) = h.claim(c, MS).unwrap();
        h.delivered(connection).unwrap();
        h.event(
            "tenant",
            &event(i * 2 + 1, TenantEventKind::Admitted, Some(&job), None),
            MS,
        )
        .unwrap();
        h.event(
            "tenant",
            &event(i * 2 + 2, TenantEventKind::Terminal, Some(&job), None),
            MS,
        )
        .unwrap();
        held.push(HeldWorkspace {
            job_id: job.parse().unwrap(),
            used_ms: MS - 100 + i,
        });
    }
    // Unknown jobs, including another VM's claims, cannot lend this VM a hash.
    held.push(HeldWorkspace {
        job_id: JOB.parse().unwrap(),
        used_ms: MS,
    });
    let mut other = h.tenants["vm"].clone();
    other.tenant = "other".into();
    other.sub = "acct:01ARZ3NDEKTSV4RRFFQ69G5FAW".into();
    h.tenants.insert("other-vm".into(), other);
    h.guest_ready("other", 1).unwrap();
    h.record_warm("other", MS, held.clone());
    assert!(h.heartbeat(MS, 0, true).warm.is_empty());
    h.record_warm("tenant", MS, held.clone());
    let warm = h.heartbeat(MS, 0, true).warm;
    assert_eq!(warm.len(), 64);
    assert_eq!(
        warm[0].workspace_key_hash,
        motor_lease::base64url(&[69; 32])
    );
    assert_eq!(
        warm[63].workspace_key_hash,
        motor_lease::base64url(&[6; 32])
    );
    assert_eq!(warm[0].tenant, claims(JOB).sub);
    assert_eq!(warm[0].vm, "vm");
    // The next probe is the entire current set, not additions to an old set.
    h.record_warm("tenant", MS + 1, vec![held[0].clone()]);
    let warm = h.heartbeat(MS + 1, 0, true).warm;
    assert_eq!(warm.len(), 1);
    assert_eq!(warm[0].workspace_key_hash, motor_lease::base64url(&[0; 32]));
    h.record_warm("tenant", MS + 2, vec![held[70].clone()]);
    assert!(
        h.heartbeat(MS + 2, 0, true).warm.is_empty(),
        "a legacy hashless job contributes nothing"
    );
    h.record_warm("tenant", MS + 3, vec![held[0].clone()]);
    assert!(
        h.heartbeat(MS + 15_003, 0, true).warm.is_empty(),
        "old status reports cannot preserve stale affinity"
    );
}

#[test]
fn a_busy_workspace_stays_in_the_warm_list_whatever_its_age() {
    // On 5 Nov 2026 a checkout whose job had run for over half an hour kept
    // getting more jobs placed on the same host to queue behind it: by start
    // time it ranked below 64 newer workspaces, so it left the warm list and
    // its busy flag with it.
    use motor_protocol::HeldWorkspace;
    let mut h = host();
    h.guest_ready("tenant", 1).unwrap();
    let mut held = Vec::new();
    for i in 0..70_u64 {
        let job = format!("00000000-0000-7000-8000-{:012x}", i + 100);
        let mut c = claims(&job);
        c.workspace_key_hash = Some(motor_lease::base64url(&[i as u8; 32]));
        let (connection, _) = h.claim(c, MS).unwrap();
        h.delivered(connection).unwrap();
        h.event(
            "tenant",
            &event(i * 2 + 1, TenantEventKind::Admitted, Some(&job), None),
            MS,
        )
        .unwrap();
        h.event(
            "tenant",
            &event(i * 2 + 2, TenantEventKind::Terminal, Some(&job), None),
            MS,
        )
        .unwrap();
        held.push(HeldWorkspace {
            job_id: job.parse().unwrap(),
            used_ms: MS - 100 + i,
        });
    }
    let long = "00000000-0000-7000-8000-000000000fff";
    let mut c = claims(long);
    c.workspace_key_hash = Some(motor_lease::base64url(&[200; 32]));
    let (connection, _) = h.claim(c, MS).unwrap();
    h.delivered(connection).unwrap();
    h.event(
        "tenant",
        &event(201, TenantEventKind::Admitted, Some(long), None),
        MS,
    )
    .unwrap();
    h.event(
        "tenant",
        &event(202, TenantEventKind::Started, Some(long), None),
        MS,
    )
    .unwrap();
    held.push(HeldWorkspace {
        job_id: long.parse().unwrap(),
        used_ms: MS - 10_000,
    });
    h.record_warm("tenant", MS, held);
    let warm = h.heartbeat(MS, 0, true).warm;
    assert_eq!(warm.len(), 64);
    assert_eq!(
        warm[0].workspace_key_hash,
        motor_lease::base64url(&[200; 32])
    );
    assert!(warm[0].busy);
    assert_eq!(
        warm[1].workspace_key_hash,
        motor_lease::base64url(&[69; 32])
    );
    assert!(warm[1..].iter().all(|entry| !entry.busy));
}

#[test]
fn usage_outbox_finals_advance_retry_bodies_are_unchanged_and_counter_is_durable() {
    let dir = scratch();
    let mut h = disk_host(dir.path());
    h.start_billing(MS).unwrap();
    started(&mut h, JOB, 1);
    end(&mut h, JOB, 3, MS + 3000, terminal(JOB, "exit"));
    h.terminal_delivered(JOB, MS + 3000).unwrap();
    let r = h.snapshot.outbox[0].clone();
    let key = ed25519_dalek::SigningKey::from_bytes(&[5; 32]);
    let first = sign_request(&mut h, "host", &r, MS + 3000, &key).unwrap();
    let bytes = r.body.clone();
    assert!(!h.finish_request(&r, 503, &json!({})).unwrap());
    drop(h);
    let mut h = disk_host(dir.path());
    h.start_billing(MS + 90_000).unwrap();
    let retry = h.snapshot.outbox[0].clone();
    let second = sign_request(&mut h, "host", &retry, MS + 90_000, &key).unwrap();
    assert!(second.counter > first.counter);
    assert!(second.timestamp > first.timestamp);
    assert_ne!(first.signature, second.signature);
    assert_eq!(h.snapshot.outbox[0].body, bytes);
    for (status, error) in [
        (409, "usage_conflict"),
        (409, "job_settled"),
        (404, "job_unknown"),
    ] {
        let mut next = h.snapshot.clone();
        next.outbox = vec![r.clone()];
        let mut h = Host::new(tenants(), next, Box::<MemoryJournal>::default());
        assert!(h
            .finish_request(&r, status, &json!({"error":error}))
            .unwrap());
        assert!(h.snapshot.outbox.is_empty());
    }
    h.expire_outbox(MS + 3000 + OUTBOX_MAX_AGE_MS).unwrap();
    assert!(h.snapshot.outbox.is_empty());
    assert_eq!(
        std::fs::read_dir(dir.path().join("outbox-expired"))
            .unwrap()
            .count(),
        1
    );
}

#[derive(Clone, Default)]
pub(super) struct Logs(pub(super) Arc<Mutex<Vec<String>>>);
impl tracing::Subscriber for Logs {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        struct Visitor(String);
        impl tracing::field::Visit for Visitor {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.push_str(&format!("{}={value:?} ", field.name()));
            }
        }
        let mut visitor = Visitor(format!("{} ", event.metadata().level()));
        event.record(&mut visitor);
        self.0.lock().unwrap().push(visitor.0);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

struct DiagnosticsControl {
    requests: Mutex<Vec<Value>>,
    arrived: std::sync::mpsc::Sender<String>,
    release: (Mutex<bool>, std::sync::Condvar),
    reply: Option<Value>,
    stops: std::sync::atomic::AtomicUsize,
    stop_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    relays: Mutex<Vec<std::os::unix::net::UnixStream>>,
    cancels: Mutex<Vec<String>>,
}
impl DiagnosticsControl {
    fn new(reply: Option<Value>, blocked: bool) -> (Arc<Self>, std::sync::mpsc::Receiver<String>) {
        let (arrived, receive) = std::sync::mpsc::channel();
        (
            Arc::new(Self {
                requests: Mutex::new(Vec::new()),
                arrived,
                release: (Mutex::new(!blocked), std::sync::Condvar::new()),
                reply,
                stops: std::sync::atomic::AtomicUsize::new(0),
                stop_hook: Mutex::new(None),
                relays: Mutex::new(Vec::new()),
                cancels: Mutex::new(Vec::new()),
            }),
            receive,
        )
    }
    fn release(&self) {
        *self.release.0.lock().unwrap() = true;
        self.release.1.notify_all();
    }
}
impl motor_host::helper::Control for DiagnosticsControl {
    fn adopt(&self, _: &str) -> anyhow::Result<bool> {
        Ok(false)
    }
    fn wake(&self, _: &str, _: Option<u64>) -> anyhow::Result<u64> {
        anyhow::bail!("unexpected wake")
    }
    fn quota(&self, _: &str, _: u32) -> anyhow::Result<()> {
        Ok(())
    }
    fn relay(&self, _: &str, _: u32) -> anyhow::Result<std::os::unix::net::UnixStream> {
        self.relays
            .lock()
            .unwrap()
            .pop()
            .ok_or_else(|| anyhow::anyhow!("unexpected relay"))
    }
    fn admin(&self, _: &str, request: &Value) -> anyhow::Result<Value> {
        if request["op"] != "diagnostics" {
            if request["op"] == "cancel" {
                self.cancels
                    .lock()
                    .unwrap()
                    .push(request["job_id"].as_str().unwrap().to_owned());
            }
            return Ok(json!({"ok":true}));
        }
        self.requests.lock().unwrap().push(request.clone());
        let _ = self
            .arrived
            .send(request["jti"].as_str().unwrap().to_owned());
        let _released = self
            .release
            .1
            .wait_while(self.release.0.lock().unwrap(), |released| !*released)
            .unwrap();
        self.reply
            .clone()
            .ok_or_else(|| anyhow::anyhow!("diagnostic transport failed"))
    }
    fn suspend(&self, _: &str) -> anyhow::Result<()> {
        anyhow::bail!("unexpected suspend")
    }
    fn available_bytes(&self) -> anyhow::Result<u64> {
        Ok(u64::MAX)
    }
    fn stop(&self, _: &str) -> anyhow::Result<()> {
        self.stops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(hook) = self.stop_hook.lock().unwrap().take() {
            hook();
        }
        Ok(())
    }
}

#[test]
fn late_terminal_after_stop_starts_does_not_cancel_the_stop() {
    let mut h = host();
    started(&mut h, JOB, 1);
    h.billing_tick(MS + 330_000).unwrap();
    let (control, _) = DiagnosticsControl::new(None, false);
    let runtime = motor_host::server::Runtime::new("host".into(), h, control.clone());
    let weak = Arc::downgrade(&runtime);
    *control.stop_hook.lock().unwrap() = Some(Box::new(move || {
        let runtime = weak.upgrade().unwrap();
        let mut host = runtime.host.lock().unwrap();
        host.event(
            "tenant",
            &event(3, TenantEventKind::Terminal, Some(JOB), None),
            MS + 331_000,
        )
        .unwrap();
        assert!(
            host.snapshot.vms["tenant"].stop_pending,
            "stop was already committed to start"
        );
        assert_eq!(host.snapshot.billing[JOB].restart_pending, Some(false));
    }));
    runtime.tick(MS + 331_000).unwrap();
    assert_eq!(control.stops.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(!runtime.host.lock().unwrap().snapshot.vms["tenant"].stop_pending);
}

#[tokio::test]
async fn pending_restart_withdrawal_requires_authoritative_proof_and_late_started_holds_vm() {
    use motor_host::helper::{read_json, write_json};
    for started_race in [false, true] {
        let mut h = host();
        started(&mut h, JOB, 1);
        admitted(&mut h, JOB2, 3);
        let mut updates = h.watch_connection(2).unwrap();
        updates.borrow_and_update();
        h.billing_tick(MS + 330_000).unwrap();
        let (control, _) = DiagnosticsControl::new(None, false);
        let (relay, mut guest) = std::os::unix::net::UnixStream::pair().unwrap();
        guest
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        control.relays.lock().unwrap().push(relay);
        let runtime = motor_host::server::Runtime::new("host".into(), h, control.clone());
        let worker_runtime = runtime.clone();
        let guest = std::thread::spawn(move || {
            let verified = read_json(&mut guest).unwrap();
            assert_eq!(verified["type"], "verified_lease");
            assert_eq!(verified["purpose"], "attach");
            assert_eq!(verified["jti"], JOB2);
            assert_eq!(read_json(&mut guest).unwrap()["type"], "hello");
            write_json(&mut guest, &json!({"type":"hello","protocol_version":1})).unwrap();
            assert_eq!(
                read_json(&mut guest).unwrap(),
                json!({"type":"withdraw", "job_id":JOB2})
            );
            if started_race {
                worker_runtime
                    .host
                    .lock()
                    .unwrap()
                    .event(
                        "tenant",
                        &event(4, TenantEventKind::Started, Some(JOB2), None),
                        MS + 330_001,
                    )
                    .unwrap();
            }
            write_json(&mut guest, &json!({"type":"withdraw_reply", "outcome":if started_race {"started"} else {"withdrawn"}})).unwrap();
        });
        runtime.tick(MS + 330_000).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), updates.changed())
            .await
            .unwrap()
            .unwrap();
        guest.join().unwrap();
        {
            let host = runtime.host.lock().unwrap();
            assert_eq!(
                host.snapshot.leases[JOB2].state,
                if started_race {
                    LeaseState::Started
                } else {
                    LeaseState::Ended
                }
            );
            if started_race {
                assert!(host.stops_due().is_empty());
                assert!(!host.snapshot.zero_usage.contains_key(JOB2));
                assert!(host.host_settled_frames(JOB2, 0).is_none());
            } else {
                let frames = host.host_settled_frames(JOB2, 0).unwrap();
                for frame in &frames {
                    motor_protocol::decode_frame(&serde_json::to_vec(frame).unwrap()).unwrap();
                }
                assert_eq!(frames[0]["outcome"]["type"], "refused_before_start");
                assert_eq!(frames[0]["ran"], "none");
                assert_eq!(host.stops_due(), vec!["tenant"]);
            }
        }
        runtime.tick(MS + 330_002).unwrap();
        assert!(
            !control
                .cancels
                .lock()
                .unwrap()
                .iter()
                .any(|job| job == JOB2),
            "withdrawal must not turn into a cancel after Started"
        );
        assert_eq!(
            control.stops.load(std::sync::atomic::Ordering::SeqCst),
            usize::from(!started_race)
        );
    }
}

#[test]
fn legacy_lost_records_keep_claims_and_completed_restarts_do_not_claim_again() {
    let mut h = host();
    started(&mut h, JOB, 1);
    started(&mut h, JOB2, 3);
    h.billing_tick(MS + 330_000).unwrap();
    let mut legacy = serde_json::to_value(&h.snapshot).unwrap();
    for job in [JOB, JOB2] {
        legacy["billing"][job]
            .as_object_mut()
            .unwrap()
            .remove("restart_pending");
    }
    h.snapshot = serde_json::from_value(legacy).unwrap();
    h.receive_terminal(JOB, terminal(JOB, "exit"), MS + 331_000)
        .unwrap();
    assert!(h.snapshot.vms["tenant"].stop_pending);
    h.receive_terminal(JOB2, terminal(JOB2, "exit"), MS + 331_001)
        .unwrap();
    assert!(!h.snapshot.vms["tenant"].stop_pending);
    // A previous VM generation's lost jobs no longer own the next restart.
    h.stopped("tenant").unwrap();
    started(&mut h, JOB3, 5);
    h.billing_tick(MS + 330_000).unwrap();
    h.receive_terminal(JOB3, terminal(JOB3, "exit"), MS + 332_000)
        .unwrap();
    assert!(!h.snapshot.vms["tenant"].stop_pending);
    assert!(
        serde_json::to_value(motor_host::billing::JobBilling::default())
            .unwrap()
            .get("restart_pending")
            .is_none()
    );
}

#[test]
fn a_missing_vm_loss_does_not_claim_the_next_generations_restart() {
    let mut h = host();
    started(&mut h, JOB, 1);
    h.lose_tenant("tenant", MS + 1000).unwrap();
    started(&mut h, JOB2, 3);
    h.billing_tick(MS + 330_000).unwrap();
    h.receive_terminal(JOB2, terminal(JOB2, "exit"), MS + 331_000)
        .unwrap();
    assert!(!h.snapshot.vms["tenant"].stop_pending);
}

#[test]
fn lost_settlement_fetches_each_job_once_caps_jobs_and_never_blocks_stop() {
    let logs = Logs::default();
    let captured = logs.0.clone();
    tracing::subscriber::with_default(logs, || {
        let mut h = host();
        h.tenants.get_mut("vm").unwrap().max_weight = Some(8);
        let lost: Vec<_> = (0..4)
            .map(|i| format!("00000000-0000-7000-8000-{:012x}", i + 100))
            .collect();
        for (index, job) in lost.iter().enumerate() {
            started(&mut h, job, index as u64 * 2 + 1);
        }
        started(&mut h, JOB, 9);
        h.snapshot.leases.get_mut(JOB).unwrap().claims.max_s = 600;
        h.guest_ready("tenant", 1).unwrap();
        let (control, arrived) =
            DiagnosticsControl::new(Some(json!({"ok":true,"journal":{"entries":[]}})), true);
        let runtime = motor_host::server::Runtime::new("host".into(), h, control.clone());
        runtime.tick(MS + 330_000).unwrap();
        let jobs: std::collections::BTreeSet<_> = (0..3)
            .map(|_| {
                arrived
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap()
            })
            .collect();
        assert_eq!(jobs, lost[..3].iter().cloned().collect());
        assert_eq!(control.stops.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(runtime.host.lock().unwrap().snapshot.vms["tenant"].stop_pending);
        runtime.tick(MS + 330_001).unwrap();
        assert_eq!(control.requests.lock().unwrap().len(), 3);
        for request in control.requests.lock().unwrap().iter() {
            assert_eq!(request["op"], "diagnostics");
            assert_eq!(request["since_unix_s"], MS / 1000);
        }
        end(
            &mut runtime.host.lock().unwrap(),
            JOB,
            11,
            MS + 331_000,
            terminal(JOB, "exit"),
        );
        let (done, completed) = std::sync::mpsc::channel();
        let tick_runtime = runtime.clone();
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        let tick = std::thread::spawn(move || {
            tracing::dispatcher::with_default(&dispatch, || {
                let _ = done.send(tick_runtime.tick(MS + 331_000));
            })
        });
        let result = completed.recv_timeout(std::time::Duration::from_secs(5));
        // Always release the blocked read-only queries, including on a failed assertion.
        control.release();
        result.expect("stop waited on a diagnostics query").unwrap();
        tick.join().unwrap();
        assert_eq!(control.stops.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(!runtime.host.lock().unwrap().snapshot.vms["tenant"].stop_pending);
    });
    let captured = captured.lock().unwrap();
    let lost: Vec<_> = captured
        .iter()
        .filter(|s| s.contains("lease_settled_lost_stop_pending"))
        .collect();
    assert_eq!(lost.len(), 4);
    for event in lost {
        assert!(event.starts_with("WARN "));
        for field in [
            "job_id=",
            "tenant=",
            "started_at_ms=",
            "max_s=30",
            "cap_ms=",
        ] {
            assert!(event.contains(field), "{event}");
        }
    }
    for name in [
        "tenant_pending_stop_released",
        "tenant_stop_pending_cleared",
    ] {
        assert_eq!(
            captured
                .iter()
                .filter(|s| s.starts_with("INFO ") && s.contains(name))
                .count(),
            1
        );
    }
}

#[test]
fn lost_diagnostics_bound_journal_entries_and_unicode_lines() {
    let entries: Vec<_> = (0..55).map(|index| json!({"unix_us":MS * 1000,
        "message":if index == 0 { "first\nsecond".into() } else { format!("{}SHOULD_NOT_APPEAR", "é".repeat(300)) }
    })).collect();
    let (control, _) = DiagnosticsControl::new(
        Some(json!({"ok":true,"journal":{"entries":entries}})),
        false,
    );
    let logs = Logs::default();
    let captured = logs.0.clone();
    tracing::subscriber::with_default(logs, || {
        crate::lost_diagnostics::collect(
            control,
            "tenant",
            vec![(JOB.into(), MS / 1000)],
            std::time::Duration::from_secs(5),
        )
    });
    let captured = captured.lock().unwrap();
    let lines: Vec<_> = captured
        .iter()
        .filter(|s| s.contains("lost_job_runner_log"))
        .collect();
    assert_eq!(lines.len(), 50);
    assert!(lines[0].contains("line=first second"), "{}", lines[0]);
    for line in &lines[1..] {
        assert_eq!(line.chars().filter(|c| *c == 'é').count(), 300);
        assert!(!line.contains("SHOULD_NOT_APPEAR"));
        assert!(line.contains(JOB));
    }
    assert!(captured
        .iter()
        .any(|s| s.contains("lost_job_diagnostics_finished") && s.contains("truncated=true")));
}

#[test]
fn lost_diagnostics_failures_and_shared_timeout_are_logged_without_late_entries() {
    for reply in [
        None,
        Some(json!({"ok":false})),
        Some(json!({"ok":true,"journal":{"unavailable":true}})),
        Some(json!({"ok":true,"journal":{"unavailable":true,"error":{
            "type":"journal_command_failed", "exit_status":7, "stderr_tail":"Permission denied"
        }}})),
    ] {
        let command_failure = reply
            .as_ref()
            .is_some_and(|reply| reply["journal"]["error"]["type"] == "journal_command_failed");
        let (control, _) = DiagnosticsControl::new(reply, false);
        let logs = Logs::default();
        let captured = logs.0.clone();
        tracing::subscriber::with_default(logs, || {
            crate::lost_diagnostics::collect(
                control,
                "tenant",
                vec![(JOB.into(), MS / 1000)],
                std::time::Duration::from_secs(5),
            )
        });
        let captured = captured.lock().unwrap();
        assert_eq!(
            captured
                .iter()
                .filter(|s| s.starts_with("WARN ") && s.contains("lost_job_diagnostics_failed"))
                .count(),
            1
        );
        assert!(!captured.iter().any(|s| s.contains("lost_job_runner_log")));
        if command_failure {
            assert!(captured
                .iter()
                .any(|s| s.contains("exit_status") && s.contains("Permission denied")));
        }
    }
    let (control, _) = DiagnosticsControl::new(
        Some(json!({"ok":true,"journal":{"entries":[{"message":"LATE_ENTRY"}]}})),
        true,
    );
    let logs = Logs::default();
    let captured = logs.0.clone();
    tracing::subscriber::with_default(logs, || {
        crate::lost_diagnostics::collect(
            control.clone(),
            "tenant",
            vec![(JOB.into(), MS / 1000), (JOB2.into(), MS / 1000)],
            std::time::Duration::from_millis(10),
        )
    });
    control.release();
    let captured = captured.lock().unwrap();
    assert_eq!(
        captured
            .iter()
            .filter(|s| s.starts_with("WARN ") && s.contains("lost_job_diagnostics_timed_out"))
            .count(),
        2
    );
    assert!(!captured.iter().any(|s| s.contains("LATE_ENTRY")));
}

#[test]
fn replaced_job_usage_conflict_is_final_and_logged_as_expected_once() {
    let logs = Logs::default();
    let captured = logs.0.clone();
    tracing::subscriber::with_default(logs, || {
        let mut h = host();
        started(&mut h, JOB, 1);
        end(&mut h, JOB, 3, MS + 3000, terminal(JOB, "exit"));
        h.terminal_delivered(JOB, MS + 3000).unwrap();
        let request = h.snapshot.outbox[0].clone();
        assert!(h
            .finish_request(&request, 409, &json!({"error":"job_conflict"}))
            .unwrap());
        assert!(h.snapshot.outbox.is_empty());
        h.billing_tick(MS + 4000).unwrap();
        assert!(h.snapshot.outbox.is_empty());
    });
    let captured = captured.lock().unwrap();
    assert_eq!(
        captured
            .iter()
            .filter(|s| s.contains("usage_ignored_after_replace"))
            .count(),
        1
    );
    assert!(!captured.iter().any(
        |s| s.contains("usage_billing_diverged") || s.contains("usage_reconciliation_required")
    ));
}

#[test]
fn differing_settlement_and_final_409_log_divergence_but_never_change_caller_figure() {
    let logs = Logs::default();
    let captured = logs.0.clone();
    tracing::subscriber::with_default(logs, || {
        for (status, reply) in [
            (
                200,
                json!({"job_id":JOB,"settled_vcpu_ms":1,"released_vcpu_ms":29999}),
            ),
            (409, json!({"error":"job_settled"})),
        ] {
            let mut h = host();
            started(&mut h, JOB, 1);
            end(&mut h, JOB, 3, MS + 3000, terminal(JOB, "exit"));
            h.terminal_delivered(JOB, MS + 3000).unwrap();
            let figure = h.snapshot.billing[JOB].hosted_terminal.clone();
            let request = h.snapshot.outbox[0].clone();
            h.finish_request(&request, status, &reply).unwrap();
            assert_eq!(h.snapshot.billing[JOB].hosted_terminal, figure);
        }
    });
    assert_eq!(
        captured
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.contains("usage_billing_diverged"))
            .count(),
        2
    );
    assert_eq!(
        captured
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.contains("usage_reconciliation_required"))
            .count(),
        1
    );
}

#[test]
fn watchdog_runtime_stops_the_only_lease_once_and_clears_durable_stop_pending() {
    use anyhow::{bail, Result};
    use motor_host::{helper::Control, server::Runtime};
    struct Stop(std::sync::atomic::AtomicUsize);
    impl Control for Stop {
        fn adopt(&self, _: &str) -> Result<bool> {
            Ok(false)
        }
        fn wake(&self, _: &str, _: Option<u64>) -> Result<u64> {
            bail!("unexpected wake")
        }
        fn quota(&self, _: &str, _: u32) -> Result<()> {
            Ok(())
        }
        fn relay(&self, _: &str, _: u32) -> Result<std::os::unix::net::UnixStream> {
            bail!("unexpected relay")
        }
        fn admin(&self, _: &str, _: &Value) -> Result<Value> {
            Ok(json!({"ok":true}))
        }
        fn suspend(&self, _: &str) -> Result<()> {
            bail!("unexpected suspend")
        }
        fn available_bytes(&self) -> Result<u64> {
            Ok(u64::MAX)
        }
        fn stop(&self, _: &str) -> Result<()> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }
    let mut h = host();
    started(&mut h, JOB, 1);
    h.guest_ready("tenant", 1).unwrap();
    let control = Arc::new(Stop(std::sync::atomic::AtomicUsize::new(0)));
    let runtime = Runtime::new("host".into(), h, control.clone());
    runtime.tick(MS + 30_000).unwrap();
    assert_eq!(runtime.host.lock().unwrap().share("tenant", MS + 30_000), 0);
    runtime.tick(MS + 330_000).unwrap();
    runtime.tick(MS + 331_000).unwrap();
    assert_eq!(control.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    let h = runtime.host.lock().unwrap();
    assert!(!h.snapshot.vms["tenant"].stop_pending);
    assert!(h.snapshot.vms["tenant"].initialized_generation.is_none());
}

#[test]
fn retention_deletes_records_just_past_the_retention_period_and_keeps_unsettled_reports() {
    use motor_host::billing::{RECORD_RETENTION_MS, RETENTION_INTERVAL_MS};
    let dir = scratch();
    let mut h = disk_host(dir.path());
    h.consume_nonce("nonce".into(), NOW, NOW).unwrap();
    // JOB ends one millisecond before JOB2. Its usage report is never
    // confirmed, so after the 30-day retry window it is set aside.
    started(&mut h, JOB, 1);
    end(&mut h, JOB, 3, MS + 3000, terminal(JOB, "exit"));
    h.terminal_delivered(JOB, MS + 3000).unwrap();
    started(&mut h, JOB2, 4);
    end(&mut h, JOB2, 6, MS + 3001, terminal(JOB2, "exit"));
    h.terminal_delivered(JOB2, MS + 3001).unwrap();
    let settled = h.snapshot.outbox[1].clone();
    assert!(h
        .finish_request(
            &settled,
            200,
            &json!({"job_id":JOB2,"settled_vcpu_ms":3001,"released_vcpu_ms":0})
        )
        .unwrap());
    h.expire_outbox(MS + 3000 + OUTBOX_MAX_AGE_MS).unwrap();
    assert_eq!(h.snapshot.expired_outbox.len(), 1);
    let archive = dir.path().join("outbox-expired");
    assert_eq!(std::fs::read_dir(&archive).unwrap().count(), 1);
    // JOB3 finished as long ago as JOB, but its report is still queued and
    // inside its retry window (re-queued a day before the pass).
    started(&mut h, JOB3, 7);
    end(&mut h, JOB3, 9, MS + 3000, terminal(JOB3, "exit"));
    h.terminal_delivered(JOB3, MS + 3000).unwrap();
    let boundary = MS + 3001 + RECORD_RETENTION_MS;
    h.snapshot.outbox[0].created_ms = boundary - 86_400_000;

    // JOB2 finished exactly one retention period ago: not more, so it stays.
    assert_eq!(h.prune_retention(boundary - 1).unwrap(), 0);
    assert_eq!(h.retention_tick(boundary).unwrap(), 1);
    assert!(h.snapshot.nonces.is_empty());
    for (job, kept) in [(JOB, false), (JOB2, true), (JOB3, true)] {
        assert_eq!(h.snapshot.leases.contains_key(job), kept, "{job} lease");
        assert_eq!(h.snapshot.billing.contains_key(job), kept, "{job} billing");
    }
    assert!(h.snapshot.expired_outbox.is_empty());
    assert_eq!(std::fs::read_dir(&archive).unwrap().count(), 0);
    // The pruned state is what the journal holds after a restart.
    drop(h);
    let mut h = disk_host(dir.path());
    assert!(!h.snapshot.leases.contains_key(JOB));
    assert!(h.snapshot.leases.contains_key(JOB2));
    // A pass is not repeated before its interval: the tick one millisecond
    // later skips JOB2 even though it is then past the retention period.
    assert_eq!(h.retention_tick(boundary).unwrap(), 0);
    assert_eq!(h.retention_tick(boundary + 1).unwrap(), 0);
    assert!(h.snapshot.leases.contains_key(JOB2));
    // A pass at that instant deletes JOB2. JOB3 is far older but still
    // unsettled inside its retry window, so it survives.
    assert_eq!(h.prune_retention(boundary + 1).unwrap(), 1);
    assert!(!h.snapshot.leases.contains_key(JOB2));
    assert!(h.snapshot.leases.contains_key(JOB3));
    assert_eq!(
        h.retention_tick(boundary + RETENTION_INTERVAL_MS).unwrap(),
        0
    );
    assert!(h.snapshot.leases.contains_key(JOB3));
    // Once the retry window lapses the report is set aside, and the next pass
    // deletes it with the rest of JOB3's records.
    let lapsed = boundary - 86_400_000 + OUTBOX_MAX_AGE_MS;
    h.expire_outbox(lapsed).unwrap();
    assert_eq!(std::fs::read_dir(&archive).unwrap().count(), 1);
    assert_eq!(h.prune_retention(lapsed).unwrap(), 1);
    assert!(h.snapshot.leases.is_empty() && h.snapshot.billing.is_empty());
    assert!(h.snapshot.outbox.iter().all(|r| r.job.is_none()));
    assert!(h.snapshot.expired_outbox.is_empty());
    assert_eq!(std::fs::read_dir(&archive).unwrap().count(), 0);
}

#[test]
fn retention_never_deletes_a_running_lease_and_drains_a_backlog_in_bounded_passes() {
    use motor_host::billing::{RECORD_RETENTION_MS, RETENTION_BATCH};
    let mut h = host();
    started(&mut h, JOB, 1);
    end(&mut h, JOB, 3, MS + 3000, terminal(JOB, "exit"));
    h.billing_tick(MS + 63_000).unwrap();
    let request = h.snapshot.outbox[0].clone();
    h.finish_request(
        &request,
        200,
        &json!({"job_id":JOB,"settled_vcpu_ms":3000,"released_vcpu_ms":0}),
    )
    .unwrap();
    let finished = h.snapshot.leases[JOB].clone();
    let total = RETENTION_BATCH + 10;
    for n in 0..total {
        h.snapshot
            .leases
            .insert(format!("old-{n:04}"), finished.clone());
    }
    started(&mut h, JOB2, 4);
    let later = MS + 3000 + RECORD_RETENTION_MS + 1;
    assert_eq!(h.retention_tick(later).unwrap(), RETENTION_BATCH);
    // A full batch leaves the next pass due at once, on the next tick.
    assert_eq!(
        h.retention_tick(later).unwrap(),
        total + 1 - RETENTION_BATCH
    );
    assert_eq!(h.snapshot.leases.keys().collect::<Vec<_>>(), vec![JOB2]);
}

#[test]
fn absent_burst_telemetry_preserves_heartbeat_and_usage_bytes() {
    let mut h = host();
    assert_eq!(
        serde_json::to_string(&h.heartbeat(MS, 245760, true)).unwrap(),
        r#"{"free_vcpus":2,"free_mem_mib":245760,"queued":0,"warm":[],"running":[],"healthy":true}"#
    );
    started(&mut h, JOB, 1);
    end(&mut h, JOB, 3, MS + 3000, terminal(JOB, "exit"));
    h.terminal_delivered(JOB, MS + 3000).unwrap();
    let request = h
        .snapshot
        .outbox
        .iter()
        .find(|r| r.path.ends_with("/usage"))
        .unwrap();
    assert_eq!(
        std::str::from_utf8(&request.body).unwrap(),
        r#"{"job_id":"00000000-0000-7000-8000-000000000001","vcpus":1,"started_at_ms":1800000000000,"ended_at_ms":1800000003000,"mem_peak_mib":null,"egress_bytes":45,"outcome":"exit"}"#
    );
}

#[test]
fn usage_reports_only_observed_memory_kill_causes_and_never_invents_overage() {
    for burst in [None, Some(8192)] {
        for cause in [
            None,
            Some("job_limit"),
            Some("host_pressure"),
            Some("host_oom"),
            Some("unknown"),
        ] {
            let mut h = host();
            let mut c = claims(JOB);
            c.mem_burst_mib = burst;
            let (connection, _) = h.claim(c, MS).unwrap();
            h.delivered(connection).unwrap();
            for (seq, kind) in [
                (1, TenantEventKind::Admitted),
                (2, TenantEventKind::Started),
            ] {
                h.event("tenant", &event(seq, kind, Some(JOB), None), MS)
                    .unwrap();
            }
            let mut frame = serde_json::to_value(
                motor_protocol::TerminalRecord::new(
                    JOB.parse().unwrap(),
                    motor_protocol::Outcome::Signal { signal: 9 },
                    3000,
                    0,
                    0,
                )
                .with_ran(motor_protocol::Ran::Remote),
            )
            .unwrap();
            if let Some(cause) = cause {
                frame["oom_kill"] =
                    serde_json::to_value(motor_protocol::OomKill::new(cause)).unwrap();
            }
            end(&mut h, JOB, 3, MS + 3000, frame);
            h.terminal_delivered(JOB, MS + 3000).unwrap();
            let request = h
                .snapshot
                .outbox
                .iter()
                .find(|r| r.path.ends_with("/usage"))
                .unwrap();
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let expected = cause.filter(|cause| *cause != "unknown");
            assert_eq!(
                body.get("oom_kill"),
                expected.map(|cause| json!(cause)).as_ref()
            );
            assert!(body.get("mem_over_mib_ms").is_none());
            assert_eq!(
                h.snapshot.billing[JOB].hosted_terminal.as_ref().unwrap()["billed_vcpu_ms"],
                3000
            );
        }
    }
}
