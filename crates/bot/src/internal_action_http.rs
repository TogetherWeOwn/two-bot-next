//! Private website-action receiver (announcement + membership). Never merge this
//! router into the health socket. Authentication and a committed nonce precede
//! JSON; a committed intent precedes the effect. Cancellation leaves durable
//! ownership, never a new execution lease.

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
    format_iso_millis,
    internal_action_config::InternalActionConfig,
    internal_action_store::{
        AuditSubject, DiscordId, InternalActionStore, InternalClaim, RequestIdentity,
        TerminalFailure, TerminalResponse,
    },
    internal_actions::{
        new_request_id, require_field_str, unmapped_event_key, validate_announcement,
        validate_event_key, validate_idempotency_key, ActionError, AuthDecision, AuthHeaders,
        AuthenticatedRequest, ErrorCode, GuildAddMemberRequest, InternalFlags, RoleAssignRequest,
        TokenBuckets, ACTIONS_PATH, MAX_BODY_BYTES, SKEW_SECONDS,
    },
    rejection_telemetry::{ActionLabel, KeyLabel, Rejection, RejectionRecord, RejectionTelemetry},
};
use two_bot_discord::executor::member::MemberOutcome;
use two_bot_discord::internal_actions::{AnnouncementExecutor, ExecutionOutcome, Refusal};
use two_bot_discord::{ActionExecutor, EventActionError, EventCall};

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

/// Membership mutations: `role.assign` (allowlisted key, hierarchy-checked)
/// and `guild.add_member` (OAuth token, transient only). The receiver owns the
/// durable claim; this trait owns only the Discord calls after the claim. The
/// OAuth token is a per-call argument only: it never enters a struct, an audit
/// row, a stored receipt, a log, or a `Debug` impl.
trait MemberEffect: Send + Sync {
    fn execute_assign<'a>(
        &'a self,
        guild_id: &'a str,
        bot_user_id: &'a str,
        request: &'a RoleAssignRequest<'a>,
    ) -> BoxFuture<'a, Result<MemberOutcome, ActionError>>;
    fn execute_add<'a>(
        &'a self,
        guild_id: &'a str,
        request: &'a GuildAddMemberRequest<'a>,
        access_token: &'a str,
    ) -> BoxFuture<'a, Result<MemberOutcome, ActionError>>;
    fn resolve_bot<'a>(&'a self) -> BoxFuture<'a, Result<String, ActionError>>;
}

impl MemberEffect for ActionExecutor {
    fn execute_assign<'a>(
        &'a self,
        guild_id: &'a str,
        bot_user_id: &'a str,
        request: &'a RoleAssignRequest<'a>,
    ) -> BoxFuture<'a, Result<MemberOutcome, ActionError>> {
        Box::pin(async move {
            self.assign_internal_role(guild_id, bot_user_id, request)
                .await
        })
    }

    fn execute_add<'a>(
        &'a self,
        guild_id: &'a str,
        request: &'a GuildAddMemberRequest<'a>,
        access_token: &'a str,
    ) -> BoxFuture<'a, Result<MemberOutcome, ActionError>> {
        Box::pin(async move {
            self.add_internal_member(guild_id, request, access_token)
                .await
        })
    }

    fn resolve_bot<'a>(&'a self) -> BoxFuture<'a, Result<String, ActionError>> {
        Box::pin(async move {
            self.current_bot_user_id()
                .await
                .map(|id| id.to_string())
                .map_err(|detail| {
                    ActionError::new(
                        ErrorCode::DiscordUnavailable,
                        "Discord bot identity is unavailable",
                        format!("bot_identity_unavailable: {}", detail.cause()),
                    )
                })
        })
    }
}

/// Keyless read-only effect: a resolved `event.read` performs one Discord GET
/// through [`ActionExecutor::execute_event`] and refreshes the Postgres mirror.
/// Reads carry no `Idempotency-Key` and take no durable idempotency claim; the
/// committed nonce is their replay guard.
trait EventReadEffect: Send + Sync {
    fn execute_read<'a>(
        &'a self,
        guild_id: &'a str,
        event_id: &'a str,
        observed_at: &'a str,
    ) -> BoxFuture<'a, Result<Value, EventActionError>>;
}

/// Production read effect: the shared event executor against the Postgres
/// mirror. The mirror write is part of the read (legacy refreshes it too); a
/// failed mirror write after a Discord success is `internal`, never a retry.
pub struct EventReadExecutor {
    executor: ActionExecutor,
    mirror: sqlx::PgPool,
}

impl EventReadExecutor {
    #[must_use]
    pub fn new(executor: ActionExecutor, mirror: sqlx::PgPool) -> Self {
        Self { executor, mirror }
    }
}

impl EventReadEffect for EventReadExecutor {
    fn execute_read<'a>(
        &'a self,
        guild_id: &'a str,
        event_id: &'a str,
        observed_at: &'a str,
    ) -> BoxFuture<'a, Result<Value, EventActionError>> {
        Box::pin(async move {
            self.executor
                .execute_event(
                    guild_id,
                    &EventCall::Read {
                        event_id: event_id.to_owned(),
                    },
                    &self.mirror,
                    observed_at,
                )
                .await
        })
    }
}

struct ReceiverState {
    config: InternalActionConfig,
    store: InternalActionStore,
    effect: Arc<dyn ActionEffect>,
    member: Arc<dyn MemberEffect>,
    event_read: Arc<dyn EventReadEffect>,
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
        member: Arc<dyn MemberEffect>,
        event_read: Arc<dyn EventReadEffect>,
    ) -> Self {
        Self {
            config,
            store: InternalActionStore::new(pool),
            effect,
            member,
            event_read,
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
        terminal_response(response, action, replayed, id)
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
    use two_bot_core::send_admission::{PgSendAdmission, SendAdmission};
    use two_bot_discord::internal_actions::CooldownGovernor;

    // One shared send-admission lane for both executors: every Discord send,
    // announcement or event read, holds the same token-wide lane. Bound as the
    // trait object so both executor constructors coerce without re-wrapping.
    let admission: Arc<dyn SendAdmission> =
        Arc::new(PgSendAdmission::new(pool.clone(), token).map_err(|_| {
            std::io::Error::other("internal-action admission configuration invalid")
        })?);
    let executor = AnnouncementExecutor::with_admission(
        Arc::new(twilight_http::Client::new(token.to_owned())),
        config.channel_keys().clone(),
        CooldownGovernor::new(),
        Arc::clone(&admission),
    )
    .map_err(|_| std::io::Error::other("internal-action executor configuration invalid"))?;
    let events =
        ActionExecutor::with_admission(token.to_owned(), None, admission).map_err(|_| {
            std::io::Error::other("internal-action event executor configuration invalid")
        })?;
    // Membership shares the same admitted transport as event reads: one
    // token-wide lane for every Discord send. Cloned before the read wrapper
    // takes ownership; role hierarchy and add-member PUTs hold the same lane.
    let member = events.clone();
    let listener = TcpListener::bind(config.listen_addr()).await?;
    Ok(BoundReceiver {
        listener,
        state: Arc::new(ReceiverState::new(
            config,
            pool.clone(),
            Arc::new(executor),
            Arc::new(member),
            Arc::new(EventReadExecutor::new(events, pool)),
        )),
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
    // The enabled set is the env-only flag gate, never the settings store: the
    // website must not be able to grant itself verbs. Verbs without a wired
    // effect adapter stay refused by the per-effect fences below.
    let flags = InternalFlags::from_env();
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
    // Read-only verbs are keyless: no Idempotency-Key header and no durable
    // idempotency claim. The committed nonce above is their replay guard.
    if decision.action == "event.read" {
        return read_event(state, &decision, id, key, action).await;
    }
    // Membership family (M3.10 fam5): validated role-key assignment and
    // OAuth-backed guild joins share the same nonce/bucket fences above. The
    // durable claim below is the same store the announcement path uses, so
    // audit (`intent`/`terminal`) and replay semantics match.
    if decision.action == "role.assign" || decision.action == "guild.add_member" {
        return execute_member(state, &decision, headers.idempotency, &raw, id, key, action).await;
    }
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

/// Keyless `event.read`: resolve the caller's `event_key` in the staging guild
/// through the Postgres mirror map, then run the single Discord GET. An
/// unmapped key, a disabled flag (refused earlier in `authorize`), a replayed
/// nonce and a forged signature all refuse before any Discord call — the only
/// wire effect below is the mapped GET itself.
async fn read_event(
    state: &ReceiverState,
    decision: &AuthDecision,
    id: &str,
    key: KeyLabel,
    action: ActionLabel,
) -> Response {
    let reject = |failure| state.reject(failure, key.clone(), action, id);
    let event_key = match validate_event_key(&decision.body) {
        Ok(key) => key.to_owned(),
        Err(error) => return reject(Failure::from_action(error)),
    };
    let guild_id = two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID;
    let event_id = match state.store.event_id_for_key(guild_id, &event_key).await {
        Ok(Some(event_id)) => event_id,
        // The key map is the whole address space: no mapping, no Discord read.
        Ok(None) => return reject(Failure::from_action(unmapped_event_key(&event_key))),
        Err(_) => return reject(Failure::code(ErrorCode::Internal)),
    };
    let observed_at = format_iso_millis(now_ms() as i64);
    match state
        .event_read
        .execute_read(guild_id, &event_id, &observed_at)
        .await
    {
        Ok(result) => event_read_response(result, id),
        Err(error) => reject(Failure::from_action(error.action_error())),
    }
}

/// Membership family: `role.assign` resolves a caller-supplied key through the
/// configured role map (never a raw snowflake), `guild.add_member` carries a
/// transient OAuth token that never enters audit rows, stored receipts, logs,
/// or `Debug`. Field validation runs before the durable claim, so malformed
/// keys and shapes refuse with no Discord call and no idempotency row. The
/// claim, terminal receipt (`Success{None,0/1}` matching the stored-member
/// executor), and replay mapping match the announcement path and the
/// `execute_stored_member` contract: `1` is the applied effect (`assigned` /
/// `added`), `0` the idempotent no-op (`already_held` / `already_member`).
/// Only a definitive Discord rejection finishes as `discord_rejected`; every
/// other post-claim failure (rate-limit, timeout, transport, identity read)
/// retains the claim as `unknown` (`needs_reconciliation`), never releasing or
/// retrying the mutation. This is deliberately conservative: local admission
/// refusals also retain rather than release, so a held lane never grants a
/// second dispatch under the same intent.
async fn execute_member(
    state: &ReceiverState,
    decision: &AuthDecision,
    idempotency_header: Option<&str>,
    raw: &[u8],
    id: &str,
    key: KeyLabel,
    action: ActionLabel,
) -> Response {
    let reject = |failure| state.reject(failure, key.clone(), action, id);
    let idempotency = match validate_idempotency_key(idempotency_header, &decision.action) {
        Ok(key) => key,
        Err(error) => return reject(Failure::from_action(error)),
    };
    let guild_id = two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID;
    // Validate before the claim: no Discord call, no idempotency row on bad input.
    // The `Failure` envelope drops validator text, so untrusted keys and the
    // OAuth token never reach the wire; `AuthDecision`'s `Debug` already hides
    // the body.
    enum Validated {
        Assign { discord_id: String, role_id: String },
        Add { discord_id: String },
    }
    let validated = match decision.action.as_str() {
        "role.assign" => {
            match RoleAssignRequest::validate(&decision.body, state.config.role_keys()) {
                Ok(request) => Validated::Assign {
                    discord_id: request.discord_id().to_owned(),
                    role_id: request.role_id().to_owned(),
                },
                Err(error) => return reject(Failure::from_action(error)),
            }
        }
        "guild.add_member" => match GuildAddMemberRequest::validate(&decision.body) {
            Ok(request) => {
                // Presence only; the value stays transient for the Discord call
                // below and is never stored or logged.
                if require_field_str(&decision.body, "access_token").is_err() {
                    return reject(Failure::code(ErrorCode::Internal));
                }
                Validated::Add {
                    discord_id: request.discord_id().to_owned(),
                }
            }
            Err(error) => return reject(Failure::from_action(error)),
        },
        _ => return reject(Failure::code(ErrorCode::ActionNotAllowed)),
    };
    let (target_id, resolved_role_id) = match &validated {
        Validated::Assign {
            discord_id,
            role_id,
        } => (discord_id.as_str(), Some(role_id.as_str())),
        Validated::Add { discord_id } => (discord_id.as_str(), None),
    };
    let subject = {
        let guild = match DiscordId::new(guild_id) {
            Ok(id) => id,
            Err(_) => return reject(Failure::code(ErrorCode::Internal)),
        };
        let target = match DiscordId::new(target_id) {
            Ok(id) => id,
            Err(_) => return reject(Failure::code(ErrorCode::Internal)),
        };
        let role = match resolved_role_id {
            Some(role) => match DiscordId::new(role) {
                Ok(id) => Some(id),
                Err(_) => return reject(Failure::code(ErrorCode::Internal)),
            },
            None => None,
        };
        AuditSubject {
            guild_id: Some(guild),
            target_id: Some(target),
            actor_id: None,
            resolved_role_id: role,
        }
    };
    let Some(caller) = state.config.caller_for(&decision.key_id) else {
        return reject(Failure::code(ErrorCode::Internal));
    };
    let identity = match RequestIdentity::new(caller, idempotency, &decision.action, raw) {
        Ok(identity) => identity,
        Err(_) => return reject(Failure::code(ErrorCode::Internal)),
    };
    let claim = match state.store.claim(&identity, &subject).await {
        Ok(InternalClaim::Claimed(claim)) => claim,
        Ok(InternalClaim::Replay(response)) => {
            return state.terminal(response, true, id, key.clone(), action);
        }
        Ok(InternalClaim::Mismatch) => {
            return reject(Failure::code(ErrorCode::VersionConflict));
        }
        Ok(InternalClaim::InFlight) => return reject(Failure::code(ErrorCode::InProgress)),
        Ok(InternalClaim::NeedsReconciliation) => return reject(Failure::reconciliation()),
        Err(_) => return reject(Failure::code(ErrorCode::Internal)),
    };
    // The claim is committed: every Discord call below runs after it, including
    // the bot-identity read for the hierarchy check.
    let outcome = match validated {
        Validated::Assign { .. } => {
            let bot_user_id = match state.member.resolve_bot().await {
                Ok(id) => id,
                Err(_) => {
                    let _ = state.store.mark_unknown(&claim).await;
                    return reject(Failure::reconciliation());
                }
            };
            // Revalidate against the same map the pre-claim check used, so the
            // request reaching Discord is the pinned allowlist entry, not a
            // retargeted mid-request edit.
            let request =
                match RoleAssignRequest::validate(&decision.body, state.config.role_keys()) {
                    Ok(request) => request,
                    Err(_) => {
                        let _ = state
                            .store
                            .finish(
                                &claim,
                                &TerminalResponse::Failure(TerminalFailure::Malformed),
                            )
                            .await;
                        return state.terminal(
                            TerminalResponse::Failure(TerminalFailure::Malformed),
                            false,
                            id,
                            key.clone(),
                            action,
                        );
                    }
                };
            state
                .member
                .execute_assign(guild_id, &bot_user_id, &request)
                .await
        }
        Validated::Add { .. } => {
            let request = match GuildAddMemberRequest::validate(&decision.body) {
                Ok(request) => request,
                Err(_) => {
                    let _ = state
                        .store
                        .finish(
                            &claim,
                            &TerminalResponse::Failure(TerminalFailure::Malformed),
                        )
                        .await;
                    return state.terminal(
                        TerminalResponse::Failure(TerminalFailure::Malformed),
                        false,
                        id,
                        key.clone(),
                        action,
                    );
                }
            };
            // Transient only: cloned for the single Discord PUT, never stored.
            let token = match require_field_str(&decision.body, "access_token") {
                Ok(token) => token.to_owned(),
                Err(_) => {
                    let _ = state
                        .store
                        .finish(
                            &claim,
                            &TerminalResponse::Failure(TerminalFailure::Malformed),
                        )
                        .await;
                    return state.terminal(
                        TerminalResponse::Failure(TerminalFailure::Malformed),
                        false,
                        id,
                        key.clone(),
                        action,
                    );
                }
            };
            let result = state.member.execute_add(guild_id, &request, &token).await;
            // Drop the secret at once; the `String` lives only for this call.
            drop(token);
            result
        }
    };
    match outcome {
        Ok(member_outcome) => {
            let affected = u32::from(matches!(
                member_outcome,
                MemberOutcome::Added | MemberOutcome::Assigned
            ));
            let response = TerminalResponse::Success {
                resource_id: None,
                affected,
            };
            if state.store.finish(&claim, &response).await.is_err() {
                let _ = state.store.mark_unknown(&claim).await;
                return reject(Failure::reconciliation());
            }
            state.terminal(response, false, id, key.clone(), action)
        }
        Err(error) if error.code == ErrorCode::DiscordRejected => {
            let response = TerminalResponse::Failure(TerminalFailure::DiscordRejected);
            if state.store.finish(&claim, &response).await.is_err() {
                let _ = state.store.mark_unknown(&claim).await;
                return reject(Failure::reconciliation());
            }
            state.terminal(response, false, id, key.clone(), action)
        }
        Err(_) => {
            let _ = state.store.mark_unknown(&claim).await;
            reject(Failure::reconciliation())
        }
    }
}

/// The 7-field read result (`outcome`, `event_id`, `name`, `starts_at`,
/// `location`, `status`, `observed_at`) is the response, not a stored
/// idempotency receipt: reads take no claim, so there is nothing to replay.
fn event_read_response(result: Value, id: &str) -> Response {
    let mut wire = (
        StatusCode::OK,
        Json(json!({"ok": true, "result": result, "request_id": id})),
    )
        .into_response();
    wire.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    wire
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

fn terminal_response(
    response: TerminalResponse,
    action: ActionLabel,
    replayed: bool,
    id: &str,
) -> Response {
    let mut wire = match response {
        TerminalResponse::Success {
            resource_id: Some(message_id),
            affected: 1,
        } => Json(json!({
            "ok": true, "result": {"message_id": message_id.as_str()}, "request_id": id,
        }))
        .into_response(),
        // Membership receipts: `None` + `0/1` is the stored-member contract.
        // `1` applied the effect, `0` is the idempotent no-op. Anything else
        // (including a message-shaped receipt for a membership action) is a
        // store inconsistency, never a success.
        TerminalResponse::Success {
            resource_id: None,
            affected,
        } => match (action.as_str(), affected) {
            ("role.assign", 1) => member_success("assigned", id),
            ("role.assign", 0) => member_success("already_held", id),
            ("guild.add_member", 1) => member_success("added", id),
            ("guild.add_member", 0) => member_success("already_member", id),
            _ => Failure::reconciliation().response(id),
        },
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

/// Membership success envelope: the legacy `{"outcome": ...}` result object.
/// Insertion order (`ok`, `result`, `request_id`) matches the stored-member
/// `success_body` wire contract.
fn member_success(outcome: &str, id: &str) -> Response {
    Json(json!({
        "ok": true, "result": {"outcome": outcome}, "request_id": id,
    }))
    .into_response()
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
