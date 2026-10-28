use super::*;
use motor_host::billing::{sign_request, Metrics, OUTBOX_MAX_AGE_MS};
use motor_host::server::LifecycleStream;
use motor_lease::{JobState, UsageOutcome};
use serde_json::{json, Value};

const MS: u64 = NOW * 1000;

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
        assert_eq!(
            (usage.mem_peak_mib, usage.egress_bytes),
            (Some(13), Some(45))
        );
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
fn watchdog_cancels_drops_share_across_restart_settles_once_and_defers_stop_for_healthy_job() {
    let dir = scratch();
    let mut h = disk_host(dir.path());
    started(&mut h, JOB, 1);
    started(&mut h, JOB2, 3);
    // The second command starts later and remains inside its own runtime cap.
    h.snapshot.leases.get_mut(JOB2).unwrap().started_at_ms = Some(MS + 20_000);
    h.snapshot.leases.get_mut(JOB2).unwrap().claims.max_s = 120;
    h.billing_tick(MS + 30_000).unwrap();
    assert_eq!(h.share("tenant", MS + 30_000), 1);
    assert!(h.snapshot.pending_cancels.contains_key(JOB));
    h.cancel_delivered(JOB).unwrap();
    drop(h);
    let mut h = disk_host(dir.path());
    assert_eq!(h.share("tenant", MS + 31_000), 1);
    h.billing_tick(MS + 89_999).unwrap();
    assert!(!h.snapshot.billing[JOB].host_settled);
    h.billing_tick(MS + 90_000).unwrap();
    assert!(h.snapshot.billing[JOB].host_settled);
    assert!(h.snapshot.vms["tenant"].stop_pending);
    assert!(h.stops_due().is_empty());
    assert!(matches!(
        h.claim(claims(JOB3), MS + 90_000),
        Err(motor_host::state::Error::Refused(
            motor_host::state::Refusal::Capacity
        ))
    ));
    end(&mut h, JOB2, 5, MS + 91_000, terminal(JOB2, "exit"));
    assert_eq!(
        h.snapshot.billing[JOB2].usage.as_ref().unwrap().outcome,
        UsageOutcome::Exit
    );
    assert_eq!(h.stops_due(), vec!["tenant"]);
    h.stopped("tenant").unwrap();
    assert!(h.stops_due().is_empty());
    h.billing_tick(MS + 150_000).unwrap();
    let original = serde_json::to_vec(&h.snapshot.billing[JOB].usage).unwrap();
    h.event(
        "tenant",
        &event(6, TenantEventKind::Terminal, Some(JOB), None),
        MS + 160_000,
    )
    .unwrap();
    h.receive_terminal(JOB, terminal(JOB, "exit"), MS + 160_000)
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
    for receipt in [59_000, 89_000] {
        let mut h = host();
        started(&mut h, JOB, 1);
        h.billing_tick(MS + 30_000).unwrap();
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
/// keeps its memory peak and egress in settlement: those VM-wide figures
/// include the fill, which is that tenant's real usage on its own VM. Only a
/// second job of the same tenant marks a job as overlapping.
#[test]
fn a_job_beside_its_tenants_fill_keeps_its_memory_and_egress() {
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
            (Some(13), Some(45)),
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
    assert_eq!(
        (usage.mem_peak_mib, usage.egress_bytes),
        (Some(13), Some(45))
    );
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
struct Logs(Arc<Mutex<Vec<String>>>);
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
            fn record_debug(&mut self, _: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.push_str(&format!("{value:?}"));
            }
        }
        let mut visitor = Visitor(String::new());
        event.record(&mut visitor);
        self.0.lock().unwrap().push(visitor.0);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
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
    runtime.tick(MS + 90_000).unwrap();
    runtime.tick(MS + 91_000).unwrap();
    assert_eq!(control.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    let h = runtime.host.lock().unwrap();
    assert!(!h.snapshot.vms["tenant"].stop_pending);
    assert!(h.snapshot.vms["tenant"].initialized_generation.is_none());
}

#[test]
fn retention_deletes_records_just_past_thirteen_months_and_keeps_unsettled_reports() {
    use motor_host::billing::{BILLING_RETENTION_MS, RETENTION_INTERVAL_MS};
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
    let boundary = MS + 3001 + BILLING_RETENTION_MS;
    h.snapshot.outbox[0].created_ms = boundary - 86_400_000;

    // JOB2 finished exactly thirteen months ago: not more than, so it stays.
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
    // later skips JOB2 even though it is then past thirteen months.
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
    use motor_host::billing::{BILLING_RETENTION_MS, RETENTION_BATCH};
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
    let later = MS + 3000 + BILLING_RETENTION_MS + 1;
    assert_eq!(h.retention_tick(later).unwrap(), RETENTION_BATCH);
    // A full batch leaves the next pass due at once, on the next tick.
    assert_eq!(
        h.retention_tick(later).unwrap(),
        total + 1 - RETENTION_BATCH
    );
    assert_eq!(h.snapshot.leases.keys().collect::<Vec<_>>(), vec![JOB2]);
}
