use super::*;

#[test]
fn linux_availability_requires_the_broker_not_a_runner_sample() {
    let now = Instant::now();
    let mut cache = HealthCache::default();
    cache.update(true, now);
    assert_eq!(
        cache.report(now).metrics.unwrap()["platforms"]["linux"]["status"],
        "unavailable"
    );
    cache.account = Some(json!({"sign_in": "signed in", "broker": "ok"}));
    let report = cache.report(now);
    assert!(!report.detail.unwrap().contains("Linux runner"));
    assert!(report.metrics.as_ref().unwrap()["connection"]
        .get("runner")
        .is_none());
    assert_eq!(
        report.metrics.unwrap()["platforms"]["linux"]["status"],
        "available"
    );
}

#[test]
fn busy_platform_is_healthy_without_host_telemetry() {
    let now = Instant::now();
    let mut cache = HealthCache::default();
    cache.update(true, now);
    cache.account = Some(json!({"sign_in": "signed in", "broker": "ok"}));
    cache.jobs = Some(json!({"linux": {"waiting_for_capacity": 1}}));
    let report = cache.report(now);
    assert_eq!(report.status, HealthStatus::Ok);
    assert!(report.detail.as_ref().unwrap().contains("linux: busy"));
    let metrics = report.metrics.unwrap();
    assert_eq!(metrics["platforms"]["linux"]["status"], "busy (queueing)");
    for key in [
        "build_server",
        "queue_wait_1h",
        "top_repos",
        "sample_age_ms",
        "headline",
        "history",
        "windows_runners",
        "hosted",
        "direct_linux_workload",
    ] {
        assert!(metrics.get(key).is_none(), "{key}");
    }
    let output = format!("{} {metrics}", report.detail.unwrap());
    for private in [
        "build server",
        "vCPU",
        "slots",
        "permits",
        "mem_available",
        "disk_free",
        "load_1m",
        "sample stale",
    ] {
        assert!(!output.contains(private), "{output}");
    }
    assert!(output.contains(machine_name()));
    assert!(!output.contains("your Mac"));
}

#[test]
fn windows_aggregates_all_endpoints_without_names_or_limits() {
    let now = Instant::now();
    let mut cache = HealthCache::default();
    cache.config.windows_runner_instance = Some("windows".into());
    cache.config.windows_runner_state_root = Some("state".into());
    cache.update(true, now);
    cache.windows = vec![
        json!({"name": "Ally", "reachable": false, "busy": null, "max_concurrent": 97}),
        json!({"name": "ovh-2", "reachable": true, "busy": true, "max_concurrent": 53}),
    ];
    let report = cache.report(now);
    assert_eq!(
        report.metrics.as_ref().unwrap()["platforms"]["windows"]["status"],
        "busy (queueing)"
    );
    let output = format!("{} {}", report.detail.unwrap(), report.metrics.unwrap());
    for private in ["Ally", "ovh-2", "max_concurrent"] {
        assert!(!output.contains(private), "{output}");
    }
    cache
        .windows
        .push(json!({"reachable": true, "busy": false}));
    assert_eq!(
        cache.report(now).metrics.unwrap()["platforms"]["windows"]["status"],
        "available"
    );
    for runner in &mut cache.windows {
        runner["reachable"] = json!(false);
    }
    assert_eq!(
        cache.report(now).metrics.unwrap()["platforms"]["windows"]["status"],
        "unavailable"
    );
}

#[test]
fn macos_availability_uses_its_own_sample_and_reports_missing_configuration() {
    let now = Instant::now();
    let mut cache = HealthCache::default();
    cache.update(true, now);
    assert_eq!(
        cache.report(now).metrics.unwrap()["platforms"]["macos"]["reason"],
        "not set up on this machine"
    );
    cache.config.macos_runner = Some(crate::MacosRunner::default());
    for (sample, expected) in [
        (json!({"reachable": false}), "unavailable"),
        (json!({"reachable": true, "busy": true}), "busy (queueing)"),
        (json!({"reachable": true, "busy": false}), "available"),
    ] {
        cache.macos = Some(sample);
        assert_eq!(
            cache.report(now).metrics.unwrap()["platforms"]["macos"]["status"],
            expected
        );
    }
}

#[test]
fn hosted_requires_sign_in_and_broker_and_uses_local_capacity_waits() {
    let now = Instant::now();
    let mut cache = HealthCache::default();
    cache.update(true, now);
    assert_eq!(
        cache.report(now).metrics.unwrap()["platforms"]["linux"]["status"],
        "unavailable"
    );
    cache.account = Some(json!({"sign_in": "signed in", "broker": "unavailable"}));
    assert_eq!(
        cache.report(now).metrics.unwrap()["platforms"]["linux"]["status"],
        "unavailable"
    );
    cache.account = Some(json!({"sign_in": "signed in", "broker": "ok"}));
    assert_eq!(
        cache.report(now).metrics.unwrap()["platforms"]["linux"]["status"],
        "available"
    );
    cache.jobs = Some(
        json!({"linux": {"running": 1, "waiting_for_capacity": 2, "unknown": 0}, "windows": {"running": 0, "waiting_for_capacity": 0, "unknown": 0}, "macos": {"running": 0, "waiting_for_capacity": 0, "unknown": 0}}),
    );
    let report = cache.report(now);
    assert_eq!(
        report.metrics.unwrap()["platforms"]["linux"]["status"],
        "busy (queueing)"
    );
    assert!(report
        .detail
        .unwrap()
        .contains("1 running, 2 waiting for capacity"));
}

#[test]
fn idle_cache_stales_after_two_minutes_and_recovers() {
    let now = Instant::now();
    let mut cache = HealthCache::default();
    cache.update(true, now);
    assert_eq!(
        cache.report(now + Duration::from_secs(119)).status,
        HealthStatus::Ok
    );
    let stale = cache.report(now + STALE_AFTER);
    assert_eq!(stale.status, HealthStatus::Degraded);
    assert_eq!(
        stale.metrics.unwrap()["platforms"]["linux"]["status"],
        "unavailable"
    );
    assert!(!stale.detail.unwrap().contains("sample stale"));
    cache.update(true, now + STALE_AFTER);
    assert_eq!(cache.report(now + STALE_AFTER).status, HealthStatus::Ok);
}

#[test]
fn unreachable_samples_and_three_failures_degrade_and_recover() {
    let now = Instant::now();
    let mut cache = HealthCache::default();
    cache.update(true, now);
    for failure in 1..=3 {
        cache.update(false, now + Duration::from_secs(15 * failure));
        let report = cache.report(now + Duration::from_secs(15 * failure));
        assert_eq!(
            report.status,
            if failure == 3 {
                HealthStatus::Degraded
            } else {
                HealthStatus::Ok
            }
        );
        assert_eq!(
            report.metrics.unwrap()["platforms"]["linux"]["status"],
            "unavailable"
        );
    }
    cache.update(true, now + Duration::from_secs(46));
    assert_eq!(
        cache.report(now + Duration::from_secs(46)).status,
        HealthStatus::Ok
    );
}

#[test]
fn never_sampled_cache_has_startup_grace_but_not_forever() {
    let cache = HealthCache::default();
    assert_eq!(cache.report(cache.created).status, HealthStatus::Ok);
    assert_eq!(
        cache.report(cache.created + STALE_AFTER).status,
        HealthStatus::Degraded
    );
}

#[test]
fn account_summary_excludes_claims_and_distinguishes_auth_from_connection() {
    for (status, broker_status, sign_in, broker) in [
        ("ok", Some(400), "signed in", "ok"),
        ("ok", Some(403), "not signed in; sign in with ck", "ok"),
        ("ok", None, "signed in", "unavailable"),
        (
            "transient",
            None,
            "not confirmed; retry sign-in",
            "unavailable",
        ),
        (
            "not_found",
            None,
            "not signed in; sign in with ck",
            "unavailable",
        ),
    ] {
        let check = crate::hosted::check::Check {
            status: status.into(),
            retry_after_ms: Some(1000),
            broker_status,
            claims: Some(crate::hosted::check::Claims {
                aud: "cortexkit-runners".into(),
                account: "private-account-id".into(),
                iat: 1,
                exp: 2,
            }),
        };
        assert_eq!(
            account_health(check),
            json!({"sign_in": sign_in, "broker": broker})
        );
    }
}

#[test]
fn local_failure_preserves_failing_status_and_marks_platforms_unavailable() {
    let now = Instant::now();
    let mut cache = HealthCache::default();
    cache.update(true, now);
    let report = cache.report_with_refusal(now, Some("storage_lease_busy"));
    assert_eq!(report.status, HealthStatus::Failing);
    assert!(report.detail.unwrap().contains("storage_lease_busy"));
    let metrics = report.metrics.unwrap();
    assert_eq!(metrics["connection"]["error"], "storage_lease_busy");
    for platform in ["linux", "windows", "macos"] {
        assert_eq!(metrics["platforms"][platform]["status"], "unavailable");
    }
}

fn hosted_text_cache(now: Instant) -> HealthCache {
    let mut cache = HealthCache::default();
    cache.update(true, now);
    cache.account = Some(json!({"sign_in": "signed in", "broker": "ok"}));
    cache.jobs = Some(json!({
        "linux": {"running": 0, "waiting_for_capacity": 0, "unknown": 0},
        "windows": {"running": 0, "waiting_for_capacity": 0, "unknown": 0},
        "macos": {"running": 0, "waiting_for_capacity": 0, "unknown": 0},
    }));
    cache
}

#[test]
fn hosted_text_names_machine_once_and_omits_zero_and_unavailable_counts() {
    let now = Instant::now();
    let mut cache = hosted_text_cache(now);
    cache.jobs.as_mut().unwrap()["windows"] =
        json!({"running": 4, "waiting_for_capacity": 5, "unknown": 6});
    let report = cache.report(now);
    assert_eq!(report.detail.unwrap(), format!(
        "{}: daemon running; broker ok; sign-in: signed in; linux: available (requests accepted); windows: not set up on this machine; macos: not set up on this machine",
        machine_name(),
    ));
    let metrics = report.metrics.unwrap();
    assert_eq!(metrics["machine"]["jobs"]["linux"]["running"], 0);
    assert_eq!(metrics["machine"]["jobs"]["windows"]["running"], 4);
    assert_eq!(metrics["platforms"]["windows"]["status"], "unavailable");
}

#[test]
fn hosted_busy_text_only_includes_nonzero_counts() {
    let now = Instant::now();
    let mut cache = hosted_text_cache(now);
    for (running, waiting, expected) in [
        (1, 2, "busy, 1 running, 2 waiting for capacity"),
        (0, 2, "busy, 2 waiting for capacity"),
        (1, 0, "available (requests accepted), 1 running"),
    ] {
        cache.jobs.as_mut().unwrap()["linux"] =
            json!({"running": running, "waiting_for_capacity": waiting, "unknown": 0});
        let report = cache.report(now);
        assert_eq!(report.detail.unwrap(), format!(
            "{}: daemon running; broker ok; sign-in: signed in; linux: {expected}; windows: not set up on this machine; macos: not set up on this machine",
            machine_name(),
        ));
        assert_eq!(
            report.metrics.unwrap()["platforms"]["linux"]["status"],
            if waiting > 0 {
                "busy (queueing)"
            } else {
                "available"
            }
        );
    }
}

#[test]
fn unconfirmed_jobs_hint_names_the_supported_per_job_status_command() {
    let now = Instant::now();
    let mut cache = hosted_text_cache(now);
    cache.jobs.as_mut().unwrap()["linux"]["unknown"] = json!(3);
    let detail = cache.report(now).detail.unwrap();
    assert!(
        detail.contains("3 job states not confirmed; ck motor attach JOB_ID --status"),
        "{detail}"
    );
    assert!(!detail.contains("ck motor status"), "{detail}");
}

#[test]
fn unavailable_connection_text_omits_job_counts_without_hiding_metrics() {
    let now = Instant::now();
    let mut cache = hosted_text_cache(now);
    cache.jobs.as_mut().unwrap()["linux"] =
        json!({"running": 7, "waiting_for_capacity": 8, "unknown": 9});
    cache.account.as_mut().unwrap()["broker"] = json!("unavailable");
    let report = cache.report(now);
    assert_eq!(report.detail.unwrap(), format!(
        "{}: daemon running; broker unavailable; sign-in: signed in; linux: unavailable (broker connection not confirmed; check network and retry); windows: not set up on this machine; macos: not set up on this machine",
        machine_name(),
    ));
    assert_eq!(
        report.metrics.unwrap()["machine"]["jobs"]["linux"],
        json!({"running": 7, "waiting_for_capacity": 8, "unknown": 9})
    );
}

#[test]
fn linux_health_never_mentions_a_direct_runner() {
    let now = Instant::now();
    let mut cache = HealthCache::default();
    cache.update(true, now);
    cache.account = Some(json!({"sign_in": "signed in", "broker": "ok"}));
    for repositories in [Vec::new(), vec!["repository".into()]] {
        cache.config.hosted_repositories = repositories;
        let report = cache.report(now);
        // The caller's hostname may itself contain "runner"; inspect only the summary.
        let detail = report.detail.unwrap();
        let summary = detail
            .strip_prefix(&format!("{}: ", machine_name()))
            .unwrap_or(&detail);
        assert!(!summary.contains("runner"), "{detail}");
        let connection = report.metrics.unwrap()["connection"].clone();
        assert!(connection.get("runner").is_none());
        assert_eq!(connection["broker"], "ok");
    }
}

#[test]
fn failing_detail_leads_with_reason_before_caller_facing_summary() {
    let now = Instant::now();
    let cache = hosted_text_cache(now);
    let report = cache.report_with_refusal(now, Some("runner_credential_grant_refused"));
    assert_eq!(report.status, HealthStatus::Failing);
    let detail = report.detail.unwrap();
    assert!(
        detail.starts_with(&format!(
            "runner_credential_grant_refused; {}: daemon running;",
            machine_name(),
        )),
        "{detail}"
    );
    assert_eq!(detail.matches("runner_credential_grant_refused").count(), 1);
    assert_eq!(
        report.metrics.unwrap()["connection"]["error"],
        "runner_credential_grant_refused"
    );
}
