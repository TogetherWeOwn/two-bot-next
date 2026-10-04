//! Member moderation Discord seam: `MemberDiscord` on the shared REST executor.
//!
//! Implements `two_bot_core::member_moderation::MemberDiscord` for
//! [`ActionExecutor`] by delegating to the existing moderation-lane verbs
//! (`ban`, `unban`, `kick_member`, `timeout_member`): one attempt each, the
//! legacy 5 s abort, no auto-retry. No private dispatcher, no second HTTP
//! client.
//!
//! Executor failures map to the member-claim vocabulary: definite refusals
//! (and local guard refusals, which never reach the wire) become `Rejected`,
//! so the idempotency claim is safe to release; timeouts, transport/5xx
//! failures and rate limits stay uncertain, so the claim remains `in_flight`
//! and the sweep never guesses.

use two_bot_core::member_moderation::{DiscordError as MemberError, MemberDiscord};

use super::executor::{ActionExecutor, DiscordError as ExecutorError};

/// Map one executor failure to the member-claim vocabulary.
///
/// `Rejected` proves no mutation happened. `Guard` never reaches the wire, so
/// it is equally safe to release. Every other failure leaves the outcome
/// unknowable: the claim stays fenced for operator reconciliation.
fn member_error(err: ExecutorError) -> MemberError {
    match err {
        ExecutorError::Rejected(detail) => MemberError::Rejected(detail),
        ExecutorError::Timeout => MemberError::Timeout,
        ExecutorError::Unavailable(detail) => MemberError::Unavailable(detail),
        ExecutorError::RateLimited => MemberError::RateLimited,
        ExecutorError::Guard(guard) => MemberError::Rejected(guard.to_string()),
    }
}

impl MemberDiscord for ActionExecutor {
    async fn ban(&self, guild_id: &str, user_id: &str, reason: &str) -> Result<(), MemberError> {
        ActionExecutor::ban(self, guild_id, user_id, reason)
            .await
            .map_err(member_error)
    }

    async fn unban(&self, guild_id: &str, user_id: &str, reason: &str) -> Result<(), MemberError> {
        ActionExecutor::unban(self, guild_id, user_id, reason)
            .await
            .map_err(member_error)
    }

    async fn kick(&self, guild_id: &str, user_id: &str, reason: &str) -> Result<(), MemberError> {
        ActionExecutor::kick_member(self, guild_id, user_id, reason)
            .await
            .map_err(member_error)
    }

    async fn timeout(
        &self,
        guild_id: &str,
        user_id: &str,
        until_iso: &str,
        reason: &str,
    ) -> Result<(), MemberError> {
        ActionExecutor::timeout_member(self, guild_id, user_id, Some(until_iso), reason)
            .await
            .map_err(member_error)
    }
}
