//! Staged keyless receiver; only the adapter settles durable send admission.
use super::{json, AuthDecision, ErrorCode, Failure, ReceiverState, Value};
use crate::gateway::GatewayState;
use std::{sync::atomic::Ordering, time::Duration};
use tokio::time::{timeout_at, Instant};
use twilight_model::{guild::Member, id::Id};
use two_bot_core::{
    backup::guild_config::TWO_STAGING_GUILD_ID as GUILD,
    internal_actions::{body_hash, VoiceMoveOutcome as Outcome, VoiceMoveRequest},
    voice_move_decision::{decide, ChannelFact, VoiceMoveDecision, VoiceMoveFacts},
    voice_move_facts::{parse_override, GuildRoleFact},
    voice_move_holds::VoiceMoveHolds,
    Snowflake,
};
use two_bot_discord::{
    executor::member::{VoiceMovePatch, VOICE_MOVE_TIMEOUT_MS},
    ActionExecutor,
};

type Receipt = (Outcome, Option<Snowflake>, Option<u64>);
const UNAVAILABLE: Receipt = (Outcome::DiscordUnavailable, None, None);
pub(super) async fn respond(
    state: &ReceiverState,
    decision: &AuthDecision,
    delivery: &str,
    request_id: &str,
) -> Result<Value, Failure> {
    let unavailable = || Failure::code(ErrorCode::DiscordUnavailable);
    let request = VoiceMoveRequest::validate(&decision.body).map_err(Failure::from_action)?;
    let caller = state
        .config
        .caller_for(&decision.key_id)
        .ok_or_else(unavailable)?;
    let ids = [request.discord_id(), request.channel_id()];
    let (outcome, from, retry) = execute(state, ids).await.unwrap_or(UNAVAILABLE);
    let from = from.map(|id| id.to_string());
    sqlx::query("INSERT INTO internal_action_log (caller_hash, delivery_hash, action, phase, \
        guild_id, target_id, response_code, http_status, affected, outcome, evidence_code, from_channel_id, resource_id, request_id) \
        VALUES ($1, $2, 'voice.move_member', 'terminal', $3, $4, $5, $6, $7, $8, 'executor', $9, $10, $11)")
        .bind(body_hash(caller.as_bytes())).bind(delivery)
        .bind(GUILD).bind(ids[0]).bind(outcome.response_code()).bind(i32::from(outcome.status()))
        .bind(outcome.affected()).bind(outcome.as_str()).bind(&from).bind(ids[1]).bind(request_id)
        .execute(state.store.pool()).await.map_err(|_| unavailable())?;
    match outcome {
        Outcome::DiscordUnavailable => Err(unavailable()),
        Outcome::RateLimited => Err(Failure {
            retry_after: retry,
            ..Failure::code(ErrorCode::RateLimited)
        }),
        Outcome::Moved => Ok(json!({"outcome": "moved", "from_channel_id": from})),
        _ => Ok(json!({"outcome": outcome.as_str()})),
    }
}

async fn execute(state: &ReceiverState, ids: [&str; 2]) -> Option<Receipt> {
    use VoiceMovePatch::{Denied, Limited, Moved, NotSent, Stale};
    let deadline = Instant::now() + super::REQUEST_TIMEOUT;
    let cutoff = deadline - Duration::from_millis(VOICE_MOVE_TIMEOUT_MS);
    let api = state.voice_move.as_ref()?;
    let probe = check(api, state, ids, true);
    let from = match timeout_at(cutoff, probe).await.ok()?? {
        Ok(from) => from,
        Err((outcome, from)) => return Some((outcome, from, None)),
    };
    let patch = api.move_voice_member(GUILD, ids[0], ids[1]).await;
    let member = decimal(ids[0])?;
    {
        let mut holds = state.voice_clock.move_holds.lock().ok()?;
        if matches!(patch, Ok(Denied | Stale | NotSent(_) | Limited { .. })) {
            holds.release_no_effect(member);
        } else {
            holds.complete(member);
        }
    }
    let from = Some(from);
    Some(match patch {
        Ok(Moved) => (Outcome::Moved, from, None),
        Ok(Denied) => (Outcome::BotNotPermitted, from, None),
        Ok(Limited { retry_after_ms, .. }) => (
            Outcome::RateLimited,
            from,
            retry_after_ms.map(|ms| ms.div_ceil(1_000).max(1)),
        ),
        // Re-read once after a stale refusal, but never issue a second PATCH.
        Ok(Stale) => match timeout_at(deadline, check(api, state, ids, false)).await {
            Ok(Some(Err((outcome, latest)))) => (outcome, latest.or(from), None),
            _ => (Outcome::DiscordUnavailable, from, None),
        },
        _ => (Outcome::DiscordUnavailable, from, None),
    })
}

async fn check(
    discord: &ActionExecutor,
    state: &ReceiverState,
    [member, target]: [&str; 2],
    reserve: bool,
) -> Option<Result<Snowflake, (Outcome, Option<Snowflake>)>> {
    let ids @ [guild, member_id, target_id] = [decimal(GUILD)?, decimal(member)?, decimal(target)?];
    let get = |path: String| async move { discord.get_json_strict(&path).await.ok() };
    let bot_id = discord.current_bot_user_id().await.ok()?;
    let doc = get(format!("/guilds/{guild}")).await??;
    (decimal(doc["id"].as_str()?)? == guild).then_some(())?;
    let refused = |outcome, from| Some(Err((outcome, from)));
    let Some(channel) = get(format!("/channels/{target}")).await? else {
        return refused(Outcome::RoomGone, None);
    };
    let target = room(&channel, target_id)?;
    let afk = doc.get("afk_channel_id")?;
    if !target.is_voice || (!afk.is_null() && decimal(afk.as_str()?)? == target_id) {
        return refused(Outcome::RoomGone, None);
    }
    let current = with_occupancy(state, ids, |source, _, _| Some(source)).await?;
    let Some(from) = current else {
        return refused(Outcome::NotConnected, None);
    };
    if from == target_id {
        return refused(Outcome::AlreadyInRoom, current);
    }
    let get_member = async |id| {
        let doc = get(format!("/guilds/{guild}/members/{id}")).await??;
        let member: Member = serde_json::from_value(doc).ok()?;
        (member.user.id.get() == id).then_some(member)
    };
    let member = get_member(member_id).await?;
    let source = room(&get(format!("/channels/{from}")).await??, from)?;
    let bot = get_member(bot_id).await?;
    let now = time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000;
    with_occupancy(state, ids, |latest, target_occupants, holds| {
        (latest == current).then_some(())?;
        let facts = VoiceMoveFacts {
            guild_id: guild,
            guild_owner_id: decimal(doc["owner_id"].as_str()?)?,
            guild_roles: list(&doc["roles"], role)?,
            target_occupants,
            source,
            target,
            member_id,
            member_channel_id: current,
            member_roles: member.roles.into_iter().map(Id::get).collect(),
            member_timed_out: member
                .communication_disabled_until
                .is_some_and(|until| i128::from(until.as_micros()) > now),
            bot_id,
            bot_roles: bot.roles.into_iter().map(Id::get).collect(),
        };
        match decide(&facts) {
            VoiceMoveDecision::Refuse(outcome) => refused(outcome, current),
            VoiceMoveDecision::Proceed { .. } if !reserve => Some(Ok(from)),
            VoiceMoveDecision::Proceed { .. } => {
                holds.reserve(member_id, target_id).ok().map(|()| Ok(from))
            }
            VoiceMoveDecision::Incomplete => None,
        }
    })
    .await
}

async fn with_occupancy<T>(
    state: &ReceiverState,
    [guild, member, target]: [Snowflake; 3],
    read: impl FnOnce(Option<Snowflake>, u32, &mut VoiceMoveHolds) -> Option<T>,
) -> Option<T> {
    let gateway = state.gateway_state.read().await;
    (*gateway == GatewayState::Connected && !state.draining.load(Ordering::SeqCst)).then_some(())?;
    let mut holds = state.voice_clock.move_holds.lock().ok()?;
    state.voice_cache.with_guild_snapshot(guild, |cache| {
        let guild = Id::new(guild);
        (cache.guild(guild)?.unavailable() != Some(true)).then_some(())?;
        // Use the rebuilt roster, not retained member voice state objects.
        let (mut current, mut count) = (None, 0u32);
        let roster = cache.guild_voice_states(guild);
        for user in roster.as_deref().into_iter().flatten() {
            let room = cache.voice_state(*user, guild)?.channel_id().get();
            if user.get() == member {
                current = Some(room);
            }
            count = count.checked_add(u32::from(room == target))?;
        }
        let occupants = holds.occupancy(member, target, count).ok()?;
        read(current, occupants, &mut holds)
    })?
}

fn decimal(text: &str) -> Option<Snowflake> {
    let value = text.parse::<Snowflake>().ok()?;
    (value != 0 && value.to_string() == text).then_some(value)
}

fn list<T>(value: &Value, parse: impl FnMut(&Value) -> Option<T>) -> Option<Vec<T>> {
    value.as_array()?.iter().map(parse).collect()
}
fn role(doc: &Value) -> Option<GuildRoleFact> {
    Some(GuildRoleFact {
        id: decimal(doc["id"].as_str()?)?,
        position: doc["position"].as_i64()?,
        permissions: doc["permissions"].as_str()?.parse().ok()?,
    })
}
fn room(doc: &Value, room: Snowflake) -> Option<ChannelFact> {
    (doc["guild_id"].as_str()? == GUILD && decimal(doc["id"].as_str()?)? == room).then_some(())?;
    let is_voice = doc["type"].as_u64()? == 2;
    let limit = doc["user_limit"]
        .as_u64()
        .and_then(|n| u32::try_from(n).ok());
    Some(ChannelFact {
        id: room,
        is_voice,
        is_afk: false,
        user_limit: if is_voice { limit? } else { 0 },
        overwrites: list(&doc["permission_overwrites"], parse_override)?,
    })
}
