//! Member moderation commands (`/ban`, `/tempban`, `/kick`, `/timeout`,
//! `/warn`) plus the tempban unban sweep.
//!
//! Thin wiring over the `member_moderation` domain + `PgMemberModerationStore`:
//! decode options → resolve actor/target role facts over REST → validate →
//! exactly-once execute through the shared REST executor → ephemeral outcome.
//! Routing, the `TWO_MODERATION` gate and the handler-level permission check
//! all happen in the shared router before these run, so the handlers assume
//! an authorized moderation invocation.
//!
//! The runtime holds the ONE guild store for this process: command paths
//! clone it, and the sweep job shares the same `Arc`, so bans and unbans for
//! one member serialize through one set of local queues. Role facts come
//! from live guild reads (the interaction carries role ids, never positions);
//! an incomplete snapshot refuses the command instead of guessing hierarchy.
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
        DiscordError as MemberDiscordError, MemberError, MemberExecution, MemberModerationService,
        MemberOutcome, MemberResult, UNBAN_SWEEP_INTERVAL_SECONDS,
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

    /// Policy stamped with the resolved bot identity. `None` keeps the
    /// service's owner fallback for sweep audit rows; commands always pass
    /// the resolved id (an unresolvable bot refuses instead).
    fn policy(&self, bot_id: Option<&str>) -> ModerationPolicy {
        ModerationPolicy {
            owen_user_id: self.policy.owen_user_id.clone(),
            protected_role_ids: self.policy.protected_role_ids.clone(),
            bot_user_id: bot_id.map(str::to_owned),
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
    let owner_id = text(&guild, "owner_id").ok_or(())?;
    let mut positions = HashMap::new();
    for role in guild
        .get("roles")
        .and_then(serde_json::Value::as_array)
        .ok_or(())?
    {
        let id = text(role, "id").ok_or(())?;
        let position = role
            .get("position")
            .and_then(serde_json::Value::as_i64)
            .ok_or(())?;
        positions.insert(id, position);
    }
    if !positions.contains_key(guild_id) {
        return Err(());
    }
    Ok(GuildFacts {
        guild_id: guild_id.to_owned(),
        owner_id,
        positions,
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

/// Roles for the target from the interaction's resolved data, if Discord
/// attached membership.
fn resolved_target_roles(
    interaction: &Interaction,
    target: Id<UserMarker>,
) -> Option<(Vec<String>, bool)> {
    let InteractionData::ApplicationCommand(data) = interaction.data.as_ref()? else {
        return None;
    };
    let resolved = data.resolved.as_ref()?;
    let member = resolved.members.get(&target)?;
    let user = resolved.users.get(&target);
    Some((
        member.roles.iter().map(|id| id.get().to_string()).collect(),
        user.is_some_and(|user| user.bot),
    ))
}

/// Membership plus bot flag from `GET /guilds/{g}/members/{u}`. `None` is a
/// departed or never-joined user (ban tolerates; kick/timeout/warn refuse).
/// An id mismatch or malformed body fails closed.
async fn fetched_target(
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
    Ok(Some((
        roles,
        user.get("bot")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    )))
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
        Ok(result) => outcome_text(&result),
        Err(reply) => reply,
    };
    if let Err(err) = executor
        .edit_interaction_response(interaction.application_id.get(), &interaction.token, &reply)
        .await
    {
        tracing::warn!(interaction_id = %interaction.id.get(), error = %err, "member command reply edit failed");
    }
}

fn outcome_text(result: &MemberResult) -> String {
    let base = match result.outcome {
        MemberOutcome::Banned => "Banned.",
        MemberOutcome::TemporarilyBanned => "Temporarily banned.",
        MemberOutcome::Kicked => "Kicked.",
        MemberOutcome::TimedOut => "Timed out.",
        MemberOutcome::Warned => "Warned.",
        MemberOutcome::Unbanned => "Unbanned.",
    };
    if result.replayed {
        format!("{base} Already applied.")
    } else {
        base.to_owned()
    }
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
        MemberError::Discord(MemberDiscordError::Rejected(detail)) => {
            format!("Discord refused the request: {detail}")
        }
        MemberError::Discord(_) => UNCERTAIN_REPLY.to_owned(),
        MemberError::Store(_) => FAILURE_REPLY.to_owned(),
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
    let facts = guild_facts(executor, &guild_id)
        .await
        .map_err(|_| FAILURE_REPLY.to_owned())?;
    let partial = interaction
        .member
        .as_ref()
        .ok_or_else(|| FAILURE_REPLY.to_owned())?;
    let actor_roles: Vec<String> = partial
        .roles
        .iter()
        .map(|id| id.get().to_string())
        .collect();
    let actor = ModerationActor {
        user_id: actor_id,
        highest_role_position: top_position(&actor_roles, &facts)
            .ok_or_else(|| FAILURE_REPLY.to_owned())?,
        role_ids: with_everyone(actor_roles, &guild_id),
        permissions: partial
            .permissions
            .map(|permissions| permissions.bits())
            .unwrap_or(0),
    };
    let target = match target {
        Some(target) => {
            let target_id = target.get().to_string();
            let resolved_or_fetched = match resolved_target_roles(interaction, target) {
                Some(resolved) => Some(resolved),
                None => fetched_target(executor, &guild_id, &target_id)
                    .await
                    .map_err(|_| FAILURE_REPLY.to_owned())?,
            };
            let (roles, is_bot) = match resolved_or_fetched {
                Some(pair) => pair,
                None if action == ModerationAction::Ban => (vec![guild_id.clone()], false),
                None => return Err(NO_MEMBER_REPLY.to_owned()),
            };
            Some(ModerationTarget {
                user_id: target_id.clone(),
                highest_role_position: top_position(&roles, &facts)
                    .ok_or_else(|| FAILURE_REPLY.to_owned())?,
                role_ids: with_everyone(roles, &guild_id),
                is_bot,
                is_guild_owner: target_id == facts.owner_id,
            })
        }
        None => None,
    };
    let bot_id = member
        .bot_id(executor)
        .await
        .map_err(|_| FAILURE_REPLY.to_owned())?;
    let bot_roles = fetched_bot_roles(executor, &guild_id, bot_id)
        .await
        .map_err(|_| FAILURE_REPLY.to_owned())?;
    // The policy requires a resolved bot position; unknown roles refuse here.
    let bot_highest_role_position =
        top_position(&bot_roles, &facts).ok_or_else(|| FAILURE_REPLY.to_owned())?;
    let service = MemberModerationService::new(
        executor.clone(),
        member.store(),
        member.policy(Some(bot_id)),
        now_millis_i64,
    );
    let execution = MemberExecution {
        action,
        guild_id,
        actor,
        target,
        bot_highest_role_position,
        reason: reason.unwrap_or_default(),
        duration_seconds,
        request_id: new_id(),
        // Discord may redeliver an interaction: the interaction id replays the
        // stored outcome instead of moderating twice.
        idempotency_key: interaction.id.get().to_string(),
    };
    service.execute(&execution).await.map_err(error_text)
}

fn with_everyone(mut roles: Vec<String>, guild_id: &str) -> Vec<String> {
    if !roles.iter().any(|role| role == guild_id) {
        roles.push(guild_id.to_owned());
    }
    roles
}

/// Bot membership roles for the hierarchy check. Fails closed like every
/// other facts read.
async fn fetched_bot_roles(
    executor: &ActionExecutor,
    guild_id: &str,
    bot_id: &str,
) -> Result<Vec<String>, ()> {
    let member = executor
        .get_json_strict(&format!("/guilds/{guild_id}/members/{bot_id}"))
        .await
        .map_err(|_| ())?
        .ok_or(())?;
    let user = member.get("user").ok_or(())?;
    if text(user, "id").as_deref() != Some(bot_id) {
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
    Ok(roles)
}

/// The supervised unban sweep for the configured guild: claim one due job
/// immediately before each dispatch (at most 25 per tick, enforced inside
/// `run_due_unbans`), stop on failure. Definite refusals requeue for the
/// next tick; uncertain dispatches stay fenced for reconciliation.
pub(crate) fn sweep_job(member: Arc<MemberRuntime>, rest: ActionExecutor) -> Job {
    let cadence = Duration::from_secs(UNBAN_SWEEP_INTERVAL_SECONDS);
    Job {
        name: NAMES[0],
        cadence,
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
    // Best-effort bot identity for sweep audit rows; dispatch itself needs no
    // hierarchy decision, so an unresolvable bot still sweeps.
    let bot_id = member.bot_id(rest).await.ok();
    let service = MemberModerationService::new(
        rest.clone(),
        member.store(),
        member.policy(bot_id),
        now_millis_i64,
    );
    match service.run_due_unbans(&member.guild_id).await {
        Ok(completed) => {
            if completed > 0 {
                tracing::info!(completed, guild = %member.guild_id, "member unban sweep completed jobs");
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

    #[test]
    fn unknown_bot_role_or_everyone_position_refuses() {
        let mut facts = GuildFacts {
            guild_id: "guild".to_owned(),
            owner_id: "owner".to_owned(),
            positions: HashMap::from([("guild".to_owned(), 0), ("bot-role".to_owned(), 10)]),
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
}
