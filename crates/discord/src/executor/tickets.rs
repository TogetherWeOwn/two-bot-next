//! Ticket verbs on the shared executor, never a separate HTTP client.
//! All mutations are single-attempt: an uncertain create is recovered by topic.
//! Protocol: https://docs.discord.com/developers/resources/channel
//! History: https://docs.discord.com/developers/resources/message#get-channel-messages

use super::*;
use serde_json::{json, Value};
use twilight_model::{
    channel::{
        permission_overwrite::PermissionOverwrite as ChannelPermissionOverwrite, ChannelType,
    },
    guild::Permissions,
};
use two_bot_core::tickets::{
    ticket_channel_name, PANEL_TEXT, TICKET_CLAIM_ID, TICKET_CLOSE_ID, TICKET_OPEN_ID,
};

/// Only a successful delete or numeric Discord code 10003 proves absence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelPresence<T> {
    Present(T),
    Absent,
}

pub struct TicketChannelRequest<'a> {
    pub guild_id: &'a str,
    pub category_id: &'a str,
    pub staff_role_id: &'a str,
    pub bot_id: &'a str,
    pub opener_id: &'a str,
    pub username: &'a str,
    pub reservation_id: &'a str,
}

/// Fixed server-owned templates, not arbitrary caller-provided mention policy.
pub enum TicketMessage<'a> {
    Panel,
    Controls { opener_id: &'a str },
}

impl ActionExecutor {
    pub async fn create_ticket_channel(
        &self,
        input: &TicketChannelRequest<'_>,
    ) -> Result<String, DiscordError> {
        let guild = snowflake(input.guild_id)?;
        let category = snowflake(input.category_id)?;
        let opener = snowflake(input.opener_id)?;
        let staff = snowflake(input.staff_role_id)?;
        let bot = snowflake(input.bot_id)?;
        if input.reservation_id.is_empty()
            || input.guild_id == input.staff_role_id
            || input.bot_id == input.opener_id
        {
            return Err(DiscordError::Rejected(
                "invalid ticket reservation or permission targets".into(),
            ));
        }
        let common = Permissions::VIEW_CHANNEL
            | Permissions::SEND_MESSAGES
            | Permissions::READ_MESSAGE_HISTORY;
        let overwrite = |id, kind, allow, deny| ChannelPermissionOverwrite {
            id,
            kind,
            allow,
            deny,
        };
        let overwrites = [
            overwrite(
                guild.cast(),
                PermissionOverwriteType::Role,
                Permissions::empty(),
                Permissions::VIEW_CHANNEL,
            ),
            overwrite(
                bot,
                PermissionOverwriteType::Member,
                common | Permissions::MANAGE_CHANNELS,
                Permissions::empty(),
            ),
            overwrite(
                opener,
                PermissionOverwriteType::Member,
                common,
                Permissions::empty(),
            ),
            overwrite(
                staff,
                PermissionOverwriteType::Role,
                common,
                Permissions::empty(),
            ),
        ];
        let name = ticket_channel_name(input.username);
        let topic = format!("two-ticket:{}", input.reservation_id);
        let req = Self::request_of(
            self.inner
                .factory
                .create_guild_channel(guild, &name)
                .kind(ChannelType::GuildText)
                .parent_id(category)
                .topic(&topic)
                .permission_overwrites(&overwrites),
        )?;
        self.pace(false).await;
        let res = self.call_once_raw(req, &[200, 201]).await?;
        let doc = ticket_json(&res)?;
        let id = doc
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| DiscordError::Unavailable("ticket create omitted channel id".into()))?;
        // Missing/malformed success evidence is uncertain, never a safe retry.
        snowflake::<ChannelMarker>(id)
            .map_err(|_| DiscordError::Unavailable("invalid ticket channel id".into()))?;
        Ok(id.to_owned())
    }

    /// A successful guild-wide query is required before orphan absence is inferred.
    pub async fn fetch_ticket_guild_channels(
        &self,
        guild_id: &str,
    ) -> Result<Vec<Value>, DiscordError> {
        let req = Self::request_of(self.inner.factory.guild_channels(snowflake(guild_id)?))?;
        self.pace(false).await;
        let res = self.call_once_raw(req, &[200]).await?;
        serde_json::from_slice(&res.body)
            .map_err(|_| DiscordError::Unavailable("invalid guild channel list".into()))
    }

    pub async fn fetch_ticket_channel(
        &self,
        channel_id: &str,
    ) -> Result<ChannelPresence<Value>, DiscordError> {
        let req = Self::request_of(self.inner.factory.channel(snowflake(channel_id)?))?;
        self.pace(false).await;
        // Accept 404 only to inspect its structured code; generic 404 is NOT absence.
        let res = self.call_once_raw(req, &[200, 404]).await?;
        if res.status == 404 {
            return if unknown_channel(&res) {
                Ok(ChannelPresence::Absent)
            } else {
                Err(throw_for_status(&res))
            };
        }
        let doc = ticket_json(&res)?;
        if !doc.is_object() {
            return Err(DiscordError::Unavailable(
                "invalid ticket channel document".into(),
            ));
        }
        Ok(ChannelPresence::Present(doc))
    }

    /// Prove effective ViewChannel + ReadMessageHistory before interpreting an
    /// empty history as absence. Public panels may grant access through roles,
    /// not a bot-member overwrite. Missing/malformed evidence fails closed.
    pub async fn ticket_history_readable(
        &self,
        guild_id: &str,
        bot_id: &str,
        channel_doc: &Value,
    ) -> Result<bool, DiscordError> {
        let guild: Id<GuildMarker> = snowflake(guild_id)?;
        let bot: Id<UserMarker> = snowflake(bot_id)?;
        ticket_evidence_id(&channel_doc["id"])?;
        if ticket_evidence_id(&channel_doc["guild_id"])? != guild.get()
            || !matches!(channel_doc["type"].as_u64(), Some(0 | 5))
        {
            return Err(invalid_ticket_permissions());
        }
        let rows = channel_doc["permission_overwrites"]
            .as_array()
            .ok_or_else(invalid_ticket_permissions)?;
        let mut overwrites = Vec::with_capacity(rows.len());
        let mut targets = std::collections::HashSet::new();
        for row in rows {
            let id = ticket_evidence_id(&row["id"])?;
            let kind = row["type"]
                .as_u64()
                .filter(|kind| *kind <= 1)
                .ok_or_else(invalid_ticket_permissions)?;
            if !targets.insert((kind, id)) {
                return Err(invalid_ticket_permissions());
            }
            overwrites.push(TicketOverwrite {
                id,
                kind,
                allow: ticket_permission_bits(&row["allow"])?,
                deny: ticket_permission_bits(&row["deny"])?,
            });
        }
        let required = (Permissions::VIEW_CHANNEL | Permissions::READ_MESSAGE_HISTORY).bits();
        let member = overwrites
            .iter()
            .find(|row| row.kind == 1 && row.id == bot.get());
        // Member allows override every role deny. Our generated private ticket
        // has these explicit grants; no guild role fetch is needed to prove them.
        if member.is_some_and(|row| row.allow & required == required && row.deny & required == 0) {
            return Ok(true);
        }

        let req = Self::request_of(self.inner.factory.guild_member(guild, bot))?;
        self.pace(false).await;
        let res = self.call_once_raw(req, &[200]).await?;
        let member_doc = ticket_json(&res)?;
        if ticket_evidence_id(&member_doc["user"]["id"])? != bot.get() {
            return Err(invalid_ticket_permissions());
        }
        let held = member_doc["roles"]
            .as_array()
            .ok_or_else(invalid_ticket_permissions)?
            .iter()
            .map(ticket_evidence_id)
            .collect::<Result<std::collections::HashSet<_>, _>>()?;
        let req = Self::request_of(self.inner.factory.roles(guild))?;
        self.pace(false).await;
        let res = self.call_once_raw(req, &[200]).await?;
        let roles_doc = ticket_json(&res)?;
        let roles = roles_doc
            .as_array()
            .ok_or_else(invalid_ticket_permissions)?;
        let mut known = std::collections::HashSet::new();
        let mut permissions = 0;
        for role in roles {
            let id = ticket_evidence_id(&role["id"])?;
            let bits = ticket_permission_bits(&role["permissions"])?;
            if !known.insert(id) {
                return Err(invalid_ticket_permissions());
            }
            if id == guild.get() || held.contains(&id) {
                permissions |= bits;
            }
        }
        // An incomplete list could hide an administrator or a role grant.
        if !known.contains(&guild.get()) || !held.is_subset(&known) {
            return Err(invalid_ticket_permissions());
        }
        if permissions & Permissions::ADMINISTRATOR.bits() != 0 {
            return Ok(true);
        }
        if let Some(everyone) = overwrites
            .iter()
            .find(|row| row.kind == 0 && row.id == guild.get())
        {
            permissions = (permissions & !everyone.deny) | everyone.allow;
        }
        let mut allow = 0;
        let mut deny = 0;
        for row in overwrites
            .iter()
            .filter(|row| row.kind == 0 && row.id != guild.get() && held.contains(&row.id))
        {
            allow |= row.allow;
            deny |= row.deny;
        }
        permissions = (permissions & !deny) | allow;
        if let Some(member) = member {
            permissions = (permissions & !member.deny) | member.allow;
        }
        Ok(permissions & required == required)
    }

    pub async fn delete_ticket_channel(&self, channel_id: &str) -> Result<(), DiscordError> {
        let req = Self::request_of(self.inner.factory.delete_channel(snowflake(channel_id)?))?;
        self.pace(false).await;
        let res = self.call_once_raw(req, &[200, 204, 404]).await?;
        if res.status == 404 && !unknown_channel(&res) {
            return Err(throw_for_status(&res));
        }
        Ok(())
    }

    /// Preserve every existing opener overwrite bit except SendMessages.
    /// Member type is explicit, unlike the moderation @everyone role seam.
    pub async fn set_ticket_opener_writes(
        &self,
        channel_id: &str,
        opener_id: &str,
        allow: u64,
        deny: u64,
        enabled: bool,
    ) -> Result<(), DiscordError> {
        let bit = Permissions::SEND_MESSAGES.bits();
        let overwrite = PermissionOverwrite {
            id: snowflake(opener_id)?,
            kind: PermissionOverwriteType::Member,
            allow: Some(Permissions::from_bits_retain(if enabled {
                allow | bit
            } else {
                allow & !bit
            })),
            deny: Some(Permissions::from_bits_retain(if enabled {
                deny & !bit
            } else {
                deny | bit
            })),
        };
        let req = Self::request_of(
            self.inner
                .factory
                .update_channel_permission(snowflake(channel_id)?, &overwrite),
        )?;
        self.pace(false).await;
        self.call_once(req, &[200, 204]).await.map(|_| ())
    }

    pub async fn post_ticket_message(
        &self,
        channel_id: &str,
        message: TicketMessage<'_>,
    ) -> Result<String, DiscordError> {
        let (content, buttons, opener) = match message {
            TicketMessage::Panel => (
                PANEL_TEXT.to_owned(),
                json!([{"type":2,"style":1,"label":"Open a ticket","custom_id":TICKET_OPEN_ID}]),
                None,
            ),
            TicketMessage::Controls { opener_id } => {
                snowflake::<UserMarker>(opener_id)?;
                (
                    format!("<@{opener_id}> A staff member will help you.\nTranscripts are retained for 90 days after closing."),
                    json!([
                        {"type":2,"style":2,"label":"Claim","custom_id":TICKET_CLAIM_ID},
                        {"type":2,"style":4,"label":"Close","custom_id":TICKET_CLOSE_ID},
                    ]),
                    Some(opener_id),
                )
            }
        };
        let mut body = json!({"content":content,"components":[{"type":1,"components":buttons}]});
        crate::message_safety::sanitize_message(&mut body);
        // The ONLY notification opt-in: a validated opener id in a fixed template.
        // No caller-supplied content or parse/role policy crosses this boundary.
        if let Some(opener) = opener {
            body["allowed_mentions"]["users"] = json!([opener]);
        }
        crate::message_safety::validate_create(&body)?;
        let bytes = serde_json::to_vec(&body)
            .map_err(|_| DiscordError::Rejected("invalid ticket message".into()))?;
        let channel: Id<ChannelMarker> = snowflake(channel_id)?;
        let req = Request::builder(&Route::CreateMessage {
            channel_id: channel.get(),
        })
        .body(bytes)
        .build()
        .map_err(|e| DiscordError::Rejected(format!("build: {e}")))?;
        self.pace(false).await;
        let res = self.call_once_raw(req, &[200, 201]).await?;
        let doc = ticket_json(&res)?;
        let id = doc
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| DiscordError::Unavailable("ticket message omitted id".into()))?;
        snowflake::<MessageMarker>(id)
            .map_err(|_| DiscordError::Unavailable("invalid ticket message id".into()))?;
        Ok(id.to_owned())
    }
}

struct TicketOverwrite {
    id: u64,
    kind: u64,
    allow: u64,
    deny: u64,
}

fn invalid_ticket_permissions() -> DiscordError {
    DiscordError::Unavailable("invalid ticket permission evidence".into())
}

fn ticket_permission_bits(value: &Value) -> Result<u64, DiscordError> {
    value
        .as_str()
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|value| value.parse().ok())
        .ok_or_else(invalid_ticket_permissions)
}

fn ticket_evidence_id(value: &Value) -> Result<u64, DiscordError> {
    let id = ticket_permission_bits(value)?;
    if id == 0 || value.as_str() != Some(id.to_string().as_str()) {
        return Err(invalid_ticket_permissions());
    }
    Ok(id)
}

fn ticket_json(res: &RawResponse) -> Result<Value, DiscordError> {
    serde_json::from_slice(&res.body)
        .map_err(|_| DiscordError::Unavailable("invalid ticket response JSON".into()))
}

fn unknown_channel(res: &RawResponse) -> bool {
    serde_json::from_slice::<Value>(&res.body)
        .ok()
        .and_then(|body| body.get("code").and_then(Value::as_u64))
        == Some(10003)
}
