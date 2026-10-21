//! Member moderation commands (`/ban`, `/tempban`, `/kick`, `/timeout`,
//! `/warn`) plus the tempban unban sweep.
//!
//! Thin wiring over the `member_moderation` domain + `PgMemberModerationStore`:
//! decode options → resolve actor/target role facts over REST → validate →
//! exactly-once execute through the shared REST executor → ephemeral outcome.
//! Routing, the `TWO_MODERATION` gate and the handler-level permission check
//! all happen in the shared router before these run. Dispatch still derives
//! current authority independently: routing is not a freshness guarantee.
//!
//! The runtime holds the ONE guild store for this process: command paths
//! clone it, and the sweep job shares the same `Arc`, so bans and unbans for
//! one member serialize through one set of local queues. Role facts come
//! from strict current guild/member reads after that lane wait; interaction
//! roles and permissions never authorize an effect. Incomplete evidence refuses.
//! Until the authenticated audit-reason seam lands, the Discord reason is the
//! plain moderator reason.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use sqlx::{Pool, Postgres};
use tokio::sync::OnceCell;
use twilight_model::{
    application::interaction::{
        application_command::CommandOptionValue, Interaction, InteractionData,
    },
    id::{marker::UserMarker, Id},
};
use two_bot_core::{
    member_moderation::{
        DiscordError as MemberDiscordError, MemberAuthority, MemberError, MemberExecution,
        MemberModerationService, MemberOutcome, MemberResult, UNBAN_SWEEP_INTERVAL_SECONDS,
    },
    member_moderation_store::PgMemberModerationStore,
    moderation::{
        ModerationAction, ModerationActor, ModerationGates, ModerationPolicy, ModerationTarget,
    },
};
use two_bot_discord::ActionExecutor;

use crate::{
    command_runtime::{actor_id, new_id},
    jobs::{self, ErrorClass, Job},
};

/// Supervised unban-sweep job name (status surface + registration).
pub const SWEEP_JOB_NAME: &str = "member_unban_sweep";
/// Job names owned by this slice.
pub const NAMES: [&str; 1] = [SWEEP_JOB_NAME];

/// Sweep attempt budget: 25 dispatches worst case, each with at most the
/// legacy 5 s REST abort plus ledger writes, fits with headroom.
const SWEEP_TIMEOUT: Duration = Duration::from_secs(180);

/// Safe reply when facts, store or reconciliation reads fail — never leak
/// REST/SQL internals to Discord.
const FAILURE_REPLY: &str = "Moderation command failed; try again.";
/// Reply when kick/timeout/warn names a user with no guild membership. Bans
/// tolerate absent members (Discord bans by id); the other verbs need one.
const NO_MEMBER_REPLY: &str = "That member is not in this server.";
/// Reply when Discord left the outcome unknowable: the claim stays fenced,
/// so a blind retry could double-moderate. Reconciliation owns the next step.
const UNCERTAIN_REPLY: &str = "Discord did not confirm the action. It may still land; ask an operator to reconcile before retrying.";
/// Fixed reply when Discord refuses the request: identical for every detail,
/// naming Discord with a next action. The full detail goes to the server log
/// with the interaction/request ids, never to the reply.
const REJECTED_REPLY: &str =
    "Discord refused the request; nothing was applied. Check the bot's permissions or ask an operator.";
const ACTOR_COOLDOWN: Duration = Duration::from_secs(5);
const ACTOR_COOLDOWN_CAPACITY: usize = 1024;
const COOLDOWN_REPLY: &str = "Wait 5 seconds before using another member moderation command.";

#[derive(Default)]
struct ActorCooldowns {
    actors: HashMap<String, (u64, Instant)>,
}

impl ActorCooldowns {
    fn admit(&mut self, actor: &str, interaction: u64, now: Instant) -> bool {
        self.actors.retain(|_, (_, until)| *until > now);
        if let Some((previous, _)) = self.actors.get(actor) {
            // Discord redelivery must still reach the durable replay check.
            return *previous == interaction;
        }
        if self.actors.len() >= ACTOR_COOLDOWN_CAPACITY {
            return false;
        }
        self.actors
            .insert(actor.to_owned(), (interaction, now + ACTOR_COOLDOWN));
        true
    }
}

/// One guild's member-moderation consumer: the shared ledger store, the static
/// policy and the cached bot identity. Built once at boot and cloned into
/// command dispatch and the sweep job — never a second consumer per guild.
pub struct MemberRuntime {
    store: PgMemberModerationStore,
    policy: ModerationPolicy,
    guild_id: String,
    bot_id: OnceCell<String>,
    actor_cooldowns: Mutex<ActorCooldowns>,
}

impl MemberRuntime {
    /// Compose the guild consumer, or park when moderation is off, invalidly
    /// configured, or aimed at a non-staging guild. Staging-only until soak:
    /// enabling is a deployment decision, and the default stays off.
    pub fn from_env(pool: Pool<Postgres>, guild_id: u64) -> Option<Arc<Self>> {
        let gates = match ModerationGates::from_env() {
            Ok(gates) => gates,
            Err(err) => {
                tracing::warn!(error = %err, "moderation gates invalid; member moderation disabled");
                return None;
            }
        };
        if !gates.enabled {
            return None;
        }
        let guild_id = guild_id.to_string();
        if !crate::self_role_handlers::staging_allowlist().contains(&guild_id) {
            tracing::warn!("moderation guild is not the staging guild; member moderation disabled");
            return None;
        }
        Some(Arc::new(Self {
            store: PgMemberModerationStore::new(pool, guild_id.clone()),
            policy: ModerationPolicy {
                owen_user_id: gates.owen_user_id,
                protected_role_ids: gates.protected_role_ids,
                bot_user_id: None,
            },
            guild_id,
            bot_id: OnceCell::new(),
            actor_cooldowns: Mutex::new(ActorCooldowns::default()),
        }))
    }

    /// Test-only consumer over an explicit pool; skips the `TWO_MODERATION`
    /// and staging gates so router tests can attach the verbs without process
    /// env or a live database (the lazy pool stays unused on the covered
    /// fail-closed paths).
    #[cfg(test)]
    pub(crate) fn for_test(pool: Pool<Postgres>, guild_id: &str) -> Arc<Self> {
        Arc::new(Self {
            store: PgMemberModerationStore::new(pool, guild_id.to_owned()),
            policy: ModerationPolicy {
                owen_user_id: "1".to_owned(),
                protected_role_ids: std::collections::HashSet::new(),
                bot_user_id: None,
            },
            guild_id: guild_id.to_owned(),
            bot_id: OnceCell::new(),
            actor_cooldowns: Mutex::new(ActorCooldowns::default()),
        })
    }

    /// Clone the shared guild store (commands and sweep share one consumer).
    pub(crate) fn store(&self) -> PgMemberModerationStore {
        self.store.clone()
    }

    /// Policy stamped with the resolved bot identity; callers resolve it first.
    fn policy(&self, bot_id: &str) -> ModerationPolicy {
        ModerationPolicy {
            owen_user_id: self.policy.owen_user_id.clone(),
            protected_role_ids: self.policy.protected_role_ids.clone(),
            bot_user_id: Some(bot_id.to_owned()),
        }
    }

    /// Bot user id, resolved once over REST and cached. A failed lookup fails
    /// the caller: hierarchy checks must not run against an unknown bot.
    async fn bot_id(&self, executor: &ActionExecutor) -> Result<&str, ()> {
        self.bot_id
            .get_or_try_init(|| async {
                executor
                    .current_bot_user_id()
                    .await
                    .map(|id| id.to_string())
                    .map_err(|_| ())
            })
            .await
            .map(String::as_str)
    }

    /// User-facing error copy with server-side truth: a Discord failure logs
    /// its full detail at WARN with the interaction and request ids on the
    /// same event, then returns exactly [`error_text`]. The `?` sigil keeps
    /// multi-line detail on one log line. Non-Discord arms carry no hidden
    /// detail, so they log nothing.
    pub(crate) fn error_text_with_log(
        err: MemberError,
        interaction_id: u64,
        request_id: &str,
    ) -> String {
        if let MemberError::Discord(discord_err) = &err {
            tracing::warn!(
                interaction_id,
                request_id,
                detail = ?discord_err,
                "member moderation discord failure"
            );
        }
        error_text(err)
    }

    /// Success reply naming the sanitized target: the verb base first, the
    /// [`sanitize_target_display`] slot next, and exactly ` Already applied.`
    /// last on replay.
    pub(crate) fn outcome_text_with_target(result: &MemberResult, target_display: &str) -> String {
        let base = match result.outcome {
            MemberOutcome::Banned => "Banned",
            MemberOutcome::TemporarilyBanned => "Temporarily banned",
            MemberOutcome::Kicked => "Kicked",
            MemberOutcome::TimedOut => "Timed out",
            MemberOutcome::Warned => "Warned",
            MemberOutcome::Unbanned => "Unbanned",
        };
        let target = sanitize_target_display(target_display);
        if result.replayed {
            format!("{base} {target}. Already applied.")
        } else {
            format!("{base} {target}.")
        }
    }
}

/// Current epoch millis as the service clock expects.
fn now_millis_i64() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

/// Role positions plus the guild owner, read live per command.
struct GuildFacts {
    guild_id: String,
    owner_id: String,
    positions: HashMap<String, i64>,
    permissions: HashMap<String, u64>,
}

fn text(value: &serde_json::Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_owned)
}

/// One `GET /guilds/{id}`: owner id plus every role position. Any malformed
/// or missing piece fails the command — hierarchy never runs on a partial
/// snapshot.
async fn guild_facts(executor: &ActionExecutor, guild_id: &str) -> Result<GuildFacts, ()> {
    let guild = executor
        .get_json_strict(&format!("/guilds/{guild_id}"))
        .await
        .map_err(|_| ())?
        .ok_or(())?;
    if text(&guild, "id").as_deref() != Some(guild_id) {
        return Err(());
    }
    let owner_id = text(&guild, "owner_id")
        .filter(|id| valid_id(id))
        .ok_or(())?;
    let mut positions = HashMap::new();
    let mut permissions = HashMap::new();
    for role in guild
        .get("roles")
        .and_then(serde_json::Value::as_array)
        .ok_or(())?
    {
        let id = text(role, "id").filter(|id| valid_id(id)).ok_or(())?;
        let position = role
            .get("position")
            .and_then(serde_json::Value::as_i64)
            .filter(|position| *position >= 0)
            .ok_or(())?;
        let bits = text(role, "permissions")
            .ok_or(())?
            .parse::<u64>()
            .map_err(|_| ())?;
        if positions.insert(id.clone(), position).is_some() {
            return Err(());
        }
        permissions.insert(id, bits);
    }
    if !positions.contains_key(guild_id) {
        return Err(());
    }
    Ok(GuildFacts {
        guild_id: guild_id.to_owned(),
        owner_id,
        positions,
        permissions,
    })
}

fn valid_id(id: &str) -> bool {
    id.parse::<u64>()
        .is_ok_and(|value| value != 0 && value.to_string() == id)
}

/// Guild-wide permissions only. Check every held role even for owners/admins;
/// an unknown role is incomplete evidence, not an implicit privilege grant.
/// Owners and administrators hold every permission, but only the guild owner
/// is exempt from role hierarchy: an administrator keeps the real top position.
fn actor_authority(
    user_id: &str,
    held: Vec<String>,
    facts: &GuildFacts,
) -> Option<ModerationActor> {
    let roles = with_everyone(held, &facts.guild_id);
    let top = top_position(&roles, facts)?;
    let mut permissions = 0;
    for role in &roles {
        permissions |= *facts.permissions.get(role)?;
    }
    let owner = user_id == facts.owner_id;
    Some(ModerationActor {
        user_id: user_id.to_owned(),
        role_ids: roles,
        highest_role_position: if owner { i64::MAX } else { top },
        permissions: if owner || permissions & (1 << 3) != 0 {
            u64::MAX
        } else {
            permissions
        },
    })
}

/// Highest position across the held roles plus `@everyone`. Unknown held
/// roles fail closed: the snapshot is incomplete, never unprotected.
fn top_position(held: &[String], facts: &GuildFacts) -> Option<i64> {
    let mut top = *facts.positions.get(&facts.guild_id)?;
    for role in held {
        top = top.max(*facts.positions.get(role)?);
    }
    Some(top)
}

/// `/ban` etc. options: `target` (user), `reason` (string),
/// `duration_seconds` (integer, tempban/timeout only). Missing values decode
/// to `None`; validation owns the refusal text.
fn command_options(
    interaction: &Interaction,
) -> (Option<Id<UserMarker>>, Option<String>, Option<i64>) {
    let mut target = None;
    let mut reason = None;
    let mut duration_seconds = None;
    let Some(InteractionData::ApplicationCommand(data)) = &interaction.data else {
        return (target, reason, duration_seconds);
    };
    for option in &data.options {
        match (option.name.as_str(), &option.value) {
            ("target", CommandOptionValue::User(id)) => target = Some(*id),
            ("reason", CommandOptionValue::String(value)) => reason = Some(value.clone()),
            ("duration_seconds", CommandOptionValue::Integer(value)) => {
                duration_seconds = Some(*value);
            }
            _ => {}
        }
    }
    (target, reason, duration_seconds)
}

/// Membership plus bot flag from `GET /guilds/{g}/members/{u}`. `None` is a
/// departed or never-joined user (ban tolerates; kick/timeout/warn refuse).
/// An id mismatch or malformed body fails closed.
async fn fetched_member(
    executor: &ActionExecutor,
    guild_id: &str,
    target: &str,
) -> Result<Option<(Vec<String>, bool)>, ()> {
    let member = match executor
        .get_json_strict(&format!("/guilds/{guild_id}/members/{target}"))
        .await
        .map_err(|_| ())?
    {
        Some(member) => member,
        None => return Ok(None),
    };
    parse_member(&member, target).map(Some)
}

fn parse_member(member: &serde_json::Value, target: &str) -> Result<(Vec<String>, bool), ()> {
    let user = member.get("user").ok_or(())?;
    if text(user, "id").as_deref() != Some(target) {
        return Err(());
    }
    let mut roles = Vec::new();
    for role in member
        .get("roles")
        .and_then(serde_json::Value::as_array)
        .ok_or(())?
    {
        roles.push(role.as_str().ok_or(())?.to_owned());
    }
    if !roles.iter().all(|role| valid_id(role)) {
        return Err(());
    }
    Ok((
        roles,
        // Discord omits this optional field for ordinary users. A present
        // malformed value is not a valid negative bot assertion.
        match user.get("bot") {
            None => false,
            Some(value) => value.as_bool().ok_or(())?,
        },
    ))
}

/// Execute one member verb for an already-routed, already-deferred
/// interaction, then edit the defer with the outcome.
pub(crate) async fn handle_command(
    member: &MemberRuntime,
    executor: &ActionExecutor,
    interaction: &Interaction,
    action: ModerationAction,
) {
    let reply = match execute(member, executor, interaction, action).await {
        Ok(result) => {
            let target = target_display(interaction);
            MemberRuntime::outcome_text_with_target(&result, &target)
        }
        Err(reply) => reply,
    };
    if let Err(err) = executor
        .edit_interaction_response(interaction.application_id.get(), &interaction.token, &reply)
        .await
    {
        tracing::warn!(interaction_id = %interaction.id.get(), error = %err, "member command reply edit failed");
    }
}

/// Moderator-visible target slot: the resolved interaction user's display
/// name when Discord sent it, else the raw target user id. The reply
/// sanitizer neutralizes anything hostile in either.
fn target_display(interaction: &Interaction) -> String {
    let target = command_options(interaction).0;
    if let (Some(id), Some(InteractionData::ApplicationCommand(data))) =
        (target, interaction.data.as_ref())
    {
        if let Some(user) = data.resolved.as_ref().and_then(|r| r.users.get(&id)) {
            return user.global_name.as_deref().unwrap_or(&user.name).to_owned();
        }
        return id.get().to_string();
    }
    String::new()
}

/// Ephemeral-reply sanitizer for a moderator-visible display name: fold
/// newlines to one space, neutralize mass mentions through the shared
/// helper, split mention pills so they render literally, then cap the slot
/// at 512 chars on char boundaries. Markdown-escaping is NOT applied here.
fn sanitize_target_display(display: &str) -> String {
    let folded = display.replace("\r\n", " ").replace(['\r', '\n'], " ");
    let neutralized = two_bot_core::message_safety::neutralize_mentions(&folded);
    let split = neutralized
        .replace("<@", "<\u{200b}@")
        .replace("<#", "<\u{200b}#")
        .replace("<:", "<\u{200b}:")
        .replace("<a:", "<\u{200b}a:");
    split.chars().take(512).collect()
}

fn error_text(err: MemberError) -> String {
    match err {
        MemberError::Policy(policy) => policy.to_string(),
        MemberError::Reason(reason) => reason.to_string(),
        MemberError::Malformed { field, message } => format!("malformed {field}: {message}"),
        MemberError::InFlight => {
            "A moderation action for this member is already in progress.".to_owned()
        }
        MemberError::KeyMismatch => {
            "This moderation request was already submitted with different details.".to_owned()
        }
        MemberError::Discord(MemberDiscordError::Rejected(_)) => REJECTED_REPLY.to_owned(),
        MemberError::Discord(MemberDiscordError::RateLimited) => {
            "Discord rate-limited the request; nothing was applied. Try again shortly.".to_owned()
        }
        MemberError::Discord(_) => UNCERTAIN_REPLY.to_owned(),
        MemberError::TargetNotMember => NO_MEMBER_REPLY.to_owned(),
        MemberError::Store(_)
        | MemberError::BotIdentityUnknown
        | MemberError::AuthorityUnavailable => FAILURE_REPLY.to_owned(),
    }
}

/// Build the execution and run it exactly once. Every `Err(String)` here is
/// already the user-facing ephemeral text.
async fn execute(
    member: &MemberRuntime,
    executor: &ActionExecutor,
    interaction: &Interaction,
    action: ModerationAction,
) -> Result<MemberResult, String> {
    let actor_id = actor_id(interaction);
    if actor_id.is_empty() {
        return Err(FAILURE_REPLY.to_owned());
    }
    let admitted = member
        .actor_cooldowns
        .lock()
        .map_err(|_| FAILURE_REPLY.to_owned())?
        .admit(&actor_id, interaction.id.get(), Instant::now());
    if !admitted {
        tracing::info!(actor = %actor_id, guild = %member.guild_id, "member moderation cooldown refused");
        return Err(COOLDOWN_REPLY.to_owned());
    }
    let (target, reason, duration_seconds) = command_options(interaction);
    let guild_id = member.guild_id.clone();
    // Identity and command content only: no interaction-time authority survives
    // into dispatch. The callback below fills these deliberately empty facts.
    let execution = MemberExecution {
        action,
        guild_id,
        actor: ModerationActor {
            user_id: actor_id,
            role_ids: Vec::new(),
            highest_role_position: 0,
            permissions: 0,
        },
        target: target.map(|id| ModerationTarget {
            user_id: id.to_string(),
            role_ids: Vec::new(),
            highest_role_position: 0,
            is_bot: false,
            is_guild_owner: false,
        }),
        bot_highest_role_position: 0,
        reason: reason.unwrap_or_default(),
        duration_seconds,
        request_id: new_id(),
        idempotency_key: interaction.id.get().to_string(),
    };
    let bot_id = member.bot_id(executor).await.ok();
    let policy = bot_id.map_or_else(|| member.policy.clone(), |id| member.policy(id));
    let service =
        MemberModerationService::new(executor.clone(), member.store(), policy, now_millis_i64);
    let result = service
        .execute_with_authority(&execution, || async {
            if interaction.guild_id.map(|id| id.to_string()).as_deref() != Some(&member.guild_id) {
                return Err(MemberError::AuthorityUnavailable);
            }
            current_authority(
                executor,
                &execution,
                bot_id.ok_or(MemberError::AuthorityUnavailable)?,
            )
            .await
        })
        .await;
    match result {
        Ok(result) => {
            two_bot_core::metrics::global()
                .moderation_enforcement(action.command_name(), "success");
            Ok(result)
        }
        Err(err) => {
            two_bot_core::metrics::global()
                .moderation_enforcement(action.command_name(), err.enforcement_outcome());
            Err(MemberRuntime::error_text_with_log(
                err,
                interaction.id.get(),
                &execution.request_id,
            ))
        }
    }
}

fn with_everyone(mut roles: Vec<String>, guild_id: &str) -> Vec<String> {
    if !roles.iter().any(|role| role == guild_id) {
        roles.push(guild_id.to_owned());
    }
    roles
}

/// Runs inside the existing ban/tempban lane, or immediately before the
/// nonserialized verbs. Every GET is the shared executor's bounded strict read;
/// a transport/403/5xx failure is never interpreted as absent membership.
async fn current_authority(
    executor: &ActionExecutor,
    exec: &MemberExecution,
    bot_id: &str,
) -> Result<MemberAuthority, MemberError> {
    let unavailable = || MemberError::AuthorityUnavailable;
    let facts = guild_facts(executor, &exec.guild_id)
        .await
        .map_err(|_| unavailable())?;
    let (actor_roles, _) = fetched_member(executor, &exec.guild_id, &exec.actor.user_id)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(unavailable)?;
    let actor =
        actor_authority(&exec.actor.user_id, actor_roles, &facts).ok_or_else(unavailable)?;
    let target = exec
        .target
        .as_ref()
        .ok_or(two_bot_core::moderation::PolicyError::MissingTarget)?;
    let (target_roles, is_bot) = target_membership(
        exec.action,
        fetched_member(executor, &exec.guild_id, &target.user_id)
            .await
            .map_err(|_| unavailable())?,
    )?;
    let target = ModerationTarget {
        user_id: target.user_id.clone(),
        highest_role_position: top_position(&target_roles, &facts).ok_or_else(unavailable)?,
        role_ids: with_everyone(target_roles, &exec.guild_id),
        is_bot,
        is_guild_owner: target.user_id == facts.owner_id,
    };
    let (bot_roles, is_bot) = fetched_member(executor, &exec.guild_id, bot_id)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(unavailable)?;
    if !is_bot {
        return Err(unavailable());
    }
    let bot_highest_role_position = top_position(&bot_roles, &facts).ok_or_else(unavailable)?;
    Ok(MemberAuthority {
        actor,
        target: Some(target),
        bot_highest_role_position,
    })
}

/// What a strict target-member read means per verb. `None` is a confirmed
/// departed or never-joined user: ban/tempban may still act on that nonmember,
/// while kick/timeout/warn have nothing to act on and refuse (kick would
/// otherwise treat the 404 as success, warn would persist a nonmember warning).
fn target_membership(
    action: ModerationAction,
    fetched: Option<(Vec<String>, bool)>,
) -> Result<(Vec<String>, bool), MemberError> {
    match fetched {
        Some(member) => Ok(member),
        None if matches!(action, ModerationAction::Ban | ModerationAction::TempBan) => {
            Ok((Vec::new(), false))
        }
        None => Err(MemberError::TargetNotMember),
    }
}

/// The supervised unban sweep for the configured guild: claim one due job
/// immediately before each dispatch (at most 25 per tick, enforced inside
/// `run_due_unbans_summary`), recording per-job failures and continuing.
/// Definite refusals requeue for the next tick; uncertain dispatches stay
/// fenced for reconciliation.
pub(crate) fn sweep_job(member: Arc<MemberRuntime>, rest: ActionExecutor) -> Job {
    let cadence = Duration::from_secs(UNBAN_SWEEP_INTERVAL_SECONDS);
    Job {
        name: NAMES[0],
        cadence,
        cadence_source: None,
        startup_jitter: jobs::startup_jitter(cadence, rand::random()),
        timeout: SWEEP_TIMEOUT,
        action: Arc::new(move || {
            let member = member.clone();
            let rest = rest.clone();
            Box::pin(async move { tick(&member, &rest).await })
        }),
    }
}

async fn tick(member: &MemberRuntime, rest: &ActionExecutor) -> Result<(), ErrorClass> {
    // Scheduled unbans are attributed to the bot, so no identity means no sweep.
    let bot_id = member.bot_id(rest).await.map_err(|()| ErrorClass::Rest)?;
    let service = MemberModerationService::new(
        rest.clone(),
        member.store(),
        member.policy(bot_id),
        now_millis_i64,
    );
    match service.run_due_unbans_summary(&member.guild_id).await {
        Ok((completed, failed)) => {
            if completed > 0 || failed > 0 {
                tracing::info!(completed, failed, guild = %member.guild_id, "member unban sweep completed jobs");
            }
            Ok(())
        }
        Err(MemberError::Store(_)) => Err(ErrorClass::Database),
        Err(MemberError::Discord(_)) => Err(ErrorClass::Rest),
        Err(_) => Err(ErrorClass::Configuration),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use two_bot_core::{
        member_moderation::StoreError,
        moderation::{PolicyError, ReasonError},
    };

    #[test]
    fn member_evidence_requires_identity_and_complete_roles_but_bot_is_optional() {
        use serde_json::json;
        assert_eq!(
            parse_member(&json!({"user": {"id": "88"}, "roles": []}), "88"),
            Ok((vec![], false))
        );
        for body in [
            json!({"roles": []}),
            json!({"user": {"id": "99"}, "roles": []}),
            json!({"user": {"id": "88"}}),
            json!({"user": {"id": "88"}, "roles": [42]}),
            json!({"user": {"id": "88"}, "roles": ["0"]}),
            json!({"user": {"id": "88", "bot": "false"}, "roles": []}),
        ] {
            assert!(parse_member(&body, "88").is_err(), "{body}");
        }
        for id in ["", "0", "01", "+1", "role", "18446744073709551616"] {
            assert!(!valid_id(id));
        }
        assert!(valid_id("88"));
        assert!(valid_id("18446744073709551615"));
    }

    #[test]
    fn current_permissions_expand_owners_and_admins_only_with_complete_role_evidence() {
        let mut facts = GuildFacts {
            guild_id: "2222".into(),
            owner_id: "99".into(),
            positions: HashMap::from([("2222".into(), 0), ("101".into(), 50)]),
            permissions: HashMap::from([("2222".into(), 4), ("101".into(), 2)]),
        };
        let actor = actor_authority("88", vec!["101".into()], &facts).expect("complete");
        assert_eq!(actor.permissions, 6);
        assert_eq!(actor.highest_role_position, 50);
        assert_eq!(actor.role_ids, vec!["101", "2222"]);
        assert_eq!(with_everyone(vec!["2222".into()], "2222"), vec!["2222"]);
        facts.permissions.insert("101".into(), 8);
        // Administrator expands permissions only: hierarchy keeps the real top
        // role, so this actor stays below a target at position 60.
        let admin = actor_authority("88", vec!["101".into()], &facts).expect("admin");
        assert_eq!(
            (admin.permissions, admin.highest_role_position),
            (u64::MAX, 50)
        );
        let everyone_only = actor_authority("88", vec![], &facts).expect("everyone only");
        assert_eq!(
            everyone_only.permissions, 4,
            "no admin bit without the role"
        );
        // Only the guild owner is exempt from role hierarchy.
        let owner = actor_authority("99", vec![], &facts).expect("owner");
        assert_eq!(
            (owner.permissions, owner.highest_role_position),
            (u64::MAX, i64::MAX)
        );
        assert!(actor_authority("99", vec!["unknown".into()], &facts).is_none());
        facts.permissions.remove("101");
        assert!(actor_authority("99", vec!["101".into()], &facts).is_none());
        facts.permissions.remove("2222");
        assert!(actor_authority("99", vec![], &facts).is_none());
        assert_eq!(
            MemberError::AuthorityUnavailable.enforcement_outcome(),
            "refused"
        );
        assert_eq!(
            MemberError::TargetNotMember.enforcement_outcome(),
            "refused"
        );
        assert_eq!(error_text(MemberError::AuthorityUnavailable), FAILURE_REPLY);
        assert_eq!(error_text(MemberError::TargetNotMember), NO_MEMBER_REPLY);
    }

    #[test]
    fn departed_target_is_tolerated_only_by_ban_and_tempban() {
        let present = Some((vec!["101".to_owned()], true));
        for action in [
            ModerationAction::Ban,
            ModerationAction::TempBan,
            ModerationAction::Kick,
            ModerationAction::Timeout,
            ModerationAction::Warn,
        ] {
            // A present member always passes through with its own roles and flag.
            assert_eq!(
                target_membership(action, present.clone()).ok(),
                present,
                "{action:?}"
            );
            let departed = target_membership(action, None);
            if matches!(action, ModerationAction::Ban | ModerationAction::TempBan) {
                assert_eq!(departed.ok(), Some((Vec::new(), false)), "{action:?}");
            } else {
                assert!(
                    matches!(departed, Err(MemberError::TargetNotMember)),
                    "{action:?}: a departed target must refuse, never act"
                );
            }
        }
    }

    #[test]
    fn unknown_bot_role_or_everyone_position_refuses() {
        let mut facts = GuildFacts {
            guild_id: "guild".to_owned(),
            owner_id: "owner".to_owned(),
            positions: HashMap::from([("guild".to_owned(), 0), ("bot-role".to_owned(), 10)]),
            permissions: HashMap::new(),
        };
        assert_eq!(top_position(&["bot-role".to_owned()], &facts), Some(10));
        assert_eq!(top_position(&["unknown".to_owned()], &facts), None);
        facts.positions.remove("guild");
        assert_eq!(top_position(&["bot-role".to_owned()], &facts), None);
    }

    #[test]
    fn actor_cooldown_spans_verbs_but_preserves_redelivery() {
        let mut cooldowns = ActorCooldowns::default();
        let now = Instant::now();
        assert!(cooldowns.admit("actor", 1, now));
        assert!(cooldowns.admit("actor", 1, now));
        assert!(!cooldowns.admit("actor", 2, now));
        assert!(cooldowns.admit("another-actor", 2, now));
        assert!(!cooldowns.admit("actor", 2, now + ACTOR_COOLDOWN - Duration::from_nanos(1)));
        assert!(cooldowns.admit("actor", 2, now + ACTOR_COOLDOWN));
    }

    #[test]
    fn actor_cooldown_is_bounded_and_expired_entries_are_reusable() {
        let mut cooldowns = ActorCooldowns::default();
        let now = Instant::now();
        for actor in 0..ACTOR_COOLDOWN_CAPACITY {
            assert!(cooldowns.admit(&actor.to_string(), 1, now));
        }
        assert!(!cooldowns.admit("overflow", 1, now));
        assert_eq!(cooldowns.actors.len(), ACTOR_COOLDOWN_CAPACITY);
        assert!(cooldowns.admit("overflow", 1, now + ACTOR_COOLDOWN));
        assert_eq!(cooldowns.actors.len(), 1);
    }

    #[test]
    fn rate_limited_reply_is_retry_safe_while_uncertain_stays_fenced() {
        assert_eq!(
            error_text(MemberError::Discord(MemberDiscordError::RateLimited)),
            "Discord rate-limited the request; nothing was applied. Try again shortly."
        );
        // Literal copy, not `UNCERTAIN_REPLY`: a changed constant must fail.
        assert_eq!(
            error_text(MemberError::Discord(MemberDiscordError::Timeout)),
            "Discord did not confirm the action. It may still land; ask an operator to reconcile before retrying."
        );
        // `Unavailable` carries a detail but renders the same fenced copy: the
        // detail must never leak into the ephemeral reply.
        assert_eq!(
            error_text(MemberError::Discord(MemberDiscordError::Unavailable(
                "503 service unavailable".to_owned()
            ))),
            "Discord did not confirm the action. It may still land; ask an operator to reconcile before retrying."
        );
    }

    #[test]
    fn policy_error_display_passthrough_is_pinned() {
        // `error_text` renders policy refusals verbatim: pin the copy and the
        // passthrough shape so either change fails loudly.
        assert_eq!(
            error_text(MemberError::Policy(PolicyError::ActorHierarchy)),
            "The target is equal to or above your highest role"
        );
        assert_eq!(
            error_text(MemberError::Policy(PolicyError::ActorHierarchy)),
            PolicyError::ActorHierarchy.to_string()
        );
    }

    #[test]
    fn reason_error_display_passthrough_is_pinned() {
        // Same verbatim rendering for reason refusals.
        assert_eq!(
            error_text(MemberError::Reason(ReasonError::Empty)),
            "\"reason\" must be a non-empty string"
        );
        assert_eq!(
            error_text(MemberError::Reason(ReasonError::Empty)),
            ReasonError::Empty.to_string()
        );
    }

    #[test]
    fn malformed_error_text_is_pinned() {
        assert_eq!(
            error_text(MemberError::Malformed {
                field: "duration_seconds",
                message: "\"duration_seconds\" must be an integer between 60 and 31536000"
                    .to_owned(),
            }),
            "malformed duration_seconds: \"duration_seconds\" must be an integer between 60 and 31536000"
        );
    }

    #[test]
    fn inflight_error_text_is_pinned() {
        assert_eq!(
            error_text(MemberError::InFlight),
            "A moderation action for this member is already in progress."
        );
    }

    #[test]
    fn key_mismatch_error_text_is_pinned() {
        assert_eq!(
            error_text(MemberError::KeyMismatch),
            "This moderation request was already submitted with different details."
        );
    }

    #[test]
    fn rejected_detail_renders_fixed_actionable_copy() {
        // Rejections never paste the provider detail: every detail renders
        // the same fixed copy naming Discord with a next action.
        let first = error_text(MemberError::Discord(MemberDiscordError::Rejected(
            "missing permissions".to_owned(),
        )));
        let second = error_text(MemberError::Discord(MemberDiscordError::Rejected(
            "totally different 50013 @everyone".to_owned(),
        )));
        assert_eq!(first, second);
        assert_eq!(
            first,
            "Discord refused the request; nothing was applied. Check the bot's permissions or ask an operator."
        );
    }

    #[test]
    fn store_and_bot_identity_unknown_copy_is_pinned() {
        // Both arms render the opaque failure reply: the store message never
        // leaks into the ephemeral copy.
        assert_eq!(
            error_text(MemberError::Store(StoreError::new("db down"))),
            "Moderation command failed; try again."
        );
        assert_eq!(
            error_text(MemberError::BotIdentityUnknown),
            "Moderation command failed; try again."
        );
    }

    #[test]
    fn ban_success_confirmation_copy_is_pinned() {
        // Reply layer only: `handle_command` renders through
        // `outcome_text_with_target`, so this pins the exact `/ban` success
        // copy with its sanitized target slot.
        assert_eq!(
            MemberRuntime::outcome_text_with_target(
                &MemberResult {
                    outcome: MemberOutcome::Banned,
                    replayed: false,
                },
                "dave"
            ),
            "Banned dave."
        );
        assert_eq!(
            MemberRuntime::outcome_text_with_target(
                &MemberResult {
                    outcome: MemberOutcome::Banned,
                    replayed: true,
                },
                "dave"
            ),
            "Banned dave. Already applied."
        );
    }

    #[test]
    fn tempban_success_confirmation_copy_is_pinned() {
        // Same reply layer as `/ban`: `handle_command` renders through
        // `outcome_text_with_target`, so this pins the exact `/tempban`
        // success copy.
        assert_eq!(
            MemberRuntime::outcome_text_with_target(
                &MemberResult {
                    outcome: MemberOutcome::TemporarilyBanned,
                    replayed: false,
                },
                "dave"
            ),
            "Temporarily banned dave."
        );
        assert_eq!(
            MemberRuntime::outcome_text_with_target(
                &MemberResult {
                    outcome: MemberOutcome::TemporarilyBanned,
                    replayed: true,
                },
                "dave"
            ),
            "Temporarily banned dave. Already applied."
        );
    }

    #[test]
    fn kick_success_confirmation_copy_is_pinned() {
        // Same reply layer as `/ban`: `handle_command` renders through
        // `outcome_text_with_target`, so this pins the exact `/kick` success
        // copy.
        assert_eq!(
            MemberRuntime::outcome_text_with_target(
                &MemberResult {
                    outcome: MemberOutcome::Kicked,
                    replayed: false,
                },
                "dave"
            ),
            "Kicked dave."
        );
        assert_eq!(
            MemberRuntime::outcome_text_with_target(
                &MemberResult {
                    outcome: MemberOutcome::Kicked,
                    replayed: true,
                },
                "dave"
            ),
            "Kicked dave. Already applied."
        );
    }

    #[test]
    fn timeout_success_confirmation_copy_is_pinned() {
        // Same reply layer as `/ban`: `handle_command` renders through
        // `outcome_text_with_target`, so this pins the exact `/timeout`
        // success copy.
        assert_eq!(
            MemberRuntime::outcome_text_with_target(
                &MemberResult {
                    outcome: MemberOutcome::TimedOut,
                    replayed: false,
                },
                "dave"
            ),
            "Timed out dave."
        );
        assert_eq!(
            MemberRuntime::outcome_text_with_target(
                &MemberResult {
                    outcome: MemberOutcome::TimedOut,
                    replayed: true,
                },
                "dave"
            ),
            "Timed out dave. Already applied."
        );
    }

    #[test]
    fn warn_success_confirmation_copy_is_pinned() {
        // Same reply layer as `/ban`: `handle_command` renders through
        // `outcome_text_with_target`, so this pins the exact `/warn` success
        // copy.
        assert_eq!(
            MemberRuntime::outcome_text_with_target(
                &MemberResult {
                    outcome: MemberOutcome::Warned,
                    replayed: false,
                },
                "dave"
            ),
            "Warned dave."
        );
        assert_eq!(
            MemberRuntime::outcome_text_with_target(
                &MemberResult {
                    outcome: MemberOutcome::Warned,
                    replayed: true,
                },
                "dave"
            ),
            "Warned dave. Already applied."
        );
    }

    #[test]
    fn unban_success_confirmation_copy_is_pinned() {
        // Same reply layer as `/ban`: `handle_command` renders through
        // `outcome_text_with_target`, so this pins the exact unban success
        // copy.
        assert_eq!(
            MemberRuntime::outcome_text_with_target(
                &MemberResult {
                    outcome: MemberOutcome::Unbanned,
                    replayed: false,
                },
                "dave"
            ),
            "Unbanned dave."
        );
        assert_eq!(
            MemberRuntime::outcome_text_with_target(
                &MemberResult {
                    outcome: MemberOutcome::Unbanned,
                    replayed: true,
                },
                "dave"
            ),
            "Unbanned dave. Already applied."
        );
    }

    #[test]
    fn replay_suffix_confirmation_copy_is_pinned() {
        // The replay suffix is one uniform arm: every outcome replays by
        // appending exactly ` Already applied.` after its named target.
        let fresh = [
            (MemberOutcome::Banned, "Banned"),
            (MemberOutcome::TemporarilyBanned, "Temporarily banned"),
            (MemberOutcome::Kicked, "Kicked"),
            (MemberOutcome::TimedOut, "Timed out"),
            (MemberOutcome::Warned, "Warned"),
            (MemberOutcome::Unbanned, "Unbanned"),
        ];
        for (outcome, base) in fresh {
            assert_eq!(
                MemberRuntime::outcome_text_with_target(
                    &MemberResult {
                        outcome,
                        replayed: true,
                    },
                    "dave"
                ),
                format!("{base} dave. Already applied.")
            );
        }
    }
}

#[cfg(test)]
#[path = "member_moderation_truthful_replies_acceptance_tests.rs"]
// The protected acceptance module keeps baseline fallback traits that the
// inherent items shadow once the builder lands; they are intentionally dead
// on a GREEN head, and the builder must not edit that file.
#[allow(dead_code)]
mod truthful_replies_acceptance_tests;
