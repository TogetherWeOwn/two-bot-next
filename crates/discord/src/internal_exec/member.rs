//! Website member actions, independent of the HTTP receiver.
//!
//! Single-attempt mutations: the caller owns retry/idempotency and action gates.
//! Never format a request builder, provider body, or transport error here: any
//! of them may contain the member's OAuth token.

use super::{ActionExecutor, RawResponse};
use crate::ratelimit_guard::GuardError;
use serde::Deserialize;
use std::time::Duration;
use twilight_http::request::TryIntoRequest;
use twilight_model::id::{marker::GenericMarker, Id};
use two_bot_core::internal_actions::{
    ActionError, ErrorCode, GuildAddMemberRequest, RoleAssignRequest,
};

#[cfg(feature = "db")]
#[path = "member_store.rs"]
pub mod store;

pub const ADD_MEMBER_TIMEOUT_MS: u64 = 1500;
pub const ROLE_TIMEOUT_MS: u64 = 2000;

/// Keep proof of an unsent mutation until the durable caller disposes its claim.
/// Public callers still receive the legacy scalar-only `ActionError` envelope.
enum MemberError {
    Guard(GuardError),
    Action(ActionError),
}

impl From<ActionError> for MemberError {
    fn from(error: ActionError) -> Self {
        Self::Action(error)
    }
}

impl MemberError {
    fn into_action_error(self) -> ActionError {
        match self {
            Self::Guard(GuardError::TokenInvalid) => ActionError::new(
                ErrorCode::DiscordUnavailable,
                "Discord bot authentication is unavailable",
                "discord_guard_refused",
            ),
            Self::Guard(_) => ActionError::new(
                ErrorCode::RateLimited,
                "Discord request paused locally before dispatch",
                "discord_guard_refused",
            )
            .with_retry_after(1),
            Self::Action(error) => error,
        }
    }
}

/// Exactly the legacy result object, without a provider response or secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberOutcome {
    Added,
    AlreadyMember,
    Assigned,
    AlreadyHeld,
}

impl MemberOutcome {
    #[must_use]
    pub fn result(self) -> serde_json::Value {
        serde_json::json!({"outcome": self})
    }

    /// Legacy successBody insertion order is part of the website wire contract.
    #[must_use]
    pub fn success_body(self, request_id: &str) -> serde_json::Value {
        serde_json::json!({"ok": true, "result": self.result(), "request_id": request_id})
    }
}

/// Legacy internal/discordActions.ts throwForStatus, not moderation's body-based
/// retry parser. Only safe status/header scalars reach the public error.
pub fn member_status_error(response: &RawResponse) -> Option<ActionError> {
    let status = response.status;
    if (200..300).contains(&status) {
        return None;
    }
    // Raw Hyper never follows redirects. Do not persist an unfollowed exchange
    // as success or forward credentials to the Location target.
    if status < 400 {
        return Some(ActionError::new(
            ErrorCode::DiscordUnavailable,
            format!("Discord returned unexpected status {status}"),
            "discord_unexpected_status",
        ));
    }
    if status == 429 {
        let retry = response
            .retry_after_header
            .as_deref()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|s| s.is_finite() && *s >= 0.0)
            .unwrap_or(1.0)
            .ceil()
            .max(1.0) as u64;
        return Some(
            ActionError::new(
                ErrorCode::RateLimited,
                "Discord rate-limited this request",
                "discord_rate_limited",
            )
            .with_retry_after(retry),
        );
    }
    if status >= 500 {
        return Some(ActionError::new(
            ErrorCode::DiscordUnavailable,
            format!("Discord returned {status}"),
            "discord_5xx",
        ));
    }
    let hint = if status == 403 {
        " (check the bot has the permission, and that its highest role is above the target role)"
    } else {
        ""
    };
    Some(ActionError::new(
        ErrorCode::DiscordRejected,
        format!("Discord refused the request with {status}{hint}"),
        format!("discord_{status}"),
    ))
}

fn numeric_id(value: &str) -> Result<Id<GenericMarker>, ActionError> {
    value
        .parse::<u64>()
        .ok()
        .and_then(Id::new_checked)
        .ok_or_else(|| {
            ActionError::new(ErrorCode::Malformed, "Invalid Discord ID", "bad_discord_id")
        })
}

fn unreadable() -> ActionError {
    ActionError::new(
        ErrorCode::DiscordUnavailable,
        "Discord returned an unreadable role snapshot",
        "role_fetch_failed",
    )
}

#[derive(Deserialize)]
struct RoleSnapshot {
    id: String,
    position: i64,
    managed: bool,
}

impl ActionExecutor {
    async fn member_request<T: TryIntoRequest>(
        &self,
        build: T,
        timeout_ms: u64,
    ) -> Result<RawResponse, MemberError> {
        let request = build.try_into_request().map_err(|_| {
            ActionError::new(
                ErrorCode::Malformed,
                "Invalid Discord request",
                "discord_request_invalid",
            )
        })?;
        let (response, _) = self
            .send_with_timeout_for(&request, None, Duration::from_millis(timeout_ms))
            .await
            .map_err(|error| match error {
                super::DiscordError::Guard(error) => MemberError::Guard(error),
                super::DiscordError::Timeout => MemberError::Action(ActionError::new(
                    ErrorCode::UpstreamTimeout,
                    format!("Discord did not answer in {timeout_ms}ms"),
                    "discord_timeout",
                )),
                _ => MemberError::Action(ActionError::new(
                    ErrorCode::DiscordUnavailable,
                    "Discord was unreachable",
                    "discord_unreachable",
                )),
            })?;
        if let Some(error) = member_status_error(&response) {
            tracing::warn!(code = error.code.as_str(), reason = %error.log_reason,
                "internal member action refused");
            return Err(error.into());
        }
        Ok(response)
    }

    async fn internal_member_roles(
        &self,
        guild_id: &str,
        user_id: &str,
    ) -> Result<Option<Vec<String>>, MemberError> {
        let response = self
            .member_request(
                self.inner
                    .factory
                    .guild_member(numeric_id(guild_id)?.cast(), numeric_id(user_id)?.cast()),
                ROLE_TIMEOUT_MS,
            )
            .await?;
        #[derive(Deserialize)]
        struct Member {
            roles: Vec<String>,
        }
        Ok(serde_json::from_slice::<Member>(&response.body)
            .ok()
            .map(|m| m.roles))
    }

    /// PUT /guilds/{g}/members/{u}. The token is a function argument and wire
    /// body only. Source: https://docs.discord.com/developers/resources/guild#add-guild-member
    pub async fn add_internal_member(
        &self,
        guild_id: &str,
        request: &GuildAddMemberRequest<'_>,
        access_token: &str,
    ) -> Result<MemberOutcome, ActionError> {
        self.add_internal_member_once(guild_id, request, access_token)
            .await
            .map_err(MemberError::into_action_error)
    }

    async fn add_internal_member_once(
        &self,
        guild_id: &str,
        request: &GuildAddMemberRequest<'_>,
        access_token: &str,
    ) -> Result<MemberOutcome, MemberError> {
        if access_token.is_empty() {
            return Err(ActionError::new(
                ErrorCode::Malformed,
                "\"access_token\" must be a non-empty string",
                "missing_access_token",
            )
            .into());
        }
        let response = self
            .member_request(
                self.inner.factory.add_guild_member(
                    numeric_id(guild_id)?.cast(),
                    numeric_id(request.discord_id())?.cast(),
                    access_token,
                ),
                ADD_MEMBER_TIMEOUT_MS,
            )
            .await?;
        Ok(if response.status == 201 {
            MemberOutcome::Added
        } else {
            MemberOutcome::AlreadyMember
        })
    }

    /// A resolved allowlisted role, with an authoritative hierarchy read before
    /// mutation. Source: https://docs.discord.com/developers/topics/permissions#permission-hierarchy
    pub async fn assign_internal_role(
        &self,
        guild_id: &str,
        bot_user_id: &str,
        request: &RoleAssignRequest<'_>,
    ) -> Result<MemberOutcome, ActionError> {
        self.assign_internal_role_once(guild_id, bot_user_id, request)
            .await
            .map_err(MemberError::into_action_error)
    }

    async fn assign_internal_role_once(
        &self,
        guild_id: &str,
        bot_user_id: &str,
        request: &RoleAssignRequest<'_>,
    ) -> Result<MemberOutcome, MemberError> {
        let guild = numeric_id(guild_id)?.cast();
        let user = numeric_id(request.discord_id())?.cast();
        let role = numeric_id(request.role_id())?.cast();
        numeric_id(bot_user_id)?;
        // Legacy: failure to read the target is not proof of absence, but a
        // redundant role PUT is naturally idempotent. A 429 must stop all further
        // REST calls during cooldown. Policy reads still fail closed.
        match self
            .internal_member_roles(guild_id, request.discord_id())
            .await
        {
            Ok(Some(held)) if held.iter().any(|id| id == request.role_id()) => {
                return Ok(MemberOutcome::AlreadyHeld);
            }
            Err(error @ MemberError::Guard(_)) => return Err(error),
            Err(MemberError::Action(error)) if error.code == ErrorCode::RateLimited => {
                return Err(error.into());
            }
            _ => {}
        }
        let bot_roles = self
            .internal_member_roles(guild_id, bot_user_id)
            .await?
            .ok_or_else(unreadable)?;
        let response = self
            .member_request(self.inner.factory.roles(guild), ROLE_TIMEOUT_MS)
            .await?;
        let roles: Vec<RoleSnapshot> =
            serde_json::from_slice(&response.body).map_err(|_| unreadable())?;
        // Reject malformed/duplicate snapshot IDs, including unknown bot roles.
        let mut ids = std::collections::HashSet::new();
        for snapshot in &roles {
            numeric_id(&snapshot.id).map_err(|_| unreadable())?;
            if !ids.insert(snapshot.id.as_str()) {
                return Err(unreadable());
            }
        }
        if !ids.contains(guild_id) || bot_roles.iter().any(|id| !ids.contains(id.as_str())) {
            return Err(unreadable());
        }
        let target = roles
            .iter()
            .find(|r| r.id == request.role_id())
            .ok_or_else(unreadable)?;
        let highest = roles
            .iter()
            .filter(|r| r.id == guild_id || bot_roles.contains(&r.id))
            .map(|r| r.position)
            .max()
            .ok_or_else(unreadable)?;
        if target.managed || target.id == guild_id || target.position >= highest {
            return Err(member_status_error(&RawResponse {
                status: 403,
                retry_after_header: None,
                body: vec![],
            })
            .expect("403 is a refusal")
            .into());
        }
        self.member_request(
            self.inner.factory.add_guild_member_role(guild, user, role),
            ROLE_TIMEOUT_MS,
        )
        .await?;
        Ok(MemberOutcome::Assigned)
    }
}
