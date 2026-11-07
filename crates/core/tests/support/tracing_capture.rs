//! Capture dependency diagnostics without relying on the application's filter.
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{span, Event, Metadata, Subscriber};

#[derive(Default)]
struct SpanState {
    fields: String,
    refs: usize,
}

#[derive(Default)]
struct Inner {
    text: Mutex<String>,
    cases: Mutex<HashMap<String, String>>,
    spans: Mutex<HashMap<u64, SpanState>>,
    next_span: AtomicU64,
}

#[derive(Clone, Default)]
pub struct Capture(Arc<Inner>);

impl Capture {
    pub fn text(&self) -> String {
        self.text_for_case(None)
    }

    /// Return complete events attributed by entered spans, or all events for
    /// `None`. Rendered message text cannot forge attribution, and embedded
    /// newlines cannot separate sensitive text from its event's case.
    pub fn text_for_case(&self, case: Option<&str>) -> String {
        match case {
            Some(case) => self
                .0
                .cases
                .lock()
                .unwrap()
                .get(case)
                .cloned()
                .unwrap_or_default(),
            None => self.0.text.lock().unwrap().clone(),
        }
    }
}

// Test-only attribution: a span carrying a `capture_case` field tags every
// event emitted while it is entered. Keep the rendered suffix for existing
// readers, but `text_for_case` selects complete records from entered spans:
// multiline messages and message text resembling a marker cannot defeat it.
// Spans without that field annotate nothing, so existing text is unchanged.
thread_local! {
    static ENTERED_CASES: std::cell::RefCell<Vec<(u64, String)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn capture_case(fields: &str) -> Option<String> {
    let rest = fields.split(" capture_case=").nth(1)?;
    let rest = rest.strip_prefix('"').unwrap_or(rest);
    let end = rest.find(['"', ' ']).unwrap_or(rest.len());
    let case = &rest[..end];
    (!case.is_empty()).then(|| case.to_owned())
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
// The support file is `#[path]`-included in several test binaries and not
// every binary uses `global()`; allow dead code so `-D warnings` stays green.
#[allow(dead_code)]
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

    fn new_span(&self, attrs: &span::Attributes<'_>) -> span::Id {
        let id = self.0.next_span.fetch_add(1, Ordering::SeqCst) + 1;
        let mut fields = String::new();
        attrs.record(&mut Fields(&mut fields));
        self.0
            .spans
            .lock()
            .unwrap()
            .insert(id, SpanState { fields, refs: 1 });
        span::Id::from_u64(id)
    }

    fn record(&self, id: &span::Id, values: &span::Record<'_>) {
        if let Some(state) = self.0.spans.lock().unwrap().get_mut(&id.into_u64()) {
            let mut extra = String::new();
            values.record(&mut Fields(&mut extra));
            state.fields.push_str(&extra);
        }
    }

    fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}

    fn clone_span(&self, id: &span::Id) -> span::Id {
        if let Some(state) = self.0.spans.lock().unwrap().get_mut(&id.into_u64()) {
            state.refs += 1;
        }
        span::Id::from_u64(id.into_u64())
    }

    fn try_close(&self, id: span::Id) -> bool {
        let mut spans = self.0.spans.lock().unwrap();
        match spans.get_mut(&id.into_u64()) {
            Some(state) if state.refs > 1 => {
                state.refs -= 1;
                false
            }
            Some(_) => {
                spans.remove(&id.into_u64());
                true
            }
            None => true,
        }
    }

    fn enter(&self, id: &span::Id) {
        let case = self
            .0
            .spans
            .lock()
            .unwrap()
            .get(&id.into_u64())
            .and_then(|state| capture_case(&state.fields));
        if let Some(case) = case {
            ENTERED_CASES.with(|entered| entered.borrow_mut().push((id.into_u64(), case)));
        }
    }

    fn exit(&self, id: &span::Id) {
        ENTERED_CASES.with(|entered| {
            let mut entered = entered.borrow_mut();
            if let Some(position) = entered
                .iter()
                .rposition(|(entered_id, _)| *entered_id == id.into_u64())
            {
                entered.remove(position);
            }
        });
    }

    fn event(&self, event: &Event<'_>) {
        let mut text = self.0.text.lock().unwrap();
        let start = text.len();
        write!(
            text,
            "{} {}",
            event.metadata().level(),
            event.metadata().target()
        )
        .unwrap();
        event.record(&mut Fields(&mut text));
        ENTERED_CASES.with(|entered| {
            for (_, case) in entered.borrow().iter() {
                write!(text, " capture_case={case}").unwrap();
            }
        });
        text.push('\n');
        ENTERED_CASES.with(|entered| {
            let mut cases = self.0.cases.lock().unwrap();
            for (_, case) in entered.borrow().iter() {
                cases
                    .entry(case.clone())
                    .or_default()
                    .push_str(&text[start..]);
            }
        });
    }
}
