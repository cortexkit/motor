use motor_protocol::{Outcome, Ran, RefusalReason, RunRequest, TerminalRecord};
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
}
impl Watch {
    pub(crate) fn new(after: u64) -> Self {
        Self {
            after,
            permits: None,
            may_have_started: false,
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
    pub(crate) fn withdrawn(&self, terminal: &TerminalRecord) -> bool {
        !self.may_have_started
            && terminal.ran == Some(Ran::None)
            && terminal.outcome == Outcome::RefusedBeforeStart { reason: RefusalReason::QueueWaitExceeded }
            // Only a currently observed capacity-permit wait authorizes spill.
            // Upload, workspace ownership and a missing heartbeat are not proof.
            && self.permits.is_some_and(|_| self.wait_ms() >= self.after.saturating_mul(1000))
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
