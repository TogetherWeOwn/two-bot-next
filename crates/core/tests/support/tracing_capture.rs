//! Capture dependency diagnostics without relying on the application's filter.
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use tracing::{span, Event, Metadata, Subscriber};

#[derive(Clone, Default)]
pub struct Capture(Arc<Mutex<String>>);

impl Capture {
    pub fn text(&self) -> String {
        self.0.lock().unwrap().clone()
    }
}

/// One process-wide capture for log-hygiene tests in the same test binary.
///
/// `tracing-core` resolves a callsite's first registration against the
/// registering thread's default while only one dispatcher is live, so a
/// thread-local `with_default`/`set_default` capture can miss events when
/// sibling tests run on other threads. A single global install (one per
/// process; each reader keeps only the text appended after its own start
/// offset) keeps callsite interest stable. All log-assertion tests in one
/// binary must share this helper instead of installing their own global.
pub fn global() -> &'static Capture {
    static GLOBAL: std::sync::OnceLock<Capture> = std::sync::OnceLock::new();
    GLOBAL.get_or_init(|| {
        let capture = Capture::default();
        tracing::subscriber::set_global_default(capture.clone())
            .expect("install global tracing capture");
        capture
    })
}

struct Fields<'a>(&'a mut String);

impl tracing::field::Visit for Fields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        write!(self.0, " {}={value:?}", field.name()).unwrap();
    }
}

impl Subscriber for Capture {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        *metadata.level() <= tracing::Level::DEBUG
    }

    fn max_level_hint(&self) -> Option<tracing::metadata::LevelFilter> {
        Some(tracing::metadata::LevelFilter::DEBUG)
    }

    fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }

    fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
    fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
    fn enter(&self, _: &span::Id) {}
    fn exit(&self, _: &span::Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut text = self.0.lock().unwrap();
        write!(
            text,
            "{} {}",
            event.metadata().level(),
            event.metadata().target()
        )
        .unwrap();
        event.record(&mut Fields(&mut text));
        text.push('\n');
    }
}
