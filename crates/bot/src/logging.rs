//! Container log contract; see docs/logging.md.

use std::fmt;

use serde_json::{Map, Value};
use tracing::{Event, Subscriber};
use tracing_subscriber::{
    filter::LevelFilter,
    fmt::{
        format::{JsonFields, Writer},
        time::{FormatTime, SystemTime},
        FmtContext, FormatEvent, FormatFields, FormattedFields, MakeWriter,
    },
    registry::LookupSpan,
    util::SubscriberInitExt,
    EnvFilter,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LogFormat {
    Json,
    Pretty,
}

impl LogFormat {
    fn from_setting(value: Option<&str>) -> Self {
        match value {
            Some("pretty") => Self::Pretty,
            _ => Self::Json,
        }
    }
}

pub fn init() {
    let format = std::env::var("LOG_FORMAT").ok();
    let rust_log = std::env::var("RUST_LOG").ok();
    let log_level = std::env::var("LOG_LEVEL").ok();
    subscriber(
        LogFormat::from_setting(format.as_deref()),
        filter(rust_log.as_deref(), log_level.as_deref()),
        std::io::stdout,
    )
    .init();
    if format
        .as_deref()
        .is_some_and(|v| v != "json" && v != "pretty")
    {
        // Never echo configuration values: even a malformed setting can be a secret.
        tracing::warn!(msg = "log_format_invalid", "using JSON log format");
    }
}

pub fn http_span(request: &axum::http::Request<axum::body::Body>) -> tracing::Span {
    // DefaultMakeSpan records the URI, including credential-bearing queries.
    // https://docs.rs/tower-http/0.7.0/tower_http/trace/index.html
    tracing::debug_span!("http_request", method = %request.method())
}

pub fn run_span() -> tracing::Span {
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let run_id = format!("{started}-{}", std::process::id());
    tracing::info_span!("run", run_id)
}

pub fn gateway_span(guild_id: u64) -> tracing::Span {
    tracing::info_span!("gateway", guild_id = %guild_id).or_current()
}

pub fn shard_closed(message: &twilight_gateway::Message) {
    if let twilight_gateway::Message::Close(frame) = message {
        tracing::warn!(msg = "shard_closed", code = frame.as_ref().map(|f| f.code));
    }
}

fn filter(rust_log: Option<&str>, log_level: Option<&str>) -> EnvFilter {
    // try_new rejects the entire malformed directive rather than writing a plain
    // text diagnostic to stderr. RUST_LOG takes precedence when valid/nonempty.
    // https://docs.rs/tracing-subscriber/0.3.23/tracing_subscriber/filter/struct.EnvFilter.html
    if let Some(value) = rust_log.filter(|v| !v.trim().is_empty()) {
        if let Ok(filter) = EnvFilter::try_new(value) {
            return filter;
        }
    }
    let level = log_level
        .and_then(|value| value.parse::<LevelFilter>().ok())
        .unwrap_or(LevelFilter::INFO);
    EnvFilter::new(level.to_string())
}

fn subscriber<W>(
    format: LogFormat,
    filter: EnvFilter,
    writer: W,
) -> Box<dyn Subscriber + Send + Sync>
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .log_internal_errors(false)
        .with_writer(writer);
    match format {
        LogFormat::Json => Box::new(builder.json().event_format(JsonEvent).finish()),
        LogFormat::Pretty => Box::new(builder.pretty().finish()),
    }
}

struct JsonEvent;

// JsonFields supplies tracing's typed value/escaping semantics. A custom event
// formatter is needed because the stock JSON envelope uses timestamp/message,
// not the legacy ts/msg keys. Span storage stays tracing-subscriber's JsonFields.
// https://docs.rs/tracing-subscriber/0.3.23/tracing_subscriber/fmt/trait.FormatEvent.html#examples
impl<S> FormatEvent<S, JsonFields> for JsonEvent
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, JsonFields>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut encoded = String::new();
        ctx.field_format()
            .format_fields(Writer::new(&mut encoded), event)?;
        let mut fields: Map<String, Value> =
            serde_json::from_str(&encoded).map_err(|_| fmt::Error)?;
        let mut spans = Vec::new();
        if let Some(scope) = ctx.event_scope() {
            for span in scope.from_root() {
                let extensions = span.extensions();
                let mut values: Map<String, Value> =
                    match extensions.get::<FormattedFields<JsonFields>>() {
                        Some(fields) => serde_json::from_str(fields).map_err(|_| fmt::Error)?,
                        None => Map::new(),
                    };
                values.insert("name".into(), span.name().into());
                spans.push(Value::Object(values));
            }
        }
        let mut correlation = Map::new();
        for span in &spans {
            for key in ["interaction_id", "guild_id", "run_id"] {
                if let Some(value) = span.get(key) {
                    correlation.insert(key.to_owned(), value.clone());
                }
            }
        }
        for (key, value) in correlation {
            fields.entry(key).or_insert(value);
        }
        if !spans.is_empty() {
            fields.insert("spans".into(), spans.into());
        } else {
            fields.remove("spans");
        }

        // Old named events (settings_reloaded, etc.) remain stable. Free-form
        // dependency/diagnostic messages get a stable fallback, not dynamic msg.
        let msg = fields
            .get("msg")
            .and_then(Value::as_str)
            .filter(|name| is_event_name(name))
            .or_else(|| {
                fields
                    .get("message")
                    .and_then(Value::as_str)
                    .filter(|name| is_event_name(name))
            })
            .unwrap_or("tracing_event")
            .to_owned();
        let mut ts = String::new();
        SystemTime.format_time(&mut Writer::new(&mut ts))?;
        // Reserved envelope keys cannot be overwritten by application fields.
        fields.insert("ts".into(), ts.into());
        fields.insert(
            "level".into(),
            event.metadata().level().as_str().to_lowercase().into(),
        );
        fields.insert("msg".into(), msg.into());
        fields.insert("target".into(), event.metadata().target().into());
        let line = serde_json::to_string(&fields).map_err(|_| fmt::Error)?;
        writeln!(writer, "{line}")
    }
}

fn is_event_name(name: &str) -> bool {
    !name.is_empty()
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Capture {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    impl Capture {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }

        fn lines(&self) -> Vec<Value> {
            self.text()
                .lines()
                .map(|line| {
                    let value: Value = serde_json::from_str(line).unwrap();
                    assert!(value.is_object());
                    for key in ["ts", "level", "msg", "target"] {
                        assert!(value[key].is_string(), "missing string {key}: {line}");
                    }
                    assert!(!line.contains('\u{1b}'));
                    value
                })
                .collect()
        }
    }

    #[test]
    fn every_line_is_json_with_stable_msg_and_flat_typed_fields() {
        let capture = Capture::default();
        tracing::subscriber::with_default(
            subscriber(LogFormat::Json, filter(None, None), capture.clone()),
            || {
                tracing::info!(
                    msg = "ready",
                    count = 3_u64,
                    resumed = false,
                    "ready for work"
                );
                tracing::warn!("settings_reloaded");
                tracing::error!(
                    reason = "a quote \" and newline\n",
                    "free-form error\nsecond line"
                );
                tracing::info!(answer = 42);
                tracing::info!(msg = 5, ts = "spoof", level = "spoof", target = "spoof");
            },
        );
        let lines = capture.lines();
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0]["msg"], "ready");
        assert_eq!(lines[0]["count"], 3);
        assert_eq!(lines[0]["resumed"], false);
        assert!(lines[0].get("fields").is_none());
        assert_eq!(lines[1]["msg"], "settings_reloaded");
        assert_eq!(lines[2]["msg"], "tracing_event");
        assert_eq!(lines[2]["reason"], "a quote \" and newline\n");
        assert_eq!(lines[3]["msg"], "tracing_event");
        assert_eq!(lines[4]["msg"], "tracing_event");
        assert_eq!(lines[4]["level"], "info");
        assert_ne!(lines[4]["ts"], "spoof");
        assert_ne!(lines[4]["target"], "spoof");
    }

    #[test]
    fn spans_preserve_correlation_record_updates_and_event_precedence() {
        let capture = Capture::default();
        tracing::subscriber::with_default(
            subscriber(LogFormat::Json, filter(None, None), capture.clone()),
            || {
                let run =
                    tracing::info_span!("run", run_id = "run-1", guild_id = "outer").entered();
                let interaction = tracing::info_span!(
                    "interaction",
                    interaction_id = "123",
                    guild_id = tracing::field::Empty
                )
                .entered();
                interaction.record("guild_id", "456");
                tracing::info!(msg = "job_started");
                tracing::info!(msg = "job_completed", guild_id = "explicit");
                drop(interaction);
                drop(run);
            },
        );
        let lines = capture.lines();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["guild_id"], "456");
        assert_eq!(lines[0]["interaction_id"], "123");
        assert_eq!(lines[0]["run_id"], "run-1");
        assert_eq!(lines[0]["spans"][0]["name"], "run");
        assert_eq!(lines[0]["spans"][1]["guild_id"], "456");
        assert_eq!(lines[1]["guild_id"], "explicit");
    }

    #[test]
    fn log_level_and_rust_log_precedence_are_respected() {
        for (rust_log, log_level, expected) in [
            (None, None, 2),
            (None, Some("error"), 0),
            (None, Some("WARN"), 1),
            (Some("info"), Some("error"), 2),
            (Some("off"), Some("info"), 0),
            (Some("two_bot=warn"), Some("debug"), 1),
            (Some("two_bot=not_a_level"), Some("warn"), 1),
            (Some(""), Some("warn"), 1),
            (None, Some("invalid"), 2),
        ] {
            let capture = Capture::default();
            tracing::subscriber::with_default(
                subscriber(
                    LogFormat::Json,
                    filter(rust_log, log_level),
                    capture.clone(),
                ),
                || {
                    tracing::info!(msg = "ready");
                    tracing::warn!(msg = "shard_closed");
                },
            );
            assert_eq!(
                capture.lines().len(),
                expected,
                "RUST_LOG={rust_log:?} LOG_LEVEL={log_level:?}"
            );
        }
    }

    #[tokio::test]
    async fn http_trace_omits_credential_bearing_uri() {
        use tower::ServiceExt;
        use tracing::instrument::WithSubscriber;

        let capture = Capture::default();
        let state = Arc::new(tokio::sync::RwLock::new(
            crate::gateway::GatewayState::Unconfigured,
        ));
        let request = axum::http::Request::builder()
            .uri("/health?access_token=do-not-log-this-query")
            .body(axum::body::Body::empty())
            .unwrap();
        let response = crate::server::router(state)
            .oneshot(request)
            .with_subscriber(subscriber(
                LogFormat::Json,
                filter(Some("debug"), None),
                capture.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert!(!capture.lines().is_empty());
        assert!(!capture.text().contains("do-not-log-this-query"));
        assert!(!capture.text().contains("access_token"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn graceful_shutdown_task_preserves_run_correlation() {
        use tracing::{instrument::WithSubscriber, Instrument};

        let capture = Capture::default();
        let dispatch = tracing::Dispatch::new(subscriber(
            LogFormat::Json,
            filter(None, None),
            capture.clone(),
        ));
        let run = tracing::dispatcher::with_default(&dispatch, run_span);
        let state = Arc::new(tokio::sync::RwLock::new(
            crate::gateway::GatewayState::Unconfigured,
        ));
        // Bind inside the run span so `http_listening` keeps run correlation,
        // matching `main.rs` where `bind` runs under the run span.
        // `WithSubscriber` sets the capture dispatcher for every poll, so no
        // ambient dispatcher is needed here.
        let listener = crate::server::bind("127.0.0.1:0")
            .instrument(run.clone())
            .with_subscriber(dispatch.clone())
            .await
            .unwrap();
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let serve = crate::server::serve_with_shutdown(
            listener,
            state,
            crate::jobs::statuses(&[], true),
            shutdown,
            async {
                tokio::task::yield_now().await;
                tracing::info!(msg = "shutdown_started", signal = "test");
            },
        )
        .instrument(run)
        .with_subscriber(dispatch);
        tokio::time::timeout(std::time::Duration::from_secs(2), serve)
            .await
            .unwrap()
            .unwrap();

        let lines = capture.lines();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["msg"], "http_listening");
        assert_eq!(lines[1]["msg"], "shutdown_started");
        assert_eq!(lines[2]["msg"], "shutdown_completed");
        let run_id = lines[0]["run_id"].as_str().unwrap();
        for event in &lines {
            assert_eq!(event["run_id"], run_id);
            assert_eq!(event["spans"][0]["name"], "run");
        }
    }

    #[test]
    fn pretty_is_explicit_opt_in() {
        assert_eq!(LogFormat::from_setting(None), LogFormat::Json);
        assert_eq!(LogFormat::from_setting(Some("json")), LogFormat::Json);
        assert_eq!(LogFormat::from_setting(Some("invalid")), LogFormat::Json);
        assert_eq!(LogFormat::from_setting(Some("pretty")), LogFormat::Pretty);
        let capture = Capture::default();
        tracing::subscriber::with_default(
            subscriber(LogFormat::Pretty, filter(None, None), capture.clone()),
            || tracing::info!(msg = "ready"),
        );
        assert!(capture.text().contains("ready"));
        assert!(serde_json::from_str::<Value>(&capture.text()).is_err());
    }
}
