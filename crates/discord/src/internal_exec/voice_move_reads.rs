//! Governed read-only evidence for a voice move. These reads never admit a PATCH.
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tokio::time::Instant;
use twilight_model::{guild::Member, id::Id};
use two_bot_core::{
    voice_move_decision::{decide, ChannelFact, VoiceMoveDecision, VoiceMoveFacts},
    voice_move_facts::{parse_override, GuildRoleFact},
    Snowflake,
};

use super::super::{throw_for_status, ActionExecutor, DiscordError, RawResponse};
use super::voice_move::retry_delay_ms;

#[derive(Debug, thiserror::Error)]
pub enum VoiceMoveReadError {
    #[error("discord rate-limited the voice evidence request")]
    RateLimited(Option<u64>),
    #[error(transparent)]
    Discord(#[from] DiscordError),
}

impl VoiceMoveReadError {
    fn from_status(response: &RawResponse) -> Self {
        if response.status == 429 {
            Self::RateLimited(retry_delay_ms(
                response.validated_retry_after_header,
                response.body_retry_after_secs(),
            ))
        } else {
            throw_for_status(response).into()
        }
    }
}

/// A validated eligible destination and the guild facts needed for permission checks.
pub struct VoiceMoveTarget {
    guild: Snowflake,
    owner: Snowflake,
    roles: Vec<GuildRoleFact>,
    bot: Snowflake,
    room: ChannelFact,
}

impl VoiceMoveTarget {
    #[must_use]
    pub const fn user_limit(&self) -> u32 {
        self.room.user_limit
    }
}

/// REST permission evidence. A caller must supply independently fenced occupancy.
pub struct VoiceMoveEvidence(VoiceMoveFacts);

impl VoiceMoveEvidence {
    #[must_use]
    pub fn decide(mut self, target_occupants: u32) -> VoiceMoveDecision {
        self.0.target_occupants = target_occupants;
        decide(&self.0)
    }
}

impl ActionExecutor {
    /// Check the destination before requiring permission facts. `None` is an
    /// authoritative missing or ineligible destination, not an evidence error.
    pub async fn voice_move_target_until(
        &self,
        guild: Snowflake,
        target: Snowflake,
        deadline: Instant,
    ) -> Result<Option<VoiceMoveTarget>, VoiceMoveReadError> {
        let bot = self
            .current_bot_user_id_until_map(deadline, VoiceMoveReadError::from_status)
            .await?;
        let doc = self
            .required_voice_read(&format!("/guilds/{guild}"), deadline)
            .await?;
        if number(&doc["id"]) != Some(guild) {
            return Err(incomplete());
        }
        let Some(room) = self
            .get_json_strict_until_map(
                &format!("/channels/{target}"),
                deadline,
                VoiceMoveReadError::from_status,
            )
            .await?
        else {
            return Ok(None);
        };
        let Some(room) = destination(guild, target, &doc, &room).ok_or_else(incomplete)? else {
            return Ok(None);
        };
        let owner = number(&doc["owner_id"]).ok_or_else(incomplete)?;
        let roles = list(&doc["roles"], |role| {
            Some(GuildRoleFact {
                id: number(&role["id"])?,
                position: role["position"].as_i64()?,
                permissions: role["permissions"].as_str()?.parse().ok()?,
            })
        })
        .ok_or_else(incomplete)?;
        Ok(Some(VoiceMoveTarget {
            guild,
            owner,
            roles,
            bot,
            room,
        }))
    }

    /// Read permission facts for the source witnessed by the caller's roster.
    /// This does not establish that the member is still in that channel.
    pub async fn voice_move_evidence_until(
        &self,
        target: VoiceMoveTarget,
        member_id: Snowflake,
        from: Snowflake,
        deadline: Instant,
    ) -> Result<VoiceMoveEvidence, VoiceMoveReadError> {
        let VoiceMoveTarget {
            guild,
            owner,
            roles,
            bot,
            room,
        } = target;
        let member = self
            .voice_move_member_read(guild, member_id, true, deadline)
            .await?;
        let source = self
            .required_voice_read(&format!("/channels/{from}"), deadline)
            .await?;
        if room_type(guild, from, &source) != Some(2) {
            return Err(incomplete());
        }
        let source = voice_room(from, &source).ok_or_else(incomplete)?;
        let bot_member = self
            .voice_move_member_read(guild, bot, false, deadline)
            .await?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| incomplete())?
            .as_micros();
        Ok(VoiceMoveEvidence(VoiceMoveFacts {
            guild_id: guild,
            guild_owner_id: owner,
            guild_roles: roles,
            target: room,
            source,
            member_channel_id: Some(from),
            target_occupants: 0,
            member_id,
            member_roles: member.roles.into_iter().map(Id::get).collect(),
            member_timed_out: member.communication_disabled_until.is_some_and(|until| {
                u128::try_from(until.as_micros()).is_ok_and(|until| until > now)
            }),
            bot_id: bot,
            bot_roles: bot_member.roles.into_iter().map(Id::get).collect(),
        }))
    }

    async fn required_voice_read(
        &self,
        path: &str,
        deadline: Instant,
    ) -> Result<Value, VoiceMoveReadError> {
        self.get_json_strict_until_map(path, deadline, VoiceMoveReadError::from_status)
            .await?
            .ok_or_else(incomplete)
    }

    async fn voice_move_member_read(
        &self,
        guild: Snowflake,
        id: Snowflake,
        require_timeout: bool,
        deadline: Instant,
    ) -> Result<Member, VoiceMoveReadError> {
        let doc = self
            .required_voice_read(&format!("/guilds/{guild}/members/{id}"), deadline)
            .await?;
        if require_timeout && doc.get("communication_disabled_until").is_none() {
            return Err(incomplete());
        }
        let member: Member = serde_json::from_value(doc).map_err(|_| incomplete())?;
        (member.user.id.get() == id)
            .then_some(member)
            .ok_or_else(incomplete)
    }
}

fn incomplete() -> VoiceMoveReadError {
    DiscordError::Unavailable("voice_move_evidence_incomplete".into()).into()
}

fn number(value: &Value) -> Option<Snowflake> {
    let text = value.as_str()?;
    let id = text.parse::<Snowflake>().ok()?;
    (id != 0 && id.to_string() == text).then_some(id)
}

fn list<T>(value: &Value, parse: impl FnMut(&Value) -> Option<T>) -> Option<Vec<T>> {
    value.as_array()?.iter().map(parse).collect()
}

fn room_type(guild: Snowflake, id: Snowflake, room: &Value) -> Option<u64> {
    (number(&room["guild_id"]) == Some(guild) && number(&room["id"]) == Some(id)).then_some(())?;
    room["type"].as_u64()
}

fn voice_room(id: Snowflake, doc: &Value) -> Option<ChannelFact> {
    Some(ChannelFact {
        id,
        is_voice: true,
        is_afk: false,
        user_limit: u32::try_from(doc["user_limit"].as_u64()?).ok()?,
        overwrites: list(&doc["permission_overwrites"], parse_override)?,
    })
}

fn destination(
    guild: Snowflake,
    id: Snowflake,
    doc: &Value,
    room: &Value,
) -> Option<Option<ChannelFact>> {
    if room_type(guild, id, room)? != 2 {
        return Some(None);
    }
    let afk = doc.get("afk_channel_id")?;
    if !afk.is_null() && number(afk)? == id {
        return Some(None);
    }
    Some(Some(voice_room(id, room)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ineligible_rooms_need_no_voice_permission_fields() {
        let mut room = json!({"guild_id":"1","id":"2","type":11});
        assert_eq!(destination(1, 2, &Value::Null, &room), Some(None));
        assert_eq!(destination(9, 2, &Value::Null, &room), None);
        room["type"] = json!(2);
        assert_eq!(
            destination(1, 2, &json!({"afk_channel_id":"2"}), &room),
            Some(None)
        );
        assert_eq!(
            destination(1, 2, &json!({"afk_channel_id":null}), &room),
            None
        );
        room["user_limit"] = json!(1);
        room["permission_overwrites"] = json!([]);
        let facts = destination(1, 2, &json!({"afk_channel_id":null}), &room)
            .flatten()
            .unwrap();
        assert_eq!(facts.user_limit, 1);
    }
}
