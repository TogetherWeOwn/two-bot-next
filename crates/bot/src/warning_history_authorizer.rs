//! One fresh authority check for the dormant warning history read.

use std::{collections::HashMap, time::Duration};

use serde_json::Value;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use tokio::time::{timeout_at, Instant};
use twilight_model::{
    guild::Permissions,
    id::{
        marker::{ChannelMarker, GuildMarker, UserMarker},
        Id,
    },
};
use two_bot_core::{
    commands::PERM_MODERATE_MEMBERS,
    moderation::ModerationActor,
    voice_permission_health::{resolve_effective_permissions, OverwriteMasks},
};

use super::{actor_authority, parse_guild, parse_member, valid_id, ActionExecutor, GuildFacts};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Refusal {
    Denied,
    Unavailable,
}

/// The invoking IDs come from authenticated guild interaction context. The
/// caller owns activation and private acknowledgement, and supplies the
/// remaining shared authority budget as an absolute deadline.
pub(super) async fn authorize(
    executor: &ActionExecutor,
    configured_guild: Id<GuildMarker>,
    invoking_guild: Option<Id<GuildMarker>>,
    channel: Id<ChannelMarker>,
    actor: Id<UserMarker>,
    target: Id<UserMarker>,
    authority_deadline: Instant,
) -> Result<(), Refusal> {
    if invoking_guild != Some(configured_guild) || actor == target {
        return Err(Refusal::Denied);
    }
    let now = Instant::now();
    let deadline = authority_deadline.min(now + Duration::from_secs(5));
    if now >= deadline {
        return Err(Refusal::Unavailable);
    }
    let decision = timeout_at(deadline, async {
        let guild = configured_guild.to_string();
        let actor_id = actor.to_string();
        let doc = read(executor, &format!("/guilds/{guild}"), deadline).await?;
        let facts = parse_guild(&doc, &guild).map_err(|_| Refusal::Unavailable)?;
        let member = read(
            executor,
            &format!("/guilds/{guild}/members/{actor_id}"),
            deadline,
        )
        .await?;
        let (roles, is_bot) = parse_member(&member, &actor_id).map_err(|_| Refusal::Unavailable)?;
        let actor = actor_authority(&actor_id, roles, &facts).ok_or(Refusal::Unavailable)?;
        if is_bot || actor.permissions & PERM_MODERATE_MEMBERS == 0 || !can_invoke(&member)? {
            return Err(Refusal::Denied);
        }

        let bot_id = executor
            .current_bot_user_id_until(deadline)
            .await
            .map_err(|_| Refusal::Unavailable)?
            .to_string();
        if bot_id == actor_id {
            return Err(Refusal::Denied);
        }
        let member = read(
            executor,
            &format!("/guilds/{guild}/members/{bot_id}"),
            deadline,
        )
        .await?;
        let (roles, true) = parse_member(&member, &bot_id).map_err(|_| Refusal::Unavailable)?
        else {
            return Err(Refusal::Unavailable);
        };
        let bot = actor_authority(&bot_id, roles, &facts).ok_or(Refusal::Unavailable)?;
        let channel = channel.to_string();
        let doc = read(executor, &format!("/channels/{channel}"), deadline).await?;
        check_channel(&doc, &channel, &facts, &actor, &bot)
    })
    .await
    .map_err(|_| Refusal::Unavailable)?;

    // A transport can finish settling a read after its wire deadline. Only
    // the executor owns that single GET's cleanup, with no caller continuation.
    if Instant::now() >= deadline {
        return Err(Refusal::Unavailable);
    }
    decision
}

async fn read(executor: &ActionExecutor, path: &str, deadline: Instant) -> Result<Value, Refusal> {
    executor
        .get_json_strict_until(path, deadline)
        .await
        .map_err(|_| Refusal::Unavailable)?
        .ok_or(Refusal::Unavailable)
}

fn can_invoke(member: &Value) -> Result<bool, Refusal> {
    let optional_flag = |value: Option<&Value>| match value {
        None => Ok(false),
        Some(value) => value.as_bool().ok_or(Refusal::Unavailable),
    };
    if optional_flag(member["user"].get("system"))? || optional_flag(member.get("pending"))? {
        return Ok(false);
    }
    match member.get("communication_disabled_until") {
        Some(Value::Null) => Ok(true),
        Some(Value::String(until)) => {
            let until = OffsetDateTime::parse(until, &Rfc3339).map_err(|_| Refusal::Unavailable)?;
            Ok(until <= OffsetDateTime::now_utc())
        }
        _ => Err(Refusal::Unavailable),
    }
}

fn check_channel(
    doc: &Value,
    channel_id: &str,
    facts: &GuildFacts,
    actor: &ModerationActor,
    bot: &ModerationActor,
) -> Result<(), Refusal> {
    if doc["id"].as_str() != Some(channel_id) || doc["guild_id"].as_str() != Some(&facts.guild_id) {
        return Err(Refusal::Unavailable);
    }
    // GuildText is the sole supported leaf. Read this channel's overwrites.
    if doc["type"].as_u64().ok_or(Refusal::Unavailable)? != 0 {
        return Err(Refusal::Denied);
    }
    let mut overwrites = HashMap::new();
    for row in doc["permission_overwrites"]
        .as_array()
        .ok_or(Refusal::Unavailable)?
    {
        let id = row["id"]
            .as_str()
            .filter(|id| valid_id(id))
            .ok_or(Refusal::Unavailable)?;
        let kind = row["type"].as_u64().ok_or(Refusal::Unavailable)?;
        if !(kind == 1 || kind == 0 && facts.permissions.contains_key(id)) {
            return Err(Refusal::Unavailable);
        }
        let bits = |key: &str| {
            row[key]
                .as_str()
                .and_then(|value| {
                    value
                        .parse::<u64>()
                        .ok()
                        .filter(|bits| bits.to_string() == value)
                })
                .ok_or(Refusal::Unavailable)
        };
        let masks = OverwriteMasks {
            allow: bits("allow")?,
            deny: bits("deny")?,
        };
        if overwrites.insert((kind, id), masks).is_some() {
            return Err(Refusal::Unavailable);
        }
    }
    let everyone = overwrites.get(&(0, facts.guild_id.as_str())).copied();
    let effective = |member: &ModerationActor| {
        let roles: Vec<_> = member
            .role_ids
            .iter()
            .filter(|id| *id != &facts.guild_id)
            .filter_map(|id| overwrites.get(&(0, id.as_str())).copied())
            .collect();
        let member_override = overwrites.get(&(1, member.user_id.as_str())).copied();
        resolve_effective_permissions(member.permissions, everyone, &roles, member_override)
    };
    let view = Permissions::VIEW_CHANNEL.bits();
    let invoke = view | Permissions::USE_SLASH_COMMANDS.bits();
    if effective(actor) & invoke != invoke || effective(bot) & view == 0 {
        return Err(Refusal::Denied);
    }
    Ok(())
}

#[cfg(test)]
#[path = "warning_history_authorizer_tests.rs"]
mod tests;
