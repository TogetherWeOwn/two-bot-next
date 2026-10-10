//! Private internal-action receiver (announcement, event read, settings, moderation,
//! automations). Never merge this router into the health socket. Authentication and a
//! committed nonce precede JSON; a committed intent precedes the effect (except
//! keyless reads). Cancellation leaves durable ownership, never a new execution
//! lease.
//!
//! Wired effects: `announcement.post` (single-attempt send), `event.read`
//! (keyless mapped GET), `settings.get`/`settings.set` (settings executors),
//! `automations.export` (keyless redacted read) / `automations.import`
//! (claimed transactional import), the restrictive member verbs
//! `moderation.ban`, `moderation.tempban`, `moderation.kick`, `moderation.warn`
//! and `moderation.timeout` (member moderation through the shared moderation
//! service), `role.assign` (allowlisted role-key assignment) and
//! `guild.add_member` (OAuth-backed join with a transient token), and the
//! channel-moderation verbs (`moderation.purge`, `moderation.slowmode`,
//! `moderation.lockdown`, `moderation.unlock`) through the shared
//! channel-moderation store and the existing purge/slowmode/lockdown planner
//! paths. Every other verb stays refused by the per-effect fences below, even
//! when the env-only flag gate authorizes it.

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
    automation_transfer::{
        diff_import, export_document, max_import_entries, parse_import_document, ImportOutcome,
        ImportParseError,
    },
    channel_moderation_store::ChannelModerationStore,
    clock_guard::ClockGuard,
    commands::{
        PERM_BAN_MEMBERS, PERM_KICK_MEMBERS, PERM_MANAGE_CHANNELS, PERM_MANAGE_MESSAGES,
        PERM_MODERATE_MEMBERS,
    },
    custom_command_service::{self, ImportServiceError},
    custom_command_store,
    custom_commands::reserved_command_names,
    format_iso_millis,
    internal_action_config::InternalActionConfig,
    internal_action_store::{
        AuditSubject, DiscordId, InternalActionStore, InternalClaim, RequestIdentity,
        TerminalFailure, TerminalResponse,
    },
    internal_actions::{
        new_request_id, require_field_str, require_snowflake, unmapped_event_key,
        validate_announcement, validate_event_key, validate_idempotency_key, ActionError,
        AuthDecision, AuthHeaders, AuthenticatedRequest, ErrorCode, GuildAddMemberRequest,
        InternalFlags, RoleAssignRequest, TokenBuckets, ACTIONS_PATH, MAX_BODY_BYTES, SKEW_SECONDS,
    },
    internal_settings::SettingsCommand,
    member_moderation_store::PgMemberModerationStore,
    rejection_telemetry::{ActionLabel, KeyLabel, Rejection, RejectionRecord, RejectionTelemetry},
    ModerationAction, ModerationActor, ModerationGates, ModerationPolicy, ModerationTarget,
};
use two_bot_cutover::{internal_settings::execute_settings, settings::SettingsStore};
use two_bot_discord::executor::member::MemberOutcome;
use two_bot_discord::internal_actions::{AnnouncementExecutor, ExecutionOutcome, Refusal};
use two_bot_discord::internal_channel_moderation::{
    InternalChannelConfig, InternalChannelExecutor, InternalChannelRequest, InternalChannelResult,
};
use two_bot_discord::internal_member_moderation::{
    InternalMemberConfig, InternalMemberExecutor, InternalMemberRequest,
};
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

/// Restrictive-moderation effect: `moderation.ban`, `moderation.tempban`,
/// `moderation.kick`, `moderation.warn` and `moderation.timeout` through the
/// shared moderation service. The test seam is module-private like
/// [`ActionEffect`]: the
/// production effect resolves actor/target from the configured staging guild
/// using live member roles, positions, permissions and bot/owner flags — never
/// from body-supplied roles or permissions. Mocks skip Discord and return a
/// canned receipt, so HTTP-layer allow/deny/key-reuse tests stay offline.
trait ModerationEffect: Send + Sync {
    fn execute_moderation<'a>(
        &'a self,
        request: &'a InternalMemberRequest,
        request_id: &'a str,
        idempotency_key: &'a str,
        now_ms: i64,
    ) -> BoxFuture<'a, Result<TerminalResponse, ActionError>>;
}

/// Production moderation effect over the shared [`InternalMemberExecutor`].
/// Resolution (guild facts, actor/target/bot snapshots) runs inside the
/// effect so the HTTP handler stays a thin validate-claim-finish fence.
/// Definitive service failures become terminal receipts; transport, rate-limit,
/// store and in-flight uncertainty stays [`Effect::Unknown`] so the outer
/// idempotency claim is retained for reconciliation, never re-executed.
struct ModerationExecutor {
    inner: InternalMemberExecutor<PgMemberModerationStore>,
    discord: ActionExecutor,
}

impl ModerationExecutor {
    fn new(
        inner: InternalMemberExecutor<PgMemberModerationStore>,
        discord: ActionExecutor,
    ) -> Self {
        Self { inner, discord }
    }
}

impl ModerationEffect for ModerationExecutor {
    fn execute_moderation<'a>(
        &'a self,
        request: &'a InternalMemberRequest,
        request_id: &'a str,
        idempotency_key: &'a str,
        now_ms: i64,
    ) -> BoxFuture<'a, Result<TerminalResponse, ActionError>> {
        Box::pin(async move {
            let guild_id = two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID;
            let facts = moderation_guild_facts(&self.discord, guild_id).await?;
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
            let (bot_roles, _) = moderation_fetched_member(&self.discord, guild_id, &bot_id)
                .await?
                .ok_or_else(|| {
                    ActionError::new(
                        ErrorCode::DiscordUnavailable,
                        "Discord member snapshot unavailable; do not retry this key",
                        "moderation_snapshot_unavailable",
                    )
                })?;
            let bot_with_everyone = moderation_with_everyone(bot_roles, guild_id);
            let bot_position =
                moderation_top_position(&bot_with_everyone, &facts).ok_or_else(|| {
                    ActionError::new(
                        ErrorCode::DiscordUnavailable,
                        "Discord member snapshot unavailable; do not retry this key",
                        "moderation_snapshot_unavailable",
                    )
                })?;
            let (actor, target, bot_position) =
                moderation_resolve(&self.discord, request, &facts, bot_position).await?;
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

/// Channel-moderation effect: `moderation.purge`, `moderation.slowmode`,
/// `moderation.lockdown` and `moderation.unlock` through the shared
/// channel-moderation store and the existing purge/slowmode/lockdown planner
/// paths. The test seam is module-private like [`ModerationEffect`]:
/// the production effect resolves the actor from the configured staging guild
/// using live member roles, positions and permissions — never from
/// body-supplied roles or permissions. Mocks skip Discord and return a canned
/// receipt, so HTTP-layer allow/deny/key-reuse tests stay offline.
trait ChannelModerationEffect: Send + Sync {
    fn execute_channel<'a>(
        &'a self,
        request: &'a InternalChannelRequest,
        request_id: &'a str,
        idempotency_key: &'a str,
        now: &'a str,
    ) -> BoxFuture<'a, Result<InternalChannelResult, ActionError>>;
}

/// Production channel effect over the shared [`InternalChannelExecutor`].
/// Actor resolution (guild facts plus live member snapshot) runs inside the
/// effect so the HTTP handler stays a thin validate-claim-finish fence.
/// Definitive service failures become terminal receipts; transport, rate-limit,
/// store and in-flight uncertainty stays [`Effect::Unknown`] so the outer
/// idempotency claim is retained for reconciliation, never re-executed.
struct ChannelModerationExecutor {
    inner: InternalChannelExecutor,
    discord: ActionExecutor,
}

impl ChannelModerationExecutor {
    fn new(inner: InternalChannelExecutor, discord: ActionExecutor) -> Self {
        Self { inner, discord }
    }
}

impl ChannelModerationEffect for ChannelModerationExecutor {
    fn execute_channel<'a>(
        &'a self,
        request: &'a InternalChannelRequest,
        request_id: &'a str,
        idempotency_key: &'a str,
        now: &'a str,
    ) -> BoxFuture<'a, Result<InternalChannelResult, ActionError>> {
        Box::pin(async move {
            let guild_id = two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID;
            let facts = moderation_guild_facts(&self.discord, guild_id).await?;
            let actor = channel_resolve(&self.discord, request, &facts).await?;
            self.inner
                .execute(request, &actor, request_id, idempotency_key, now)
                .await
        })
    }
}

/// Required Discord permission per channel verb: purge gates Manage Messages,
/// slowmode/lockdown/unlock gate Manage Channels (parity §1 #8–#11).
fn channel_required_permission(action: ModerationAction) -> u64 {
    match action {
        ModerationAction::Purge => PERM_MANAGE_MESSAGES,
        ModerationAction::Slowmode | ModerationAction::Lockdown | ModerationAction::Unlock => {
            PERM_MANAGE_CHANNELS
        }
        _ => u64::MAX,
    }
}

/// Wire outcome name per channel verb (legacy [`ChannelOutcome::name`]):
/// purge renders `purged`, slowmode `slowmode_updated`, lockdown `locked_down`,
/// unlock `unlocked`.
fn channel_action_outcome(action: &str) -> Option<&'static str> {
    match action {
        "moderation.purge" => Some("purged"),
        "moderation.slowmode" => Some("slowmode_updated"),
        "moderation.lockdown" => Some("locked_down"),
        "moderation.unlock" => Some("unlocked"),
        _ => None,
    }
}

/// Resolve the website-attributed actor from the staging guild using live
/// roles, positions and permissions — never from body-supplied roles. Channel
/// verbs skip member hierarchy entirely; only the per-verb Manage permission
/// gates the actor. An unresolvable snapshot fails closed into fenced
/// uncertainty; a non-member or unpermitted actor is a definitive refusal.
async fn channel_resolve(
    executor: &ActionExecutor,
    request: &InternalChannelRequest,
    facts: &ModerationGuildFacts,
) -> Result<ModerationActor, ActionError> {
    let guild_id = &facts.guild_id;
    let refuse_unavailable = || {
        ActionError::new(
            ErrorCode::DiscordUnavailable,
            "Discord member snapshot unavailable; do not retry this key",
            "moderation_snapshot_unavailable",
        )
    };
    let channel_action = match request.action().action_name() {
        "moderation.purge"
        | "moderation.slowmode"
        | "moderation.lockdown"
        | "moderation.unlock" => request.action(),
        _ => {
            return Err(ActionError::new(
                ErrorCode::ActionNotAllowed,
                "not a channel moderation action",
                "moderation_action_mismatch",
            ));
        }
    };
    let (actor_roles, _) = moderation_fetched_member(executor, guild_id, request.actor_id())
        .await?
        .ok_or_else(|| {
            ActionError::new(
                ErrorCode::ActionNotAllowed,
                "moderation actor is not a guild member",
                "moderation_actor_not_member",
            )
        })?;
    let actor_with_everyone = moderation_with_everyone(actor_roles, guild_id);
    let actor = ModerationActor {
        user_id: request.actor_id().to_owned(),
        highest_role_position: moderation_top_position(&actor_with_everyone, facts)
            .ok_or_else(refuse_unavailable)?,
        role_ids: actor_with_everyone.clone(),
        permissions: moderation_permissions(&actor_with_everyone, facts, request.actor_id())
            .ok_or_else(refuse_unavailable)?,
    };
    let required = channel_required_permission(channel_action);
    if actor.permissions & required != required && actor.user_id != facts.owner_id {
        return Err(ActionError::new(
            ErrorCode::ActionNotAllowed,
            "moderation actor lacks channel permission",
            "moderation_actor_forbidden",
        ));
    }
    Ok(actor)
}

/// Discord `ADMINISTRATOR` bit (1 << 3): holders pass every permission check.
const ADMINISTRATOR_BIT: u64 = 1 << 3;

/// The five website-wired restrictive verbs. Channel verbs belong to the
/// channel family slice: none of them may reach this resolver.
fn is_wired_moderation_verb(action: ModerationAction) -> bool {
    matches!(
        action,
        ModerationAction::Ban
            | ModerationAction::TempBan
            | ModerationAction::Kick
            | ModerationAction::Warn
            | ModerationAction::Timeout
    )
}

/// Per-verb actor gate (legacy `permissionFor`): bans check Ban Members,
/// kicks check Kick Members, warns and timeouts check Moderate Members. The
/// resolver refuses verbs outside [`is_wired_moderation_verb`] before this
/// runs; the shared service re-checks the same gate from its own table.
fn moderation_required_permission(action: ModerationAction) -> u64 {
    match action {
        ModerationAction::Ban | ModerationAction::TempBan => PERM_BAN_MEMBERS,
        ModerationAction::Kick => PERM_KICK_MEMBERS,
        _ => PERM_MODERATE_MEMBERS,
    }
}

/// Bans address users, not just members: Discord bans a departed or
/// never-joined user id outright. Kicks, warns and timeouts act on a guild
/// member, so they refuse a departed target instead of synthesizing one.
fn moderation_tolerates_departed_target(action: ModerationAction) -> bool {
    matches!(action, ModerationAction::Ban | ModerationAction::TempBan)
}

/// Live guild snapshot for hierarchy and permission resolution. Read per
/// website moderation call: hierarchy never runs on a partial or cached
/// snapshot.
struct ModerationGuildFacts {
    guild_id: String,
    owner_id: String,
    positions: std::collections::HashMap<String, i64>,
    permissions: std::collections::HashMap<String, u64>,
}

fn moderation_text(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_owned)
}

async fn moderation_guild_facts(
    executor: &ActionExecutor,
    guild_id: &str,
) -> Result<ModerationGuildFacts, ActionError> {
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
    let owner_id = moderation_text(&guild, "owner_id").ok_or_else(refuse)?;
    let mut positions = std::collections::HashMap::new();
    let mut permissions = std::collections::HashMap::new();
    for role in guild
        .get("roles")
        .and_then(Value::as_array)
        .ok_or_else(refuse)?
    {
        let id = moderation_text(role, "id").ok_or_else(refuse)?;
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
    Ok(ModerationGuildFacts {
        guild_id: guild_id.to_owned(),
        owner_id,
        positions,
        permissions,
    })
}

fn moderation_top_position(held: &[String], facts: &ModerationGuildFacts) -> Option<i64> {
    let mut top = *facts.positions.get(&facts.guild_id)?;
    for role in held {
        top = top.max(*facts.positions.get(role)?);
    }
    Some(top)
}

fn moderation_with_everyone(mut roles: Vec<String>, guild_id: &str) -> Vec<String> {
    if !roles.iter().any(|r| r == guild_id) {
        roles.push(guild_id.to_owned());
    }
    roles
}

/// Guild-level permission union for the held roles plus `@everyone`.
/// Unknown held roles fail closed; the snapshot is incomplete, never
/// unprotected. The guild owner and `ADMINISTRATOR` holders pass everything.
fn moderation_permissions(
    held: &[String],
    facts: &ModerationGuildFacts,
    user_id: &str,
) -> Option<u64> {
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
/// departed or never-joined user: bans tolerate it (Discord bans bare user
/// ids), kicks and warns refuse. An id mismatch or malformed body fails
/// closed into fenced uncertainty.
async fn moderation_fetched_member(
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
    if moderation_text(user, "id").as_deref() != Some(user_id) {
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
async fn moderation_resolve(
    executor: &ActionExecutor,
    request: &InternalMemberRequest,
    facts: &ModerationGuildFacts,
    bot_highest_role_position: i64,
) -> Result<(ModerationActor, ModerationTarget, i64), ActionError> {
    if !is_wired_moderation_verb(request.action()) {
        return Err(ActionError::new(
            ErrorCode::ActionNotAllowed,
            "not a wired moderation action",
            "moderation_action_mismatch",
        ));
    }
    let guild_id = &facts.guild_id;
    let refuse_unavailable = || {
        ActionError::new(
            ErrorCode::DiscordUnavailable,
            "Discord member snapshot unavailable; do not retry this key",
            "moderation_snapshot_unavailable",
        )
    };
    let (actor_roles, _) = moderation_fetched_member(executor, guild_id, request.actor_id())
        .await?
        .ok_or_else(|| {
            ActionError::new(
                ErrorCode::ActionNotAllowed,
                "moderation actor is not a guild member",
                "moderation_actor_not_member",
            )
        })?;
    let actor_with_everyone = moderation_with_everyone(actor_roles, guild_id);
    let actor = ModerationActor {
        user_id: request.actor_id().to_owned(),
        highest_role_position: moderation_top_position(&actor_with_everyone, facts)
            .ok_or_else(refuse_unavailable)?,
        role_ids: actor_with_everyone.clone(),
        permissions: moderation_permissions(&actor_with_everyone, facts, request.actor_id())
            .ok_or_else(refuse_unavailable)?,
    };
    let required = moderation_required_permission(request.action());
    if actor.permissions & required != required && actor.user_id != facts.owner_id {
        return Err(ActionError::new(
            ErrorCode::ActionNotAllowed,
            "moderation actor lacks the permission for this action",
            "moderation_actor_forbidden",
        ));
    }
    // A departed target bans at `@everyone` standing: hierarchy still applies
    // against the actor and the bot. The bot flag is unknowable off-guild, so
    // the shared service's any-bot guard cannot fire for departed users; the
    // owner-id and hierarchy guards still can.
    let (target_roles, is_bot) =
        match moderation_fetched_member(executor, guild_id, request.target_id()).await? {
            Some(found) => found,
            None if moderation_tolerates_departed_target(request.action()) => (Vec::new(), false),
            None => {
                return Err(ActionError::new(
                    ErrorCode::ActionNotAllowed,
                    "moderation target is not a guild member",
                    "moderation_target_not_member",
                ));
            }
        };
    let target_with_everyone = moderation_with_everyone(target_roles, guild_id);
    let target = ModerationTarget {
        user_id: request.target_id().to_owned(),
        highest_role_position: moderation_top_position(&target_with_everyone, facts)
            .ok_or_else(refuse_unavailable)?,
        role_ids: target_with_everyone,
        is_bot,
        is_guild_owner: request.target_id() == facts.owner_id,
    };
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

struct ReceiverState {
    config: InternalActionConfig,
    store: InternalActionStore,
    effect: Arc<dyn ActionEffect>,
    member: Arc<dyn MemberEffect>,
    event_read: Arc<dyn EventReadEffect>,
    moderation: Arc<dyn ModerationEffect>,
    channel: Arc<dyn ChannelModerationEffect>,
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
        moderation: Arc<dyn ModerationEffect>,
        channel: Arc<dyn ChannelModerationEffect>,
    ) -> Self {
        Self {
            config,
            store: InternalActionStore::new(pool),
            effect,
            member,
            event_read,
            moderation,
            channel,
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
                TerminalFailure::VersionConflict => ErrorCode::VersionConflict,
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

/// Build the production moderation executor from the process environment.
/// `enabled` requires every wired restrictive verb's flag, which all derive
/// from BOTH `TWO_MODERATION` and `TWO_INTERNAL_ALLOW_MODERATION` (see
/// [`InternalFlags`]): the website must never grant itself verbs through the
/// settings store. Invalid enabled moderation configuration is fatal, like
/// the receiver bind itself; a disabled executor still constructs and refuses
/// every moderation call with `action_not_allowed`.
///
/// The moderation settings (gates, audit secret) parse only when the website
/// verbs are enabled: a disabled executor never touches policy or secrets, so
/// malformed moderation settings must not fail the bind while every website
/// verb stays refused.
fn moderation_executor_from_env(
    pool: sqlx::PgPool,
    discord: ActionExecutor,
) -> Result<ModerationExecutor, String> {
    use two_bot_core::mac::moderation_audit_secret;

    let guild_id = two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID.to_owned();
    let vars: std::collections::HashMap<String, String> = std::env::vars().collect();
    let flags = InternalFlags::from_map(&vars);
    let enabled = [
        "moderation.ban",
        "moderation.tempban",
        "moderation.kick",
        "moderation.warn",
        "moderation.timeout",
    ]
    .iter()
    .all(|verb| flags.is_enabled(verb));
    let (policy, audit_secret) = if enabled {
        let gates = ModerationGates::from_map(&vars).map_err(|e| e.to_string())?;
        if !gates.enabled {
            return Err("moderation flag without TWO_MODERATION".to_owned());
        }
        let audit_secret = moderation_audit_secret(&vars, None)
            .map_err(|e| e.to_string())?
            .map(|s| s.expose().to_owned());
        (
            ModerationPolicy {
                owen_user_id: gates.owen_user_id,
                protected_role_ids: gates.protected_role_ids,
                bot_user_id: None,
            },
            audit_secret,
        )
    } else {
        (
            ModerationPolicy {
                owen_user_id: String::new(),
                protected_role_ids: std::collections::HashSet::new(),
                bot_user_id: None,
            },
            None,
        )
    };
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
    Ok(ModerationExecutor::new(inner, discord))
}

/// Build the production channel-moderation executor from the process
/// environment. `enabled` combines BOTH `TWO_MODERATION` and
/// `TWO_INTERNAL_ALLOW_MODERATION` (see [`InternalFlags`]): the website must
/// never grant itself verbs through the settings store. The four channel verbs
/// share one gate — the internal allowlist and the moderation publish gate
/// enable them together — so the executor is on only when every channel verb
/// is enabled. Invalid enabled moderation configuration is fatal, like the
/// receiver bind itself; a disabled executor still constructs and refuses
/// every channel verb with `action_not_allowed`, tolerating invalid moderation
/// gates so a bad `TWO_MODERATION_PROTECTED_ROLE_IDS` (or a stray
/// `TWO_MODERATION=1` without `TWO_OWEN_USER_ID`) can never stop the receiver
/// from binding while the channel verbs are off.
fn channel_executor_from_env(
    pool: sqlx::PgPool,
    discord: ActionExecutor,
) -> Result<ChannelModerationExecutor, String> {
    use two_bot_core::mac::moderation_audit_secret;

    let guild_id = two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID.to_owned();
    let vars: std::collections::HashMap<String, String> = std::env::vars().collect();
    let flags = InternalFlags::from_map(&vars);
    let enabled = [
        "moderation.purge",
        "moderation.slowmode",
        "moderation.lockdown",
        "moderation.unlock",
    ]
    .iter()
    .all(|verb| flags.is_enabled(verb));
    let policy = if enabled {
        let gates = ModerationGates::from_map(&vars).map_err(|e| e.to_string())?;
        if !gates.enabled {
            return Err("moderation channel flag without TWO_MODERATION".to_owned());
        }
        ModerationPolicy {
            owen_user_id: gates.owen_user_id,
            protected_role_ids: gates.protected_role_ids,
            bot_user_id: None,
        }
    } else {
        // Disabled: refuse everything without trusting the gates. Invalid
        // moderation settings must not fail the receiver bind while the verbs
        // are off; fall back to an empty policy the disabled executor never
        // consults (see `InternalChannelExecutor::execute`).
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
    let store = ChannelModerationStore::from_pool(pool);
    let inner = InternalChannelExecutor::new(
        store,
        discord.clone(),
        InternalChannelConfig {
            guild_id,
            enabled,
            policy,
            audit_secret,
        },
    )
    .map_err(|e| e.to_string())?;
    Ok(ChannelModerationExecutor::new(inner, discord))
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
    // event reads, moderation calls and channel moderation all hold the same
    // token-wide lane. Bound as the trait object so every executor constructor
    // coerces without re-wrapping.
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
    // Membership shares the same admitted transport as event reads: one
    // token-wide lane for every Discord send. Cloned before the read wrapper
    // takes ownership; role hierarchy and add-member PUTs hold the same lane.
    let member = events.clone();
    let moderation_discord =
        ActionExecutor::with_admission(token.to_owned(), None, Arc::clone(&admission)).map_err(
            |_| std::io::Error::other("internal-action moderation executor configuration invalid"),
        )?;
    let moderation =
        moderation_executor_from_env(pool.clone(), moderation_discord).map_err(|_| {
            std::io::Error::other("internal-action moderation executor configuration invalid")
        })?;
    let channel_discord = ActionExecutor::with_admission(token.to_owned(), None, admission)
        .map_err(|_| {
            std::io::Error::other("internal-action channel executor configuration invalid")
        })?;
    let channel = channel_executor_from_env(pool.clone(), channel_discord).map_err(|_| {
        std::io::Error::other("internal-action channel executor configuration invalid")
    })?;
    let listener = TcpListener::bind(config.listen_addr()).await?;
    Ok(BoundReceiver {
        listener,
        state: Arc::new(ReceiverState::new(
            config,
            pool.clone(),
            Arc::new(executor),
            Arc::new(member),
            Arc::new(EventReadExecutor::new(events, pool)),
            Arc::new(moderation),
            Arc::new(channel),
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
        burned.authorize(&flags, true, true, &mut buckets)
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
    if decision.action == "settings.get" {
        return read_setting(state, &decision, id, key, action).await;
    }
    if decision.action == "settings.set" {
        return write_setting(state, &decision, headers.idempotency, &raw, id, key, action).await;
    }
    if decision.action == "automations.export" {
        return export_automations(state, &decision, id, key, action).await;
    }
    if decision.action == "automations.import" {
        return import_automations(
            state,
            &decision,
            &raw,
            headers.idempotency,
            flags.allow_automation_overwrite,
            id,
            key,
            action,
        )
        .await;
    }
    // Family 1 (settings, M3.10), the automations pair (M3.10) plus the five
    // wired restrictive-moderation verbs. Every other verb without an adapter
    // stays refused below, even when the flag gate authorizes it — shipping an
    // implementation must never widen the allowlist by itself.
    if matches!(
        decision.action.as_str(),
        "moderation.ban"
            | "moderation.tempban"
            | "moderation.kick"
            | "moderation.warn"
            | "moderation.timeout"
    ) {
        return moderation_member(state, &decision, &raw, headers.idempotency, id, key, action)
            .await;
    }
    // Channel-moderation family (M3.10): purge, slowmode, lockdown and unlock
    // through the shared channel-moderation store and the existing
    // purge/slowmode/lockdown planner paths. Every remaining verb without an
    // adapter stays refused below.
    if channel_action_outcome(&decision.action).is_some() {
        return moderate_channel(state, &decision, &raw, headers.idempotency, id, key, action)
            .await;
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

/// Website restrictive moderation (`moderation.ban`, `moderation.tempban`,
/// `moderation.kick`, `moderation.warn`, `moderation.timeout`):
/// idempotency-key validation and the durable claim match the announcement
/// path. The body parses via [`InternalMemberRequest::from_body`]
/// (actor/target snowflakes, trimmed reason, tempban `duration_seconds`
/// 60–365d, timeout `duration_seconds` 60–28d); actor and target resolve from
/// the configured staging guild using live member roles, positions,
/// permissions and bot/owner flags — never from body-supplied roles. The outer
/// [`InternalActionStore`] claim guards the exact signed bytes (replay,
/// mismatch, in-flight); the inner [`InternalMemberExecutor`] guards the
/// moderation content and writes the shared `moderation_audit` ledger, so rows
/// are identical to the slash-command path. Only the five wired verbs reach
/// here; every other verb stays refused by the fences in [`receive`].
#[allow(clippy::too_many_arguments)]
async fn moderation_member(
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
    let request = match InternalMemberRequest::from_body(&decision.action, &decision.body) {
        Ok(request) => request,
        Err(error) => return reject(Failure::from_action(error)),
    };
    if !is_wired_moderation_verb(request.action()) {
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
            return moderation_terminal(
                state,
                &decision.action,
                response,
                true,
                id,
                key.clone(),
                action,
            );
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
        .execute_moderation(&request, id, &idempotency, now)
        .await
    {
        Ok(response) => {
            if state.store.finish(&claim, &response).await.is_err() {
                let _ = state.store.mark_unknown(&claim).await;
                return reject(Failure::reconciliation());
            }
            moderation_terminal(
                state,
                &decision.action,
                response,
                false,
                id,
                key.clone(),
                action,
            )
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
            moderation_terminal(
                state,
                &decision.action,
                response,
                false,
                id,
                key.clone(),
                action,
            )
        }
        Err(_) => {
            let _ = state.store.mark_unknown(&claim).await;
            reject(Failure::reconciliation())
        }
    }
}

/// Legacy outcome string per wired verb (`MemberOutcome::as_str`): `banned`,
/// `temporarily_banned`, `kicked`, `warned`, `timed_out`. The stored receipt
/// carries only the affected target, so the wire renders the outcome from the
/// action — the first and every replayed response are identical.
fn moderation_outcome(action: &str) -> Option<&'static str> {
    match action {
        "moderation.ban" => Some("banned"),
        "moderation.tempban" => Some("temporarily_banned"),
        "moderation.kick" => Some("kicked"),
        "moderation.warn" => Some("warned"),
        "moderation.timeout" => Some("timed_out"),
        _ => None,
    }
}

/// Render a moderation terminal receipt. Success carries the acted-on target
/// as the stored `resource_id` (audit) and reports the verb's outcome on the
/// wire; failures share the announcement failure codes. Replays set the same
/// `idempotent-replay` header as the announcement path.
#[allow(clippy::too_many_arguments)]
fn moderation_terminal(
    state: &ReceiverState,
    action: &str,
    response: TerminalResponse,
    replayed: bool,
    id: &str,
    key: KeyLabel,
    action_label: ActionLabel,
) -> Response {
    match response {
        TerminalResponse::Success { .. } => {
            let Some(outcome) = moderation_outcome(action) else {
                return state.reject(Failure::code(ErrorCode::Internal), key, action_label, id);
            };
            let mut wire = (
                StatusCode::OK,
                Json(json!({"ok": true, "result": {"outcome": outcome}, "request_id": id})),
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
        TerminalResponse::Failure(_) => state.terminal(response, replayed, id, key, action_label),
    }
}

/// Website channel moderation (`moderation.purge`, `moderation.slowmode`,
/// `moderation.lockdown`, `moderation.unlock`): idempotency-key validation and
/// durable claim match the announcement path. The body parses via
/// [`InternalChannelRequest::from_body`] (actor/channel snowflakes, trimmed
/// reason, `count` 1–100 for purge and `seconds` 0–6h for slowmode); the actor
/// resolves from the configured staging guild using live member roles,
/// positions and permissions — never from body-supplied roles. Purge runs the
/// existing list-then-delete path with the 14-day bulk-delete limit intact;
/// lockdown/unlock run the existing planner path with recovery masks intact.
/// The outer [`InternalActionStore`] claim guards the exact signed bytes
/// (replay, mismatch, in-flight); the inner [`InternalChannelExecutor`] guards
/// the moderation content and writes the shared channel ledger, so rows are
/// identical to the slash-command path. Only the four channel verbs reach
/// here; every other verb stays refused by the fences in [`receive`].
#[allow(clippy::too_many_arguments)]
async fn moderate_channel(
    state: &ReceiverState,
    decision: &AuthDecision,
    raw: &[u8],
    idempotency_header: Option<&str>,
    id: &str,
    key: KeyLabel,
    action: ActionLabel,
) -> Response {
    let reject = |failure| state.reject(failure, key.clone(), action, id);
    let outcome = match channel_action_outcome(&decision.action) {
        Some(outcome) => outcome,
        None => return reject(Failure::code(ErrorCode::ActionNotAllowed)),
    };
    let idempotency = match validate_idempotency_key(idempotency_header, &decision.action) {
        Ok(key) => key.to_owned(),
        Err(error) => return reject(Failure::from_action(error)),
    };
    let request = match InternalChannelRequest::from_body(&decision.action, &decision.body) {
        Ok(request) => request,
        Err(error) => return reject(Failure::from_action(error)),
    };
    if channel_action_outcome(request.action().action_name()) != Some(outcome) {
        return reject(Failure::code(ErrorCode::ActionNotAllowed));
    }
    let subject = match (
        DiscordId::new(two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID),
        DiscordId::new(request.actor_id()),
        DiscordId::new(request.channel_id()),
    ) {
        (Ok(guild_id), Ok(actor_id), Ok(channel_id)) => AuditSubject {
            guild_id: Some(guild_id),
            actor_id: Some(actor_id),
            target_id: Some(channel_id),
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
            return moderate_channel_terminal(
                state,
                response,
                outcome,
                None,
                true,
                &decision.action,
                id,
                key.clone(),
                action,
            );
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
    // Actor resolution and the shared channel-moderation service run inside
    // the effect. Definitive refusals become terminal receipts; snapshot,
    // transport and rate-limit uncertainty stays fenced.
    let now = format_iso_millis(now_ms() as i64);
    match state
        .channel
        .execute_channel(&request, id, &idempotency, &now)
        .await
    {
        Ok(result) => {
            let affected = result.affected.unwrap_or(1);
            let stored_affected = u32::try_from(affected).unwrap_or(u32::MAX);
            let channel_id = DiscordId::new(request.channel_id()).map_err(|_| {
                ActionError::new(
                    ErrorCode::Internal,
                    "Internal action storage unavailable",
                    "moderation_receipt_invalid",
                )
            });
            let Ok(channel_id) = channel_id else {
                let _ = state.store.mark_unknown(&claim).await;
                return reject(Failure::reconciliation());
            };
            let response = TerminalResponse::Success {
                resource_id: Some(channel_id),
                affected: stored_affected,
            };
            if state.store.finish(&claim, &response).await.is_err() {
                let _ = state.store.mark_unknown(&claim).await;
                return reject(Failure::reconciliation());
            }
            moderate_channel_terminal(
                state,
                response,
                &result.outcome,
                result.affected,
                false,
                &decision.action,
                id,
                key.clone(),
                action,
            )
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
            moderate_channel_terminal(
                state,
                response,
                outcome,
                None,
                false,
                &decision.action,
                id,
                key.clone(),
                action,
            )
        }
        Err(_) => {
            let _ = state.store.mark_unknown(&claim).await;
            reject(Failure::reconciliation())
        }
    }
}

/// Render a channel-moderation terminal receipt. Success carries the moderated
/// channel as the stored `resource_id` (audit) and reports the planner outcome
/// (`purged`, `slowmode_updated`, `locked_down`, `unlocked`) on the wire with
/// the purged count when present; failures share the announcement failure
/// codes. Replays set the same `idempotent-replay` header as the announcement
/// path. A replayed outer receipt re-derives the outcome from the authorized
/// action (which maps 1:1 to the planner outcome) and the affected count from
/// the stored receipt, so the wire stays identical without trusting a second
/// body parse.
#[allow(clippy::too_many_arguments)]
fn moderate_channel_terminal(
    state: &ReceiverState,
    response: TerminalResponse,
    outcome: &str,
    affected: Option<u64>,
    replayed: bool,
    action: &str,
    id: &str,
    key: KeyLabel,
    action_label: ActionLabel,
) -> Response {
    match response {
        TerminalResponse::Success {
            affected: stored_affected,
            ..
        } => {
            let wire_outcome = channel_action_outcome(action).unwrap_or(outcome);
            let mut result = json!({"outcome": wire_outcome});
            // Purge surfaces its deleted count; other verbs report no count.
            // A fresh purge carries the planner count, a replay carries the
            // stored receipt count, and a zero count never serializes.
            let count = affected.filter(|n| *n > 0).map_or_else(
                || {
                    if wire_outcome == "purged" && stored_affected > 0 {
                        Some(u64::from(stored_affected))
                    } else {
                        None
                    }
                },
                Some,
            );
            if let Some(count) = count {
                result["affected"] = json!(count);
            }
            let mut wire = (
                StatusCode::OK,
                Json(json!({"ok": true, "result": result, "request_id": id})),
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
        TerminalResponse::Failure(_) => state.terminal(response, replayed, id, key, action_label),
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

/// Keyless `settings.get`: read the guild's stored override only, never the
/// process environment. The flag gate already ran in `authorize`; the committed
/// nonce above is the replay guard, so no idempotency claim is taken.
async fn read_setting(
    state: &ReceiverState,
    decision: &AuthDecision,
    id: &str,
    key: KeyLabel,
    action: ActionLabel,
) -> Response {
    let reject = |failure| state.reject(failure, key.clone(), action, id);
    let command = match SettingsCommand::parse("settings.get", &decision.body) {
        Ok(command) => command,
        Err(error) => return reject(Failure::from_action(error)),
    };
    let guild_id = two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID;
    let store = SettingsStore::new(state.store.pool());
    match execute_settings(&store, guild_id, &command).await {
        Ok(outcome) => settings_read_response(outcome.result, outcome.observed_version, id),
        Err(error) => reject(Failure::from_action(error)),
    }
}

/// `settings.set`: validated writes claim the outer durable idempotency key
/// before executing. Replaying the stored terminal returns the first
/// value-free `{key,outcome}` result without a second write, audit row or
/// version bump. A stale `expected_version` is a non-retryable 409
/// `version_conflict`: the save that landed in between is never silently
/// reverted.
#[allow(clippy::too_many_arguments)]
async fn write_setting(
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
    let command = match SettingsCommand::parse("settings.set", &decision.body) {
        Ok(command) => command,
        Err(error) => return reject(Failure::from_action(error)),
    };
    let guild_id = two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID;
    let (_, actor, _) = command.write().expect("set parses as a write");
    let subject = AuditSubject {
        guild_id: Some(DiscordId::new(guild_id).expect("staging guild ID")),
        actor_id: Some(DiscordId::new(actor).expect("validated actor ID")),
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
            return replay_setting(state, &decision.body, response, id, key, action).await;
        }
        Ok(InternalClaim::Mismatch) => return reject(Failure::code(ErrorCode::VersionConflict)),
        Ok(InternalClaim::InFlight) => return reject(Failure::code(ErrorCode::InProgress)),
        Ok(InternalClaim::NeedsReconciliation) => return reject(Failure::reconciliation()),
        Err(_) => return reject(Failure::code(ErrorCode::Internal)),
    };
    let store = SettingsStore::new(state.store.pool());
    match execute_settings(&store, guild_id, &command).await {
        Ok(outcome) => {
            let deleted = command.write().expect("set parses as a write").0.is_none();
            let terminal = TerminalResponse::Success {
                resource_id: None,
                affected: u32::from(!deleted),
            };
            if state.store.finish(&claim, &terminal).await.is_err() {
                let _ = state.store.mark_unknown(&claim).await;
                return reject(Failure::reconciliation());
            }
            settings_write_response(outcome.result, outcome.observed_version, id, false)
        }
        Err(error) => {
            let terminal = match error.code {
                ErrorCode::VersionConflict => {
                    Some(TerminalResponse::Failure(TerminalFailure::VersionConflict))
                }
                ErrorCode::ActionNotAllowed => {
                    Some(TerminalResponse::Failure(TerminalFailure::ActionNotAllowed))
                }
                ErrorCode::Malformed => Some(TerminalResponse::Failure(TerminalFailure::Malformed)),
                _ => None,
            };
            if let Some(terminal) = terminal {
                if state.store.finish(&claim, &terminal).await.is_err() {
                    let _ = state.store.mark_unknown(&claim).await;
                    return reject(Failure::reconciliation());
                }
                return reject(Failure::from_action(error));
            }
            let _ = state.store.mark_unknown(&claim).await;
            reject(Failure::reconciliation())
        }
    }
}

/// Replay a stored `settings.set` terminal without re-executing the write.
/// Success rebuilds the value-free `{key,outcome}` result from the claimed
/// body (the payload hash guarantees it matches the first execution); failures
/// reuse the generic terminal envelope with the replay marker. A version
/// side-read failure fails closed to reconciliation like every other store
/// failure on this path, never a fabricated `version: 0` success.
async fn replay_setting(
    state: &ReceiverState,
    body: &Map<String, Value>,
    response: TerminalResponse,
    id: &str,
    key: KeyLabel,
    action: ActionLabel,
) -> Response {
    match response {
        TerminalResponse::Success { .. } => {
            let rebuilt = SettingsCommand::parse("settings.set", body)
                .map(|command| {
                    let deleted = command.write().expect("claimed set is a write").0.is_none();
                    let outcome = if deleted { "unset" } else { "saved" };
                    json!({"key": command.key(), "outcome": outcome})
                })
                .unwrap_or_else(|_| json!({"key": "", "outcome": "saved"}));
            let version = match current_setting_version(state, body).await {
                Ok(version) => version,
                Err(()) => return state.reject(Failure::reconciliation(), key, action, id),
            };
            settings_write_response(rebuilt, version, id, true)
        }
        TerminalResponse::Failure(_) => state.terminal(response, true, id, key, action),
    }
}

/// CAS token the replayed save committed. The claim stores no version, so
/// report the key's current token: identical absent a later save, and a later
/// save's token otherwise, which a blind retry must see rather than revert.
/// Any failure to determine the token (unparsable claimed body or store
/// error) fails closed: the caller reconciles instead of trusting `0`.
async fn current_setting_version(
    state: &ReceiverState,
    body: &Map<String, Value>,
) -> Result<i64, ()> {
    let command = SettingsCommand::parse("settings.set", body).map_err(|_| ())?;
    let guild_id = two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID;
    let store = SettingsStore::new(state.store.pool());
    let stored = store.get(guild_id, command.key()).await.map_err(|_| ())?;
    Ok(stored.map(|(_, version)| version).unwrap_or(0))
}

/// Legacy `result` (`key`/`value`/`source`) plus the CAS `version` as envelope
/// metadata, never an extra field inside `result`. Zero means absent; otherwise
/// feed it back as `expected_version` for the next save.
fn settings_read_response(result: Value, version: i64, id: &str) -> Response {
    let mut wire = (
        StatusCode::OK,
        Json(json!({"ok": true, "result": result, "version": version, "request_id": id})),
    )
        .into_response();
    wire.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    wire
}

/// Successful saves carry the committed CAS `version` beside `result`, like
/// reads: the website feeds it back as `expected_version` instead of
/// re-reading first, so a save that landed in between cannot be silently
/// reverted by a blind retry.
fn settings_write_response(result: Value, version: i64, id: &str, replayed: bool) -> Response {
    let mut wire = (
        StatusCode::OK,
        Json(json!({"ok": true, "result": result, "version": version, "request_id": id})),
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

/// Keyless `automations.export`: read the guild's stored custom commands and
/// return the versioned redacted document (`version`, `commands` with only
/// `name`/`description`/`template`/`text_trigger`). The flag gate already ran
/// in `authorize`; the committed nonce above is the replay guard, so no
/// idempotency claim is taken. Reads perform no writes, so retries cannot
/// double-apply. `export_document` carries no guild, creator, timestamp,
/// enabled or audit material — redaction is structural, not a field filter.
async fn export_automations(
    state: &ReceiverState,
    _decision: &AuthDecision,
    id: &str,
    key: KeyLabel,
    action: ActionLabel,
) -> Response {
    let reject = |failure| state.reject(failure, key.clone(), action, id);
    let guild_id = two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID;
    let rows = match custom_command_store::list_commands(state.store.pool(), guild_id).await {
        Ok(rows) => rows,
        Err(_) => return reject(Failure::code(ErrorCode::Internal)),
    };
    let document = export_document(&rows);
    let result = match serde_json::to_value(&document) {
        Ok(result) => result,
        Err(_) => return reject(Failure::code(ErrorCode::Internal)),
    };
    let mut wire = (
        StatusCode::OK,
        Json(json!({"ok": true, "result": result, "request_id": id})),
    )
        .into_response();
    wire.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    wire
}

/// Map an import-envelope refusal to a fixed `malformed` error. Messages are
/// the stable contract strings from `automation_transfer` (no entry content);
/// `log_reason` stays a scalar class so no request text reaches the logs.
fn import_parse_error(error: ImportParseError) -> ActionError {
    let (message, reason) = match &error {
        ImportParseError::NotAnImportDocument => (
            "import document must be an array or an object with a commands array",
            "automation_import_not_a_document",
        ),
        ImportParseError::EntriesNotArray => (
            "\"commands\" must be an array of command objects",
            "automation_import_entries_not_array",
        ),
        ImportParseError::TooManyEntries { .. } => (
            "import holds more entries than the guild budget allows",
            "automation_import_too_many_entries",
        ),
        ImportParseError::BadOverwrite => (
            "\"overwrite\" must be a boolean",
            "automation_import_bad_overwrite",
        ),
        ImportParseError::UnsupportedVersion(_) => (
            "unsupported import version, expected 1",
            "automation_import_unsupported_version",
        ),
        ImportParseError::SchedulesNotSupported => (
            "scheduled-message import is not supported yet; remove schedules and import commands only",
            "automation_import_schedules_not_supported",
        ),
    };
    ActionError::new(ErrorCode::Malformed, message, reason)
}

/// `automations.import`: decode + lint + strictly validate the import envelope
/// before any effect, then claim the outer durable idempotency key before the
/// transactional apply. Replaying the stored terminal returns the first
/// `{imported,skipped,conflicts}` result without a second apply, audit row or
/// command rewrite. Destructive `overwrite` needs the separately configured
/// overwrite capability on top of the base automations flag; without it the
/// request refuses as `action_not_allowed` before any claim or audit row.
#[allow(clippy::too_many_arguments)]
async fn import_automations(
    state: &ReceiverState,
    decision: &AuthDecision,
    raw: &[u8],
    idempotency_header: Option<&str>,
    overwrite_allowed: bool,
    id: &str,
    key: KeyLabel,
    action: ActionLabel,
) -> Response {
    let reject = |failure| state.reject(failure, key.clone(), action, id);
    let idempotency = match validate_idempotency_key(idempotency_header, &decision.action) {
        Ok(key) => key.to_owned(),
        Err(error) => return reject(Failure::from_action(error)),
    };
    let actor = match require_snowflake(&decision.body, "actor_id") {
        Ok(actor) => actor.to_owned(),
        Err(error) => return reject(Failure::from_action(error)),
    };
    // The import document rides top-level alongside `action`/`actor_id`:
    // `{action, actor_id, commands, version?, schedules?, overwrite?}`.
    // `parse_import_document` reads only the document fields, so routing keys
    // are ignored without a second parser. Bare-array MEE6 travels as an
    // object with a `commands` array.
    let parsed =
        match parse_import_document(&Value::Object(decision.body.clone()), max_import_entries()) {
            Ok(parsed) => parsed,
            Err(error) => return reject(Failure::from_action(import_parse_error(error))),
        };
    if parsed.overwrite && !overwrite_allowed {
        return reject(Failure::from_action(ActionError::new(
            ErrorCode::ActionNotAllowed,
            "Destructive automation imports are not enabled on this bot",
            "automation_overwrite_not_allowed",
        )));
    }
    let guild_id = two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID;
    let subject = AuditSubject {
        guild_id: Some(DiscordId::new(guild_id).expect("staging guild ID")),
        actor_id: Some(DiscordId::new(&actor).expect("validated actor ID")),
        ..AuditSubject::default()
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
            return replay_import(
                state,
                &decision.body,
                &idempotency,
                response,
                id,
                key,
                action,
            )
            .await;
        }
        Ok(InternalClaim::Mismatch) => return reject(Failure::code(ErrorCode::VersionConflict)),
        Ok(InternalClaim::InFlight) => return reject(Failure::code(ErrorCode::InProgress)),
        Ok(InternalClaim::NeedsReconciliation) => return reject(Failure::reconciliation()),
        Err(_) => return reject(Failure::code(ErrorCode::Internal)),
    };
    let at = format_iso_millis(now_ms() as i64);
    match custom_command_service::import(
        state.store.pool(),
        guild_id,
        &actor,
        &parsed,
        overwrite_allowed,
        &idempotency,
        &at,
    )
    .await
    {
        Ok(outcome) => {
            let affected = u32::try_from(outcome.imported).unwrap_or(u32::MAX);
            let terminal = TerminalResponse::Success {
                resource_id: None,
                affected,
            };
            if state.store.finish(&claim, &terminal).await.is_err() {
                let _ = state.store.mark_unknown(&claim).await;
                return reject(Failure::reconciliation());
            }
            import_success_response(&outcome, id, false)
        }
        Err(ImportServiceError::OverwriteNotAllowed) => {
            let terminal = TerminalResponse::Failure(TerminalFailure::ActionNotAllowed);
            if state.store.finish(&claim, &terminal).await.is_err() {
                let _ = state.store.mark_unknown(&claim).await;
                return reject(Failure::reconciliation());
            }
            reject(Failure::code(ErrorCode::ActionNotAllowed))
        }
        Err(
            ImportServiceError::Parse(_)
            | ImportServiceError::OverCapacity(_)
            | ImportServiceError::Validation(_),
        ) => {
            let terminal = TerminalResponse::Failure(TerminalFailure::Malformed);
            if state.store.finish(&claim, &terminal).await.is_err() {
                let _ = state.store.mark_unknown(&claim).await;
                return reject(Failure::reconciliation());
            }
            reject(Failure::code(ErrorCode::Malformed))
        }
        Err(ImportServiceError::Storage(_)) => {
            let _ = state.store.mark_unknown(&claim).await;
            reject(Failure::reconciliation())
        }
    }
}

/// Replay a stored `automations.import` terminal without re-executing the
/// apply. Success returns the first `{imported,skipped,conflicts}` result
/// verbatim from the `{idempotency}#summary` audit row the first apply
/// committed (no writes, no new audit rows): a blind retry after a lost
/// response sees exactly what the first call reported, even when an admin
/// edited a command out of band between the two calls. When the summary row
/// is missing or unparseable (a receipt predating the conflict-names edge),
/// the result rebuilds from a read-only diff instead. Failures reuse the
/// generic terminal envelope with the replay marker.
async fn replay_import(
    state: &ReceiverState,
    body: &Map<String, Value>,
    idempotency: &str,
    response: TerminalResponse,
    id: &str,
    key: KeyLabel,
    action: ActionLabel,
) -> Response {
    match response {
        TerminalResponse::Success { affected, .. } => {
            let imported = usize::try_from(affected).unwrap_or(usize::MAX);
            let stored =
                stored_import_outcome(state, &format!("{idempotency}#summary"), imported).await;
            let rebuilt = match stored {
                Some(outcome) => outcome,
                None => rebuilt_import_outcome(state, body, imported).await,
            };
            import_success_response(&rebuilt, id, true)
        }
        TerminalResponse::Failure(_) => state.terminal(response, true, id, key, action),
    }
}

/// Load the first `{imported,skipped,conflicts}` result from the summary audit
/// row. Read-only; `None` on any miss or mismatch so the caller falls back to
/// the diff rebuild.
async fn stored_import_outcome(
    state: &ReceiverState,
    summary_id: &str,
    imported: usize,
) -> Option<ImportOutcome> {
    let (outcome, reason) = custom_command_store::load_audit(state.store.pool(), summary_id)
        .await
        .ok()??;
    parse_stored_import_outcome(&outcome, reason.as_deref(), imported)
}

/// Parse a summary row back into the first result. The outcome string keeps
/// the stable `imported:N,skipped:S,conflicts:C` shape and `reason` carries
/// the conflict names as a JSON array; anything else (including a count/name
/// mismatch or an `imported` that disagrees with the stored receipt) is `None`.
fn parse_stored_import_outcome(
    outcome: &str,
    reason: Option<&str>,
    imported: usize,
) -> Option<ImportOutcome> {
    let (counts, rest) = outcome.split_once("imported:")?;
    if !counts.is_empty() {
        return None;
    }
    let (imported_raw, rest) = rest.split_once(",skipped:")?;
    let (skipped_raw, rest) = rest.split_once(",conflicts:")?;
    if rest.contains(',') {
        return None;
    }
    let stored_imported: usize = imported_raw.parse().ok()?;
    let skipped: usize = skipped_raw.parse().ok()?;
    let conflict_count: usize = rest.parse().ok()?;
    if stored_imported != imported {
        return None;
    }
    let conflicts: Vec<String> = match reason {
        Some(names) => serde_json::from_str(names).ok()?,
        // Rows predating the conflict-names edge carry no reason; with a
        // zero count the empty list is still the verbatim first result.
        None if conflict_count == 0 => Vec::new(),
        None => return None,
    };
    if conflicts.len() != conflict_count {
        return None;
    }
    Some(ImportOutcome {
        imported,
        skipped,
        conflicts,
    })
}

/// Read-only rebuild of an import result for replay, used only when the
/// summary audit row is missing or unparseable (see [`stored_import_outcome`]).
/// Never writes. Falls back to the stored `imported` count with empty
/// `skipped`/`conflicts` when the claimed body no longer parses or the store
/// is unavailable — still no second apply.
async fn rebuilt_import_outcome(
    state: &ReceiverState,
    body: &Map<String, Value>,
    imported: usize,
) -> ImportOutcome {
    let fallback = || ImportOutcome {
        imported,
        skipped: 0,
        conflicts: Vec::new(),
    };
    let Ok(parsed) = parse_import_document(&Value::Object(body.clone()), max_import_entries())
    else {
        return fallback();
    };
    let guild_id = two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID;
    let Ok(rows) = custom_command_store::list_commands(state.store.pool(), guild_id).await else {
        return fallback();
    };
    let Ok(diff) = diff_import(&rows, &parsed, &reserved_command_names(), parsed.overwrite) else {
        return fallback();
    };
    let mut conflicts = parsed.translation_conflicts.clone();
    conflicts.extend(
        diff.rejected
            .iter()
            .filter(|rejection| {
                matches!(
                    rejection.code.as_str(),
                    "would_overwrite" | "trigger_in_use"
                )
            })
            .map(|rejection| rejection.name.clone()),
    );
    ImportOutcome {
        imported,
        skipped: parsed.invalid_entries + diff.rejected.len(),
        conflicts,
    }
}

/// Successful imports carry the transactional counts on the wire. The stored
/// receipt keeps only `affected = imported`; `skipped`/`conflicts` replay
/// from the summary audit row without a second apply, rebuilding from a
/// read-only diff only as a fallback (see [`replay_import`]).
fn import_success_response(outcome: &ImportOutcome, id: &str, replayed: bool) -> Response {
    let mut wire = (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "result": {
                "imported": outcome.imported,
                "skipped": outcome.skipped,
                "conflicts": outcome.conflicts,
            },
            "request_id": id,
        })),
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
            TerminalFailure::VersionConflict => Failure::code(ErrorCode::VersionConflict),
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
