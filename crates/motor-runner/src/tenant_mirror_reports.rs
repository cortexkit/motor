use super::*;

pub(super) const REPORT_LIMIT: usize = 64;
#[cfg(any(target_os = "linux", test))]
const DRAIN_MS: u64 = 10_000;

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct MirrorJournal {
    queue: VecDeque<MirrorRefReport>,
    dropped: u64,
}

impl MirrorJournal {
    pub(super) fn open(path: &Path) -> io::Result<Self> {
        let loaded = (|| {
            let file = std::fs::File::open(path)?;
            if file.metadata()?.len() > 4 * 1024 * 1024 {
                return Err(io::Error::other("oversized mirror report journal"));
            }
            serde_json::from_reader::<_, Self>(file).map_err(io::Error::other)
        })();
        let mut journal = match loaded {
            Ok(journal) => journal,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(_) => {
                let quarantine =
                    path.with_file_name(format!("mirror-reports.invalid-{}.json", Uuid::now_v7()));
                std::fs::rename(path, &quarantine)?;
                tracing::warn!(path = %quarantine.display(), "set aside malformed mirror report journal; host must clear have record");
                let journal = Self {
                    dropped: 1,
                    ..Self::default()
                };
                atomic_json_private(path, &journal)?;
                return Ok(journal);
            }
        };
        while journal.queue.len() > REPORT_LIMIT {
            journal.queue.pop_front();
            journal.dropped = journal.dropped.saturating_add(1);
        }
        Ok(journal)
    }
}

impl EventLog {
    pub(crate) fn enable_reports(&self, per_job: bool, opted_in: bool) -> io::Result<()> {
        // Old tenant images cannot decode the new event kind on rollback.
        // Per-job boots discard tenant-events.json; the separate mirror journal
        // survives to replay any reports a bounded shutdown could not drain.
        if self.per_job && per_job && opted_in {
            self.reports
                .store(true, std::sync::atomic::Ordering::Release);
            self.publish_reports()?;
        }
        Ok(())
    }

    pub(crate) fn reports_enabled(&self) -> bool {
        self.per_job
    }

    pub(crate) fn mirror_refs(
        &self,
        imported: bool,
        _mirror: &Path,
        refs: Vec<MirrorRef>,
    ) -> io::Result<()> {
        if refs.is_empty() || !self.per_job {
            return Ok(());
        }
        let mut guard = self.mirror_journal.lock().unwrap();
        let mut journal = guard.clone();
        let report = MirrorRefReport {
            imported,
            refs,
            dropped_reports: 0,
        };
        if !journal.queue.contains(&report) {
            if journal.queue.len() == REPORT_LIMIT {
                journal.queue.pop_front();
                journal.dropped = journal.dropped.saturating_add(1);
            }
            journal.queue.push_back(report);
        }
        atomic_json_private(&self.mirror_path, &journal)?;
        *guard = journal;
        drop(guard);
        self.publish_reports()
    }

    pub(super) fn publish_reports(&self) -> io::Result<()> {
        if !self.per_job || !self.reports.load(std::sync::atomic::Ordering::Acquire) {
            return Ok(());
        }
        let mut guard = self.mirror_journal.lock().unwrap();
        let mut journal = guard.clone();
        if journal.dropped != 0 {
            if journal.queue.is_empty() {
                journal.queue.push_back(MirrorRefReport {
                    imported: false,
                    refs: vec![],
                    dropped_reports: 0,
                });
            }
            let dropped = std::mem::take(&mut journal.dropped);
            journal.queue.front_mut().unwrap().dropped_reports = journal
                .queue
                .front()
                .unwrap()
                .dropped_reports
                .saturating_add(dropped);
            atomic_json_private(&self.mirror_path, &journal)?;
            *guard = journal;
        }
        // Keep journal -> event-state lock order consistent with publication.
        self.update(|s| {
            for report in &guard.queue {
                if s.pending
                    .iter()
                    .any(|e| e.mirror_refs.as_ref() == Some(report))
                {
                    continue;
                }
                s.push(TenantEventKind::MirrorRefs, None, None);
                s.pending.back_mut().unwrap().mirror_refs = Some(report.clone());
            }
        })
    }

    pub(super) fn ack_report(&self, report: &MirrorRefReport) -> io::Result<()> {
        let mut guard = self.mirror_journal.lock().unwrap();
        let mut next = guard.clone();
        if let Some(index) = next.queue.iter().position(|r| r == report) {
            next.queue.remove(index);
            // The caller has already synced the event acknowledgment. Removing
            // the retained report first could lose it on a crash between saves.
            atomic_json_private(&self.mirror_path, &next)?;
            *guard = next;
        }
        Ok(())
    }

    pub(crate) fn retired_names(&self, mirror: &Path, names: &[String]) -> io::Result<()> {
        let (repository, dependency) = crate::workspace::mirror_identity(mirror);
        let refs = names
            .iter()
            .map(|name| MirrorRef {
                repository: repository.clone(),
                dependency: dependency.clone(),
                reference: name.clone(),
                commit: String::new(),
            })
            .collect();
        self.mirror_refs(false, mirror, refs)
    }

    pub(crate) fn terminal_stored(&self, id: Uuid, now: u64) {
        self.terminal_stored
            .lock()
            .unwrap()
            .get_or_insert((id, now));
    }
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn is_terminal_stored(&self, id: Uuid) -> bool {
        self.terminal_stored
            .lock()
            .unwrap()
            .is_some_and(|(stored, _)| stored == id)
    }
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn reports_drain_ready(&self, now: u64) -> bool {
        if !self.reports.load(std::sync::atomic::Ordering::Acquire) {
            return true;
        }
        let started = self
            .terminal_stored
            .lock()
            .unwrap()
            .map(|(_, started)| started);
        self.mirror_journal.lock().unwrap().queue.is_empty()
            || started.is_some_and(|started| now.saturating_sub(started) >= DRAIN_MS)
    }
}
