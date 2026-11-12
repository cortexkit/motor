use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<String>>>);

impl tracing::Subscriber for Logs {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        struct Visitor(String);
        impl tracing::field::Visit for Visitor {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.push_str(&format!("{}={value:?} ", field.name()));
            }
        }
        let mut visitor = Visitor(format!("{} ", event.metadata().level()));
        event.record(&mut visitor);
        self.0.lock().unwrap().push(visitor.0);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

pub(crate) fn capture(f: impl FnOnce()) -> Vec<String> {
    let logs = Logs::default();
    let dispatch = tracing::Dispatch::new(logs.clone());
    // Tracing can cache enabled callsites for a single subscriber. A second
    // live subscriber lets this capture see callsites first used by other tests.
    let _spare = tracing::Dispatch::new(Logs::default());
    tracing::dispatcher::with_default(&dispatch, || {
        tracing::callsite::rebuild_interest_cache();
        f();
    });
    let lines = logs.0.lock().unwrap();
    lines.clone()
}
