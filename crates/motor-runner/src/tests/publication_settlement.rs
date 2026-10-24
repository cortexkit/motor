//! Tests while fill settlement runs without the scheduler state lock,
//! including when publication is disabled.
use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::Condvar;

#[derive(Default)]
struct Pause {
    armed: AtomicBool,
    entered: AtomicUsize,
    released: Mutex<bool>,
    wake: Condvar,
}

impl Pause {
    fn block(&self) {
        if self.armed.load(Ordering::SeqCst) {
            self.entered.fetch_add(1, Ordering::SeqCst);
            let mut released = self.released.lock().unwrap();
            while !*released {
                released = self.wake.wait(released).unwrap();
            }
        }
    }

    fn arm(self: &Arc<Self>) -> Release {
        self.armed.store(true, Ordering::SeqCst);
        Release(self.clone())
    }

    fn wait(&self) {
        assert!(poll(Duration::from_secs(5), || self
            .entered
            .load(Ordering::SeqCst)
            > 0));
    }
}

struct Release(Arc<Pause>);
impl Drop for Release {
    fn drop(&mut self) {
        *self.0.released.lock().unwrap() = true;
        self.0.wake.notify_all();
    }
}

fn paused_runner(f: &Fixture, label: &'static str) -> (Runner, Arc<Pause>) {
    let pause = Arc::new(Pause::default());
    let hook = pause.clone();
    let runner = controlled_runner_with(f, |config| {
        config.long_work = Some(Arc::new(move |step| {
            if step == label {
                hook.block();
            }
        }));
    });
    (runner, pause)
}

// Fetch a real declared plan, but stop before dispatch: an early setup failure
// has no child process, and its terminal can precede slow writer cleanup.
fn terminal_attempt(f: &Fixture, runner: &Runner) -> Job {
    let (request, bytes) = f.run("true");
    f.submit(runner, &request, &bytes);
    let mut inner = runner.core.inner.lock().unwrap();
    inner.jobs.retain(|_, job| job.base);
    let mut job = inner.jobs.values().find(|job| job.base).unwrap().clone();
    job.fill_identity = runner.publishing_identity();
    job.terminal_ended(Outcome::OutcomeUnknown, runner.now());
    inner.writer = Some(job.id);
    inner.jobs.insert(job.id, job.clone());
    job
}

fn begin_settlement(runner: &Runner, job: &Job) {
    runner
        .finish_base(
            &mut runner.core.inner.lock().unwrap(),
            job.request.as_ref().unwrap(),
            false,
            None,
        )
        .unwrap();
}

fn wait_settlement(runner: &Runner, id: Uuid) {
    assert!(poll(Duration::from_secs(5), || {
        let inner = runner.core.inner.lock().unwrap();
        inner.writer.is_none() && !inner.base_finishes.contains_key(&id)
    }));
}

#[test]
fn pending_settlement_fences_job_arrival_and_timer_even_with_publication_off() {
    bounded(|| {
        for publishing in [false, true] {
            let f = Fixture::new();
            declare(&f, &controlled_command(&f));
            let (runner, pause) = paused_runner(&f, "base_cleanup");
            if !publishing {
                set_publishing(&runner, State::new(false));
            }
            let job = terminal_attempt(&f, &runner);
            let held = pause.arm();
            begin_settlement(&runner, &job);
            pause.wait();
            let desired = repository(&f, &runner).desired.unwrap();
            runner.queue_next_base(&f.root, &desired).unwrap();
            runner.tick().unwrap();
            let (count, wanted) = {
                let inner = runner.core.inner.lock().unwrap();
                (
                    inner.jobs.values().filter(|job| job.base).count(),
                    Runner::desired_bases(&inner).len(),
                )
            };
            assert_eq!(
                count, 1,
                "a terminal fill queued an unauthorized retry during settlement"
            );
            assert_eq!(wanted, 0, "the timer selected a repository still settling");
            drop(held);
            wait_settlement(&runner, job.id);
            runner.tick().unwrap();
            assert_eq!(
                count_fills(&runner),
                1,
                "the failed descriptor ran again without a change or restart"
            );
        }
    });
}

#[test]
fn pending_settlement_blocks_prequeued_writer_dispatch() {
    bounded(|| {
        let f = Fixture::new();
        declare(&f, &controlled_command(&f));
        let (runner, pause) = paused_runner(&f, "catalog_save");
        set_publishing(&runner, State::new(false));
        let job = terminal_attempt(&f, &runner);
        let held = pause.arm();
        begin_settlement(&runner, &job);
        pause.wait();
        let mut queued = job.clone();
        queued.id = Uuid::now_v7();
        queued.request.as_mut().unwrap().job_id = queued.id;
        queued.terminal = None;
        queued.finished_ms = None;
        queued.state = JobState::Queued;
        {
            let mut inner = runner.core.inner.lock().unwrap();
            assert!(inner.writer.is_none());
            inner.jobs.insert(queued.id, queued.clone());
            runner.dispatch(&mut inner, runner.now()).unwrap();
        }
        let starting = runner
            .core
            .inner
            .lock()
            .unwrap()
            .starting
            .contains(&queued.id);
        assert!(
            !starting,
            "dispatch started a writer before prior settlement was durable"
        );
        runner.core.inner.lock().unwrap().jobs.remove(&queued.id);
        drop(held);
        wait_settlement(&runner, job.id);
    });
}

#[test]
fn deploy_drain_waits_for_terminal_fill_settlement() {
    bounded(|| {
        let f = Fixture::new();
        declare(&f, &controlled_command(&f));
        let (runner, pause) = paused_runner(&f, "base_cleanup");
        let job = terminal_attempt(&f, &runner);
        let held = pause.arm();
        begin_settlement(&runner, &job);
        pause.wait();
        let waiter = runner.begin_drain_request(Duration::from_secs(60), true);
        assert_eq!(
            runner.drain_reply_for(&waiter),
            None,
            "deploy acknowledged an in-memory settlement"
        );
        drop(held);
        wait_settlement(&runner, job.id);
        assert_eq!(runner.drain_reply_for(&waiter), Some((false, 0)));
    });
}

#[test]
fn deploy_drain_waits_for_writer_and_execution_finalizers() {
    bounded(|| {
        let f = Fixture::new();
        declare(&f, &controlled_command(&f));
        let runner = controlled_runner(&f);
        let job = terminal_attempt(&f, &runner);
        let waiter = runner.begin_drain_request(Duration::from_secs(60), true);
        assert_eq!(
            runner.drain_reply_for(&waiter),
            None,
            "deploy ignored the reserved writer"
        );
        {
            let mut inner = runner.core.inner.lock().unwrap();
            inner.writer = None;
            inner.finalizing.insert(job.id);
        }
        assert_eq!(
            runner.drain_reply_for(&waiter),
            None,
            "deploy ignored an execution finalizer"
        );
        runner.core.inner.lock().unwrap().finalizing.remove(&job.id);
        assert_eq!(runner.drain_reply_for(&waiter), Some((false, 0)));
    });
}

#[test]
fn clearing_execution_finalizing_cannot_start_a_second_settler() {
    bounded(|| {
        let f = Fixture::new();
        declare(&f, &controlled_command(&f));
        let (runner, pause) = paused_runner(&f, "base_cleanup");
        let job = terminal_attempt(&f, &runner);
        let held = pause.arm();
        begin_settlement(&runner, &job);
        pause.wait();
        let id = job.id;
        drop(Marker::new(&runner, move |inner| {
            inner.finalizing.remove(&id);
        }));
        runner.tick().unwrap();
        let execution_claim = runner.core.inner.lock().unwrap().finalizing.contains(&id);
        assert!(
            !execution_claim,
            "a settlement retry reclaimed execution's finalizing flag"
        );
        assert_eq!(
            pause.entered.load(Ordering::SeqCst),
            1,
            "a second settler consumed the same attempt"
        );
        drop(held);
        wait_settlement(&runner, job.id);
    });
}

#[test]
fn normal_fill_cleanup_failure_is_bounded_and_releases_the_writer() {
    bounded(|| {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let f = Fixture::new();
            declare(&f, &controlled_command(&f));
            let runner = controlled_runner(&f);
            submit_reader(&f, &runner).await;
            let job = running_fill(&runner, 0).await;
            let scratch = runner
                .config()
                .writer_scratch_dir()
                .join(job.id.to_string());
            fs::create_dir_all(&scratch).unwrap();
            let trash = runner.trash();
            if trash.exists() {
                fs::rename(&trash, f.scratch.join("saved-trash")).unwrap();
            }
            fs::write(&trash, "blocks scratch rename").unwrap();
            fs::write(release_path(&f), "").unwrap();
            terminal(&runner, job.id).await;
            let released = poll(Duration::from_secs(3), || {
                let inner = runner.core.inner.lock().unwrap();
                inner.writer.is_none() && !inner.base_finishes.contains_key(&job.id)
            });
            fs::remove_file(&trash).unwrap();
            assert!(released, "scratch cleanup stranded the singleton writer");
            assert!(
                scratch.exists(),
                "failed cleanup did not leave scratch for garbage collection"
            );
        })
    });
}

#[test]
fn deferred_fill_cleanup_stops_after_three_attempts_and_leaves_scratch_for_gc() {
    bounded(|| {
        let f = Fixture::new();
        declare(&f, &controlled_command(&f));
        let retries = Arc::new(AtomicUsize::new(0));
        let count = retries.clone();
        let runner = controlled_runner_with(&f, |config| {
            config.long_work = Some(Arc::new(move |step| {
                if step == "base_scratch_retry" {
                    count.fetch_add(1, Ordering::SeqCst);
                }
            }));
        });
        let job = terminal_attempt(&f, &runner);
        let scratch = runner
            .config()
            .writer_scratch_dir()
            .join(job.id.to_string());
        fs::create_dir_all(&scratch).unwrap();
        let trash = runner.trash();
        if trash.exists() {
            fs::rename(&trash, f.scratch.join("saved-trash")).unwrap();
        }
        fs::write(&trash, "blocks scratch rename").unwrap();
        begin_settlement(&runner, &job);
        let released = poll(Duration::from_secs(3), || {
            let inner = runner.core.inner.lock().unwrap();
            inner.writer.is_none() && !inner.base_finishes.contains_key(&job.id)
        });
        fs::remove_file(&trash).unwrap();
        assert!(
            released,
            "deferred scratch cleanup retried forever with the writer reserved"
        );
        assert_eq!(
            retries.load(Ordering::SeqCst),
            2,
            "scratch cleanup was not bounded at three total attempts"
        );
        assert!(scratch.exists());
    });
}

#[cfg(target_os = "linux")]
#[test]
fn cache_fill_setup_save_failure_keeps_a_retryable_settlement() {
    bounded(|| {
        let f = Fixture::new();
        declare(&f, &controlled_command(&f));
        let fail = Arc::new(AtomicBool::new(false));
        let hook = fail.clone();
        let failures = Arc::new(AtomicUsize::new(0));
        let seen = failures.clone();
        let pause = Arc::new(Pause::default());
        let pause_hook = pause.clone();
        let runner = controlled_runner_with(&f, |config| {
            config.long_work = Some(Arc::new(move |step| {
                if step == "base_terminal_save" {
                    pause_hook.block();
                }
            }));
            config.before_job_write = Some(Arc::new(move |_| {
                if hook.load(Ordering::SeqCst) {
                    seen.fetch_add(1, Ordering::SeqCst);
                    Err(io::Error::other("injected setup save failure"))
                } else {
                    Ok(())
                }
            }));
        });
        let mut job = terminal_attempt(&f, &runner);
        job.terminal = None;
        job.finished_ms = None;
        runner
            .core
            .inner
            .lock()
            .unwrap()
            .jobs
            .insert(job.id, job.clone());
        fail.store(true, Ordering::SeqCst);
        let held = pause.arm();
        let pending = {
            let mut inner = runner.core.inner.lock().unwrap();
            let _ =
                runner.fail_cache_fill(&mut inner, &job, io::Error::other("cache setup failed"));
            inner.base_finishes.contains_key(&job.id)
        };
        assert!(
            pending,
            "failed setup save lost both its terminal and its settlement"
        );
        pause.wait();
        let log = runner.job_log(job.id);
        assert!(
            log.write.try_lock().is_ok(),
            "setup terminal save holds job log lock"
        );
        assert!(
            log.signalling.try_lock().is_ok(),
            "setup terminal save holds signalling lock"
        );
        assert!(
            runner.core.inner.try_lock().is_ok(),
            "setup terminal save holds state lock"
        );
        drop(held);
        assert!(poll(Duration::from_secs(5), || failures
            .load(Ordering::SeqCst)
            > 0));
        fail.store(false, Ordering::SeqCst);
        runner.tick().unwrap();
        wait_settlement(&runner, job.id);
        let saved: Job = read_json(&runner.record_path(job.id)).unwrap();
        assert!(saved.terminal.is_some());
    });
}
