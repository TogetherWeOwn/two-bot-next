//! Self-role REST operations on the shared executor, never a private client.
//!
//! The runtime owns event/panel claims, renewal, intent, compensation and audit.
//! A role step reserves the shared paced lane, checks ownership immediately
//! before sending, and checks it again after the single bounded exchange. No
//! retry can escape those fences. An accepted exchange with lost ownership is
//! still an observed effect, not success of the enclosing self-role operation.

use super::{ActionExecutor, RawResponse, MODERATION_TIMEOUT_MS};
use serde::{de::DeserializeOwned, Deserialize};
use std::{collections::HashSet, future::Future, time::Duration};
use twilight_http::request::TryIntoRequest;
use twilight_model::id::{marker::GenericMarker, Id};
use two_bot_core::self_roles::{
    is_snowflake, validate_self_role_dispatch, ChannelOverwrite, ChannelSnapshot, DispatchCheck,
    DispatchFailure, DispatchRole, SelfRolePanel,
};

/// Scalar errors only: neither provider bodies nor transport detail reach logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SelfRoleRestError {
    #[error("invalid self-role Discord id")]
    InvalidId,
    #[error("self-role snapshot could not be verified")]
    Snapshot,
    #[error("self-role claim is no longer owned")]
    StaleClaim,
    #[error("Discord refused the self-role request ({0})")]
    Rejected(u16),
    #[error("Discord rate-limited the self-role request")]
    RateLimited,
    #[error("self-role Discord exchange has an uncertain result")]
    Ambiguous,
}

/// Even when the post-call fence fails, retain the exchange result for auditing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleExchange {
    pub result: Result<(), SelfRoleRestError>,
    pub owned_after: bool,
}

#[derive(Debug, Clone)]
pub struct SelfRoleSnapshot {
    pub member_role_ids: HashSet<String>,
    pub member_is_bot: bool,
    pub roles: Vec<DispatchRole>,
    pub channels: Vec<ChannelSnapshot>,
    pub bot_has_manage_roles: bool,
    pub bot_highest_position: i64,
}

impl SelfRoleSnapshot {
    #[must_use]
    pub fn validate(
        &self,
        guild_id: &str,
        panel: &SelfRolePanel,
        role_ids: &[String],
    ) -> Option<DispatchFailure> {
        validate_self_role_dispatch(&DispatchCheck {
            guild_id,
            panel,
            role_ids,
            roles: &self.roles,
            channels: &self.channels,
            bot_has_manage_roles: self.bot_has_manage_roles,
            bot_highest_position: self.bot_highest_position,
        })
    }
}

#[derive(Deserialize)]
struct User {
    id: String,
    #[serde(default)]
    bot: bool,
}
#[derive(Deserialize)]
struct Member {
    user: User,
    roles: Vec<String>,
}
#[derive(Deserialize)]
struct Role {
    id: String,
    permissions: String,
    color: u32,
    managed: bool,
    position: i64,
}
#[derive(Deserialize)]
struct Overwrite {
    id: String,
    #[serde(rename = "type")]
    kind: u8,
    allow: String,
    deny: String,
}
#[derive(Deserialize)]
struct Channel {
    id: String,
    guild_id: String,
    name: Option<String>,
    permission_overwrites: Vec<Overwrite>,
}
#[derive(Deserialize)]
struct Message {
    id: String,
    channel_id: String,
}

fn numeric_id(value: &str) -> Result<Id<GenericMarker>, SelfRoleRestError> {
    if !is_snowflake(value) {
        return Err(SelfRoleRestError::InvalidId);
    }
    value
        .parse::<u64>()
        .ok()
        .and_then(Id::new_checked)
        .ok_or(SelfRoleRestError::InvalidId)
}

fn mask(value: &str) -> Result<u64, SelfRoleRestError> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(SelfRoleRestError::Snapshot);
    }
    value.parse().map_err(|_| SelfRoleRestError::Snapshot)
}

fn status(response: &RawResponse) -> Result<(), SelfRoleRestError> {
    match response.status {
        200..=299 => Ok(()),
        429 => Err(SelfRoleRestError::RateLimited),
        400..=499 => Err(SelfRoleRestError::Rejected(response.status)),
        _ => Err(SelfRoleRestError::Ambiguous),
    }
}

impl ActionExecutor {
    async fn self_role_read<T: DeserializeOwned>(
        &self,
        builder: impl TryIntoRequest,
    ) -> Result<T, SelfRoleRestError> {
        let request = builder
            .try_into_request()
            .map_err(|_| SelfRoleRestError::InvalidId)?;
        let mut lane = self.paced_lane(false).await;
        *lane = std::time::Instant::now();
        let response = tokio::time::timeout(
            Duration::from_millis(MODERATION_TIMEOUT_MS),
            self.send(&request),
        )
        .await
        .map_err(|_| SelfRoleRestError::Ambiguous)?
        .map_err(|_| SelfRoleRestError::Ambiguous)?;
        status(&response)?;
        serde_json::from_slice(&response.body).map_err(|_| SelfRoleRestError::Snapshot)
    }

    /// Reaction removes carry no member, and adds may carry stale member data.
    /// Fetch the configured message through the shared REST seam for either
    /// partial path; do not infer its identity from an unfetched cache entry.
    pub async fn fetch_self_role_message(
        &self,
        channel_id: &str,
        message_id: &str,
    ) -> Result<(), SelfRoleRestError> {
        let message: Message = self
            .self_role_read(self.inner.factory.message(
                numeric_id(channel_id)?.cast(),
                numeric_id(message_id)?.cast(),
            ))
            .await?;
        if message.id != message_id || message.channel_id != channel_id {
            return Err(SelfRoleRestError::Snapshot);
        }
        Ok(())
    }

    /// Force-fetch all policy and member data AFTER lane acquisition, including
    /// channels for overwrite safety. No gateway cache or caller permission mask
    /// is authoritative. Unknown/duplicate IDs and partial policy fail closed.
    pub async fn fetch_self_role_snapshot(
        &self,
        guild_id: &str,
        member_id: &str,
        bot_id: &str,
    ) -> Result<SelfRoleSnapshot, SelfRoleRestError> {
        let guild = numeric_id(guild_id)?.cast();
        let member: Member = self
            .self_role_read(
                self.inner
                    .factory
                    .guild_member(guild, numeric_id(member_id)?.cast()),
            )
            .await?;
        let bot: Member = self
            .self_role_read(
                self.inner
                    .factory
                    .guild_member(guild, numeric_id(bot_id)?.cast()),
            )
            .await?;
        if member.user.id != member_id || bot.user.id != bot_id || !bot.user.bot {
            return Err(SelfRoleRestError::Snapshot);
        }
        let roles: Vec<Role> = self.self_role_read(self.inner.factory.roles(guild)).await?;
        let mut ids = HashSet::new();
        let mut resolved = Vec::with_capacity(roles.len());
        for role in roles {
            numeric_id(&role.id).map_err(|_| SelfRoleRestError::Snapshot)?;
            if !ids.insert(role.id.clone()) || role.position < 0 {
                return Err(SelfRoleRestError::Snapshot);
            }
            resolved.push(DispatchRole {
                id: role.id,
                permissions: mask(&role.permissions)?,
                color: role.color,
                managed: role.managed,
                position: role.position,
            });
        }
        if !ids.contains(guild_id)
            || member
                .roles
                .iter()
                .chain(&bot.roles)
                .any(|id| !ids.contains(id))
        {
            return Err(SelfRoleRestError::Snapshot);
        }
        let mut permissions = 0;
        let mut highest = 0;
        for role in &resolved {
            if role.id == guild_id || bot.roles.contains(&role.id) {
                permissions |= role.permissions;
                highest = highest.max(role.position);
            }
        }
        let channels: Vec<Channel> = self
            .self_role_read(self.inner.factory.guild_channels(guild))
            .await?;
        let mut seen = HashSet::new();
        let mut channel_snapshots = Vec::with_capacity(channels.len());
        for channel in channels {
            numeric_id(&channel.id).map_err(|_| SelfRoleRestError::Snapshot)?;
            if channel.guild_id != guild_id || !seen.insert(channel.id.clone()) {
                return Err(SelfRoleRestError::Snapshot);
            }
            let mut overwrite_ids = HashSet::new();
            let mut overwrites = Vec::with_capacity(channel.permission_overwrites.len());
            for overwrite in channel.permission_overwrites {
                numeric_id(&overwrite.id).map_err(|_| SelfRoleRestError::Snapshot)?;
                if overwrite.kind > 1
                    || !overwrite_ids.insert((overwrite.id.clone(), overwrite.kind))
                {
                    return Err(SelfRoleRestError::Snapshot);
                }
                overwrites.push(ChannelOverwrite {
                    id: overwrite.id,
                    kind: overwrite.kind,
                    allow: mask(&overwrite.allow)?,
                    deny: mask(&overwrite.deny)?,
                });
            }
            channel_snapshots.push(ChannelSnapshot {
                id: channel.id,
                name: channel.name,
                overwrites,
            });
        }
        Ok(SelfRoleSnapshot {
            member_role_ids: member.roles.into_iter().collect(),
            member_is_bot: member.user.bot,
            roles: resolved,
            channels: channel_snapshots,
            bot_has_manage_roles: permissions & ((1 << 28) | (1 << 3)) != 0,
            bot_highest_position: highest,
        })
    }

    /// One singular role operation. The caller passes a freshly validated plan
    /// and a DB-backed check of BOTH claims. Reservation includes the late check
    /// and exchange, preventing a request from sitting unfenced behind pacing.
    /// Renewal must run concurrently in the runtime; false/error stops the send.
    pub async fn self_role_step<F, Fut>(
        &self,
        guild_id: &str,
        member_id: &str,
        role_id: &str,
        add: bool,
        owns: F,
    ) -> Result<RoleExchange, SelfRoleRestError>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<bool, SelfRoleRestError>>,
    {
        self.self_role_step_journaled(guild_id, member_id, role_id, add, owns, || async { Ok(()) })
            .await
    }

    /// The runtime journals send intent inside the shared paced reservation.
    /// A failed checkpoint prevents the send; ownership is rechecked after the
    /// database wait. Cancellation after the checkpoint leaves unresolved intent
    /// for authoritative recovery, never an invented successful exchange.
    pub async fn self_role_step_journaled<F, Fut, J, Journal>(
        &self,
        guild_id: &str,
        member_id: &str,
        role_id: &str,
        add: bool,
        owns: F,
        journal: J,
    ) -> Result<RoleExchange, SelfRoleRestError>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<bool, SelfRoleRestError>>,
        J: FnOnce() -> Journal,
        Journal: Future<Output = Result<(), SelfRoleRestError>>,
    {
        let guild = numeric_id(guild_id)?.cast();
        let member = numeric_id(member_id)?.cast();
        let role = numeric_id(role_id)?.cast();
        if role_id == guild_id {
            return Err(SelfRoleRestError::InvalidId);
        }
        let request = if add {
            self.inner
                .factory
                .add_guild_member_role(guild, member, role)
                .try_into_request()
        } else {
            self.inner
                .factory
                .remove_guild_member_role(guild, member, role)
                .try_into_request()
        }
        .map_err(|_| SelfRoleRestError::InvalidId)?;
        let mut lane = self.paced_lane(false).await;
        if !owns().await? {
            return Err(SelfRoleRestError::StaleClaim);
        }
        journal().await?;
        if !owns().await? {
            return Err(SelfRoleRestError::StaleClaim);
        }
        *lane = std::time::Instant::now();
        let result = match tokio::time::timeout(
            Duration::from_millis(MODERATION_TIMEOUT_MS),
            self.send(&request),
        )
        .await
        {
            Ok(Ok(response)) if response.status == 204 => Ok(()),
            Ok(Ok(response)) => status(&response).and(Err(SelfRoleRestError::Ambiguous)),
            _ => Err(SelfRoleRestError::Ambiguous),
        };
        let owned_after = owns().await.unwrap_or(false);
        Ok(RoleExchange {
            result,
            owned_after,
        })
    }
}
