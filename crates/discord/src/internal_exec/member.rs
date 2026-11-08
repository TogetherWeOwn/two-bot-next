//! Website member actions, independent of the HTTP receiver.
//!
//! Single-attempt mutations: the caller owns retry/idempotency and action gates.
//! Never format a request builder, provider body, or transport error here: any
//! of them may contain the member's OAuth token.

use super::{ActionExecutor, RawResponse};
use crate::ratelimit_guard::GuardError;
use serde::Deserialize;
use std::time::Duration;
use twilight_http::request::{Request, TryIntoRequest};
use twilight_model::id::{marker::GenericMarker, Id};
use two_bot_core::internal_actions::{
    ActionError, ErrorCode, GuildAddMemberRequest, RoleAssignRequest,
};

#[cfg(feature = "db")]
#[path = "member_store.rs"]
pub mod store;

pub const ADD_MEMBER_TIMEOUT_MS: u64 = 1500;
pub const ROLE_TIMEOUT_MS: u64 = 2000;
pub const VOICE_MOVE_TIMEOUT_MS: u64 = 2000;

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

/// Discord `Retry-After` seconds, defaulting to 1 and never 0: a
/// `Retry-After: 0` invites a hot loop. Shared by the member refusal mapper
/// and the voice-move classifier.
fn discord_retry_after_secs(response: &RawResponse) -> u64 {
    response
        .retry_after_header
        .as_deref()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|s| s.is_finite() && *s >= 0.0)
        .unwrap_or(1.0)
        .ceil()
        .max(1.0) as u64
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
        return Some(
            ActionError::new(
                ErrorCode::RateLimited,
                "Discord rate-limited this request",
                "discord_rate_limited",
            )
            .with_retry_after(discord_retry_after_secs(response)),
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

/// Status-distinguishing result of the voice-move PATCH. Only the status and
/// the Discord `Retry-After` travel back: never a provider body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceMovePatch {
    /// Discord applied the move (2xx).
    Moved,
    /// 403: the local pre-checks passed but Discord refused — the bot's live
    /// grant disagrees with the evaluated snapshot.
    Denied,
    /// 400/404: member or room state raced the PATCH; re-evaluate live state.
    Stale,
    /// 429 with the Discord-supplied wait.
    Limited { retry_after_secs: u64 },
    /// Anything else (5xx, redirect, transport): outcome unknown, retryable.
    Unavailable,
    /// Proven pre-wire refusal (durable send admission blocked/unavailable,
    /// or the bot credential guard refused): no request reached Discord. Retryable, and
    /// the caller may release any unobserved-seat reservation.
    NotSent,
}

fn move_patch_outcome(response: &RawResponse) -> VoiceMovePatch {
    match response.status {
        200..300 => VoiceMovePatch::Moved,
        403 => VoiceMovePatch::Denied,
        400 | 404 => VoiceMovePatch::Stale,
        429 => VoiceMovePatch::Limited {
            retry_after_secs: discord_retry_after_secs(response),
        },
        _ => VoiceMovePatch::Unavailable,
    }
}

/// Shared mapping of a failed bounded exchange to the scalar-only envelope.
fn send_error(error: super::DiscordError, timeout_ms: u64) -> MemberError {
    match error {
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
    }
}

impl ActionExecutor {
    // Only building the request depends on its Twilight builder type. Keep the
    // bounded exchange in one concrete async state machine; callers own the
    // refusal mapping (`member_request` for legacy throw-for-status, the
    // voice-move classifier for status-distinguishing PATCH answers).
    async fn discord_send(
        &self,
        request: Request,
        timeout_ms: u64,
    ) -> Result<RawResponse, MemberError> {
        let (response, _) = self
            .send_with_timeout_for(&request, None, Duration::from_millis(timeout_ms))
            .await
            .map_err(|error| send_error(error, timeout_ms))?;
        Ok(response)
    }

    #[inline(never)]
    async fn member_request(
        &self,
        request: Result<Request, twilight_http::Error>,
        timeout_ms: u64,
    ) -> Result<RawResponse, MemberError> {
        let request = request.map_err(|_| {
            ActionError::new(
                ErrorCode::Malformed,
                "Invalid Discord request",
                "discord_request_invalid",
            )
        })?;
        let response = self.discord_send(request, timeout_ms).await?;
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
                    .guild_member(numeric_id(guild_id)?.cast(), numeric_id(user_id)?.cast())
                    .try_into_request(),
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
    /// body only. Source: <https://docs.discord.com/developers/resources/guild#add-guild-member>
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
        let mut response = self
            .member_request(
                self.inner
                    .factory
                    .add_guild_member(
                        numeric_id(guild_id)?.cast(),
                        numeric_id(request.discord_id())?.cast(),
                        access_token,
                    )
                    .try_into_request(),
                ADD_MEMBER_TIMEOUT_MS,
            )
            .await?;
        let outcome = match response.status {
            201 => MemberOutcome::Added,
            200 | 204 => MemberOutcome::AlreadyMember,
            _ => return Err(unreadable().into()),
        };
        // This endpoint's accepted status distinguishes added/already present;
        // no message/resource id is consumed by the caller.
        response.complete().await;
        Ok(outcome)
    }

    /// A resolved allowlisted role, with an authoritative hierarchy read before
    /// mutation. Source: <https://docs.discord.com/developers/topics/permissions#permission-hierarchy>
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
            .member_request(
                self.inner.factory.roles(guild).try_into_request(),
                ROLE_TIMEOUT_MS,
            )
            .await?;
        let roles: Vec<RoleSnapshot> =
            serde_json::from_slice(&response.body).map_err(|_| unreadable())?;
        // Reject malformed/duplicate snapshot IDs, including unknown bot roles.
        let mut ids = std::collections::HashSet::new();
        for snapshot in &roles {
            numeric_id(&snapshot.id).map_err(|_| unreadable())?;
            if !ids.insert(snapshot.id.as_str()) {
                return Err(unreadable().into());
            }
        }
        if !ids.contains(guild_id) || bot_roles.iter().any(|id| !ids.contains(id.as_str())) {
            return Err(unreadable().into());
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
                completion: None,
            })
            .expect("403 is a refusal")
            .into());
        }
        let mut response = self
            .member_request(
                self.inner
                    .factory
                    .add_guild_member_role(guild, user, role)
                    .try_into_request(),
                ROLE_TIMEOUT_MS,
            )
            .await?;
        if response.status != 204 {
            return Err(unreadable().into());
        }
        response.complete().await;
        Ok(MemberOutcome::Assigned)
    }

    /// PATCH /guilds/{guild}/members/{user} with `channel_id`: the whole
    /// voice move in one bounded attempt. The receiver owns gates, ordering
    /// and retries; this reports what Discord said, never a guess. Only a
    /// 2xx retains its permit to the receipt boundary, like the other
    /// mutations; every other answer drops it. Source:
    /// <https://docs.discord.com/developers/resources/guild#modify-guild-member>
    pub async fn move_voice_member(
        &self,
        guild_id: &str,
        member_id: &str,
        channel_id: &str,
    ) -> Result<VoiceMovePatch, ActionError> {
        let request = self
            .inner
            .factory
            .update_guild_member(numeric_id(guild_id)?.cast(), numeric_id(member_id)?.cast())
            .channel_id(Some(numeric_id(channel_id)?.cast()))
            .try_into_request()
            .map_err(|_| {
                ActionError::new(
                    ErrorCode::Malformed,
                    "Invalid Discord request",
                    "discord_request_invalid",
                )
            })?;
        // Keep the typed error until classified: a durable admission refusal
        // or an invalid-credential guard refusal proves nothing was sent.
        let mut response = match self
            .send_with_timeout_for(&request, None, Duration::from_millis(VOICE_MOVE_TIMEOUT_MS))
            .await
        {
            Ok((response, _)) => response,
            Err(super::DiscordError::Guard(GuardError::TokenInvalid)) => {
                return Ok(VoiceMovePatch::NotSent);
            }
            Err(error) if error.is_admission_blocked() || error.is_admission_storage() => {
                return Ok(VoiceMovePatch::NotSent);
            }
            Err(error) => {
                return Err(send_error(error, VOICE_MOVE_TIMEOUT_MS).into_action_error());
            }
        };
        let outcome = move_patch_outcome(&response);
        if outcome == VoiceMovePatch::Moved {
            response.complete().await;
        }
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(status: u16, retry_after: Option<&str>) -> RawResponse {
        RawResponse {
            status,
            retry_after_header: retry_after.map(str::to_owned),
            body: vec![],
            completion: None,
        }
    }

    #[test]
    fn voice_move_patch_classifies_statuses() {
        assert_eq!(move_patch_outcome(&raw(200, None)), VoiceMovePatch::Moved);
        assert_eq!(move_patch_outcome(&raw(204, None)), VoiceMovePatch::Moved);
        assert_eq!(move_patch_outcome(&raw(403, None)), VoiceMovePatch::Denied);
        assert_eq!(move_patch_outcome(&raw(400, None)), VoiceMovePatch::Stale);
        assert_eq!(move_patch_outcome(&raw(404, None)), VoiceMovePatch::Stale);
        assert_eq!(
            move_patch_outcome(&raw(429, Some("7"))),
            VoiceMovePatch::Limited {
                retry_after_secs: 7
            }
        );
        assert_eq!(
            move_patch_outcome(&raw(429, None)),
            VoiceMovePatch::Limited {
                retry_after_secs: 1
            }
        );
        assert_eq!(
            move_patch_outcome(&raw(429, Some("garbage"))),
            VoiceMovePatch::Limited {
                retry_after_secs: 1
            }
        );
        assert_eq!(
            move_patch_outcome(&raw(502, None)),
            VoiceMovePatch::Unavailable
        );
        assert_eq!(
            move_patch_outcome(&raw(301, None)),
            VoiceMovePatch::Unavailable
        );
    }

    #[test]
    fn discord_retry_after_never_zero_or_nan() {
        assert_eq!(discord_retry_after_secs(&raw(429, Some("0"))), 1);
        assert_eq!(discord_retry_after_secs(&raw(429, Some("1.2"))), 2);
        assert_eq!(discord_retry_after_secs(&raw(429, Some("NaN"))), 1);
        assert_eq!(discord_retry_after_secs(&raw(429, Some("-3"))), 1);
    }

    #[tokio::test]
    async fn invalid_member_request_keeps_its_redacted_error() {
        let executor = ActionExecutor::with_proxy(
            "fixture-bot-token".into(),
            Some("http://127.0.0.1:1".into()),
        )
        .expect("loopback executor");
        let content = "fixture-private-content".repeat(100);
        let error = executor
            .member_request(
                executor
                    .inner
                    .factory
                    .create_message(Id::new(1))
                    .content(&content)
                    .try_into_request(),
                ROLE_TIMEOUT_MS,
            )
            .await
            .expect_err("oversized request must fail before dispatch");
        let MemberError::Action(error) = error else {
            panic!("invalid request must not reach admission");
        };
        assert_eq!(error.code, ErrorCode::Malformed);
        assert_eq!(error.message, "Invalid Discord request");
        assert_eq!(error.log_reason, "discord_request_invalid");
        assert!(!format!("{error:?}").contains("fixture"));
    }
}
