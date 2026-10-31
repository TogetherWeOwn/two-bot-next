//! Container log contract; see docs/logging.md.

use std::fmt;

use serde_json::{Map, Value};
use tracing::{Event, Subscriber};
use tracing_log::NormalizeEvent;
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
    let (rust_log_invalid, log_level_invalid) =
        boot_warning_flags(rust_log.as_deref(), log_level.as_deref());
    subscriber(
        LogFormat::from_setting(format.as_deref()),
        filter(rust_log.as_deref(), log_level.as_deref()),
        std::io::stdout,
    )
    .init();
    emit_boot_warnings(rust_log_invalid, log_level_invalid);
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

/// Emit one warn-level line per present-but-unparsable log key, naming the
/// key without echoing its value. Call after the subscriber is installed so
/// the lines flow through the fallback filter (subject to it, like
/// `log_format_invalid`).
fn emit_boot_warnings(rust_log_invalid: bool, log_level_invalid: bool) {
    if rust_log_invalid {
        // Never echo configuration values: even a malformed setting can be a secret.
        tracing::warn!(
            msg = "rust_log_invalid",
            "ignoring invalid RUST_LOG; using LOG_LEVEL fallback"
        );
    }
    if log_level_invalid {
        // Never echo configuration values: even a malformed setting can be a secret.
        tracing::warn!(
            msg = "log_level_invalid",
            "ignoring invalid LOG_LEVEL; using INFO fallback"
        );
    }
}

/// Present-but-unparsable `RUST_LOG`: nonempty (after trim) yet rejected by
/// `EnvFilter::try_new`. Absent, empty and valid values stay quiet.
fn rust_log_unparsable(rust_log: Option<&str>) -> bool {
    matches!(
        rust_log.filter(|value| !value.trim().is_empty()),
        Some(value) if EnvFilter::try_new(value).is_err()
    )
}

/// Valid `RUST_LOG` that takes precedence over `LOG_LEVEL`: nonempty (after
/// trim) and accepted by `EnvFilter::try_new`, mirroring `filter()`.
fn rust_log_valid(rust_log: Option<&str>) -> bool {
    matches!(
        rust_log.filter(|value| !value.trim().is_empty()),
        Some(value) if EnvFilter::try_new(value).is_ok()
    )
}

/// Boot-warning flags as `init()` computes them: each present-but-unparsable
/// key warns, except an invalid `LOG_LEVEL` stays quiet when a valid
/// `RUST_LOG` takes precedence (then `LOG_LEVEL` is ignored entirely).
fn boot_warning_flags(rust_log: Option<&str>, log_level: Option<&str>) -> (bool, bool) {
    let rust_log_invalid = rust_log_unparsable(rust_log);
    let log_level_invalid = log_level_unparsable(log_level) && !rust_log_valid(rust_log);
    (rust_log_invalid, log_level_invalid)
}

/// Present-but-unparsable `LOG_LEVEL`: nonempty (after trim) yet rejected by
/// `LevelFilter` parsing. Absent, empty and valid values stay quiet. The parse
/// runs on the raw value so the fallback filter stays exactly as today.
fn log_level_unparsable(log_level: Option<&str>) -> bool {
    matches!(
        log_level.filter(|value| !value.trim().is_empty()),
        Some(value) if value.parse::<LevelFilter>().is_err()
    )
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
    // Empty/blank levels count as missing: `LevelFilter` parses `""` as
    // ERROR, which would silently hide info/warn lines (including boot
    // warnings). Skip empties so absent, empty and whitespace-only all fall
    // back to INFO, matching the detectors and docs.
    let level = log_level
        .filter(|value| !value.trim().is_empty())
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

/// Build identity stamped on every JSON line (docs/logging.md): the image's
/// git SHA, `"unknown"` for local builds. `build_sha` names the built commit
/// and `release` the released commit; the single-image deploy makes them the
/// same value, so both keys carry it rather than inventing a second source.
const BUILD_REVISION: &str = match option_env!("BOT_BUILD_REVISION") {
    Some(revision) => revision,
    None => "unknown",
};

/// Span carrying the end-to-end correlation id (see
/// `two_bot_core::internal_actions::REQUEST_ID_HEADER`). Receivers enter one
/// per request and queued follow-up tasks inherit it, so every request-scoped
/// line joins across Worker, Durable Object, container and queued work.
pub fn request_span(request_id: &str) -> tracing::Span {
    tracing::info_span!("request", request_id = %request_id)
}

/// Correlation id for a request span: the inbound `x-request-id` when it has
/// a generated shape, else a fresh [`two_bot_core::internal_actions`]
/// id. Only the validated shape is ever honored or logged, so a
/// caller-supplied secret can never become a join key.
pub fn incoming_request_id(headers: &axum::http::HeaderMap) -> String {
    use two_bot_core::internal_actions::{is_request_id, new_request_id, REQUEST_ID_HEADER};
    headers
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| is_request_id(value))
        .map(str::to_owned)
        .unwrap_or_else(|| {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| {
                    elapsed.as_millis().try_into().unwrap_or(u64::MAX)
                });
            new_request_id(now_ms, &rand::random())
        })
}

struct CorrelationSpans;

impl<S: Subscriber> Filter<S> for CorrelationSpans {
    fn enabled(&self, metadata: &tracing::Metadata<'_>, _: &Context<'_, S>) -> bool {
        metadata.is_span()
            && (metadata.fields().field("run_id").is_some()
                || metadata.fields().field("guild_id").is_some()
                || metadata.fields().field("interaction_id").is_some()
                || metadata.fields().field("request_id").is_some()
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
        // `.init()` bridges `log` records (sqlx, hyper, ...) through tracing-log,
        // which gives them the synthetic target `log` and carries the real
        // target/module/file/line in `log.*` fields. Report the real target, as
        // the stock formatters do, and keep the envelope free of `log.*` keys.
        // https://docs.rs/tracing-log/0.2.0/tracing_log/trait.NormalizeEvent.html
        let normalized = event.normalized_metadata();
        let metadata = normalized.as_ref().unwrap_or_else(|| event.metadata());
        if event.is_log() {
            fields.retain(|key, _| !key.starts_with("log."));
        }
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
            for key in ["interaction_id", "guild_id", "run_id", "request_id"] {
                if let Some(value) = span.get(key) {
                    correlation.insert(key.to_owned(), value.clone());
                }
            }
        }
        for (key, value) in correlation {
            fields.entry(key).or_insert(value);
        }
        // Correlation IDs must stay decimal strings (docs/logging.md):
        // snowflakes exceed 2^53, so JSON numbers would silently round for
        // JS/jq consumers. Normalize numbers from either the event or its
        // spans; every current call site already emits strings, so this is
        // a fail-closed guard, not a live code path.
        for key in ["interaction_id", "guild_id", "run_id"] {
            if fields.get(key).is_some_and(Value::is_number) {
                if let Some(value) = fields.remove(key) {
                    fields.insert(key.into(), value.to_string().into());
                }
            }
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
            metadata.level().as_str().to_lowercase().into(),
        );
        fields.insert("msg".into(), msg.into());
        fields.insert("target".into(), metadata.target().into());
        fields.insert("build_sha".into(), BUILD_REVISION.into());
        fields.insert("release".into(), BUILD_REVISION.into());
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

/// In-memory JSON log capture for receiver redaction tests (see
/// `eventsub_http::tests`): drives the production formatter so the test
/// proves what staging would emit, not a fixture shape.
#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

#[cfg(test)]
impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
impl<'a> MakeWriter<'a> for Capture {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[cfg(test)]
impl Capture {
    pub(crate) fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }

    pub(crate) fn lines(&self) -> Vec<Value> {
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

/// Production JSON subscriber writing into a [`Capture`]: the redaction tests'
/// entry point to the real envelope (build identity, span promotion).
#[cfg(test)]
pub(crate) fn capture_subscriber(capture: Capture) -> Box<dyn Subscriber + Send + Sync> {
    subscriber(LogFormat::Json, filter(None, None), capture)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn every_line_carries_build_sha_and_release() {
        let capture = Capture::default();
        tracing::subscriber::with_default(
            subscriber(LogFormat::Json, filter(None, None), capture.clone()),
            || {
                tracing::info!(msg = "ready");
                tracing::warn!(msg = "shard_closed");
                // Application fields must not spoof build identity.
                tracing::info!(msg = "ready", build_sha = "spoof", release = "spoof");
            },
        );
        let lines = capture.lines();
        assert_eq!(lines.len(), 3);
        for line in &lines {
            for key in ["build_sha", "release"] {
                assert!(
                    line[key].as_str().is_some_and(|v| !v.is_empty()),
                    "missing {key}: {line}"
                );
                assert_ne!(line[key], "spoof", "{key} must not be spoofable: {line}");
            }
        }
    }

    #[test]
    fn incoming_request_id_honors_only_generated_shapes() {
        use two_bot_core::internal_actions::{is_request_id, REQUEST_ID_HEADER};
        let sent = two_bot_core::internal_actions::new_request_id(1_720_000_000_000, &[7u8; 16]);
        let honored = axum::http::HeaderMap::from_iter([(
            axum::http::header::HeaderName::from_static(REQUEST_ID_HEADER),
            axum::http::header::HeaderValue::from_str(&sent).unwrap(),
        )]);
        assert_eq!(incoming_request_id(&honored), sent);
        for bad in [
            "Bearer synthetic-id-secret",
            "https://user:synthetic-id-secret@example.invalid/",
            "short",
        ] {
            let headers = axum::http::HeaderMap::from_iter([(
                axum::http::header::HeaderName::from_static(REQUEST_ID_HEADER),
                axum::http::header::HeaderValue::from_str(bad).unwrap(),
            )]);
            let minted = incoming_request_id(&headers);
            assert!(is_request_id(&minted), "must mint a fresh id for {bad:?}");
            assert_ne!(minted, bad);
        }
        let minted = incoming_request_id(&axum::http::HeaderMap::new());
        assert!(is_request_id(&minted), "missing header must mint an id");
    }

    #[test]
    fn request_id_promotes_from_spans_with_event_precedence() {
        let capture = Capture::default();
        tracing::subscriber::with_default(
            subscriber(LogFormat::Json, filter(None, None), capture.clone()),
            || {
                let outer = tracing::info_span!("request", request_id = "outer-id").entered();
                tracing::info!(msg = "ready");
                let inner = tracing::info_span!("request", request_id = "inner-id").entered();
                tracing::info!(msg = "ready");
                tracing::info!(msg = "ready", request_id = "explicit-id");
                drop(inner);
                drop(outer);
                tracing::info!(msg = "ready");
            },
        );
        let lines = capture.lines();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0]["request_id"], "outer-id");
        // Inner span wins over outer; explicit event field wins over spans.
        assert_eq!(lines[1]["request_id"], "inner-id");
        assert_eq!(lines[2]["request_id"], "explicit-id");
        assert!(lines[3].get("request_id").is_none());
        // Correlation survives restrictive filters: the request span stays
        // available (via `CorrelationSpans`) while quiet events stay off.
        let strict = Capture::default();
        tracing::subscriber::with_default(
            subscriber(LogFormat::Json, filter(None, Some("error")), strict.clone()),
            || {
                let outer = tracing::info_span!("request", request_id = "outer-id").entered();
                tracing::warn!(msg = "shard_closed");
                tracing::error!(msg = "gateway_failed");
                drop(outer);
            },
        );
        let lines = strict.lines();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["msg"], "gateway_failed");
        assert_eq!(lines[0]["request_id"], "outer-id");
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
    fn bridged_log_records_report_their_real_target_without_log_fields() {
        // `log::error!` from a dependency reaches us as a tracing event whose
        // metadata target is the synthetic "log"; docs/logging.md promises the
        // real module target. Dispatch through tracing-log's own bridge so the
        // event has the exact production shape (`log.*` fields included).
        let capture = Capture::default();
        tracing::subscriber::with_default(
            subscriber(LogFormat::Json, filter(None, None), capture.clone()),
            || {
                tracing_log::format_trace(
                    &tracing_log::log::Record::builder()
                        .args(format_args!("pool timed out"))
                        .level(tracing_log::log::Level::Error)
                        .target("sqlx::pool")
                        .module_path(Some("sqlx::pool"))
                        .file(Some("pool.rs"))
                        .line(Some(7))
                        .build(),
                )
                .unwrap();
                tracing::error!(msg = "settings_reloaded", log.target = "app_field");
            },
        );
        let lines = capture.lines();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["target"], "sqlx::pool");
        assert_eq!(lines[0]["level"], "error");
        assert_eq!(lines[0]["msg"], "tracing_event");
        assert_eq!(lines[0]["message"], "pool timed out");
        assert!(lines[0]
            .as_object()
            .unwrap()
            .keys()
            .all(|key| !key.starts_with("log.")));
        // Native events keep their own target, and their fields are untouched.
        assert!(lines[1]["target"].as_str().unwrap().starts_with("two_bot"));
        assert_eq!(lines[1]["log.target"], "app_field");
    }

    #[test]
    fn numeric_correlation_fields_normalize_to_decimal_strings() {
        // Snowflakes exceed 2^53: a raw u64 field would serialize as a JSON
        // number and silently round for JS/jq consumers (docs/logging.md).
        let capture = Capture::default();
        tracing::subscriber::with_default(
            subscriber(LogFormat::Json, filter(None, None), capture.clone()),
            || {
                let run = tracing::info_span!("run", run_id = "run-9").entered();
                tracing::info!(
                    guild_id = 1545644954272137297u64,
                    interaction_id = 1469137636663758888u64,
                    msg = "job_started"
                );
                drop(run);
            },
        );
        let lines = capture.lines();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["guild_id"], "1545644954272137297");
        assert_eq!(lines[0]["interaction_id"], "1469137636663758888");
        assert_eq!(lines[0]["run_id"], "run-9");
    }

    #[test]
    fn invalid_log_settings_warn_then_fallback() {
        // Present-but-unparsable keys warn; absent/empty/valid keys stay quiet.
        assert!(log_level_unparsable(Some("debgu")));
        assert!(rust_log_unparsable(Some("two_bot=not_a_level")));
        for absent_or_empty in [None, Some(""), Some("   ")] {
            assert!(
                !log_level_unparsable(absent_or_empty),
                "LOG_LEVEL={absent_or_empty:?} must stay quiet"
            );
            assert!(
                !rust_log_unparsable(absent_or_empty),
                "RUST_LOG={absent_or_empty:?} must stay quiet"
            );
        }
        for valid in ["info", "WARN", "off", "debug"] {
            assert!(
                !log_level_unparsable(Some(valid)),
                "LOG_LEVEL={valid:?} must stay quiet"
            );
        }
        for valid in ["info", "warn", "two_bot=warn", "error,two_bot=debug"] {
            assert!(
                !rust_log_unparsable(Some(valid)),
                "RUST_LOG={valid:?} must stay quiet"
            );
        }
        // Warn-then-fallback: invalid LOG_LEVEL keeps today's INFO fallback
        // (info + warn pass), invalid RUST_LOG keeps today's fallthrough to
        // LOG_LEVEL (warn passes, info filtered).
        for (rust_log, log_level, expected) in [
            (None, Some("debgu"), 2),
            (Some("two_bot=not_a_level"), Some("warn"), 1),
            (Some("two_bot=not_a_level"), Some("debgu"), 2),
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
        // Boot warnings flow through the fallback filter, name the key, and
        // never echo the value.
        for (rust_log, log_level, expected_msgs) in [
            (None, Some("debgu"), vec!["log_level_invalid"]),
            (
                Some("two_bot=not_a_level"),
                Some("warn"),
                vec!["rust_log_invalid"],
            ),
            (
                Some("two_bot=not_a_level"),
                Some("debgu"),
                vec!["rust_log_invalid", "log_level_invalid"],
            ),
            (None, None, vec![]),
            (Some(""), Some("warn"), vec![]),
            // Valid RUST_LOG wins: an invalid LOG_LEVEL is ignored entirely,
            // so it stays quiet instead of claiming the INFO fallback is in use.
            (Some("warn"), Some("debgu"), vec![]),
            (Some("two_bot=debug"), Some("debgu"), vec![]),
        ] {
            let capture = Capture::default();
            tracing::subscriber::with_default(
                subscriber(
                    LogFormat::Json,
                    filter(rust_log, log_level),
                    capture.clone(),
                ),
                || {
                    let (rust_log_invalid, log_level_invalid) =
                        boot_warning_flags(rust_log, log_level);
                    emit_boot_warnings(rust_log_invalid, log_level_invalid);
                },
            );
            let lines = capture.lines();
            let msgs: Vec<&str> = lines
                .iter()
                .map(|line| line["msg"].as_str().unwrap())
                .collect();
            assert_eq!(
                msgs, expected_msgs,
                "RUST_LOG={rust_log:?} LOG_LEVEL={log_level:?}"
            );
            for line in &lines {
                assert_eq!(line["level"], "warn");
            }
            let text = capture.text();
            assert!(
                !text.contains("debgu") && !text.contains("not_a_level"),
                "boot warnings must name the key, never the value: {text}"
            );
            if !expected_msgs.is_empty() {
                assert!(
                    text.contains("RUST_LOG") || text.contains("LOG_LEVEL"),
                    "boot warning must name its key: {text}"
                );
            }
        }
    }

    #[test]
    fn log_level_and_rust_log_precedence_are_respected() {
        for (rust_log, log_level, expected) in [
            (None, None, 2),
            // Empty/blank LOG_LEVEL counts as missing (INFO fallback): `""`
            // must not silently parse as ERROR.
            (None, Some(""), 2),
            (None, Some("   "), 2),
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
            // Every line that survives the filter still carries the run it
            // served (the marker, plus the downgraded 503 failure at DEBUG).
            assert!(!lines.is_empty(), "no lines captured at {level}");
            for line in &lines {
                assert_eq!(line["run_id"], run_id, "{level}: {line}");
                if line.get("spans").is_some() {
                    assert_eq!(line["spans"][0]["name"], "run", "{level}: {line}");
                }
            }
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
            filter(Some("error,two_bot::server=debug"), None),
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
        let lines = capture.lines();
        let http: Vec<_> = lines
            .iter()
            .filter(|line| {
                line["target"].as_str() == Some("two_bot::server")
                    && line["message"].as_str() == Some("response failed")
            })
            .collect();
        assert!(!http.is_empty(), "no failure line for the 503");
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

    /// The `x-request-id` join key: a generated-shape inbound value is honored
    /// on every request-scoped line; anything else (bearer-looking secrets,
    /// URLs, free text, or a missing header) mints a fresh id and never logs
    /// the attacker value.
    #[tokio::test]
    async fn request_id_header_is_honored_or_minted() {
        use tower::ServiceExt;
        use tracing::instrument::WithSubscriber;

        async fn request_lines(
            header: Option<&str>,
        ) -> (axum::http::StatusCode, Vec<Value>, String) {
            let capture = Capture::default();
            let state = crate::server::SharedState::new(
                Arc::new(tokio::sync::RwLock::new(
                    crate::gateway::GatewayState::Unconfigured,
                )),
                None,
            );
            let mut builder = axum::http::Request::builder().uri("/health");
            if let Some(value) = header {
                builder = builder.header("x-request-id", value);
            }
            let request = builder.body(axum::body::Body::empty()).unwrap();
            let response = crate::server::router(state)
                .oneshot(request)
                .with_subscriber(subscriber(
                    LogFormat::Json,
                    filter(Some("debug"), None),
                    capture.clone(),
                ))
                .await
                .unwrap();
            let status = response.status();
            let text = capture.text();
            (status, capture.lines(), text)
        }

        let sent = two_bot_core::internal_actions::new_request_id(1_720_000_000_000, &[7u8; 16]);
        let (status, lines, _) = request_lines(Some(&sent)).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert!(
            lines.iter().any(|line| line["request_id"] == sent),
            "honored id missing: {lines:?}"
        );

        for attacker in [
            "Bearer synthetic-header-secret",
            "https://user:synthetic-url-secret@example.invalid/",
            "not-an-id",
        ] {
            let (status, lines, text) = request_lines(Some(attacker)).await;
            assert_eq!(status, axum::http::StatusCode::OK);
            assert!(
                !text.contains("synthetic-header-secret") && !text.contains("synthetic-url-secret"),
                "attacker header value reached logs: {text}"
            );
            let minted: Vec<_> = lines
                .iter()
                .filter_map(|line| line["request_id"].as_str())
                .collect();
            assert!(!minted.is_empty(), "minted id missing: {lines:?}");
            assert!(
                minted
                    .iter()
                    .copied()
                    .all(two_bot_core::internal_actions::is_request_id),
                "minted id has wrong shape: {minted:?}"
            );
        }

        let (status, lines, _) = request_lines(None).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert!(
            lines
                .iter()
                .filter_map(|line| line["request_id"].as_str())
                .any(two_bot_core::internal_actions::is_request_id),
            "missing header must still mint an id: {lines:?}"
        );
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
