//! Member-notify DM delivery (`reminder.send`, `place_offer.send`).
//!
//! Single-attempt sends over the member transport's bounded exchange: the
//! caller owns gating, retry and idempotency, and the claim fence guarantees
//! a single execution. Never format a request builder, provider body, or
//! transport error here: any of them may contain member details. A delivered
//! DM never consumes a place on its own; the site's state machine decides
//! that on the linked page.

use super::{
    is_definitive_rejection,
    member::{member_status_error, numeric_id, MemberError},
    ActionExecutor, RawResponse,
};
use serde::Deserialize;
use twilight_http::request::TryIntoRequest;
use twilight_model::channel::message::AllowedMentions;
use two_bot_core::{
    internal_actions::{ActionError, ErrorCode, PlaceOfferSendRequest, ReminderSendRequest},
    message_safety,
};

/// Per-call Discord budget for opening the DM channel and for posting to it.
/// Both calls plus claim work must fit inside the receiver's 20s
/// `REQUEST_TIMEOUT`, like the role-call budget this matches.
pub const NOTIFY_DM_TIMEOUT_MS: u64 = 2000;

/// Exactly what the receiver needs: the posted message id for the audit row,
/// or proof the member cannot be DM'd — closed DMs, a blocked bot, or no
/// shared guild are a 200 `dm_unavailable` outcome, never a place consumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyOutcome {
    Sent { message_id: String },
    DmUnavailable,
}

/// A definitive Discord refusal from either DM call means the member cannot
/// be DM'd — deliberately including "not in guild", which arrives as a 403
/// or 404 from the channel-open call. Anything else (rate limits, 5xx,
/// timeouts, unreadable receipts) stays an error so the caller reconciles or
/// retries instead of reporting a delivery that never happened.
fn classify_notify_error(error: ActionError) -> Result<NotifyOutcome, ActionError> {
    match error.code {
        ErrorCode::DiscordRejected => Ok(NotifyOutcome::DmUnavailable),
        _ => Err(error),
    }
}

// Membership preserves its legacy 4xx mapping. Notifications must instead
// retain uncertainty for ambiguous statuses, and a bot-token outage is never
// a member's DM refusal. Keep classification at the raw status boundary.
fn notify_status_error(response: &RawResponse) -> Option<ActionError> {
    let error = member_status_error(response)?;
    if error.code == ErrorCode::DiscordRejected
        && (response.status == 401 || !is_definitive_rejection(response.status))
    {
        return Some(ActionError::new(
            ErrorCode::DiscordUnavailable,
            "Discord delivery could not be confirmed",
            "dm_delivery_unconfirmed",
        ));
    }
    Some(error)
}

fn unreadable_receipt() -> ActionError {
    ActionError::new(
        ErrorCode::DiscordUnavailable,
        "Discord returned an unreadable DM receipt",
        "dm_receipt_unreadable",
    )
}

#[derive(Deserialize)]
struct OpenedChannel {
    id: String,
}

#[derive(Deserialize)]
struct PostedMessage {
    id: String,
}

impl ActionExecutor {
    /// Open (or reuse) the DM channel with `discord_id`, then post the
    /// bot-rendered `text` with no mentions allowed. One attempt per call.
    /// Sources: <https://docs.discord.com/developers/resources/user#create-dm>
    /// and <https://docs.discord.com/developers/resources/message#create-message>.
    async fn send_notify_dm_once(
        &self,
        discord_id: &str,
        text: &str,
    ) -> Result<NotifyOutcome, MemberError> {
        let mut channel_response = self
            .member_request_with_status(
                self.inner
                    .factory
                    .create_private_channel(numeric_id(discord_id)?.cast())
                    .try_into_request(),
                NOTIFY_DM_TIMEOUT_MS,
                notify_status_error,
            )
            .await?;
        let channel: OpenedChannel =
            serde_json::from_slice(&channel_response.body).map_err(|_| unreadable_receipt())?;
        let channel_id = numeric_id(&channel.id)
            .map_err(|_| unreadable_receipt())?
            .cast();
        channel_response.complete().await;
        let mentions = AllowedMentions::default();
        let mut message_response = self
            .member_request_with_status(
                self.inner
                    .factory
                    .create_message(channel_id)
                    .content(&message_safety::content(text))
                    .allowed_mentions(Some(&mentions))
                    .try_into_request(),
                NOTIFY_DM_TIMEOUT_MS,
                notify_status_error,
            )
            .await?;
        let message: PostedMessage =
            serde_json::from_slice(&message_response.body).map_err(|_| unreadable_receipt())?;
        // The message id lands in the audit row, so it must be a snowflake.
        let message_id = numeric_id(&message.id)
            .map_err(|_| unreadable_receipt())?
            .get()
            .to_string();
        message_response.complete().await;
        Ok(NotifyOutcome::Sent { message_id })
    }

    /// `reminder.send` delivery: post the validated bot-side template.
    /// The caller owns gating, idempotency and the `sent`/`dm_unavailable`
    /// receipt mapping.
    pub async fn send_reminder_dm(
        &self,
        request: &ReminderSendRequest<'_>,
    ) -> Result<NotifyOutcome, ActionError> {
        self.send_notify_dm_once(request.discord_id(), &request.dm_text())
            .await
            .map_err(MemberError::into_action_error)
            .or_else(classify_notify_error)
    }

    /// `place_offer.send` delivery: post the validated bot-side template.
    /// The caller owns gating, idempotency and the `sent`/`dm_unavailable`
    /// receipt mapping.
    pub async fn send_place_offer_dm(
        &self,
        request: &PlaceOfferSendRequest<'_>,
    ) -> Result<NotifyOutcome, ActionError> {
        self.send_notify_dm_once(request.discord_id(), &request.dm_text())
            .await
            .map_err(MemberError::into_action_error)
            .or_else(classify_notify_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ratelimit_guard::GuardError;

    #[test]
    fn definitive_refusals_are_dm_unavailable_everything_else_is_an_error() {
        for status in [400, 403, 404, 405, 413, 415, 422] {
            let refused = ActionError::new(
                ErrorCode::DiscordRejected,
                format!("Discord refused the request with {status}"),
                format!("discord_{status}"),
            );
            assert_eq!(
                classify_notify_error(refused),
                Ok(NotifyOutcome::DmUnavailable),
                "{status}"
            );
        }
        for error in [
            ActionError::new(
                ErrorCode::RateLimited,
                "Discord rate-limited this request",
                "discord_rate_limited",
            )
            .with_retry_after(7),
            ActionError::new(
                ErrorCode::DiscordUnavailable,
                "Discord returned 503",
                "discord_5xx",
            ),
            unreadable_receipt(),
        ] {
            let retry_after = error.retry_after_secs;
            let code = error.code;
            let err = classify_notify_error(error).expect_err("not a refusal");
            assert_eq!(err.code, code);
            assert_eq!(err.retry_after_secs, retry_after);
        }
    }

    #[test]
    fn guard_refusals_keep_their_member_transport_mapping() {
        // The notify transport reuses the member guard mapping: an invalid
        // token is an unknown-outcome failure, any other local pause is a
        // retryable rate limit. Neither is a refusal, so neither becomes
        // `dm_unavailable`.
        let err =
            classify_notify_error(MemberError::Guard(GuardError::TokenInvalid).into_action_error())
                .expect_err("invalid token is unknown");
        assert_eq!(err.code, ErrorCode::DiscordUnavailable);
        let err =
            classify_notify_error(MemberError::Guard(GuardError::CircuitOpen).into_action_error())
                .expect_err("local pause is retryable");
        assert_eq!(err.code, ErrorCode::RateLimited);
        assert_eq!(err.retry_after_secs, Some(1));
    }
}
