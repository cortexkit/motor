//! Host-side fill lifecycle, isolated from billable job executions.
use super::{Assignment, Resources};
use crate::{
    helper::Control,
    state::{jobs::fills::Reservation, Host},
};
use anyhow::Result;
use std::sync::{
    mpsc::{self, Receiver},
    Arc, Mutex,
};

#[derive(Clone, Debug)]
pub struct Candidate {
    pub account: String,
    pub repository_key: String,
    pub mem_mib: u64,
    pub last_active_ms: u64,
}

pub struct Publication {
    pub have: Vec<motor_protocol::MirrorRef>,
    pub base_published: bool,
    pub cache_write_keys: Option<u64>,
}

/// The missing runner operation must obtain trusted repository objects without
/// mounting a job-written disk, run today's motor-cache fill, report verified
/// mirror pins and successful publication, then cleanly power off its VM.
pub trait FillGuest: Send + Sync {
    fn candidates(&self) -> Result<Vec<Candidate>>;
    fn initialize(
        &self,
        fill: &Reservation,
        assignment: &Assignment,
        control: &dyn Control,
        start_reply: &serde_json::Value,
    ) -> Result<()>;
    fn run(&self, fill: &Reservation, control: &dyn Control) -> Result<Publication>;
}

#[derive(Debug)]
pub struct GuestUnavailable;
impl std::fmt::Display for GuestUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("fill guest operation unavailable")
    }
}
impl std::error::Error for GuestUnavailable {}

pub struct UnavailableFillGuest;
impl FillGuest for UnavailableFillGuest {
    fn candidates(&self) -> Result<Vec<Candidate>> {
        Err(GuestUnavailable.into())
    }
    fn initialize(
        &self,
        _: &Reservation,
        _: &Assignment,
        _: &dyn Control,
        _: &serde_json::Value,
    ) -> Result<()> {
        Err(GuestUnavailable.into())
    }
    fn run(&self, _: &Reservation, _: &dyn Control) -> Result<Publication> {
        Err(GuestUnavailable.into())
    }
}

pub struct Scheduler {
    guest: Arc<dyn FillGuest>,
    disabled: bool,
    worker: Option<Receiver<Result<Publication>>>,
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new(Arc::new(UnavailableFillGuest))
    }
}

impl Scheduler {
    pub fn new(guest: Arc<dyn FillGuest>) -> Self {
        Self {
            guest,
            disabled: false,
            worker: None,
        }
    }

    pub fn tick(
        &mut self,
        host: Arc<Mutex<Host>>,
        control: Arc<dyn Control>,
        resources: Arc<dyn Resources>,
        now_ms: u64,
    ) -> Result<()> {
        let active = host
            .lock()
            .expect("host lock")
            .snapshot
            .per_job
            .fills
            .values()
            .next()
            .cloned();
        if let Some(fill) = active {
            let publication = if let Some(worker) = &self.worker {
                match worker.try_recv() {
                    Ok(result) => {
                        self.worker = None;
                        match result {
                            Ok(publication) => Some(publication),
                            Err(error) => {
                                if error.is::<GuestUnavailable>() {
                                    self.disabled = true;
                                    tracing::info!("fill guest operation unavailable; fill scheduling disabled");
                                } else {
                                    tracing::warn!(fill_id = fill.fill.fill_id, %error, "fill_failed");
                                }
                                None
                            }
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => return Ok(()),
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.worker = None;
                        None
                    }
                }
            } else {
                None
            };
            if let Some(publication) = &publication {
                if let Some(samples) = publication.cache_write_keys.filter(|samples| *samples > 0) {
                    tracing::info!(
                        fill_id = fill.fill.fill_id,
                        guest_reported_cache_write_samples = samples,
                        "fill_cache_writes"
                    );
                }
                tracing::info!(
                    fill_id = fill.fill.fill_id,
                    base_published = publication.base_published,
                    "fill_completed"
                );
            }
            if let Some(publication) = publication.filter(|p| p.base_published) {
                host.lock()
                    .expect("host lock")
                    .fill_publication(&fill.fill.fill_id, publication.have)?;
            }
            // A restart before the publication intent loses the guest result,
            // so that disk is discarded, never guessed to be clean.
            let published = host.lock().expect("host lock").snapshot.per_job.fills
                [&fill.fill.fill_id]
                .publication
                .is_some();
            let published = control.finish_fill(&fill.fill, published)?;
            for report in control.job_exits()? {
                if report.job_id == fill.fill.fill_id {
                    control.ack_job_exit(&report)?;
                }
            }
            host.lock()
                .expect("host lock")
                .fill_finished(&fill.fill.fill_id, published, now_ms)?;
            resources.release(&fill.fill.fill_id)?;
            return Ok(());
        }
        if self.disabled {
            return Ok(());
        }
        let mut candidates = match self.guest.candidates() {
            Ok(candidates) => candidates,
            Err(error) if error.is::<GuestUnavailable>() => {
                self.disabled = true;
                tracing::info!("fill guest operation unavailable; fill scheduling disabled");
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        candidates.sort_by(|a, b| {
            b.last_active_ms
                .cmp(&a.last_active_ms)
                .then_with(|| a.repository_key.cmp(&b.repository_key))
                .then_with(|| a.account.cmp(&b.account))
        });
        let Some(candidate) = candidates.first() else {
            return Ok(());
        };
        let Some(fill) = host.lock().expect("host lock").reserve_fill(
            &candidate.account,
            &candidate.repository_key,
            candidate.mem_mib,
            now_ms,
        )?
        else {
            return Ok(());
        };
        let guest = self.guest.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = (|| {
                let assignment = resources.acquire_fill(&fill)?;
                control.prepare_fill(&fill.fill)?;
                let reply = control.start_job(&motor_vm_helper::job::Start {
                    cache_write_blocked: false,
                    kind: motor_vm_helper::job::VmKind::Fill,
                    job_id: fill.fill.fill_id.clone(),
                    account: fill.fill.account.clone(),
                    workspace_disk_id: fill.fill.disk_id(),
                    image_generation_id: assignment.image_generation_id.clone(),
                    vcpus: fill.vcpus,
                    memory_mib: fill.guest_ram_mib,
                    burst_mib: None,
                })?;
                guest.initialize(&fill, &assignment, &*control, &reply)?;
                host.lock()
                    .expect("host lock")
                    .fill_runner_hello(&fill.fill.fill_id)?;
                guest.run(&fill, &*control)
            })();
            let _ = tx.send(result);
        });
        self.worker = Some(rx);
        Ok(())
    }
}

pub struct BundleFillGuest {
    pub host: Arc<Mutex<Host>>,
    /// The runtime's configured job VM resolvers, shared so a fill VM gets
    /// the same guest identity fields as a job VM.
    pub resolvers: Arc<std::sync::RwLock<Vec<String>>>,
}
impl FillGuest for BundleFillGuest {
    fn candidates(&self) -> Result<Vec<Candidate>> {
        let host = self.host.lock().expect("host lock");
        let now = crate::server::unix_ms();
        Ok(host
            .snapshot
            .per_job
            .fill_sources
            .values()
            .filter(|slot| {
                slot.last_fill_started_ms.is_none_or(|at| {
                    now.saturating_sub(at.min(now)) >= super::fill_source::REFILL_INTERVAL_MS
                })
            })
            .map(|slot| Candidate {
                account: slot.request.account.clone(),
                repository_key: super::fill_source::repository_key(&slot.request.repository_root),
                mem_mib: slot.request.mem_mib,
                last_active_ms: slot.request.observed_ms,
            })
            .collect())
    }
    fn initialize(
        &self,
        fill: &Reservation,
        assignment: &Assignment,
        control: &dyn Control,
        start_reply: &serde_json::Value,
    ) -> Result<()> {
        use anyhow::{ensure, Context};
        let tap = start_reply["tap_index"]
            .as_u64()
            .filter(|n| (1..=254).contains(n))
            .context("missing fill network allocation")?;
        let request = super::guest_attach_request(
            &assignment.guest_init,
            &self.resolvers,
            &fill.fill.fill_id,
            tap,
        )?;
        // A fill VM has no boot deadline of its own; 60 s covers a cold guest
        // boot (about a second) many times over.
        let reply = control.job_admin_after_start(
            &fill.fill.fill_id,
            &request,
            std::time::Duration::from_secs(60),
        )?;
        ensure!(
            reply["ok"] == true,
            "fill initialization refused: {}",
            reply["error"]
        );
        Ok(())
    }
    fn run(&self, fill: &Reservation, control: &dyn Control) -> Result<Publication> {
        use anyhow::{ensure, Context};
        let (source, mut files) = {
            let host = self.host.lock().expect("host lock");
            let slot = host
                .snapshot
                .per_job
                .fill_sources
                .values()
                .find(|slot| {
                    slot.request.account == fill.fill.account
                        && super::fill_source::repository_key(&slot.request.repository_root)
                            == fill.fill.repository_key
                })
                .context("fill source evicted")?;
            let directory = super::fill_source::directory(
                &host.journal_directory().context("fill source journal")?,
                &slot.generation,
            );
            // Open every file while holding the slot pointer: a newer export may
            // unlink the old generation, but cannot change these opened bytes.
            let files = slot
                .source
                .bundles
                .iter()
                .map(|bundle| std::fs::File::open(directory.join(bundle.index.to_string())))
                .collect::<std::io::Result<Vec<_>>>()?;
            (slot.source.clone(), files)
        };
        source.validate(motor_protocol::FILL_SOURCE_MAX_BYTES)?;
        let mut socket = control.job_relay(&fill.fill.fill_id, 54)?;
        socket.set_read_timeout(Some(std::time::Duration::from_secs(6 * 60 * 60)))?;
        socket.set_write_timeout(Some(std::time::Duration::from_secs(
            motor_protocol::FILL_SOURCE_EXPORT_TIMEOUT_S,
        )))?;
        crate::helper::write_json(
            &mut socket,
            &serde_json::json!({"op":"fill","source":source}),
        )?;
        super::fill_source::send(&mut socket, &source, &mut files)?;
        let reply = crate::helper::read_json(&mut socket)?;
        ensure!(reply["ok"] == true, "fill refused: {}", reply["error"]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while control.adopt_job(&fill.fill.fill_id)? {
            ensure!(
                std::time::Instant::now() < deadline,
                "fill did not power off"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        Ok(Publication {
            cache_write_keys: reply["cache_write_keys"]
                .as_u64()
                .filter(|samples| *samples > 0),
            have: serde_json::from_value(reply["have"].clone())?,
            base_published: reply["base_published"] == true,
        })
    }
}
