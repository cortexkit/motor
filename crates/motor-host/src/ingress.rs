//! Bound work before requester authentication, including TLS handshakes.
use crate::server::CloseReport;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub const UNAUTHENTICATED_LIMIT: usize = 64;

pub struct Ingress {
    slots: Arc<Semaphore>,
    closed: AtomicU64,
    capped: AtomicU64,
}

impl Default for Ingress {
    fn default() -> Self {
        Self {
            slots: Arc::new(Semaphore::new(UNAUTHENTICATED_LIMIT)),
            closed: AtomicU64::new(0),
            capped: AtomicU64::new(0),
        }
    }
}

impl Ingress {
    /// Never queue a socket waiting for a slot: queued scanners would retain FDs.
    pub fn admit(&self, peer: SocketAddr) -> Option<OwnedSemaphorePermit> {
        match self.slots.clone().try_acquire_owned() {
            Ok(slot) => Some(slot),
            Err(_) => {
                self.capped.fetch_add(1, Ordering::Relaxed);
                tracing::info!(%peer, "unauthenticated_connection_capped");
                None
            }
        }
    }

    pub fn closed(&self, peer: SocketAddr, report: &CloseReport) {
        if report.jti.is_none() && report.side == "client" && report.step == "lease_hello" {
            self.closed.fetch_add(1, Ordering::Relaxed);
            tracing::info!(%peer, side = report.side, step = report.step, cause = %report.cause, "connection_closed");
        } else {
            tracing::warn!(%peer, jti = report.jti, side = report.side, step = report.step, cause = %report.cause, "connection_closed");
        }
    }

    pub fn tls_closed(&self, peer: SocketAddr, cause: &'static str) {
        self.closed.fetch_add(1, Ordering::Relaxed);
        tracing::info!(%peer, side = "client", step = "tls", cause, "connection_closed");
    }

    pub fn summarize(&self) -> (u64, u64) {
        let closed = self.closed.swap(0, Ordering::Relaxed);
        let capped = self.capped.swap(0, Ordering::Relaxed);
        if closed != 0 || capped != 0 {
            tracing::info!(
                closed,
                capped,
                window_s = 60,
                "unauthenticated_connection_summary"
            );
        }
        (closed, capped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct Logs(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Logs {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn scanner_closes_are_info_with_peer_and_counted_once_but_job_failures_warn() {
        let logs = Logs::default();
        let subscriber = motor_protocol::logging::LineSubscriber::new("[host]", logs.clone());
        tracing::subscriber::with_default(subscriber, || {
            let ingress = Ingress::default();
            let peer = "192.0.2.1:1234".parse().unwrap();
            let mut report = CloseReport {
                side: "client",
                step: "lease_hello",
                jti: None,
                cause: "invalid frame length".into(),
            };
            ingress.closed(peer, &report);
            assert_eq!(ingress.summarize(), (1, 0));
            assert_eq!(ingress.summarize(), (0, 0));
            report.jti = Some("job".into());
            report.side = "guest";
            report.step = "relay";
            ingress.closed(peer, &report);
            assert_eq!(ingress.summarize(), (0, 0));
        });
        let output = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<_> = output.lines().collect();
        assert_eq!(lines.len(), 3, "{output}");
        assert!(
            lines[0].contains("INFO") && lines[0].contains("192.0.2.1:1234"),
            "{output}"
        );
        assert!(
            lines[1].contains("unauthenticated_connection_summary")
                && lines[1].contains("closed=1"),
            "{output}"
        );
        assert!(
            lines[2].contains("WARN") && lines[2].contains("jti=job"),
            "{output}"
        );
    }

    #[test]
    fn unauthenticated_sockets_are_capped_without_queueing_and_slots_are_reused() {
        // This test logs the same summary statement as the one above. While
        // exactly one subscriber is registered, tracing decides a statement's
        // interest from the first thread that reaches it; reaching it here
        // with no subscriber could disable it for the test above. A
        // discarding subscriber keeps the statement enabled.
        let sink = motor_protocol::logging::LineSubscriber::new("[host]", std::io::sink());
        let _subscriber = tracing::subscriber::set_default(sink);
        let ingress = Ingress::default();
        let peer = "192.0.2.1:1234".parse().unwrap();
        let mut slots: Vec<_> = (0..UNAUTHENTICATED_LIMIT)
            .map(|_| ingress.admit(peer).unwrap())
            .collect();
        assert!(ingress.admit(peer).is_none());
        assert_eq!(ingress.summarize(), (0, 1));
        assert_eq!(ingress.summarize(), (0, 0));
        slots.pop();
        assert!(ingress.admit(peer).is_some());
    }
}
