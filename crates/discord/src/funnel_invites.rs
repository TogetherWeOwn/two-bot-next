//! Permanent invites and vanity-URL checks for the Discord-native funnel.
//!
//! The funnel keeps exactly one invite code: a permanent (never-expiring,
//! unlimited-use) channel invite that the website links to. Rotation creates
//! the replacement through [`ActionExecutor::create_permanent_invite`] and
//! hands the old/new pair to the invite tracker's rotation; no campaign slot
//! is added. [`ActionExecutor::check_vanity_url`] reports whether the guild
//! can show a vanity URL at all — a refusal there is a capability gate, never
//! a funnel failure.

use twilight_http::request::Request;
use twilight_http::routing::Route;
use twilight_model::id::marker::{ChannelMarker, GuildMarker};

use crate::executor::snowflake;
use crate::{ActionExecutor, DiscordError};

/// One permanent channel invite: the code plus the channel it was created in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermanentInvite {
    pub code: String,
    pub channel_id: String,
}

/// What the guild's vanity URL read reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VanityStatus {
    /// The guild has a vanity URL with this code.
    Active { code: String },
    /// The read succeeded and no vanity code is set.
    Absent,
    /// Discord refused the read (forbidden, unknown endpoint): this guild
    /// cannot have a vanity URL. The check reports the gate; it never fails
    /// the funnel.
    CapabilityGated,
}

/// Failures for the permanent-invite and vanity-URL calls.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InviteActionError {
    /// Discord created an invite, but it expires or is use-limited: it is not
    /// a permanent funnel invite.
    #[error("Discord answered with an expiring invite")]
    NotPermanent,
    /// Discord answered with an unreadable invite payload.
    #[error("Discord answered with an unreadable invite")]
    InvalidResponse,
    /// Transport or Discord failure.
    #[error(transparent)]
    Discord(#[from] DiscordError),
}

impl ActionExecutor {
    /// Create a permanent invite in `channel_id`.
    ///
    /// Sends `POST /channels/{channel_id}/invites` with an empty body (an
    /// empty body is valid and requests Discord's defaults, which the caller
    /// must still verify). A reply with `max_age` 0 and `max_uses` 0 reports
    /// the code; any other expiry reports [`InviteActionError::NotPermanent`].
    /// No automatic retry: an unreadable success keeps the execution fence
    /// (fail closed), exactly like the scheduled-event executors.
    pub async fn create_permanent_invite(
        &self,
        channel_id: &str,
    ) -> Result<PermanentInvite, InviteActionError> {
        let channel = snowflake::<ChannelMarker>(channel_id)?;
        let route = Route::CreateInvite {
            channel_id: channel.get(),
        };
        let request = Request::builder(&route)
            .build()
            .map_err(|error| DiscordError::Rejected(format!("build: {error}")))?;
        let mut response = self.call_once_raw(request, &[200, 201]).await?;
        let body: serde_json::Value = serde_json::from_slice(&response.body)
            .map_err(|_| InviteActionError::InvalidResponse)?;
        let code = body
            .get("code")
            .and_then(serde_json::Value::as_str)
            .ok_or(InviteActionError::InvalidResponse)?;
        let max_age = body.get("max_age").and_then(serde_json::Value::as_i64);
        let max_uses = body.get("max_uses").and_then(serde_json::Value::as_i64);
        // The creation reached the wire either way, so the validated outcome
        // consumes completion; only an unreadable reply keeps the fence.
        response.complete().await;
        if max_age != Some(0) || max_uses != Some(0) {
            return Err(InviteActionError::NotPermanent);
        }
        Ok(PermanentInvite {
            code: code.to_owned(),
            channel_id: channel_id.to_owned(),
        })
    }

    /// Check the guild's vanity URL.
    ///
    /// Sends `GET /guilds/{guild_id}/vanity-url`. A set code reports
    /// [`VanityStatus::Active`], a null code reports [`VanityStatus::Absent`],
    /// and a refusal (forbidden, unknown endpoint) reports
    /// [`VanityStatus::CapabilityGated`] instead of failing.
    pub async fn check_vanity_url(
        &self,
        guild_id: &str,
    ) -> Result<VanityStatus, InviteActionError> {
        let guild = snowflake::<GuildMarker>(guild_id)?;
        let route = Route::GetGuildVanityUrl {
            guild_id: guild.get(),
        };
        // Forbidden and unknown-endpoint are accepted outcomes, not transport
        // errors: they ARE the capability answer, so they consume completion
        // like any validated read instead of holding the lane.
        let mut response = self
            .call_once_raw(Request::from_route(&route), &[200, 403, 404])
            .await?;
        if response.status == 403 || response.status == 404 {
            response.complete().await;
            return Ok(VanityStatus::CapabilityGated);
        }
        let body: serde_json::Value = serde_json::from_slice(&response.body)
            .map_err(|_| InviteActionError::InvalidResponse)?;
        let status = match body.get("code").and_then(serde_json::Value::as_str) {
            Some(code) => VanityStatus::Active {
                code: code.to_owned(),
            },
            None => VanityStatus::Absent,
        };
        response.complete().await;
        Ok(status)
    }
}
