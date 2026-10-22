use motor_protocol::{
    Outcome, Ran, RefusalReason, RunRequest, TerminalRecord, SPILL_WITHDRAWAL_DETAIL_PREFIX,
};
use tokio::time::Instant;

pub(crate) fn eligible(
    config: &crate::Config,
    request: &RunRequest,
    gate: bool,
    opt_in: bool,
) -> bool {
    config.spill_to_hosted
        && (!gate || opt_in)
        && request.network.is_none()
        && matches!(
            request.platform,
            None | Some(motor_protocol::Platform::Linux)
        )
        && request.weight_hint.unwrap_or(2) <= config.spill_max_vcpus
        && crate::hosted::validate(request).is_ok()
}

pub(crate) struct Watch {
    after: u64,
    permits: Option<(Instant, u64)>,
    may_have_started: bool,
    /// The capacity wait when a spill withdrawal was sent, if one was.
    spill_requested: Option<u64>,
}
impl Watch {
    pub(crate) fn new(after: u64) -> Self {
        Self {
            after,
            permits: None,
            may_have_started: false,
            spill_requested: None,
        }
    }
    pub(crate) fn observe(&mut self, elapsed: Option<u64>) {
        self.permits = elapsed.map(|ms| (Instant::now(), ms));
    }
    pub(crate) fn started(&mut self) {
        self.may_have_started = true;
        self.permits = None;
    }
    pub(crate) fn disconnected(&mut self) {
        self.permits = None;
    }
    pub(crate) fn wait_ms(&self) -> u64 {
        self.permits.map_or(0, |(at, elapsed)| {
            elapsed.saturating_add(at.elapsed().as_millis().try_into().unwrap_or(u64::MAX))
        })
    }
    /// Record a withdrawal being sent, with whether it asked to spill.
    pub(crate) fn withdraw_sent(&mut self, spill: bool) {
        if spill {
            self.spill_requested = Some(self.wait_ms());
        }
    }
    /// The capacity wait to report for a spill: at least the wait observed
    /// when the spill withdrawal was sent, since later heartbeats may no
    /// longer carry one.
    pub(crate) fn spill_wait_ms(&self) -> u64 {
        self.spill_requested.unwrap_or(0).max(self.wait_ms())
    }
    pub(crate) fn withdrawn(&self, terminal: &TerminalRecord) -> bool {
        // Once the runner withdraws the job, its heartbeats stop carrying a
        // queue wait, and one can arrive before the terminal record. That
        // clears the observed permit wait, so `ready` alone would let the
        // terminal through as a refusal. A spill request sent while `ready`
        // held still counts when the runner marks the terminal as a spill
        // withdrawal, which it does only for a job that never started.
        let spill_acknowledged = self.spill_requested.is_some()
            && !self.may_have_started
            && terminal
                .refusal_detail()
                .is_some_and(|detail| detail.starts_with(SPILL_WITHDRAWAL_DETAIL_PREFIX));
        (self.ready() || spill_acknowledged)
            && terminal.ran == Some(Ran::None)
            && terminal.outcome
                == Outcome::RefusedBeforeStart {
                    reason: RefusalReason::QueueWaitExceeded,
                }
    }
    pub(crate) fn ready(&self) -> bool {
        // Only a currently observed capacity-permit wait authorizes spill.
        // Upload, workspace ownership and a missing heartbeat are not proof.
        !self.may_have_started
            && self
                .permits
                .is_some_and(|_| self.wait_ms() >= self.after.saturating_mul(1000))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn spill_is_opt_in_and_excludes_gate_net_windows_and_oversize() {
        let mut config = crate::Config::default();
        let mut request = RunRequest::new(
            "/home/test/repo",
            "/home/test/repo",
            "/home/test/repo",
            "true",
        )
        .with_timeout(30);
        assert!(!eligible(&config, &request, false, false));
        config.spill_to_hosted = true;
        assert!(eligible(&config, &request, false, false));
        assert!(!eligible(&config, &request, true, false));
        assert!(eligible(&config, &request, true, true));
        request.network = Some(motor_protocol::Network::Outbound);
        assert!(!eligible(&config, &request, false, false));
        request.network = None;
        request.platform = Some(motor_protocol::Platform::Windows);
        assert!(!eligible(&config, &request, false, false));
        request.platform = None;
        request.weight_hint = Some(17);
        assert!(!eligible(&config, &request, false, false));
        config.spill_max_vcpus = 8;
        request.weight_hint = Some(9);
        assert!(!eligible(&config, &request, false, false));
    }
    #[tokio::test(start_paused = true)]
    async fn permits_only_spill_fence_rejects_busy_workspace_and_unobserved_wait() {
        let terminal = TerminalRecord::new(
            motor_protocol::Uuid::now_v7(),
            Outcome::RefusedBeforeStart {
                reason: RefusalReason::QueueWaitExceeded,
            },
            0,
            0,
            0,
        )
        .with_ran(Ran::None);
        let mut watch = Watch::new(45);
        tokio::time::advance(Duration::from_secs(90)).await;
        assert!(
            !watch.withdrawn(&terminal),
            "unobserved wait must not spill"
        );
        watch.observe(Some(45_000));
        assert!(watch.withdrawn(&terminal));
        watch.observe(None);
        assert!(!watch.withdrawn(&terminal), "busy workspace must not spill");
    }
    #[tokio::test(start_paused = true)]
    async fn spill_request_survives_a_heartbeat_without_queue_wait_before_the_terminal() {
        let terminal = |detail: &str| {
            TerminalRecord::new(
                motor_protocol::Uuid::now_v7(),
                Outcome::RefusedBeforeStart {
                    reason: RefusalReason::QueueWaitExceeded,
                },
                0,
                0,
                0,
            )
            .with_ran(Ran::None)
            .with_refusal_detail(detail)
        };
        let spilled = terminal(&format!(
            "{SPILL_WITHDRAWAL_DETAIL_PREFIX}waiting_on=permits; requested_permits=4"
        ));
        let refused = terminal("waiting_on=permits; requested_permits=4");
        let mut watch = Watch::new(45);
        watch.observe(Some(45_000));
        assert!(watch.ready());
        watch.withdraw_sent(true);
        // The runner has withdrawn the job, so its next heartbeat carries no
        // queue wait, and it can arrive before the terminal record.
        watch.observe(None);
        assert!(
            watch.withdrawn(&spilled),
            "the requested spill must not become a refusal"
        );
        assert_eq!(watch.spill_wait_ms(), 45_000);
        assert!(
            !watch.withdrawn(&refused),
            "only a terminal the runner marks as a spill withdrawal counts"
        );
        let mut unrequested = Watch::new(45);
        unrequested.observe(Some(45_000));
        unrequested.withdraw_sent(false);
        unrequested.observe(None);
        assert!(!unrequested.withdrawn(&spilled));
        let mut started = Watch::new(45);
        started.observe(Some(45_000));
        started.withdraw_sent(true);
        started.started();
        assert!(!started.withdrawn(&spilled));
    }
    #[tokio::test(start_paused = true)]
    async fn started_or_disconnected_job_cannot_spill() {
        let terminal = TerminalRecord::new(
            motor_protocol::Uuid::now_v7(),
            Outcome::RefusedBeforeStart {
                reason: RefusalReason::QueueWaitExceeded,
            },
            0,
            0,
            0,
        )
        .with_ran(Ran::None);
        let mut watch = Watch::new(45);
        watch.observe(Some(45_000));
        watch.started();
        watch.observe(Some(45_000));
        assert!(!watch.withdrawn(&terminal));
        let mut watch = Watch::new(45);
        watch.observe(Some(45_000));
        watch.disconnected();
        assert!(!watch.withdrawn(&terminal));
    }
}
