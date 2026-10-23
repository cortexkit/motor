//! The circuit breaker in front of one gateway endpoint, shared by every job
//! on the host.
//!
//! Counted failures are transport errors, timeouts, upstream 5xx and (read
//! endpoint only) a GET whose body arrives below the throughput floor. After
//! [`FAILURES_TO_OPEN`] consecutive counted failures the endpoint answers at
//! once for [`OPEN_WINDOW`], then admits the next request, of any method, as a
//! probe. A probe that succeeds closes the breaker; one that fails reopens it
//! for another window at once. While the probe is in flight the read endpoint
//! answers other requests at once, and the write endpoint holds them until the
//! probe resolves, so a fill's concurrent PUTs are not refused against a store
//! that has recovered.
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tokio::time::{Duration, Instant};

pub(crate) const FAILURES_TO_OPEN: u32 = 5;
pub(crate) const OPEN_WINDOW: Duration = Duration::from_secs(60);

/// What other requests get while the probe is in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WhileProbing {
    /// Answer at once with the endpoint's failure answer.
    Answer,
    /// Hold the request until the probe resolves.
    Hold,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Closed { consecutive: u32 },
    Open { until: Instant },
    Probing,
}

/// How one request ended, as far as the breaker is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    /// The store answered: any 2xx, 3xx or 4xx, or a complete transfer.
    Success,
    /// A transport error, a timeout, an upstream 5xx or a slow read.
    Failure,
    /// Nothing was learned about the store: the client went away or held
    /// back, or the request never reached the store.
    Neutral,
}

#[derive(Debug)]
pub(super) struct Breaker {
    state: Mutex<State>,
    while_probing: WhileProbing,
    /// Bumped on every state change, so held requests wake to re-check.
    changes: watch::Sender<u64>,
}

/// The breaker's answer to one request.
pub(super) enum Admission {
    /// Forward the request; report its outcome on the ticket.
    Admitted(Ticket),
    /// Give the endpoint's immediate failure answer.
    Refused,
}

impl Breaker {
    pub(super) fn new(while_probing: WhileProbing) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::Closed { consecutive: 0 }),
            while_probing,
            changes: watch::channel(0).0,
        })
    }

    fn set(&self, state: &mut State, next: State) {
        *state = next;
        self.changes.send_modify(|n| *n = n.wrapping_add(1));
    }

    /// Admits a request, waiting no later than `deadline` when it must be
    /// held behind a probe.
    pub(super) async fn admit(self: &Arc<Self>, deadline: Instant) -> Admission {
        let mut changes = self.changes.subscribe();
        loop {
            {
                let mut state = self.state.lock().unwrap();
                match *state {
                    State::Closed { .. } => return Admission::Admitted(self.ticket(false)),
                    State::Open { until } if Instant::now() < until => return Admission::Refused,
                    State::Open { .. } => {
                        self.set(&mut state, State::Probing);
                        return Admission::Admitted(self.ticket(true));
                    }
                    State::Probing => {
                        if self.while_probing == WhileProbing::Answer {
                            return Admission::Refused;
                        }
                    }
                }
                changes.borrow_and_update();
            }
            match tokio::time::timeout_at(deadline, changes.changed()).await {
                Ok(Ok(())) => continue,
                _ => return Admission::Refused,
            }
        }
    }

    fn ticket(self: &Arc<Self>, probe: bool) -> Ticket {
        Ticket {
            breaker: self.clone(),
            probe,
            reported: false,
        }
    }

    fn report(&self, probe: bool, outcome: Outcome) {
        let mut state = self.state.lock().unwrap();
        let now = Instant::now();
        if probe {
            let next = match outcome {
                Outcome::Success => State::Closed { consecutive: 0 },
                Outcome::Failure => State::Open {
                    until: now + OPEN_WINDOW,
                },
                // The probe learned nothing, so the next request probes again
                // at once.
                Outcome::Neutral => State::Open { until: now },
            };
            self.set(&mut state, next);
            return;
        }
        // Requests admitted before the breaker opened may finish later; only
        // the probe changes an open breaker.
        if let State::Closed { consecutive } = *state {
            match outcome {
                Outcome::Success if consecutive != 0 => {
                    self.set(&mut state, State::Closed { consecutive: 0 })
                }
                Outcome::Failure if consecutive + 1 >= FAILURES_TO_OPEN => self.set(
                    &mut state,
                    State::Open {
                        until: now + OPEN_WINDOW,
                    },
                ),
                Outcome::Failure => self.set(
                    &mut state,
                    State::Closed {
                        consecutive: consecutive + 1,
                    },
                ),
                _ => {}
            }
        }
    }

    /// When the breaker is open, the end of its open window. A breaker past
    /// its window, or with a probe in flight, reports none.
    pub(crate) fn open_until(&self) -> Option<Instant> {
        match *self.state.lock().unwrap() {
            State::Open { until } if Instant::now() < until => Some(until),
            _ => None,
        }
    }
}

/// One admitted request's obligation to report how it ended. Dropping it
/// unreported counts as [`Outcome::Neutral`], so a probe always resolves.
#[derive(Debug)]
pub(super) struct Ticket {
    breaker: Arc<Breaker>,
    probe: bool,
    reported: bool,
}

impl Ticket {
    /// Reports the request's outcome; only the first report counts.
    pub(super) fn report(&mut self, outcome: Outcome) {
        if !self.reported {
            self.reported = true;
            self.breaker.report(self.probe, outcome);
        }
    }
    pub(super) fn reported(&self) -> bool {
        self.reported
    }
    /// This request is the breaker's probe.
    pub(super) fn is_probe(&self) -> bool {
        self.probe
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.report(Outcome::Neutral);
    }
}
