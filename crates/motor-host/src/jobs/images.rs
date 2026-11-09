//! The host records image assignments and space reservations before this adapter
//! asks the helper to publish images or prepare account copies. Helper calls run
//! outside the Host mutex, so claims never wait for those copies.
use super::{Assignment, Resources};
use crate::{
    helper::Control,
    state::{
        jobs::{recovery::Recovery, Execution, WorkspaceDisk},
        Host,
    },
};
use anyhow::{Context, Result};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

pub struct Images {
    host: Arc<Mutex<Host>>,
    control: Arc<dyn Control>,
    operations: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
}

impl Images {
    pub fn new(host: Arc<Mutex<Host>>, control: Arc<dyn Control>) -> Self {
        Self {
            host,
            control,
            operations: Mutex::new(BTreeMap::new()),
        }
    }

    fn operation(&self, id: &str) -> Arc<Mutex<()>> {
        self.operations
            .lock()
            .expect("image operations")
            .entry(id.into())
            .or_default()
            .clone()
    }

    fn assignment(&self, id: &str, job: &str) -> Result<Assignment> {
        let host = self.host.lock().expect("host lock");
        let generation = host
            .snapshot
            .per_job
            .images
            .get(id)
            .context("assigned image missing")?;
        anyhow::ensure!(
            !generation.pending && !generation.deleting,
            "assigned image unavailable"
        );
        let root = host
            .journal_directory()
            .context("durable image manager requires a journal directory")?;
        Ok(Assignment {
            image_generation_id: id.into(),
            hello: generation.manifest.hello.clone(),
            guest_init: generation.manifest.guest_init.clone(),
            spool: root.join("job-spool").join(job),
        })
    }

    fn prepare_account(&self, id: &str, account: &str) -> Result<()> {
        let operation = self.operation(id);
        let _operation = operation.lock().expect("image operation");
        let ready = {
            let host = self.host.lock().expect("host lock");
            *host
                .snapshot
                .per_job
                .images
                .get(id)
                .context("assigned image missing")?
                .accounts
                .get(account)
                .context("account copy not reserved")?
        };
        if !ready {
            self.control.prepare_job_images(id, account)?;
            self.host
                .lock()
                .expect("host lock")
                .image_account_ready(id, account)?;
        }
        Ok(())
    }

    pub fn publish(&self, id: &str, now_ms: u64) -> Result<()> {
        let operation = self.operation(id);
        let _operation = operation.lock().expect("image operation");
        let manifest = self.control.inspect_job_images(id)?;
        self.host
            .lock()
            .expect("host lock")
            .begin_image_publication(id, manifest.clone(), now_ms)?;
        let result = (|| -> Result<()> {
            self.control.publish_job_images(id)?;
            self.host
                .lock()
                .expect("host lock")
                .image_artifacts_ready(id)?;
            loop {
                let missing = {
                    let mut host = self.host.lock().expect("host lock");
                    host.begin_image_publication(id, manifest.clone(), now_ms)?;
                    host.snapshot.per_job.images[id]
                        .accounts
                        .iter()
                        .filter(|(_, ready)| !**ready)
                        .map(|(account, _)| account.clone())
                        .collect::<Vec<_>>()
                };
                for account in missing {
                    self.control.prepare_job_images(id, &account)?;
                    self.host
                        .lock()
                        .expect("host lock")
                        .image_account_ready(id, &account)?;
                }
                let mut host = self.host.lock().expect("host lock");
                if host
                    .snapshot
                    .per_job
                    .disks
                    .values()
                    .filter(|d| !d.deleted)
                    .all(|d| {
                        host.snapshot.per_job.images[id]
                            .accounts
                            .contains_key(&d.sub)
                    })
                {
                    host.finish_image_publication(id)?;
                    break;
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            // A timed-out helper may still be copying. Its image lock refuses
            // removal until that work ends. Whenever removal fails, retain the
            // reservation so claims cannot reuse space still needed by cleanup.
            if self.control.remove_job_images(id).is_ok() {
                self.host
                    .lock()
                    .expect("host lock")
                    .abort_image_publication(id)?;
            }
            return Err(error);
        }
        drop(_operation);
        self.gc()?;
        Ok(())
    }

    pub fn gc(&self) -> Result<()> {
        let candidates = self
            .host
            .lock()
            .expect("host lock")
            .snapshot
            .per_job
            .images
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for id in candidates {
            let operation = self.operation(&id);
            let _operation = operation.lock().expect("image operation");
            if self
                .host
                .lock()
                .expect("host lock")
                .begin_image_gc(&id)
                .is_err()
            {
                continue;
            }
            self.control.remove_job_images(&id)?;
            self.host.lock().expect("host lock").finish_image_gc(&id)?;
        }
        Ok(())
    }
}

impl Resources for Images {
    fn acquire_fill(&self, fill: &crate::state::jobs::fills::Reservation) -> Result<Assignment> {
        let id = fill
            .image_generation
            .as_deref()
            .context("fill has no assigned image")?;
        anyhow::ensure!(
            self.host
                .lock()
                .expect("host lock")
                .snapshot
                .per_job
                .fills
                .get(&fill.fill.fill_id)
                == Some(fill),
            "fill not durably reserved"
        );
        self.prepare_account(id, &fill.fill.account)?;
        self.assignment(id, &fill.fill.fill_id)
    }
    fn acquire(&self, execution: &Execution) -> Result<Assignment> {
        let id = execution
            .image_generation
            .as_deref()
            .context("claim has no image generation")?;
        let assignment = self.assignment(id, &execution.claims.jti)?;
        let host = self.host.lock().expect("host lock");
        anyhow::ensure!(
            host.snapshot.per_job.executions.get(&execution.claims.jti) == Some(execution)
                && !execution.ended,
            "execution not durably reserved"
        );
        Ok(assignment)
    }
    fn prepare(&self, execution: &Execution) -> Result<()> {
        self.prepare_account(
            execution
                .image_generation
                .as_deref()
                .context("claim has no image generation")?,
            &execution.claims.sub,
        )
    }
    fn release(&self, job: &str) -> Result<()> {
        motor_vm_helper::job::validate_job_id(job)?;
        self.host
            .lock()
            .expect("host lock")
            .release_image_holder(job)?;
        let spool: Option<PathBuf> = self
            .host
            .lock()
            .expect("host lock")
            .journal_directory()
            .map(|root| root.join("job-spool").join(job));
        if let Some(spool) = spool {
            match std::fs::remove_file(spool) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        self.gc()
    }
    fn acquire_recovery(
        &self,
        _disk_id: &str,
        disk: &WorkspaceDisk,
        recovery: &Recovery,
    ) -> Result<Assignment> {
        let id = self
            .host
            .lock()
            .expect("host lock")
            .reserve_recovery_image(&recovery.id, &disk.sub, crate::server::unix_ms())?;
        self.prepare_account(&id, &disk.sub)?;
        self.assignment(&id, &recovery.id)
    }
}
