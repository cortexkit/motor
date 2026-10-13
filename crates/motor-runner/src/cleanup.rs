use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

/// Reserve cleanup before its asynchronous owner can finish or be spawned.
/// The reservation lasts through deletion, including errors and unwinding.
pub(crate) struct BackgroundDelete(Arc<AtomicUsize>);
impl BackgroundDelete {
    pub(crate) fn new(count: Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count)
    }
}
impl Drop for BackgroundDelete {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reservations_release_on_success_error_and_panic() {
        let count = Arc::new(AtomicUsize::new(0));
        let reservation = BackgroundDelete::new(count.clone());
        assert_eq!(count.load(Ordering::SeqCst), 1);
        drop(reservation);
        assert_eq!(count.load(Ordering::SeqCst), 0);
        let result = std::panic::catch_unwind({
            let count = count.clone();
            move || {
                let _reservation = BackgroundDelete::new(count);
                panic!("deliberate cleanup panic");
            }
        });
        assert!(result.is_err());
        assert_eq!(count.load(Ordering::SeqCst), 0);
        fn fail_while_reserved(count: Arc<AtomicUsize>) -> Result<(), ()> {
            let _reservation = BackgroundDelete::new(count);
            Err(())
        }
        assert!(fail_while_reserved(count.clone()).is_err());
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }
}
