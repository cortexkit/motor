use std::sync::Mutex;
use tokio::sync::watch;

#[derive(Default)]
pub(super) struct WriteBlock {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    blocked: bool,
    uploads: Vec<(watch::Sender<bool>, watch::Receiver<bool>)>,
}

pub(super) struct Upload {
    cancel: watch::Receiver<bool>,
    finished: watch::Sender<bool>,
}

impl WriteBlock {
    pub(super) fn blocked(&self) -> bool {
        self.state.lock().unwrap().blocked
    }

    pub(super) fn admit(&self) -> Option<Upload> {
        let mut state = self.state.lock().unwrap();
        if state.blocked {
            return None;
        }
        state.uploads.retain(|(_, done)| !*done.borrow());
        let (cancel, cancelled) = watch::channel(false);
        let (finished, done) = watch::channel(false);
        state.uploads.push((cancel, done));
        Some(Upload {
            cancel: cancelled,
            finished,
        })
    }

    pub(super) async fn set(&self, blocked: bool) {
        let uploads = {
            let mut state = self.state.lock().unwrap();
            state.blocked = blocked;
            state.uploads.retain(|(_, done)| !*done.borrow());
            if blocked {
                state
                    .uploads
                    .iter()
                    .map(|(cancel, done)| {
                        cancel.send_replace(true);
                        done.clone()
                    })
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            }
        };
        // Acknowledgement waits until unfinished uploads have dropped their
        // upstream connections. Clearing cannot revive a cancelled upload.
        for mut done in uploads {
            while !*done.borrow_and_update() {
                if done.changed().await.is_err() {
                    break;
                }
            }
        }
    }
}

impl Upload {
    pub(super) async fn cancelled(&mut self) {
        while !*self.cancel.borrow_and_update() {
            if self.cancel.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }
}

impl Drop for Upload {
    fn drop(&mut self) {
        self.finished.send_replace(true);
    }
}
