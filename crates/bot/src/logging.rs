//! Container log contract; see docs/logging.md.

use std::fmt;

use serde_json::{Map, Value};
use tracing::{Event, Subscriber};
use tracing_subscriber::{
    filter::{FilterExt, LevelFilter},
    fmt::{
        format::{JsonFields, Writer},
        time::{FormatTime, SystemTime},
        FmtContext, FormatEvent, FormatFields, FormattedFields, MakeWriter,
    },
    layer::{Context, Filter, SubscriberExt},
    registry::LookupSpan,
    util::SubscriberInitExt,
    EnvFilter, Layer,
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

pub fn with_http_context(router: axum::Router) -> axum::Router {
    use axum::{extract::Request, middleware::Next};
    use tracing::{instrument::WithSubscriber, Instrument};

    // Axum spawns connection tasks without the serving future's context. Capture
    // it at construction and restore it around the entire TraceLayer invocation.
    // https://docs.rs/axum/0.8.9/axum/middleware/fn.from_fn.html
    let span = tracing::Span::current();
    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    router.layer(axum::middleware::from_fn(
        move |request: Request, next: Next| {
            next.run(request)
                .instrument(span.clone())
                .with_subscriber(dispatch.clone())
        },
    ))
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
    // Scope verbosity to our crates (`EnvFilter` matches target prefixes, so
    // one directive covers two_bot, two_bot_core, two_bot_discord and the rest).
    // Anything else logs at ERROR only: dependency chatter (tower-http request
    // lines, sqlx, twilight) stays out unless RUST_LOG opts in. RUST_LOG still
    // overrides everything.
    EnvFilter::new(format!("error,two_bot={level}"))
}

fn subscriber<W>(
    format: LogFormat,
    filter: EnvFilter,
    writer: W,
) -> Box<dyn Subscriber + Send + Sync>
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    // Correlation spans remain available even with WARN/ERROR/OFF directives;
    // only events obey verbosity. Never enable an event through this predicate.
    // https://docs.rs/tracing-subscriber/0.3.23/tracing_subscriber/filter/trait.FilterExt.html#method.or
    let filter = filter.or(CorrelationSpans);
    let layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .log_internal_errors(false)
        .with_writer(writer);
    match format {
        LogFormat::Json => Box::new(
            tracing_subscriber::registry()
                .with(layer.json().event_format(JsonEvent).with_filter(filter)),
        ),
        LogFormat::Pretty => {
            Box::new(tracing_subscriber::registry().with(layer.pretty().with_filter(filter)))
        }
    }
}

struct CorrelationSpans;

impl<S: Subscriber> Filter<S> for CorrelationSpans {
    fn enabled(&self, metadata: &tracing::Metadata<'_>, _: &Context<'_, S>) -> bool {
        metadata.is_span()
            && (metadata.fields().field("run_id").is_some()
                || metadata.fields().field("guild_id").is_some()
                || metadata.fields().field("interaction_id").is_some()
                // server.rs's redacted request span: tower-http parents its
                // request/failure events on it, so it must exist even when
                // DEBUG spans are filtered out or those events lose the run.
                || metadata.fields().field("http.request.method").is_some())
    }

    // FilterFn's default event_enabled is true. Under OR that would bypass
    // EnvFilter's dynamic field/span directives; this branch never enables events.
    // https://docs.rs/tracing-subscriber/0.3.23/tracing_subscriber/layer/trait.Filter.html#method.event_enabled
    fn event_enabled(&self, _: &Event<'_>, _: &Context<'_, S>) -> bool {
        false
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

    #[test]
    fn restrictive_filters_keep_correlation_without_enabling_quiet_events() {
        for (rust_log, level, expected) in [
            (None, "warn", 2),
            (None, "error", 1),
            (None, "off", 0),
            (Some("off,[gateway]=error"), "debug", 1),
        ] {
            let capture = Capture::default();
            tracing::subscriber::with_default(
                subscriber(
                    LogFormat::Json,
                    filter(rust_log, Some(level)),
                    capture.clone(),
                ),
                || {
                    let run = run_span().entered();
                    assert!(run.id().is_some());
                    let gateway = gateway_span(456).entered();
                    let interaction = tracing::info_span!(
                        "interaction",
                        interaction_id = "123",
                        guild_id = "456"
                    )
                    .entered();
                    tracing::info!(msg = "ready");
                    tracing::warn!(msg = "shard_closed");
                    tracing::error!(msg = "gateway_failed");
                    drop(interaction);
                    drop(gateway);
                    drop(run);
                },
            );
            let lines = capture.lines();
            assert_eq!(
                lines.len(),
                expected,
                "RUST_LOG={rust_log:?} LOG_LEVEL={level}"
            );
            for line in lines {
                assert!(line["run_id"].as_str().is_some_and(|id| !id.is_empty()));
                assert_eq!(line["guild_id"], "456");
                assert_eq!(line["interaction_id"], "123");
                assert_ne!(line["msg"], "ready");
            }
        }
    }

    #[tokio::test]
    async fn unsupported_dsn_parameters_cannot_reach_json_logs() {
        use tracing::instrument::WithSubscriber;

        let capture = Capture::default();
        // Rejected before parsing/connecting: only synthetic data, no DB access.
        for key in ["sslpassword", "api%5Fkey"] {
            let result = two_bot_cutover::connect(
                &format!("postgres://agent_test@agent-testdb/db?{key}=synthetic-log-secret"),
                1,
                true,
            )
            .with_subscriber(subscriber(
                LogFormat::Json,
                filter(None, None),
                capture.clone(),
            ))
            .await;
            match result.unwrap_err() {
                sqlx::Error::InvalidArgument(message) => {
                    assert_eq!(message, "unsupported database URL parameter");
                }
                _ => panic!("DSN was not rejected before connection"),
            }
        }
        assert!(
            capture.text().is_empty(),
            "parser must not log rejected DSNs"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tcp_request_logs_preserve_run_at_debug_warn_and_error() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tracing::{instrument::WithSubscriber, Instrument};

        for level in ["debug", "warn", "error"] {
            let capture = Capture::default();
            let dispatch = tracing::Dispatch::new(subscriber(
                LogFormat::Json,
                filter(None, Some(level)),
                capture.clone(),
            ));
            let run = tracing::dispatcher::with_default(&dispatch, run_span);
            let run_id = tracing::dispatcher::with_default(&dispatch, || {
                let _guard = run.enter();
                tracing::error!(msg = "test_run_marker");
                capture.lines()[0]["run_id"].as_str().unwrap().to_owned()
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let state = crate::server::SharedState::new(
                Arc::new(tokio::sync::RwLock::new(
                    crate::gateway::GatewayState::Unconfigured,
                )),
                None,
            );
            let (shutdown, _) = tokio::sync::watch::channel(false);
            let (stop, stopped) = tokio::sync::oneshot::channel();
            let server = tokio::spawn(
                crate::server::serve_with_shutdown(
                    listener,
                    state,
                    crate::jobs::statuses(&[], true),
                    shutdown,
                    async {
                        let _ = stopped.await;
                    },
                )
                .instrument(run)
                .with_subscriber(dispatch),
            );
            let requests = async {
                for (path, status) in [("/health", "200"), ("/readyz", "503")] {
                    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
                    stream.write_all(format!(
                        "GET {path}?access_token=synthetic-http-secret HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
                    ).as_bytes()).await.unwrap();
                    let mut response = Vec::new();
                    stream.read_to_end(&mut response).await.unwrap();
                    assert!(String::from_utf8(response)
                        .unwrap()
                        .starts_with(&format!("HTTP/1.1 {status}")));
                }
            };
            let requests_result =
                tokio::time::timeout(std::time::Duration::from_secs(3), requests).await;
            stop.send(()).unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(3), server)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            requests_result.unwrap();
            let lines = capture.lines();
            let http: Vec<_> = lines
                .iter()
                .filter(|line| {
                    line["target"]
                        .as_str()
                        .is_some_and(|target| target.starts_with("tower_http::trace"))
                })
                .collect();
            // The default filter scopes verbosity to our crates (`error` floor
            // elsewhere), so no tower line flows here at any level — in
            // particular the routine /readyz 503 never logs at ERROR.
            assert!(
                http.is_empty(),
                "tower lines must stay filtered at {level}: {http:?}"
            );
            assert!(!capture.text().contains("synthetic-http-secret"));
            assert!(!capture.text().contains("access_token"));
        }
    }

    /// When `RUST_LOG` admits tower-http, the routine /readyz 503 still must
    /// not log at ERROR: the trace layer downgrades it to DEBUG.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn readyz_503_downgraded_when_tower_admitted() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tracing::{instrument::WithSubscriber, Instrument};

        let capture = Capture::default();
        let dispatch = tracing::Dispatch::new(subscriber(
            LogFormat::Json,
            filter(Some("error,tower_http=debug"), None),
            capture.clone(),
        ));
        let run = tracing::dispatcher::with_default(&dispatch, run_span);
        let run_id = tracing::dispatcher::with_default(&dispatch, || {
            let _guard = run.enter();
            tracing::error!(msg = "test_run_marker");
            capture.lines()[0]["run_id"].as_str().unwrap().to_owned()
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = crate::server::SharedState::new(
            Arc::new(tokio::sync::RwLock::new(
                crate::gateway::GatewayState::Unconfigured,
            )),
            None,
        );
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(
            crate::server::serve_with_shutdown(
                listener,
                state,
                crate::jobs::statuses(&[], true),
                shutdown,
                async {
                    let _ = stopped.await;
                },
            )
            .instrument(run)
            .with_subscriber(dispatch),
        );
        let requests = async {
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            stream
                .write_all(b"GET /readyz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.unwrap();
            assert!(String::from_utf8(response)
                .unwrap()
                .starts_with("HTTP/1.1 503"));
        };
        let requests_result =
            tokio::time::timeout(std::time::Duration::from_secs(3), requests).await;
        stop.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        requests_result.unwrap();
        let http: Vec<_> = capture
            .lines()
            .iter()
            .filter(|line| {
                line["target"]
                    .as_str()
                    .is_some_and(|target| target.starts_with("tower_http::trace"))
            })
            .collect();
        assert!(!http.is_empty(), "no tower failure line for the 503");
        assert!(
            http.iter().all(|line| line["level"] != "error"),
            "no failure may log at ERROR: {http:?}"
        );
        assert!(
            http.iter()
                .any(|line| line["level"] == "debug" && line["message"] == "response failed"),
            "expected the downgraded 503 failure line: {http:?}"
        );
        for line in http {
            assert_eq!(line["run_id"], run_id, "{line}");
            assert_eq!(line["spans"][0]["name"], "run");
        }
    }

    #[tokio::test]
    async fn http_trace_omits_credential_bearing_uri() {
        use tower::ServiceExt;
        use tracing::instrument::WithSubscriber;

        let capture = Capture::default();
        let state = crate::server::SharedState::new(
            Arc::new(tokio::sync::RwLock::new(
                crate::gateway::GatewayState::Unconfigured,
            )),
            None,
        );
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
        let state = crate::server::SharedState::new(
            Arc::new(tokio::sync::RwLock::new(
                crate::gateway::GatewayState::Unconfigured,
            )),
            None,
        );
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
