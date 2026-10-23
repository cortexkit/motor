//! Per-tool transport counts the gateway reports in health. They describe the
//! gateway's own traffic only; compiler-cache reuse comes from each tool's
//! statistics.
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use tokio::time::{Duration, Instant};

/// Latencies are kept per minute for the trailing hour.
const BUCKET: Duration = Duration::from_secs(60);
const BUCKETS: u64 = 60;
/// Upper bounds, in milliseconds, of the latency histogram's bins; anything
/// slower falls in a final open bin reported as the last bound.
const BOUNDS_MS: [u64; 24] = [
    1, 2, 3, 5, 8, 12, 20, 30, 50, 80, 120, 200, 300, 500, 800, 1_200, 2_000, 3_000, 5_000, 8_000,
    12_000, 20_000, 60_000, 120_000,
];

/// A snapshot of one tool's counts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ToolStats {
    /// GETs answered 200 or 206 by the store, excluding probes.
    pub(crate) hits: u64,
    /// Reads of sccache's `.sccache_check` startup key the store answered with
    /// a status below 500, whatever that status.
    pub(crate) probes: u64,
    /// Reads the store answered 404, excluding probes.
    pub(crate) misses: u64,
    /// Answers given because the endpoint's breaker was open.
    pub(crate) breaker_open: u64,
    /// Failures counted by a breaker.
    pub(crate) errors: u64,
    /// Requests refused on the read endpoint.
    pub(crate) read_refusals: u64,
    /// Requests refused on the write endpoint.
    pub(crate) write_refusals: u64,
    /// Upstream responses aborted because the object exceeded the size limit.
    pub(crate) oversize: u64,
    /// The 95th percentile latency, over the trailing 60 minutes, of requests
    /// that reached the store (refusals and breaker answers are excluded), as
    /// the upper bound of its histogram bin; `None` with no such requests.
    pub(crate) p95_ms: Option<u64>,
}

#[derive(Default)]
pub(super) struct ToolCounters {
    pub(super) hits: AtomicU64,
    pub(super) probes: AtomicU64,
    pub(super) misses: AtomicU64,
    pub(super) breaker_open: AtomicU64,
    pub(super) errors: AtomicU64,
    pub(super) read_refusals: AtomicU64,
    pub(super) write_refusals: AtomicU64,
    pub(super) oversize: AtomicU64,
    latency: Mutex<VecDeque<(u64, [u64; BOUNDS_MS.len() + 1])>>,
}

pub(super) fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

impl ToolCounters {
    pub(super) fn record_latency(&self, origin: Instant, elapsed: Duration) {
        let minute = origin.elapsed().as_secs() / BUCKET.as_secs();
        let millis = elapsed.as_millis() as u64;
        let bin = BOUNDS_MS
            .iter()
            .position(|bound| millis <= *bound)
            .unwrap_or(BOUNDS_MS.len());
        let mut latency = self.latency.lock().unwrap();
        while latency
            .front()
            .is_some_and(|(at, _)| at + BUCKETS <= minute)
        {
            latency.pop_front();
        }
        match latency.back_mut() {
            Some((at, bins)) if *at == minute => bins[bin] += 1,
            _ => {
                let mut bins = [0; BOUNDS_MS.len() + 1];
                bins[bin] = 1;
                latency.push_back((minute, bins));
            }
        }
    }

    pub(super) fn snapshot(&self, origin: Instant) -> ToolStats {
        let minute = origin.elapsed().as_secs() / BUCKET.as_secs();
        let mut totals = [0u64; BOUNDS_MS.len() + 1];
        for (at, bins) in self.latency.lock().unwrap().iter() {
            if at + BUCKETS > minute {
                for (total, count) in totals.iter_mut().zip(bins) {
                    *total += count;
                }
            }
        }
        let count: u64 = totals.iter().sum();
        let p95_ms = (count > 0).then(|| {
            let rank = count.saturating_mul(95).div_ceil(100);
            let mut seen = 0;
            let mut bin = BOUNDS_MS.len();
            for (index, n) in totals.iter().enumerate() {
                seen += n;
                if seen >= rank {
                    bin = index;
                    break;
                }
            }
            BOUNDS_MS[bin.min(BOUNDS_MS.len() - 1)]
        });
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        ToolStats {
            hits: load(&self.hits),
            probes: load(&self.probes),
            misses: load(&self.misses),
            breaker_open: load(&self.breaker_open),
            errors: load(&self.errors),
            read_refusals: load(&self.read_refusals),
            write_refusals: load(&self.write_refusals),
            oversize: load(&self.oversize),
            p95_ms,
        }
    }
}
