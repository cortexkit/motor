//! What a registered host advertises in its heartbeat, and the two gates that
//! zero parts of it: the host storage refusal latch and a tenant's pending or
//! refused start. The broker places leases only on hosts whose `free_vcpus` and
//! `free_mem_mib` cover the job, so these values decide where work goes, while
//! [`Host::claim`] stays the real admission gate.
use super::{Host, Tenant};
use crate::capacity::Capacity;

/// A guest memory sample older than this is not used (two heartbeat intervals).
pub const MEMORY_FRESH_MS: u64 = 30_000;
/// How often a running tenant's runner is asked for its memory sample.
pub const MEMORY_PROBE_MS: u64 = 15_000;
/// After the helper refuses to start a tenant, the next start attempt waits
/// this long. This bounds VM boots for a failing activation to one a minute.
pub const START_RETRY_MS: u64 = 60_000;
const MIB: u64 = 1 << 20;

/// The storage refusal latch. While it is set, new leases are refused and the
/// heartbeat advertises no free vCPUs. It sets when free bytes fall below the
/// threshold or cannot be measured, and clears only when a successful probe
/// finds the threshold plus `storage_margin_bytes` free.
pub(crate) struct StorageLatch {
    /// Only a registered host started by `main` enforces the latch; tests
    /// and the provider sandbox construct hosts without it.
    enforced: bool,
    latched: bool,
    /// Set once per low period, so `host_storage_low` is logged once (only
    /// an enforcing host logs it).
    low_logged: bool,
}
impl Default for StorageLatch {
    fn default() -> Self {
        // A host starts latched, so a restart cannot skip the hysteresis.
        Self {
            enforced: false,
            latched: true,
            low_logged: false,
        }
    }
}

/// Per-tenant advertisement state, keyed by tenant name. None of it is
/// durable: after a restart the tenant is probed and, if it fails to start
/// again, marked again.
#[derive(Default)]
pub(crate) struct TenantAdvert {
    memory: Option<MemorySample>,
    /// When the tenant was first seen running without a sample.
    ready_seen_ms: Option<u64>,
    /// The case last logged as `tenant_memory_unavailable`, so each case is
    /// logged once when it starts.
    memory_case: Option<MemoryCase>,
    start_refused: Option<StartRefusal>,
    recovery_failure: Option<RecoveryFailure>,
    wake_pending: bool,
    /// The permits the tenant runner's own cache fill held at the last probe.
    fill: Option<FillSample>,
    warm: Option<WarmSample>,
}

struct WarmSample {
    at_ms: u64,
    held: Vec<motor_protocol::HeldWorkspace>,
}

struct FillSample {
    at_ms: u64,
    permits: u32,
}

struct MemorySample {
    at_ms: u64,
    /// The guest's `mem_available_bytes`, `None` when the runner's health
    /// reply did not carry it, or why the probe failed.
    reading: Result<Option<u64>, String>,
}

/// Why a running tenant advertises no free memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryCase {
    MissingField,
    ProbeFailed,
    Stale,
}
impl MemoryCase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MissingField => "missing_mem_available_bytes",
            Self::ProbeFailed => "probe_failed",
            Self::Stale => "stale_sample",
        }
    }
}

struct StartRefusal {
    case: String,
    retry_at_ms: u64,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct RecoveryFailure {
    #[serde(flatten)]
    pub timeout: crate::helper::RecoveryTimeout,
    pub attempts: u32,
    pub stuck: bool,
}

impl Host {
    pub fn recovery_failure(&self, tenant: &str) -> Option<&RecoveryFailure> {
        self.advert.get(tenant)?.recovery_failure.as_ref()
    }
    pub fn recovery_stuck(&self, tenant: &str) -> bool {
        self.recovery_failure(tenant)
            .is_some_and(|failure| failure.stuck)
    }
    pub(crate) fn recovery_timed_out(
        &mut self,
        tenant: &str,
        timeout: crate::helper::RecoveryTimeout,
    ) {
        let advert = self.advert.entry(tenant.to_owned()).or_default();
        let attempts = advert
            .recovery_failure
            .as_ref()
            .map_or(1, |f| f.attempts.saturating_add(1));
        advert.recovery_failure = Some(RecoveryFailure {
            timeout,
            attempts,
            stuck: attempts >= 2,
        });
    }
    pub(crate) fn clear_recovery_timeouts(&mut self, tenant: &str) {
        if let Some(advert) = self.advert.get_mut(tenant) {
            advert.recovery_failure = None;
        }
    }
    /// Turns on the storage refusal for a registered host. The latch is
    /// already set, so nothing is admitted until a probe clears it.
    pub fn enforce_storage(&mut self) {
        self.storage.enforced = true;
    }
    /// Whether this host enforces the storage refusal at all.
    pub fn storage_enforced(&self) -> bool {
        self.storage.enforced
    }
    /// Whether new leases are refused because host storage is low or unknown.
    pub fn storage_refusing(&self) -> bool {
        self.storage.enforced && self.storage.latched
    }
    /// The free bytes below which new leases are refused: the largest
    /// per-tenant term, `host_floor_bytes` plus the suspend reserve (the
    /// guest RAM when the tenant suspends when idle, otherwise 0), plus
    /// `storage_margin_bytes`. The reserve keeps suspend possible at the
    /// threshold. `None` when the sum overflows.
    pub fn storage_threshold(&self, capacity: &Capacity) -> Option<u64> {
        self.tenants
            .values()
            .filter(|t| !self.is_removed(&t.tenant))
            .map(|t| {
                let reserve = if t.idle_suspend_s > 0 {
                    t.guest_ram_mib.checked_mul(MIB)?
                } else {
                    0
                };
                t.host_floor_bytes.checked_add(reserve)
            })
            .try_fold(0u64, |max, term| term.map(|term| max.max(term)))?
            .checked_add(capacity.storage_margin_bytes)
    }
    /// Records one probe of the capacity file and of free bytes on the host
    /// storage filesystem, and moves the latch. A failed probe, an unusable
    /// capacity file or free bytes below the threshold set it; it clears only
    /// at or above the threshold plus `storage_margin_bytes`.
    pub fn observe_storage(
        &mut self,
        capacity: Result<Capacity, String>,
        free_bytes: Result<u64, String>,
    ) {
        self.capacity = capacity.as_ref().ok().cloned();
        let measured = capacity.and_then(|capacity| {
            let free = free_bytes?;
            let threshold = self
                .storage_threshold(&capacity)
                .ok_or("storage threshold overflows")?;
            let release = threshold
                .checked_add(capacity.storage_margin_bytes)
                .ok_or("storage release point overflows")?;
            Ok((free, threshold, release))
        });
        match measured {
            Err(detail) => self.storage_low(&detail),
            Ok((free, threshold, _)) if free < threshold => {
                self.storage_low(&format!("free_bytes={free} threshold_bytes={threshold}"))
            }
            Ok((free, threshold, release)) if self.storage.latched && free >= release => {
                self.storage.latched = false;
                self.storage.low_logged = false;
                if self.storage.enforced {
                    tracing::info!(
                        free_bytes = free,
                        threshold_bytes = threshold,
                        release_bytes = release,
                        "host_storage_recovered"
                    );
                }
            }
            Ok(_) => {}
        }
    }
    fn storage_low(&mut self, detail: &str) {
        self.storage.latched = true;
        if !self.storage.low_logged {
            self.storage.low_logged = true;
            if self.storage.enforced {
                tracing::warn!(detail, "host_storage_low");
            }
        }
    }

    /// Marks a tenant the helper refused to start. Until a wake succeeds its
    /// advertisement is zero and its leases are refused without calling the
    /// helper; the next start attempt waits [`START_RETRY_MS`].
    pub fn start_refused(&mut self, tenant: &str, case: &str, now_ms: u64) {
        self.advert
            .entry(tenant.to_owned())
            .or_default()
            .start_refused = Some(StartRefusal {
            case: case.to_owned(),
            retry_at_ms: now_ms.saturating_add(START_RETRY_MS),
        });
    }
    /// When the next start of a start-refused tenant may be attempted.
    pub fn start_retry_at(&self, tenant: &str) -> Option<u64> {
        self.start_refusal(tenant).map(|r| r.retry_at_ms)
    }
    /// The helper's refusal case for a start-refused tenant.
    pub fn start_refusal_case(&self, tenant: &str) -> Option<&str> {
        self.start_refusal(tenant).map(|r| r.case.as_str())
    }
    fn start_refusal(&self, tenant: &str) -> Option<&StartRefusal> {
        self.advert.get(tenant)?.start_refused.as_ref()
    }
    /// Start-refused tenants whose next start attempt is due.
    pub(crate) fn start_retries_due(&self, now_ms: u64) -> Vec<String> {
        self.advert
            .iter()
            .filter(|(tenant, advert)| {
                !self.is_removed(tenant)
                    && !self.recovery_stuck(tenant)
                    && advert
                        .start_refused
                        .as_ref()
                        .is_some_and(|r| now_ms >= r.retry_at_ms)
            })
            .map(|(tenant, _)| tenant.clone())
            .collect()
    }

    /// Records a probe of a running tenant's guest memory: the runner's
    /// `mem_available_bytes`, `None` when its health reply lacks it, or the
    /// probe's failure.
    pub fn record_memory(
        &mut self,
        tenant: &str,
        now_ms: u64,
        reading: Result<Option<u64>, String>,
    ) {
        self.advert.entry(tenant.to_owned()).or_default().memory = Some(MemorySample {
            at_ms: now_ms,
            reading,
        });
    }
    /// Whether a running tenant needs a new memory sample.
    pub(crate) fn memory_probe_due(&self, tenant: &str, now_ms: u64, force: bool) -> bool {
        // Only a VM with known hashes needs an extra membership probe before
        // a lifecycle heartbeat. Other VMs retain the memory renewal cadence.
        let force = force
            && self.snapshot.leases.values().any(|lease| {
                lease.tenant == tenant
                    && lease.delivered
                    && lease.claims.workspace_key_hash.is_some()
            });
        self.is_ready(tenant)
            && !self.is_suspending(tenant)
            && self.start_refusal(tenant).is_none()
            && (force
                || self
                    .advert
                    .get(tenant)
                    .and_then(|a| a.memory.as_ref())
                    .is_none_or(|s| now_ms.saturating_sub(s.at_ms) >= MEMORY_PROBE_MS))
    }
    /// A sample describes one run of the guest: a wake or stop discards it.
    pub(super) fn reset_memory(&mut self, tenant: &str) {
        if let Some(advert) = self.advert.get_mut(tenant) {
            advert.memory = None;
            advert.ready_seen_ms = None;
            // A fill dies with the guest run it belonged to; a restarted
            // guest reports its own fill, if any, at its first probe.
            advert.fill = None;
            advert.warm = None;
        }
    }
    /// Records the permits a running tenant's own cache fill holds, from the
    /// same runner probe as its memory. The guest is root-controlled, so this
    /// report is tenant input: it only lowers the `free_vcpus` the host
    /// advertises for that tenant, so the broker places new jobs elsewhere.
    /// It never raises the VM's CPU quota and never changes any usage or
    /// billing figure; a fill runs within the CPU the tenant's leases already
    /// hold, or on the idle quota of one vCPU.
    pub fn record_fill(&mut self, tenant: &str, now_ms: u64, permits: u32) {
        self.advert.entry(tenant.to_owned()).or_default().fill = Some(FillSample {
            at_ms: now_ms,
            permits,
        });
    }
    /// The permits of a tenant's own cache fill that lower its advertised
    /// capacity now: the latest probe's, while it is fresh. A probe that stops
    /// answering lets the count lapse after [`MEMORY_FRESH_MS`], so a report
    /// never holds capacity down forever.
    pub fn fill_claim(&self, tenant: &str, now_ms: u64) -> u32 {
        self.advert
            .get(tenant)
            .and_then(|advert| advert.fill.as_ref())
            .filter(|fill| now_ms.saturating_sub(fill.at_ms) <= MEMORY_FRESH_MS)
            .map_or(0, |fill| fill.permits)
    }

    /// Each heartbeat starts with no cache claim. A skipped or failed probe
    /// must not preserve a workspace the guest may have evicted meanwhile.
    pub(crate) fn clear_warm(&mut self) {
        for advert in self.advert.values_mut() {
            advert.warm = None;
        }
    }

    pub fn record_warm(
        &mut self,
        tenant: &str,
        now_ms: u64,
        held: Vec<motor_protocol::HeldWorkspace>,
    ) {
        self.advert.entry(tenant.to_owned()).or_default().warm = Some(WarmSample {
            at_ms: now_ms,
            held,
        });
    }

    pub(crate) fn warm_workspaces(&self, now_ms: u64) -> Vec<motor_lease::WarmVm> {
        let mut entries = Vec::new();
        for (tenant, advert) in &self.advert {
            if !self.is_ready(tenant) || self.is_suspending(tenant) {
                continue;
            }
            let Some(sample) = advert
                .warm
                .as_ref()
                .filter(|s| now_ms.saturating_sub(s.at_ms) < MEMORY_PROBE_MS)
            else {
                continue;
            };
            for held in &sample.held {
                if held.used_ms == 0 || held.used_ms > sample.at_ms {
                    continue;
                }
                let Some(lease) = self.snapshot.leases.get(&held.job_id.to_string()) else {
                    continue;
                };
                // Guest reports are tenant input: only this VM's delivered,
                // authenticated jobs can identify one of its workspaces.
                if lease.tenant != *tenant || !lease.delivered {
                    continue;
                }
                let Some(hash) = lease.claims.workspace_key_hash.as_ref() else {
                    continue;
                };
                if !self
                    .binding(&lease.claims)
                    .is_ok_and(|t| t.tenant == *tenant)
                {
                    continue;
                }
                // A cold placement's token names no VM, so the VM id comes from
                // this host's own binding for the tenant. That is the id the
                // broker echoes back in a warm placement and that admission looks
                // the token's placement.vm up by.
                let Some(vm) = self
                    .tenants
                    .iter()
                    .find(|(_, t)| t.tenant == *tenant && t.sub == lease.claims.sub)
                    .map(|(id, _)| id)
                else {
                    continue;
                };
                entries.push((
                    held.used_ms,
                    motor_lease::WarmVm {
                        tenant: lease.claims.sub.clone(),
                        workspace_key_hash: hash.clone(),
                        vm: vm.clone(),
                    },
                ));
            }
        }
        entries.sort_by(|a, b| {
            b.0.cmp(&a.0).then_with(|| {
                (&a.1.tenant, &a.1.workspace_key_hash, &a.1.vm).cmp(&(
                    &b.1.tenant,
                    &b.1.workspace_key_hash,
                    &b.1.vm,
                ))
            })
        });
        let mut seen = std::collections::BTreeSet::new();
        entries
            .into_iter()
            .filter_map(|(_, warm)| {
                seen.insert((
                    warm.tenant.clone(),
                    warm.workspace_key_hash.clone(),
                    warm.vm.clone(),
                ))
                .then_some(warm)
            })
            .take(64)
            .collect()
    }
    pub(super) fn start_succeeded(&mut self, tenant: &str) {
        if let Some(advert) = self.advert.get_mut(tenant) {
            advert.start_refused = None;
            advert.wake_pending = false;
        }
    }

    /// Cold capacity can attract the first lease, but an in-flight wake must
    /// not attract more work before guest initialization and activation finish.
    pub(crate) fn start_pending(&mut self, tenant: &str) {
        self.advert
            .entry(tenant.to_owned())
            .or_default()
            .wake_pending = true;
    }

    pub(crate) fn start_attempt_failed(&mut self, tenant: &str) {
        if let Some(advert) = self.advert.get_mut(tenant) {
            advert.wake_pending = false;
        }
    }

    pub(crate) fn is_waking(&self, tenant: &str) -> bool {
        self.advert.get(tenant).is_some_and(|a| a.wake_pending)
    }

    fn start_fenced(&self, tenant: &str) -> bool {
        self.advert
            .get(tenant)
            .is_some_and(|a| a.wake_pending || a.start_refused.is_some())
    }

    /// The heartbeat's `free_vcpus` term for one tenant: its permit budget
    /// minus the counted claims and the permits of its own running cache
    /// fill, whether the VM is running, stopped, or suspended; 0 while a wake
    /// is pending or start-refused. Saturating at 0, a fill report of any size
    /// can take away only this tenant's own budget.
    pub(crate) fn tenant_free_vcpus(&self, tenant: &Tenant, now_ms: u64) -> u64 {
        if self.start_fenced(&tenant.tenant) {
            return 0;
        }
        u64::from(
            tenant.budget().saturating_sub(
                self.share(&tenant.tenant, now_ms)
                    .saturating_add(self.fill_claim(&tenant.tenant, now_ms)),
            ),
        )
    }
    /// The heartbeat's `free_mem_mib`: the largest per-tenant value. A
    /// tenant that is not running advertises its guest RAM, so the broker can
    /// place the lease that wakes it; a running tenant advertises its guest's
    /// measured available memory. Both are floored to MiB and reduced by
    /// `tenant_memory_reserve_bytes` rounded up to whole MiB, saturating at
    /// 0. A running tenant without a usable sample, a pending/refused start,
    /// and any tenant while no valid capacity file has been read advertise 0.
    /// Logs each running tenant's unusable-sample case once when it starts.
    pub fn free_mem_mib(&mut self, now_ms: u64) -> u64 {
        let tenants: Vec<Tenant> = self.tenants.values().cloned().collect();
        tenants
            .iter()
            .map(|tenant| self.tenant_free_mem_mib(tenant, now_ms))
            .max()
            .unwrap_or(0)
    }
    fn tenant_free_mem_mib(&mut self, tenant: &Tenant, now_ms: u64) -> u64 {
        let Some(reserve_mib) = self
            .capacity
            .as_ref()
            .map(|c| c.tenant_memory_reserve_bytes.div_ceil(MIB))
        else {
            return 0;
        };
        if self.start_fenced(&tenant.tenant) {
            return 0;
        }
        if !self.is_ready(&tenant.tenant) || self.is_suspending(&tenant.tenant) {
            return tenant.guest_ram_mib.saturating_sub(reserve_mib);
        }
        let advert = self.advert.entry(tenant.tenant.clone()).or_default();
        let (value, case, detail) = match &advert.memory {
            Some(sample) if now_ms.saturating_sub(sample.at_ms) > MEMORY_FRESH_MS => {
                (0, Some(MemoryCase::Stale), String::new())
            }
            Some(MemorySample {
                reading: Ok(Some(bytes)),
                ..
            }) => (
                (bytes / MIB).saturating_sub(reserve_mib),
                None,
                String::new(),
            ),
            Some(MemorySample {
                reading: Ok(None), ..
            }) => (0, Some(MemoryCase::MissingField), String::new()),
            Some(MemorySample {
                reading: Err(error),
                ..
            }) => (0, Some(MemoryCase::ProbeFailed), error.clone()),
            None => {
                // The first probe follows a wake within a heartbeat step;
                // only a running tenant never sampled for the freshness
                // bound counts as stale.
                let seen = *advert.ready_seen_ms.get_or_insert(now_ms);
                let stale = now_ms.saturating_sub(seen) > MEMORY_FRESH_MS;
                (0, stale.then_some(MemoryCase::Stale), String::new())
            }
        };
        if case != advert.memory_case {
            if let Some(case) = case {
                tracing::warn!(
                    tenant = tenant.tenant,
                    case = case.as_str(),
                    detail,
                    "tenant_memory_unavailable"
                );
            }
            advert.memory_case = case;
        }
        value
    }
}
