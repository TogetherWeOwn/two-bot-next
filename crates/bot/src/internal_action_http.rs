//! Private internal-action receiver. Never merge this router into the health socket.
//! Authentication and a committed nonce precede JSON; a committed intent precedes
//! the effect. Cancellation leaves durable ownership, never a new execution lease.
//!
//! Wired effects: `announcement.post` (single-attempt send), `event.read`
//! (keyless mapped GET), `event.upsert`/`event.cancel` (Guild Scheduled Event
//! mutations through the shared event executor) and `moderation.timeout`
//! (member timeout through the shared moderation service). Every other verb
//! stays refused by the per-effect fences below, even when the env-only flag
//! gate authorizes it.

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
    commands::PERM_MODERATE_MEMBERS,
    format_iso_millis,
    internal_action_config::InternalActionConfig,
    internal_action_store::{
        AuditSubject, DiscordId, EventOutcome, InternalActionStore, InternalClaim, RequestIdentity,
        TerminalFailure, TerminalResponse,
    },
    internal_actions::{
        new_request_id, unmapped_event_key, validate_announcement, validate_event_input,
        validate_event_key, validate_idempotency_key, ActionError, AuthDecision, AuthHeaders,
        AuthenticatedRequest, ErrorCode, InternalFlags, TokenBuckets, ACTIONS_PATH, MAX_BODY_BYTES,
        SKEW_SECONDS,
    },
    member_moderation_store::PgMemberModerationStore,
    rejection_telemetry::{ActionLabel, KeyLabel, Rejection, RejectionRecord, RejectionTelemetry},
    ModerationAction, ModerationActor, ModerationGates, ModerationPolicy, ModerationTarget,
};
use two_bot_discord::internal_actions::{
    supports_event_mutation, AnnouncementExecutor, ExecutionOutcome, Refusal,
};
use two_bot_discord::internal_member_moderation::{
    InternalMemberConfig, InternalMemberExecutor, InternalMemberRequest,
};
use two_bot_discord::{ActionExecutor, DiscordError, EventActionError, EventCall};

const MAX_HEADER_BYTES: usize = 8192;
const MAX_HEADERS: usize = 64;
const MAX_REQUESTS: usize = 32;
const BODY_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Queued event work waits at most this long, then is refused `in_progress`
/// before any claim.
const EVENT_GATE_WAIT: Duration = Duration::from_secs(8);

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
                        outcome: None,
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

/// Member-timeout effect: `moderation.timeout` through the shared moderation
/// service. The test seam is module-private like [`ActionEffect`]: the
/// production effect resolves actor/target from the configured staging guild
/// using live member roles, positions, permissions and bot/owner flags — never
/// from body-supplied roles or permissions. Mocks skip Discord and return a
/// canned receipt, so HTTP-layer allow/deny/key-reuse tests stay offline.
trait ModerationTimeoutEffect: Send + Sync {
    fn execute_timeout<'a>(
        &'a self,
        request: &'a InternalMemberRequest,
        request_id: &'a str,
        idempotency_key: &'a str,
        now_ms: i64,
    ) -> BoxFuture<'a, Result<TerminalResponse, ActionError>>;
}

/// Production timeout effect over the shared [`InternalMemberExecutor`].
/// Resolution (guild facts, actor/target/bot snapshots) runs inside the
/// effect so the HTTP handler stays a thin validate-claim-finish fence.
/// Definitive service failures become terminal receipts; transport, rate-limit,
/// store and in-flight uncertainty stays [`Effect::Unknown`] so the outer
/// idempotency claim is retained for reconciliation, never re-executed.
struct ModerationTimeoutExecutor {
    inner: InternalMemberExecutor<PgMemberModerationStore>,
    discord: ActionExecutor,
}

impl ModerationTimeoutExecutor {
    fn new(
        inner: InternalMemberExecutor<PgMemberModerationStore>,
        discord: ActionExecutor,
    ) -> Self {
        Self { inner, discord }
    }
}

impl ModerationTimeoutEffect for ModerationTimeoutExecutor {
    fn execute_timeout<'a>(
        &'a self,
        request: &'a InternalMemberRequest,
        request_id: &'a str,
        idempotency_key: &'a str,
        now_ms: i64,
    ) -> BoxFuture<'a, Result<TerminalResponse, ActionError>> {
        Box::pin(async move {
            let guild_id = two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID;
            let facts = timeout_guild_facts(&self.discord, guild_id).await?;
            let bot_id = self
                .discord
                .current_bot_user_id()
                .await
                .map(|id| id.to_string())
                .map_err(|_| {
                    ActionError::new(
                        ErrorCode::DiscordUnavailable,
                        "Discord member snapshot unavailable; do not retry this key",
                        "moderation_snapshot_unavailable",
                    )
                })?;
            let (bot_roles, _) = timeout_fetched_member(&self.discord, guild_id, &bot_id)
                .await?
                .ok_or_else(|| {
                    ActionError::new(
                        ErrorCode::DiscordUnavailable,
                        "Discord member snapshot unavailable; do not retry this key",
                        "moderation_snapshot_unavailable",
                    )
                })?;
            let bot_with_everyone = timeout_with_everyone(bot_roles, guild_id);
            let bot_position =
                timeout_top_position(&bot_with_everyone, &facts).ok_or_else(|| {
                    ActionError::new(
                        ErrorCode::DiscordUnavailable,
                        "Discord member snapshot unavailable; do not retry this key",
                        "moderation_snapshot_unavailable",
                    )
                })?;
            let (actor, target, bot_position) =
                timeout_resolve(&self.discord, request, &facts, bot_position).await?;
            match self
                .inner
                .execute(
                    request,
                    &actor,
                    &target,
                    bot_position,
                    request_id,
                    idempotency_key,
                    now_ms,
                )
                .await
            {
                Ok(result) => {
                    let target_id = DiscordId::new(request.target_id()).map_err(|_| {
                        ActionError::new(
                            ErrorCode::Internal,
                            "Internal action storage unavailable",
                            "moderation_receipt_invalid",
                        )
                    })?;
                    let _ = result.outcome;
                    Ok(TerminalResponse::Success {
                        resource_id: Some(target_id),
                        affected: 1,
                    })
                }
                Err(error) => match error.code {
                    ErrorCode::Malformed
                    | ErrorCode::ActionNotAllowed
                    | ErrorCode::DiscordRejected => {
                        let failure = match error.code {
                            ErrorCode::Malformed => TerminalFailure::Malformed,
                            ErrorCode::ActionNotAllowed => TerminalFailure::ActionNotAllowed,
                            _ => TerminalFailure::DiscordRejected,
                        };
                        Ok(TerminalResponse::Failure(failure))
                    }
                    _ => Err(error),
                },
            }
        })
    }
}

/// Discord `ADMINISTRATOR` bit (1 << 3): holders pass every permission check.
const ADMINISTRATOR_BIT: u64 = 1 << 3;

/// Live guild snapshot for hierarchy and permission resolution. Read per
/// website timeout: hierarchy never runs on a partial or cached snapshot.
struct TimeoutGuildFacts {
    guild_id: String,
    owner_id: String,
    positions: std::collections::HashMap<String, i64>,
    permissions: std::collections::HashMap<String, u64>,
}

fn timeout_text(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_owned)
}

async fn timeout_guild_facts(
    executor: &ActionExecutor,
    guild_id: &str,
) -> Result<TimeoutGuildFacts, ActionError> {
    let refuse = || {
        ActionError::new(
            ErrorCode::DiscordUnavailable,
            "Discord member snapshot unavailable; do not retry this key",
            "moderation_snapshot_unavailable",
        )
    };
    let guild = executor
        .get_json_strict(&format!("/guilds/{guild_id}"))
        .await
        .map_err(|_| refuse())?
        .ok_or_else(refuse)?;
    let owner_id = timeout_text(&guild, "owner_id").ok_or_else(refuse)?;
    let mut positions = std::collections::HashMap::new();
    let mut permissions = std::collections::HashMap::new();
    for role in guild
        .get("roles")
        .and_then(Value::as_array)
        .ok_or_else(refuse)?
    {
        let id = timeout_text(role, "id").ok_or_else(refuse)?;
        let position = role
            .get("position")
            .and_then(Value::as_i64)
            .ok_or_else(refuse)?;
        let perm = role
            .get("permissions")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(refuse)?;
        positions.insert(id.clone(), position);
        permissions.insert(id, perm);
    }
    if !positions.contains_key(guild_id) {
        return Err(refuse());
    }
    Ok(TimeoutGuildFacts {
        guild_id: guild_id.to_owned(),
        owner_id,
        positions,
        permissions,
    })
}

fn timeout_top_position(held: &[String], facts: &TimeoutGuildFacts) -> Option<i64> {
    let mut top = *facts.positions.get(&facts.guild_id)?;
    for role in held {
        top = top.max(*facts.positions.get(role)?);
    }
    Some(top)
}

fn timeout_with_everyone(mut roles: Vec<String>, guild_id: &str) -> Vec<String> {
    if !roles.iter().any(|r| r == guild_id) {
        roles.push(guild_id.to_owned());
    }
    roles
}

/// Guild-level permission union for the held roles plus `@everyone`.
/// Unknown held roles fail closed; the snapshot is incomplete, never
/// unprotected. The guild owner and `ADMINISTRATOR` holders pass everything.
fn timeout_permissions(held: &[String], facts: &TimeoutGuildFacts, user_id: &str) -> Option<u64> {
    if user_id == facts.owner_id {
        return Some(u64::MAX);
    }
    let mut bits = *facts.permissions.get(&facts.guild_id)?;
    for role in held {
        bits |= *facts.permissions.get(role)?;
    }
    if bits & ADMINISTRATOR_BIT != 0 {
        return Some(u64::MAX);
    }
    Some(bits)
}

/// Membership plus bot flag from `GET /guilds/{g}/members/{u}`. `None` is a
/// departed or never-joined user: timeout refuses (ban tolerates). An id
/// mismatch or malformed body fails closed into fenced uncertainty.
async fn timeout_fetched_member(
    executor: &ActionExecutor,
    guild_id: &str,
    user_id: &str,
) -> Result<Option<(Vec<String>, bool)>, ActionError> {
    let refuse = || {
        ActionError::new(
            ErrorCode::DiscordUnavailable,
            "Discord member snapshot unavailable; do not retry this key",
            "moderation_snapshot_unavailable",
        )
    };
    let member = executor
        .get_json_strict(&format!("/guilds/{guild_id}/members/{user_id}"))
        .await
        .map_err(|_| refuse())?;
    let Some(member) = member else {
        return Ok(None);
    };
    let user = member.get("user").ok_or_else(refuse)?;
    if timeout_text(user, "id").as_deref() != Some(user_id) {
        return Err(refuse());
    }
    let mut roles = Vec::new();
    for role in member
        .get("roles")
        .and_then(Value::as_array)
        .ok_or_else(refuse)?
    {
        roles.push(role.as_str().ok_or_else(refuse)?.to_owned());
    }
    let is_bot = user.get("bot").and_then(Value::as_bool).unwrap_or(false);
    Ok(Some((roles, is_bot)))
}

#[allow(clippy::too_many_arguments)]
async fn timeout_resolve(
    executor: &ActionExecutor,
    request: &InternalMemberRequest,
    facts: &TimeoutGuildFacts,
    bot_highest_role_position: i64,
) -> Result<(ModerationActor, ModerationTarget, i64), ActionError> {
    let guild_id = &facts.guild_id;
    let refuse_unavailable = || {
        ActionError::new(
            ErrorCode::DiscordUnavailable,
            "Discord member snapshot unavailable; do not retry this key",
            "moderation_snapshot_unavailable",
        )
    };
    let (actor_roles, _) = timeout_fetched_member(executor, guild_id, request.actor_id())
        .await?
        .ok_or_else(|| {
            ActionError::new(
                ErrorCode::ActionNotAllowed,
                "moderation actor is not a guild member",
                "moderation_actor_not_member",
            )
        })?;
    let actor_with_everyone = timeout_with_everyone(actor_roles, guild_id);
    let actor = ModerationActor {
        user_id: request.actor_id().to_owned(),
        highest_role_position: timeout_top_position(&actor_with_everyone, facts)
            .ok_or_else(refuse_unavailable)?,
        role_ids: actor_with_everyone.clone(),
        permissions: timeout_permissions(&actor_with_everyone, facts, request.actor_id())
            .ok_or_else(refuse_unavailable)?,
    };
    if actor.permissions & PERM_MODERATE_MEMBERS == 0 && actor.user_id != facts.owner_id {
        return Err(ActionError::new(
            ErrorCode::ActionNotAllowed,
            "moderation actor lacks timeout permission",
            "moderation_actor_forbidden",
        ));
    }
    let (target_roles, is_bot) = timeout_fetched_member(executor, guild_id, request.target_id())
        .await?
        .ok_or_else(|| {
            ActionError::new(
                ErrorCode::ActionNotAllowed,
                "moderation target is not a guild member",
                "moderation_target_not_member",
            )
        })?;
    let target_with_everyone = timeout_with_everyone(target_roles, guild_id);
    let target = ModerationTarget {
        user_id: request.target_id().to_owned(),
        highest_role_position: timeout_top_position(&target_with_everyone, facts)
            .ok_or_else(refuse_unavailable)?,
        role_ids: target_with_everyone,
        is_bot,
        is_guild_owner: request.target_id() == facts.owner_id,
    };
    if request.action() != ModerationAction::Timeout {
        return Err(ActionError::new(
            ErrorCode::ActionNotAllowed,
            "not a timeout action",
            "moderation_action_mismatch",
        ));
    }
    Ok((actor, target, bot_highest_role_position))
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

/// Mutating event effect: the receiver validates the body, enforces the
/// env-only flag gate in `authorize`, commits the idempotency claim and
/// resolves `event_key` to a trusted Discord id before invoking this. The call
/// itself carries no website fields, only the trusted mapping result. Mirror
/// writes are part of the mutation: a failed write after a Discord success is
/// an unknown outcome, never a second attempt.
trait EventMutateEffect: Send + Sync {
    fn execute_mutation<'a>(
        &'a self,
        guild_id: &'a str,
        call: &'a EventCall,
        observed_at: &'a str,
    ) -> BoxFuture<'a, Result<Value, EventActionError>>;
}

/// Production mutation effect: the shared event executor against the Postgres
/// mirror. Shares the token-wide send-admission lane with the read path.
pub struct EventMutationExecutor {
    executor: ActionExecutor,
    mirror: sqlx::PgPool,
}

impl EventMutationExecutor {
    #[must_use]
    pub fn new(executor: ActionExecutor, mirror: sqlx::PgPool) -> Self {
        Self { executor, mirror }
    }
}

impl EventMutateEffect for EventMutationExecutor {
    fn execute_mutation<'a>(
        &'a self,
        guild_id: &'a str,
        call: &'a EventCall,
        observed_at: &'a str,
    ) -> BoxFuture<'a, Result<Value, EventActionError>> {
        Box::pin(async move {
            self.executor
                .execute_event(guild_id, call, &self.mirror, observed_at)
                .await
        })
    }
}

struct ReceiverState {
    config: InternalActionConfig,
    store: InternalActionStore,
    effect: Arc<dyn ActionEffect>,
    event_read: Arc<dyn EventReadEffect>,
    event_mutate: Arc<dyn EventMutateEffect>,
    moderation: Arc<dyn ModerationTimeoutEffect>,
    clock: Mutex<ClockGuard>,
    buckets: Mutex<TokenBuckets>,
    telemetry: Mutex<RejectionTelemetry>,
    capacity: Arc<Semaphore>,
    event_gate: tokio::sync::Mutex<()>,
    event_gate_wait: Duration,
}

impl ReceiverState {
    fn new(
        config: InternalActionConfig,
        pool: sqlx::PgPool,
        effect: Arc<dyn ActionEffect>,
        event_read: Arc<dyn EventReadEffect>,
        event_mutate: Arc<dyn EventMutateEffect>,
        moderation: Arc<dyn ModerationTimeoutEffect>,
    ) -> Self {
        Self {
            config,
            store: InternalActionStore::new(pool),
            effect,
            event_read,
            event_mutate,
            moderation,
            clock: Mutex::new(ClockGuard::new()),
            buckets: Mutex::new(TokenBuckets::new()),
            telemetry: Mutex::new(RejectionTelemetry::default()),
            capacity: Arc::new(Semaphore::new(MAX_REQUESTS)),
            event_gate: tokio::sync::Mutex::new(()),
            event_gate_wait: EVENT_GATE_WAIT,
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

/// Build the production timeout executor from the process environment.
/// `enabled` combines BOTH `TWO_MODERATION` and `TWO_INTERNAL_ALLOW_MODERATION`
/// (see [`InternalFlags`]): the website must never grant itself verbs through
/// the settings store. Invalid enabled moderation configuration is fatal, like
/// the receiver bind itself; a disabled executor still constructs and refuses
/// every timeout with `action_not_allowed`, tolerating invalid moderation
/// gates so a bad `TWO_MODERATION_PROTECTED_ROLE_IDS` (or a stray
/// `TWO_MODERATION=1` without `TWO_OWEN_USER_ID`) can never stop the receiver
/// from binding while the timeout verb is off.
fn moderation_executor_from_env(
    pool: sqlx::PgPool,
    discord: ActionExecutor,
) -> Result<ModerationTimeoutExecutor, String> {
    use two_bot_core::mac::moderation_audit_secret;

    let guild_id = two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID.to_owned();
    let vars: std::collections::HashMap<String, String> = std::env::vars().collect();
    let flags = InternalFlags::from_map(&vars);
    let enabled = flags.is_enabled("moderation.timeout");
    let policy = if enabled {
        let gates = ModerationGates::from_map(&vars).map_err(|e| e.to_string())?;
        if !gates.enabled {
            return Err("moderation timeout flag without TWO_MODERATION".to_owned());
        }
        ModerationPolicy {
            owen_user_id: gates.owen_user_id,
            protected_role_ids: gates.protected_role_ids,
            bot_user_id: None,
        }
    } else {
        // Disabled: refuse everything without trusting the gates. Invalid
        // moderation settings must not fail the receiver bind while the verb
        // is off; fall back to an empty policy the disabled executor never
        // consults (see `InternalMemberExecutor::execute`).
        let (owen_user_id, protected_role_ids) = match ModerationGates::from_map(&vars) {
            Ok(gates) => (gates.owen_user_id, gates.protected_role_ids),
            Err(_) => (String::new(), std::collections::HashSet::new()),
        };
        ModerationPolicy {
            owen_user_id,
            protected_role_ids,
            bot_user_id: None,
        }
    };
    let audit_secret = moderation_audit_secret(&vars, None)
        .map_err(|e| e.to_string())?
        .map(|s| s.expose().to_owned());
    let store = PgMemberModerationStore::new(pool, guild_id.clone());
    let inner = InternalMemberExecutor::new(
        store,
        discord.clone(),
        InternalMemberConfig {
            guild_id,
            enabled,
            policy,
            audit_secret,
        },
    )
    .map_err(|e| e.to_string())?;
    Ok(ModerationTimeoutExecutor::new(inner, discord))
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

    // One shared send-admission lane for every executor: announcement sends,
    // event reads and moderation timeouts all hold the same token-wide lane.
    // Bound as the trait object so every executor constructor coerces without
    // re-wrapping.
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
    let events = ActionExecutor::with_admission(token.to_owned(), None, Arc::clone(&admission))
        .map_err(|_| {
            std::io::Error::other("internal-action event executor configuration invalid")
        })?;
    let moderation_discord = ActionExecutor::with_admission(token.to_owned(), None, admission)
        .map_err(|_| {
            std::io::Error::other("internal-action moderation executor configuration invalid")
        })?;
    let moderation =
        moderation_executor_from_env(pool.clone(), moderation_discord).map_err(|_| {
            std::io::Error::other("internal-action moderation executor configuration invalid")
        })?;
    let listener = TcpListener::bind(config.listen_addr()).await?;
    Ok(BoundReceiver {
        listener,
        state: Arc::new(ReceiverState::new(
            config,
            pool.clone(),
            Arc::new(executor),
            Arc::new(EventReadExecutor::new(events.clone(), pool.clone())),
            Arc::new(EventMutationExecutor::new(events, pool)),
            Arc::new(moderation),
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
        // Dropping the future retains an announcement claim (a fresh nonce sees
        // InFlight or NeedsReconciliation); event mutations finish in their own task.
        Err(_) => state.reject(
            Failure::code(ErrorCode::UpstreamTimeout),
            KeyLabel::Invalid,
            ActionLabel::Unknown,
            &id,
        ),
    }
}

async fn receive(state: &Arc<ReceiverState>, request: Request, id: &str) -> Response {
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
    // Mutating event verbs take the same durable claim/audit path as
    // announcements below, with their own validation and key mapping.
    if supports_event_mutation(&decision.action) {
        // Runs to its receipt even if the client has gone: a claimed create that
        // was dropped mid-flight must still register its key mapping.
        let task_state = Arc::clone(state);
        let task_decision = decision.clone();
        let task_raw = raw.to_vec();
        let task_idempotency = headers.idempotency.map(str::to_owned);
        let task_id = id.to_owned();
        let task_key = key.clone();
        let task = tokio::spawn(async move {
            mutate_event(
                &task_state,
                &task_decision,
                &task_raw,
                task_idempotency.as_deref(),
                &task_id,
                task_key,
                action,
            )
            .await
        });
        return match task.await {
            Ok(response) => response,
            Err(_) => state.reject(Failure::reconciliation(), key, action, id),
        };
    }
    // Family 1 (M3.10): the only wired moderation verb. Every other verb
    // without an adapter stays refused below, even when the flag gate
    // authorizes it — shipping an implementation must never widen the
    // allowlist by itself.
    if decision.action == "moderation.timeout" {
        return timeout_member(state, &decision, &raw, headers.idempotency, id, key, action).await;
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
    // Shares the mutation gate so a read's mirror write cannot overtake a cancel's.
    let Ok(_gate) = tokio::time::timeout(state.event_gate_wait, state.event_gate.lock()).await
    else {
        return reject(Failure::code(ErrorCode::InProgress));
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

/// Website `moderation.timeout`: idempotency-key validation and durable claim
/// match the announcement path. The body parses via
/// [`InternalMemberRequest::from_body`] (actor/target snowflakes, trimmed
/// reason, `duration_seconds` 60–28d); actor and target resolve from the
/// configured staging guild using live member roles, positions, permissions
/// and bot/owner flags — never from body-supplied roles. The outer
/// [`InternalActionStore`] claim guards the exact signed bytes (replay,
/// mismatch, in-flight); the inner [`InternalMemberExecutor`] guards the
/// moderation content and writes the shared `moderation_audit` ledger, so rows
/// are identical to the slash-command path. Only `moderation.timeout` reaches
/// here; every other verb stays refused by the fences in [`receive`].
#[allow(clippy::too_many_arguments)]
async fn timeout_member(
    state: &ReceiverState,
    decision: &AuthDecision,
    raw: &[u8],
    idempotency_header: Option<&str>,
    id: &str,
    key: KeyLabel,
    action: ActionLabel,
) -> Response {
    let reject = |failure| state.reject(failure, key.clone(), action, id);
    let idempotency = match validate_idempotency_key(idempotency_header, &decision.action) {
        Ok(key) => key.to_owned(),
        Err(error) => return reject(Failure::from_action(error)),
    };
    let request = match InternalMemberRequest::from_body("moderation.timeout", &decision.body) {
        Ok(request) => request,
        Err(error) => return reject(Failure::from_action(error)),
    };
    if request.action() != ModerationAction::Timeout {
        return reject(Failure::code(ErrorCode::ActionNotAllowed));
    }
    let subject = match (
        DiscordId::new(two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID),
        DiscordId::new(request.actor_id()),
        DiscordId::new(request.target_id()),
    ) {
        (Ok(guild_id), Ok(actor_id), Ok(target_id)) => AuditSubject {
            guild_id: Some(guild_id),
            actor_id: Some(actor_id),
            target_id: Some(target_id),
            ..AuditSubject::default()
        },
        _ => return reject(Failure::code(ErrorCode::Internal)),
    };
    let Some(caller) = state.config.caller_for(&decision.key_id) else {
        return reject(Failure::code(ErrorCode::Internal));
    };
    let caller = caller.to_owned();
    let identity = match RequestIdentity::new(&caller, &idempotency, &decision.action, raw) {
        Ok(identity) => identity,
        Err(_) => return reject(Failure::code(ErrorCode::Internal)),
    };
    let claim = match state.store.claim(&identity, &subject).await {
        Ok(InternalClaim::Claimed(claim)) => claim,
        Ok(InternalClaim::Replay(response)) => {
            return moderation_timeout_terminal(state, response, true, id, key.clone(), action);
        }
        Ok(InternalClaim::Mismatch) => {
            return reject(Failure::code(ErrorCode::VersionConflict));
        }
        Ok(InternalClaim::InFlight) => {
            return reject(Failure::code(ErrorCode::InProgress));
        }
        Ok(InternalClaim::NeedsReconciliation) => {
            return reject(Failure::reconciliation());
        }
        Err(_) => return reject(Failure::code(ErrorCode::Internal)),
    };
    // Resolution (guild facts, actor/target/bot snapshots) and the shared
    // moderation service run inside the effect. Definitive refusals become
    // terminal receipts; snapshot/transport uncertainty stays fenced.
    let now = now_ms() as i64;
    match state
        .moderation
        .execute_timeout(&request, id, &idempotency, now)
        .await
    {
        Ok(response) => {
            if state.store.finish(&claim, &response).await.is_err() {
                let _ = state.store.mark_unknown(&claim).await;
                return reject(Failure::reconciliation());
            }
            moderation_timeout_terminal(state, response, false, id, key.clone(), action)
        }
        Err(error)
            if matches!(
                error.code,
                ErrorCode::Malformed | ErrorCode::ActionNotAllowed | ErrorCode::DiscordRejected
            ) =>
        {
            let failure = match error.code {
                ErrorCode::Malformed => TerminalFailure::Malformed,
                ErrorCode::ActionNotAllowed => TerminalFailure::ActionNotAllowed,
                _ => TerminalFailure::DiscordRejected,
            };
            let response = TerminalResponse::Failure(failure);
            if state.store.finish(&claim, &response).await.is_err() {
                let _ = state.store.mark_unknown(&claim).await;
                return reject(Failure::reconciliation());
            }
            moderation_timeout_terminal(state, response, false, id, key.clone(), action)
        }
        Err(_) => {
            let _ = state.store.mark_unknown(&claim).await;
            reject(Failure::reconciliation())
        }
    }
}

/// Render a timeout terminal receipt. Success carries the timed-out target as
/// the stored `resource_id` (audit) and reports the `timed_out` outcome on the
/// wire; failures share the announcement failure codes. Replays set the same
/// `idempotent-replay` header as the announcement path.
fn moderation_timeout_terminal(
    state: &ReceiverState,
    response: TerminalResponse,
    replayed: bool,
    id: &str,
    key: KeyLabel,
    action: ActionLabel,
) -> Response {
    match response {
        TerminalResponse::Success { .. } => {
            let mut wire = (
                StatusCode::OK,
                Json(json!({"ok": true, "result": {"outcome": "timed_out"}, "request_id": id})),
            )
                .into_response();
            if replayed {
                wire.headers_mut()
                    .insert("idempotent-replay", HeaderValue::from_static("true"));
            }
            wire.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            wire
        }
        TerminalResponse::Failure(_) => state.terminal(response, replayed, id, key, action),
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

/// Mutating `event.upsert` / `event.cancel`: validate the caller's `event_key`
/// (plus the full event input for upserts) and resolve it in the staging guild
/// before taking the durable claim — the same order as the announcement
/// channel-key check. An unmapped cancel key refuses before any claim or
/// Discord call; an unmapped upsert key creates. The committed outcome makes
/// replay byte-identical, including `created` after its key was registered.
async fn mutate_event(
    state: &ReceiverState,
    decision: &AuthDecision,
    raw: &[u8],
    idempotency_header: Option<&str>,
    id: &str,
    key: KeyLabel,
    action: ActionLabel,
) -> Response {
    let reject = |failure| state.reject(failure, key.clone(), action, id);
    let upsert = decision.action == "event.upsert";
    let idempotency = match validate_idempotency_key(idempotency_header, &decision.action) {
        Ok(key) => key,
        Err(error) => return reject(Failure::from_action(error)),
    };
    let event_key = match validate_event_key(&decision.body) {
        Ok(key) => key.to_owned(),
        Err(error) => return reject(Failure::from_action(error)),
    };
    let input = if upsert {
        match validate_event_input(&decision.body, state.config.channel_keys()) {
            Ok(input) => Some(input),
            Err(error) => return reject(Failure::from_action(error)),
        }
    } else {
        None
    };
    let Ok(_gate) = tokio::time::timeout(state.event_gate_wait, state.event_gate.lock()).await
    else {
        return reject(Failure::code(ErrorCode::InProgress));
    };
    let guild_id = two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID;
    let mapped = match state.store.event_id_for_key(guild_id, &event_key).await {
        Ok(mapped) => mapped,
        Err(_) => return reject(Failure::code(ErrorCode::Internal)),
    };
    // Cancel names a mapped key, never a raw Discord id: no mapping means no
    // Discord call, exactly like the read path.
    let event_id = match (upsert, mapped) {
        (true, mapped) => mapped,
        (false, Some(event_id)) => Some(event_id),
        (false, None) => return reject(Failure::from_action(unmapped_event_key(&event_key))),
    };
    let target_id = match event_id.as_deref() {
        // Creates name no event yet: the guild alone is the audit subject.
        None => None,
        Some(id) => match DiscordId::new(id) {
            Ok(id) => Some(id),
            Err(_) => return reject(Failure::code(ErrorCode::Internal)),
        },
    };
    let (call, outcome) = if upsert {
        let created = event_id.is_none();
        (
            EventCall::Upsert {
                event_id,
                input: input.expect("upsert validated its event input"),
            },
            if created {
                EventOutcome::Created
            } else {
                EventOutcome::Updated
            },
        )
    } else {
        (
            EventCall::Cancel {
                event_id: event_id.expect("cancel resolved a mapping"),
            },
            EventOutcome::Cancelled,
        )
    };
    let subject = AuditSubject {
        guild_id: Some(DiscordId::new(guild_id).expect("staging guild ID")),
        target_id,
        ..AuditSubject::default()
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
        Ok(InternalClaim::Mismatch) => return reject(Failure::code(ErrorCode::VersionConflict)),
        Ok(InternalClaim::InFlight) => return reject(Failure::code(ErrorCode::InProgress)),
        Ok(InternalClaim::NeedsReconciliation) => return reject(Failure::reconciliation()),
        Err(_) => return reject(Failure::code(ErrorCode::Internal)),
    };
    let observed_at = format_iso_millis(now_ms() as i64);
    match state
        .event_mutate
        .execute_mutation(guild_id, &call, &observed_at)
        .await
    {
        Ok(result) => {
            let event_id = match result
                .get("event_id")
                .and_then(Value::as_str)
                .map(DiscordId::new)
            {
                Some(Ok(event_id)) => event_id,
                // The executor only returns normalized mirror rows, so an
                // unreadable id is an unknown outcome, never a receipt.
                _ => {
                    let _ = state.store.mark_unknown(&claim).await;
                    return reject(Failure::reconciliation());
                }
            };
            // Upsert registers (or re-points) the key only after Discord
            // confirms; cancel retains the mapping so a later edit cannot
            // silently recreate the event.
            if upsert
                && state
                    .store
                    .put_event_key(guild_id, &event_key, event_id.as_str())
                    .await
                    .is_err()
            {
                let _ = state.store.mark_unknown(&claim).await;
                return reject(Failure::reconciliation());
            }
            let response = TerminalResponse::Success {
                resource_id: Some(event_id),
                affected: 1,
                outcome: Some(outcome),
            };
            if state.store.finish(&claim, &response).await.is_err() {
                // Do not return success before its audit/receipt is committed.
                // An unavailable store leaves the existing claim occupied.
                let _ = state.store.mark_unknown(&claim).await;
                return reject(Failure::reconciliation());
            }
            state.terminal(response, false, id, key.clone(), action)
        }
        Err(error) => {
            // A local admission refusal proves nothing was sent, so the claim
            // is released and the same key may retry. A 429 proves no effect
            // and, like announcements, is recorded as a refusal, never resent.
            // Anything uncertain retains the fence for reconciliation.
            if matches!(error, EventActionError::Discord(DiscordError::Guard(_)))
                || error.is_admission_blocked()
            {
                let wire = error.action_error();
                if state.store.release_proven_not_sent(claim).await.is_err() {
                    return reject(Failure::code(ErrorCode::Internal));
                }
                return reject(Failure::from_action(wire));
            }
            match error {
                EventActionError::Discord(
                    DiscordError::RateLimited | DiscordError::Rejected(_),
                ) => {
                    let response = TerminalResponse::Failure(TerminalFailure::DiscordRejected);
                    if state.store.finish(&claim, &response).await.is_err() {
                        let _ = state.store.mark_unknown(&claim).await;
                        return reject(Failure::reconciliation());
                    }
                    state.terminal(response, false, id, key.clone(), action)
                }
                _ => {
                    let _ = state.store.mark_unknown(&claim).await;
                    reject(Failure::reconciliation())
                }
            }
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
            outcome: None,
        } => Json(json!({
            "ok": true, "result": {"message_id": message_id.as_str()}, "request_id": id,
        }))
        .into_response(),
        TerminalResponse::Success {
            resource_id: Some(event_id),
            affected: 1,
            outcome: Some(outcome),
        } => Json(json!({
            "ok": true,
            "result": {"outcome": outcome.as_str(), "event_id": event_id.as_str()},
            "request_id": id,
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
