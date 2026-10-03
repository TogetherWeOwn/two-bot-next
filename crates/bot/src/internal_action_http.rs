//! Private announcement receiver. Never merge this router into the health socket.
//! Authentication and a committed nonce precede JSON; a committed intent precedes
//! the effect. Cancellation leaves durable ownership, never a new execution lease.

use std::{
    future::IntoFuture,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::to_bytes,
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Json, Router,
};
use futures_util::future::BoxFuture;
use serde_json::{json, Map, Value};
use tokio::{net::TcpListener, sync::Semaphore};
use two_bot_core::{
    clock_guard::ClockGuard,
    internal_action_config::InternalActionConfig,
    internal_action_store::{
        AuditSubject, DiscordId, InternalActionStore, InternalClaim, RequestIdentity,
        TerminalFailure, TerminalResponse,
    },
    internal_actions::{
        new_request_id, validate_announcement, validate_idempotency_key, ActionError, AuthHeaders,
        AuthenticatedRequest, ErrorCode, InternalFlags, TokenBuckets, ACTIONS_PATH, MAX_BODY_BYTES,
        SKEW_SECONDS,
    },
    rejection_telemetry::{ActionLabel, KeyLabel, Rejection, RejectionRecord, RejectionTelemetry},
};
use two_bot_discord::internal_actions::{AnnouncementExecutor, ExecutionOutcome, Refusal};

const MAX_HEADER_BYTES: usize = 8192;
const MAX_HEADERS: usize = 64;
const MAX_REQUESTS: usize = 32;
const BODY_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// The test seam is module-private: runtime effects can only use the admitted
/// announcement adapter. It does not expose an origin override or a resend API.
enum Effect {
    Terminal(TerminalResponse),
    Unknown,
}

trait ActionEffect: Send + Sync {
    fn execute<'a>(&'a self, body: &'a Map<String, Value>) -> BoxFuture<'a, Effect>;
}

impl ActionEffect for AnnouncementExecutor {
    fn execute<'a>(&'a self, body: &'a Map<String, Value>) -> BoxFuture<'a, Effect> {
        Box::pin(async move {
            let response = match self.execute("announcement.post", body).await {
                ExecutionOutcome::Posted(receipt) => {
                    let Ok(message_id) = DiscordId::new(&receipt.message_id().to_string()) else {
                        return Effect::Unknown;
                    };
                    TerminalResponse::Success {
                        resource_id: Some(message_id),
                        affected: 1,
                    }
                }
                ExecutionOutcome::NoEffect(refusal) => TerminalResponse::Failure(match refusal {
                    Refusal::Malformed => TerminalFailure::Malformed,
                    Refusal::ActionNotAllowed => TerminalFailure::ActionNotAllowed,
                    Refusal::DiscordRejected => TerminalFailure::DiscordRejected,
                    Refusal::InvalidChannelConfiguration
                    | Refusal::LocalConfiguration
                    | Refusal::SendAdmissionBlocked
                    | Refusal::CoolingDown => TerminalFailure::NoEffect,
                }),
                // The adapter installs both its local governor and the durable
                // token-wide admission hold before it returns. Never resend 429.
                ExecutionOutcome::RateLimited(_) => {
                    TerminalResponse::Failure(TerminalFailure::DiscordRejected)
                }
                ExecutionOutcome::Unknown(_) => return Effect::Unknown,
            };
            Effect::Terminal(response)
        })
    }
}

struct ReceiverState {
    config: InternalActionConfig,
    store: InternalActionStore,
    effect: Arc<dyn ActionEffect>,
    clock: Mutex<ClockGuard>,
    buckets: Mutex<TokenBuckets>,
    telemetry: Mutex<RejectionTelemetry>,
    capacity: Arc<Semaphore>,
}

impl ReceiverState {
    fn new(
        config: InternalActionConfig,
        pool: sqlx::PgPool,
        effect: Arc<dyn ActionEffect>,
    ) -> Self {
        Self {
            config,
            store: InternalActionStore::new(pool),
            effect,
            clock: Mutex::new(ClockGuard::new()),
            buckets: Mutex::new(TokenBuckets::new()),
            telemetry: Mutex::new(RejectionTelemetry::default()),
            capacity: Arc::new(Semaphore::new(MAX_REQUESTS)),
        }
    }

    fn reject(&self, failure: Failure, key: KeyLabel, action: ActionLabel, id: &str) -> Response {
        let records = self
            .telemetry
            .lock()
            .expect("telemetry lock")
            .record(Rejection::new(failure.class_code, key, action), now_ms());
        log_records(records);
        failure.response(id)
    }

    fn terminal(
        &self,
        response: TerminalResponse,
        replayed: bool,
        id: &str,
        key: KeyLabel,
        action: ActionLabel,
    ) -> Response {
        if let TerminalResponse::Failure(failure) = &response {
            let code = match failure {
                TerminalFailure::Malformed => ErrorCode::Malformed,
                TerminalFailure::ActionNotAllowed => ErrorCode::ActionNotAllowed,
                TerminalFailure::DiscordRejected => ErrorCode::DiscordRejected,
                TerminalFailure::NoEffect => ErrorCode::DiscordUnavailable,
            };
            log_records(
                self.telemetry
                    .lock()
                    .expect("telemetry lock")
                    .record(Rejection::new(code, key, action), now_ms()),
            );
        }
        terminal_response(response, replayed, id)
    }

    fn flush(&self, shutdown: bool) {
        let mut telemetry = self.telemetry.lock().expect("telemetry lock");
        let records = if shutdown {
            telemetry.close_window()
        } else {
            telemetry.flush(now_ms())
        };
        log_records(records);
    }
}

/// Binding completes before any gateway/job task starts. Invalid enabled
/// configuration and bind failure are fatal; there is no health-only fallback.
pub struct BoundReceiver {
    listener: TcpListener,
    state: Arc<ReceiverState>,
}

pub async fn bind(
    config: InternalActionConfig,
    pool: sqlx::PgPool,
    token: &str,
) -> std::io::Result<BoundReceiver> {
    use two_bot_core::send_admission::PgSendAdmission;
    use two_bot_discord::internal_actions::CooldownGovernor;

    let admission = PgSendAdmission::new(pool.clone(), token)
        .map_err(|_| std::io::Error::other("internal-action admission configuration invalid"))?;
    let executor = AnnouncementExecutor::with_admission(
        Arc::new(twilight_http::Client::new(token.to_owned())),
        config.channel_keys().clone(),
        CooldownGovernor::new(),
        Arc::new(admission),
    )
    .map_err(|_| std::io::Error::other("internal-action executor configuration invalid"))?;
    let listener = TcpListener::bind(config.listen_addr()).await?;
    Ok(BoundReceiver {
        listener,
        state: Arc::new(ReceiverState::new(config, pool, Arc::new(executor))),
    })
}

impl BoundReceiver {
    /// Supervised by website_jobs along with the other runtime services.
    pub async fn serve(self, shutdown: tokio::sync::watch::Receiver<bool>) -> std::io::Result<()> {
        let server = axum::serve(self.listener, router(Arc::clone(&self.state)))
            .with_graceful_shutdown(crate::server::shutdown_requested(shutdown))
            .into_future();
        tokio::pin!(server);
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        let result = loop {
            tokio::select! {
                result = &mut server => break result,
                _ = tick.tick() => self.state.flush(false),
            }
        };
        self.state.flush(true);
        result
    }
}

fn router(state: Arc<ReceiverState>) -> Router {
    // `any` lets the boundary give non-POSTs the same scalar envelope. No JSON
    // extractor or generic TraceLayer: neither request paths nor headers may log.
    Router::new()
        .route(ACTIONS_PATH, any(handle))
        .fallback(not_found)
        .with_state(state)
}

async fn not_found(State(state): State<Arc<ReceiverState>>) -> Response {
    state.reject(
        Failure::http(
            StatusCode::NOT_FOUND,
            "not_found",
            false,
            ErrorCode::Malformed,
        ),
        KeyLabel::Invalid,
        ActionLabel::Unknown,
        &request_id(),
    )
}

async fn handle(State(state): State<Arc<ReceiverState>>, request: Request) -> Response {
    let id = request_id();
    let Ok(_permit) = Arc::clone(&state.capacity).try_acquire_owned() else {
        return state.reject(
            Failure::http(
                StatusCode::SERVICE_UNAVAILABLE,
                "busy",
                true,
                ErrorCode::RateLimited,
            ),
            KeyLabel::Invalid,
            ActionLabel::Unknown,
            &id,
        );
    };
    match tokio::time::timeout(REQUEST_TIMEOUT, receive(&state, request, &id)).await {
        Ok(response) => response,
        // If a claim was committed, dropping the future retains it. A fresh
        // nonce with that intent can only see InFlight/NeedsReconciliation.
        Err(_) => state.reject(
            Failure::code(ErrorCode::UpstreamTimeout),
            KeyLabel::Invalid,
            ActionLabel::Unknown,
            &id,
        ),
    }
}

async fn receive(state: &ReceiverState, request: Request, id: &str) -> Response {
    let reject_boundary =
        |failure| state.reject(failure, KeyLabel::Invalid, ActionLabel::Unknown, id);
    if request.uri().path_and_query().map(|path| path.as_str()) != Some(ACTIONS_PATH) {
        return reject_boundary(Failure::http(
            StatusCode::NOT_FOUND,
            "not_found",
            false,
            ErrorCode::Malformed,
        ));
    }
    if request.method() != Method::POST {
        let mut response = reject_boundary(Failure::http(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            false,
            ErrorCode::Malformed,
        ));
        response
            .headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("POST"));
        return response;
    }
    let (parts, body) = request.into_parts();
    let headers = match wire_headers(&parts.headers) {
        Ok(headers) => headers,
        Err(failure) => return reject_boundary(failure),
    };
    let key = KeyLabel::new(
        headers.auth.key_id,
        state.config.keys().contains(headers.auth.key_id),
    );
    let reject = |failure, action| state.reject(failure, key.clone(), action, id);
    let raw = match tokio::time::timeout(BODY_TIMEOUT, to_bytes(body, MAX_BODY_BYTES)).await {
        Ok(Ok(raw)) => raw,
        Ok(Err(_)) => {
            return reject(
                Failure::http(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "malformed",
                    false,
                    ErrorCode::Malformed,
                ),
                ActionLabel::Unknown,
            )
        }
        Err(_) => {
            return reject(
                Failure::http(
                    StatusCode::REQUEST_TIMEOUT,
                    "request_timeout",
                    true,
                    ErrorCode::Malformed,
                ),
                ActionLabel::Unknown,
            )
        }
    };
    let verified = {
        let mut clock = state.clock.lock().expect("clock lock");
        AuthenticatedRequest::verify(
            &headers.auth,
            &raw,
            state.config.keys(),
            SKEW_SECONDS,
            now_ms(),
            &mut clock,
        )
    };
    let verified = match verified {
        Ok(verified) => verified,
        Err(error) => return reject(Failure::from_action(error), ActionLabel::Unknown),
    };
    let burned = match verified.burn_durably(&state.store).await {
        Ok(burned) => burned,
        Err(error) => return reject(Failure::from_action(error), ActionLabel::Unknown),
    };
    // The enabled set is intentionally the executor intersection, not runtime
    // flags that could accidentally advertise a different effect adapter.
    let flags = InternalFlags {
        enabled: ["announcement.post".to_owned()].into_iter().collect(),
        allow_automation_overwrite: false,
    };
    let decision = {
        let mut buckets = state.buckets.lock().expect("buckets lock");
        burned.authorize(&flags, true, false, &mut buckets)
    };
    let decision = match decision {
        Ok(decision) => decision,
        Err(error) => {
            let action = if error.code == ErrorCode::ActionNotAllowed {
                ActionLabel::from_body(&raw)
            } else {
                ActionLabel::Unknown
            };
            return reject(Failure::from_action(error), action);
        }
    };
    let action = ActionLabel::new(Some(&decision.action));
    // This second fence is explicit: core phase-1 defaults are not capabilities.
    if !AnnouncementExecutor::supports(&decision.action) {
        return reject(Failure::code(ErrorCode::ActionNotAllowed), action);
    }
    let idempotency = match validate_idempotency_key(headers.idempotency, &decision.action) {
        Ok(key) => key,
        Err(error) => return reject(Failure::from_action(error), action),
    };
    let channel = match validate_announcement(&decision.body, state.config.channel_keys()) {
        Ok(channel) => channel,
        Err(error) => return reject(Failure::from_action(error), action),
    };
    let subject = AuditSubject {
        guild_id: Some(
            DiscordId::new(two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID)
                .expect("staging guild ID"),
        ),
        target_id: Some(DiscordId::new(channel).expect("validated channel ID")),
        ..AuditSubject::default()
    };
    let Some(caller) = state.config.caller_for(&decision.key_id) else {
        return reject(Failure::code(ErrorCode::Internal), action);
    };
    let identity = match RequestIdentity::new(caller, idempotency, &decision.action, &raw) {
        Ok(identity) => identity,
        Err(_) => return reject(Failure::code(ErrorCode::Internal), action),
    };
    let claim = match state.store.claim(&identity, &subject).await {
        Ok(InternalClaim::Claimed(claim)) => claim,
        Ok(InternalClaim::Replay(response)) => {
            return state.terminal(response, true, id, key.clone(), action)
        }
        Ok(InternalClaim::Mismatch) => {
            return reject(Failure::code(ErrorCode::VersionConflict), action)
        }
        Ok(InternalClaim::InFlight) => return reject(Failure::code(ErrorCode::InProgress), action),
        Ok(InternalClaim::NeedsReconciliation) => return reject(Failure::reconciliation(), action),
        Err(_) => return reject(Failure::code(ErrorCode::Internal), action),
    };
    match state.effect.execute(&decision.body).await {
        Effect::Terminal(response) => {
            if state.store.finish(&claim, &response).await.is_err() {
                // Do not return success before its audit/receipt is committed.
                // An unavailable store leaves the existing claim occupied.
                let _ = state.store.mark_unknown(&claim).await;
                return reject(Failure::reconciliation(), action);
            }
            state.terminal(response, false, id, key.clone(), action)
        }
        Effect::Unknown => {
            let _ = state.store.mark_unknown(&claim).await;
            reject(Failure::reconciliation(), action)
        }
    }
}

struct WireHeaders<'a> {
    auth: AuthHeaders<'a>,
    idempotency: Option<&'a str>,
}

fn one_header<'a>(
    headers: &'a HeaderMap,
    name: &str,
    max: usize,
) -> Result<Option<&'a str>, Failure> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() || value.as_bytes().len() > max {
        return Err(Failure::code(ErrorCode::Malformed));
    }
    value
        .to_str()
        .map(Some)
        .map_err(|_| Failure::code(ErrorCode::Malformed))
}

fn wire_headers(headers: &HeaderMap) -> Result<WireHeaders<'_>, Failure> {
    if headers.len() > MAX_HEADERS
        || headers
            .iter()
            .map(|(name, value)| name.as_str().len() + value.as_bytes().len())
            .sum::<usize>()
            > MAX_HEADER_BYTES
    {
        return Err(Failure::http(
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            "malformed",
            false,
            ErrorCode::Malformed,
        ));
    }
    let media = one_header(headers, header::CONTENT_TYPE.as_str(), 128)?;
    if !matches!(media, Some(value) if value.eq_ignore_ascii_case("application/json")
        || value.eq_ignore_ascii_case("application/json; charset=utf-8"))
        || headers.contains_key(header::CONTENT_ENCODING)
    {
        return Err(Failure::http(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "malformed",
            false,
            ErrorCode::Malformed,
        ));
    }
    if let Some(length) = one_header(headers, header::CONTENT_LENGTH.as_str(), 20)? {
        match length.parse::<usize>() {
            Ok(length) if length <= MAX_BODY_BYTES => {}
            _ => {
                return Err(Failure::http(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "malformed",
                    false,
                    ErrorCode::Malformed,
                ))
            }
        }
    }
    let required = |name, max| {
        one_header(headers, name, max)?
            .filter(|v| !v.is_empty())
            .ok_or_else(|| Failure::code(ErrorCode::Unauthorized))
    };
    Ok(WireHeaders {
        auth: AuthHeaders {
            key_id: required("x-two-key-id", 128)?,
            timestamp: required("x-two-timestamp", 15)?,
            nonce: required("x-two-nonce", 32)?,
            signature: required("x-two-signature", 71)?,
        },
        idempotency: one_header(headers, "idempotency-key", 200)?,
    })
}

struct Failure {
    status: StatusCode,
    code: &'static str,
    retryable: bool,
    retry_after: Option<u64>,
    class_code: ErrorCode,
}

impl Failure {
    fn code(code: ErrorCode) -> Self {
        Self::http(
            StatusCode::from_u16(code.status()).expect("fixed HTTP status"),
            code.as_str(),
            code.retryable(),
            code,
        )
    }

    fn from_action(error: ActionError) -> Self {
        // Do not forward message/log_reason: validators may echo request data.
        let mut failure = Self::code(error.code);
        failure.retry_after = error.retry_after_secs;
        failure
    }

    fn http(
        status: StatusCode,
        code: &'static str,
        retryable: bool,
        class_code: ErrorCode,
    ) -> Self {
        Self {
            status,
            code,
            retryable,
            retry_after: None,
            class_code,
        }
    }

    fn reconciliation() -> Self {
        Self::http(
            StatusCode::CONFLICT,
            "needs_reconciliation",
            false,
            ErrorCode::InProgress,
        )
    }

    fn response(self, id: &str) -> Response {
        let message = if self.code == "unauthorized" {
            two_bot_core::internal_actions::AUTH_FAILURE_MESSAGE
        } else {
            "Internal action refused"
        };
        let mut response = (
            self.status,
            Json(json!({
                "ok": false,
                "error": {"code": self.code, "message": message, "retryable": self.retryable},
                "request_id": id,
            })),
        )
            .into_response();
        if let Some(seconds) = self.retry_after {
            if let Ok(header) = HeaderValue::from_str(&seconds.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, header);
            }
        }
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response
    }
}

fn terminal_response(response: TerminalResponse, replayed: bool, id: &str) -> Response {
    let mut wire = match response {
        TerminalResponse::Success {
            resource_id: Some(message_id),
            affected: 1,
        } => Json(json!({
            "ok": true, "result": {"message_id": message_id.as_str()}, "request_id": id,
        }))
        .into_response(),
        TerminalResponse::Success { .. } => Failure::reconciliation().response(id),
        TerminalResponse::Failure(failure) => match failure {
            TerminalFailure::Malformed => Failure::code(ErrorCode::Malformed),
            TerminalFailure::ActionNotAllowed => Failure::code(ErrorCode::ActionNotAllowed),
            TerminalFailure::DiscordRejected => Failure::code(ErrorCode::DiscordRejected),
            TerminalFailure::NoEffect => Failure::http(
                StatusCode::BAD_GATEWAY,
                "no_effect",
                false,
                ErrorCode::DiscordUnavailable,
            ),
        }
        .response(id),
    };
    if replayed {
        wire.headers_mut()
            .insert("idempotent-replay", HeaderValue::from_static("true"));
    }
    wire.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    wire
}

fn log_records(records: Vec<RejectionRecord>) {
    for record in records {
        tracing::warn!(
            kind = record.kind.as_str(),
            class = record.class.as_str(),
            key = record.key.as_str(),
            action = record.action.as_str(),
            count = record.count,
            suppressed = record.suppressed,
            "internal action refused"
        );
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            elapsed.as_millis().try_into().unwrap_or(u64::MAX)
        })
}

fn request_id() -> String {
    new_request_id(now_ms(), &rand::random())
}

#[cfg(test)]
mod tests;
