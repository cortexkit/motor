//! Host-side fill lifecycle. The guest's trusted-source operation is not yet
//! available; production disables scheduling rather than booting an empty fill.
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
            if let Some(publication) = publication {
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
