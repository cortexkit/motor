//! Guest-only phase records; package output and registry URLs never enter the log.
use super::Refusal;
use std::time::Instant;

pub struct PreparationTimings<F> {
    emit: F,
    started: Instant,
    deadline: Instant,
}

impl<F: FnMut(&str)> PreparationTimings<F> {
    pub fn new(emit: F, started: Instant, deadline: Instant) -> Self {
        Self {
            emit,
            started,
            deadline,
        }
    }

    pub fn measure<T>(
        &mut self,
        phase: &str,
        project: &str,
        run: impl FnOnce() -> Result<T, Refusal>,
    ) -> Result<T, Refusal> {
        let start = Instant::now();
        self.record(phase, project, start, "started", None);
        let result = run();
        self.record(
            phase,
            project,
            start,
            if result.is_ok() { "ok" } else { "failed" },
            result.as_ref().err(),
        );
        result
    }

    pub fn skipped(&mut self, phase: &str, project: &str, refusal: &Refusal) {
        self.record(phase, project, Instant::now(), "skipped", Some(refusal));
    }

    fn record(
        &mut self,
        phase: &str,
        project: &str,
        start: Instant,
        outcome: &str,
        refusal: Option<&Refusal>,
    ) {
        let now = Instant::now();
        let line = serde_json::json!({
            "event": "bun_preparation",
            "phase": phase,
            "project": project,
            "outcome": outcome,
            "phase_ms": now.duration_since(start).as_millis(),
            "total_ms": now.duration_since(self.started).as_millis(),
            "budget_ms": self.deadline.duration_since(self.started).as_millis(),
            "remaining_ms": self.deadline.saturating_duration_since(now).as_millis(),
            "refusal": refusal.map(|r| r.kind),
            "check": refusal.and_then(|r| r.check),
        })
        .to_string();
        (self.emit)(&line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bun::{Check, RefusalKind, refuse};
    use std::time::Duration;

    #[test]
    fn phase_records_include_failures_skips_and_the_shared_budget_without_package_output() {
        let mut lines = Vec::new();
        let start = Instant::now();
        let mut timings = PreparationTimings::new(
            |line: &str| lines.push(serde_json::from_str::<serde_json::Value>(line).unwrap()),
            start,
            start + Duration::from_secs(300),
        );
        assert_eq!(timings.measure("cache_expansion", "", || Ok(7)), Ok(7));
        let failure = refuse(RefusalKind::BunOfflineInstallFailed, "m/\"project", "", "")
            .at(Check::InstallDeadline)
            .detail("private package output");
        assert!(
            timings
                .measure::<()>("project_install", &failure.project, || Err(failure.clone()))
                .is_err()
        );
        timings.skipped("project_install", &failure.project, &failure);
        assert_eq!(lines.len(), 5);
        for (line, (phase, outcome)) in lines.iter().zip([
            ("cache_expansion", "started"),
            ("cache_expansion", "ok"),
            ("project_install", "started"),
            ("project_install", "failed"),
            ("project_install", "skipped"),
        ]) {
            assert_eq!(line["event"], "bun_preparation");
            assert_eq!(line["phase"], phase);
            assert_eq!(line["outcome"], outcome);
            assert_eq!(line["budget_ms"], 300_000);
            assert!(line["phase_ms"].as_u64().is_some());
            assert!(line["remaining_ms"].as_u64().unwrap() <= 300_000);
            assert!(!line.to_string().contains("private package output"));
        }
        assert_eq!(lines[3]["project"], "m/\"project");
        assert_eq!(lines[3]["check"], "InstallDeadline");
        assert_eq!(lines[0]["check"], serde_json::Value::Null);
    }
}
