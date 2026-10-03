//! Website member moderation, independent of the HTTP receiver.
//!
//! Executes `moderation.ban`, `moderation.tempban`, `moderation.kick`,
//! `moderation.timeout` and `moderation.warn` through the shared
//! [`MemberModerationService`](two_bot_core::member_moderation::MemberModerationService),
//! so the durable ledger rows are identical whether the action was triggered
//! by a slash command or by the website. The receiver must authorize the
//! internal request (HMAC, allowlist, nonce/replay rules), parse its body with
//! [`InternalMemberRequest::from_body`], and resolve both the actor and the
//! target from the configured guild using actual member roles and permissions
//! — never from body-supplied roles or permissions. Request ids and timestamps
//! are server-generated, not website body values.
//!
//! The audit ledger keeps the plain human reason; only the Discord wire reason
//! carries the `core::mac` marker, signed per idempotency key. Refusals
//! (hierarchy, protected roles, missing permission) happen before any claim or
//! wire call. The unban sweep itself stays with the service slice; this module
//! only schedules tempban expiries through it.

use serde_json::{Map, Value};
use two_bot_core::internal_actions::{
    require_reason, require_snowflake, validate_idempotency_key, validate_moderation_numbers,
    ActionError, ErrorCode,
};
use two_bot_core::mac::moderation_audit_reason;
use two_bot_core::member_moderation::{
    DiscordError as MemberDiscordError, MemberDiscord, MemberError, MemberExecution,
    MemberModerationService, MemberModerationStore,
};
use two_bot_core::{ModerationAction, ModerationActor, ModerationPolicy, ModerationTarget};

use crate::{ActionExecutor, DiscordError};

/// Parsed website fields; construct only after request authorization. Actor and
/// target ids are attribution requests, not proof of guild membership or
/// permissions — the runtime resolves both before calling
/// [`InternalMemberExecutor::execute`].
#[derive(Debug, Clone)]
pub struct InternalMemberRequest {
    action: ModerationAction,
    actor_id: String,
    target_id: String,
    reason: String,
    duration_seconds: Option<i64>,
}

impl InternalMemberRequest {
    pub fn from_body(action: &str, body: &Map<String, Value>) -> Result<Self, ActionError> {
        let action = match action {
            "moderation.ban" => ModerationAction::Ban,
            "moderation.tempban" => ModerationAction::TempBan,
            "moderation.kick" => ModerationAction::Kick,
            "moderation.timeout" => ModerationAction::Timeout,
            "moderation.warn" => ModerationAction::Warn,
            _ => {
                return Err(error(
                    ErrorCode::ActionNotAllowed,
                    "not a member moderation action",
                ))
            }
        };
        let actor_id = require_snowflake(body, "actor_id")?.to_owned();
        let target_id = require_snowflake(body, "discord_id")?.to_owned();
        // Tighten the legacy shape check to canonical, nonzero u64 identities:
        // ledger keys and REST identities must never refer to different members.
        for id in [&actor_id, &target_id] {
            if !canonical_id(id) {
                return Err(error(ErrorCode::Malformed, "invalid Discord id"));
            }
        }
        let reason = require_reason(body.get("reason").unwrap_or(&Value::Null))?;
        validate_moderation_numbers(action, body.get("duration_seconds"), None, None)?;
        let duration_seconds = match body.get("duration_seconds") {
            None => None,
            Some(value) => Some(value.as_i64().ok_or_else(|| {
                error(ErrorCode::Malformed, "duration_seconds must be an integer")
            })?),
        };
        Ok(Self {
            action,
            actor_id,
            target_id,
            reason,
            duration_seconds,
        })
    }

    #[must_use]
    pub fn action(&self) -> ModerationAction {
        self.action
    }

    #[must_use]
    pub fn actor_id(&self) -> &str {
        &self.actor_id
    }

    #[must_use]
    pub fn target_id(&self) -> &str {
        &self.target_id
    }

    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }

    #[must_use]
    pub fn duration_seconds(&self) -> Option<i64> {
        self.duration_seconds
    }
}

/// Runtime-only configuration. `enabled` must reflect BOTH `TWO_MODERATION`
/// and `TWO_INTERNAL_ALLOW_MODERATION`. No Debug: the audit secret must not
/// enter logs.
#[derive(Clone)]
pub struct InternalMemberConfig {
    pub guild_id: String,
    pub enabled: bool,
    pub policy: ModerationPolicy,
    pub audit_secret: Option<String>,
}

#[derive(Clone)]
pub struct InternalMemberExecutor<S> {
    store: S,
    discord: ActionExecutor,
    config: InternalMemberConfig,
}

/// Legacy `runModerationAction` outcome, separate from the HTTP envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InternalMemberResult {
    pub outcome: String,
    pub replayed: bool,
}

impl InternalMemberResult {
    #[must_use]
    pub fn response(&self) -> Value {
        let mut response = serde_json::json!({
            "result": {"outcome": self.outcome},
            "outcome": self.outcome,
        });
        if self.replayed {
            response["innerReplayed"] = Value::Bool(true);
        }
        response
    }
}

impl<S> InternalMemberExecutor<S>
where
    S: MemberModerationStore + Clone,
{
    pub fn new(
        store: S,
        discord: ActionExecutor,
        config: InternalMemberConfig,
    ) -> Result<Self, ActionError> {
        if !canonical_id(&config.guild_id) {
            return Err(error(ErrorCode::Malformed, "invalid configured guild id"));
        }
        Ok(Self {
            store,
            discord,
            config,
        })
    }

    /// `actor` and `target` must be resolved in the configured guild by the
    /// runtime (legacy resolver), NEVER built from caller-supplied
    /// roles/permissions. `bot_highest_role_position` is the runtime's read of
    /// the bot's own standing in the guild. `now_ms` is the caller's clock
    /// reading in unix millis; it is injected so tests assert exact expiries.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute(
        &self,
        request: &InternalMemberRequest,
        actor: &ModerationActor,
        target: &ModerationTarget,
        bot_highest_role_position: Option<i64>,
        request_id: &str,
        idempotency_key: &str,
        now_ms: i64,
    ) -> Result<InternalMemberResult, ActionError> {
        validate_idempotency_key(Some(idempotency_key), request.action.action_name())?;
        if !self.config.enabled
            || actor.user_id != request.actor_id
            || target.user_id != request.target_id
        {
            return Err(error(
                ErrorCode::ActionNotAllowed,
                "moderation disabled or actor/target not resolved",
            ));
        }
        let action_name = request.action.action_name();
        let signing = SigningDiscord {
            inner: &self.discord,
            guild_id: self.config.guild_id.clone(),
            idempotency_key: idempotency_key.to_owned(),
            action: action_name,
            actor_id: actor.user_id.clone(),
            secret: self.config.audit_secret.clone(),
        };
        let service = MemberModerationService::new(
            signing,
            self.store.clone(),
            self.config.policy.clone(),
            move || now_ms,
        );
        let execution = MemberExecution {
            action: request.action,
            guild_id: self.config.guild_id.clone(),
            actor: actor.clone(),
            target: Some(target.clone()),
            bot_highest_role_position,
            reason: request.reason.clone(),
            duration_seconds: request.duration_seconds,
            request_id: request_id.to_owned(),
            idempotency_key: idempotency_key.to_owned(),
        };
        let result = service.execute(&execution).await.map_err(member_error)?;
        Ok(InternalMemberResult {
            outcome: result.outcome.as_str().to_owned(),
            replayed: result.replayed,
        })
    }
}

/// Signs the Discord-bound reason while the ledger keeps the human one, so
/// slash-command and website rows stay identical. The marker binds guild, key,
/// action and actor; only the human suffix shortens to fit 512 UTF-16 units.
struct SigningDiscord<'a> {
    inner: &'a ActionExecutor,
    guild_id: String,
    idempotency_key: String,
    action: &'static str,
    actor_id: String,
    secret: Option<String>,
}

impl SigningDiscord<'_> {
    fn signed(&self, reason: &str) -> String {
        bounded_audit_reason(
            self.secret.as_deref(),
            &self.guild_id,
            &self.idempotency_key,
            self.action,
            &self.actor_id,
            reason,
        )
    }
}

impl MemberDiscord for SigningDiscord<'_> {
    async fn ban(
        &self,
        guild_id: &str,
        user_id: &str,
        reason: &str,
    ) -> Result<(), MemberDiscordError> {
        self.inner
            .ban(guild_id, user_id, &self.signed(reason))
            .await
            .map_err(map_discord_error)
    }

    async fn unban(
        &self,
        guild_id: &str,
        user_id: &str,
        reason: &str,
    ) -> Result<(), MemberDiscordError> {
        self.inner
            .unban(guild_id, user_id, &self.signed(reason))
            .await
            .map_err(map_discord_error)
    }

    async fn kick(
        &self,
        guild_id: &str,
        user_id: &str,
        reason: &str,
    ) -> Result<(), MemberDiscordError> {
        self.inner
            .kick_member(guild_id, user_id, &self.signed(reason))
            .await
            .map_err(map_discord_error)
    }

    async fn timeout(
        &self,
        guild_id: &str,
        user_id: &str,
        until_iso: &str,
        reason: &str,
    ) -> Result<(), MemberDiscordError> {
        self.inner
            .timeout_member(guild_id, user_id, Some(until_iso), &self.signed(reason))
            .await
            .map_err(map_discord_error)
    }
}

/// Local guard refusals never reached the wire, so like confirmed 4xx they
/// prove no mutation happened and the claim is safe to release; the message
/// stays truthful about the pause.
fn map_discord_error(error: DiscordError) -> MemberDiscordError {
    match error {
        DiscordError::Rejected(detail) => MemberDiscordError::Rejected(detail),
        DiscordError::Guard(refusal) => MemberDiscordError::Rejected(refusal.to_string()),
        DiscordError::Timeout => MemberDiscordError::Timeout,
        DiscordError::RateLimited => MemberDiscordError::RateLimited,
        DiscordError::Unavailable(detail) => MemberDiscordError::Unavailable(detail),
    }
}

fn member_error(error: MemberError) -> ActionError {
    let (code, message) = match &error {
        MemberError::Policy(refusal) => (ErrorCode::ActionNotAllowed, refusal.to_string()),
        MemberError::Reason(reason) => (ErrorCode::Malformed, reason.to_string()),
        MemberError::Malformed { field, message } => (
            ErrorCode::Malformed,
            format!("malformed {field}: {message}"),
        ),
        MemberError::InFlight => (
            ErrorCode::InProgress,
            "moderation attempt is still in progress".to_owned(),
        ),
        MemberError::KeyMismatch => (
            ErrorCode::Malformed,
            "idempotency key was used for different moderation content".to_owned(),
        ),
        MemberError::Discord(MemberDiscordError::Rejected(detail)) => {
            (ErrorCode::DiscordRejected, detail.clone())
        }
        MemberError::Discord(MemberDiscordError::Timeout) => {
            (ErrorCode::UpstreamTimeout, error.to_string())
        }
        MemberError::Discord(MemberDiscordError::RateLimited) => {
            (ErrorCode::RateLimited, error.to_string())
        }
        MemberError::Discord(MemberDiscordError::Unavailable(_)) => {
            (ErrorCode::DiscordUnavailable, error.to_string())
        }
        MemberError::Store(_) => (
            ErrorCode::Internal,
            "moderation persistence failed; do not repeat an uncertain effect".to_owned(),
        ),
    };
    let action = ActionError::new(code, message, "internal_member_moderation");
    match code {
        ErrorCode::RateLimited => action.with_retry_after(1),
        _ => action,
    }
}

fn canonical_id(value: &str) -> bool {
    two_bot_core::internal_actions::is_snowflake(value)
        && value
            .parse::<u64>()
            .is_ok_and(|n| n != 0 && n.to_string() == value)
}

/// Preserve the complete MAC marker, truncating only the unsigned human suffix
/// at scalar boundaries so the final header stays within 512 UTF-16 units.
fn bounded_audit_reason(
    secret: Option<&str>,
    guild: &str,
    key: &str,
    action: &str,
    actor: &str,
    reason: &str,
) -> String {
    let prefix = moderation_audit_reason(secret, guild, key, action, actor, "");
    if prefix.is_empty() {
        return reason.to_owned();
    }
    let mut budget = 512usize.saturating_sub(prefix.encode_utf16().count());
    let suffix: String = reason
        .chars()
        .take_while(|c| {
            if c.len_utf16() > budget {
                false
            } else {
                budget -= c.len_utf16();
                true
            }
        })
        .collect();
    format!("{prefix}{suffix}")
}

fn error(code: ErrorCode, message: impl Into<String>) -> ActionError {
    ActionError::new(code, message, "internal_member_moderation")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(extra: Value) -> Map<String, Value> {
        let mut body = serde_json::json!({
            "actor_id": "111111111111111111",
            "discord_id": "333333333333333333",
            "reason": "  spam  ",
        });
        for (key, value) in extra.as_object().unwrap() {
            body[key] = value.clone();
        }
        body.as_object().unwrap().clone()
    }

    #[test]
    fn validation_rejects_before_any_store_or_wire() {
        for (action, extra) in [
            ("moderation.purge", serde_json::json!({})),
            ("announcement.post", serde_json::json!({})),
            (
                "moderation.tempban",
                serde_json::json!({"duration_seconds": 59}),
            ),
            (
                "moderation.tempban",
                serde_json::json!({"duration_seconds": 365 * 24 * 60 * 60 + 1}),
            ),
            (
                "moderation.tempban",
                serde_json::json!({"duration_seconds": "3600"}),
            ),
            (
                "moderation.timeout",
                serde_json::json!({"duration_seconds": 28 * 24 * 60 * 60 + 1}),
            ),
            (
                "moderation.kick",
                serde_json::json!({"actor_id": "00000000000000000"}),
            ),
            (
                "moderation.warn",
                serde_json::json!({"discord_id": "99999999999999999999"}),
            ),
            ("moderation.warn", serde_json::json!({"reason": ""})),
            (
                "moderation.ban",
                serde_json::json!({"reason": "🦀".repeat(257)}),
            ),
        ] {
            let err =
                InternalMemberRequest::from_body(action, &body(extra)).expect_err("must refuse");
            assert!(
                matches!(err.code, ErrorCode::Malformed | ErrorCode::ActionNotAllowed),
                "{action}: {err:?}"
            );
        }
    }

    #[test]
    fn validation_accepts_all_five_verbs_with_bounds() {
        for (action, extra, duration) in [
            ("moderation.ban", serde_json::json!({}), None),
            (
                "moderation.tempban",
                serde_json::json!({"duration_seconds": 3600}),
                Some(3600),
            ),
            ("moderation.kick", serde_json::json!({}), None),
            (
                "moderation.timeout",
                serde_json::json!({"duration_seconds": 60}),
                Some(60),
            ),
            ("moderation.warn", serde_json::json!({}), None),
        ] {
            let request = InternalMemberRequest::from_body(action, &body(extra)).expect("valid");
            assert_eq!(request.action.action_name(), action);
            assert_eq!(request.duration_seconds, duration);
            // The shared moderation reason trims; the ledger stores the trimmed form.
            assert_eq!(request.reason, "spam");
        }
    }

    #[test]
    fn result_response_marks_replay() {
        let fresh = InternalMemberResult {
            outcome: "banned".to_owned(),
            replayed: false,
        };
        assert_eq!(
            fresh.response(),
            serde_json::json!({"result": {"outcome": "banned"}, "outcome": "banned"})
        );
        let replayed = InternalMemberResult {
            outcome: "kicked".to_owned(),
            replayed: true,
        };
        assert_eq!(
            replayed.response(),
            serde_json::json!({
                "result": {"outcome": "kicked"},
                "outcome": "kicked",
                "innerReplayed": true,
            })
        );
    }
}
