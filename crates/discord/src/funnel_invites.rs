//! Permanent invites and vanity-URL checks for the Discord-native funnel.
//!
//! The funnel keeps exactly one invite code: a permanent (never-expiring,
//! unlimited-use) channel invite that the website links to. Rotation creates
//! the replacement through [`ActionExecutor::create_permanent_invite`] and
//! hands the old/new pair to the invite tracker's rotation; no campaign slot
//! is added. [`ActionExecutor::check_vanity_url`] reports whether the guild
//! can show a vanity URL at all — the guild-side refusal (no vanity feature)
//! is a capability gate, never a funnel failure; any other refusal fails.
//! Callers must confirm the bot is a member of the guild before calling:
//! Discord returns 50001 and 10004 inconsistently for unauthorized guild
//! access, so the 50001 mapping below is only a "no vanity feature" answer
//! under that precondition.

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
    /// Discord refused the read with code 50001 (Missing Access) while the
    /// caller has confirmed the bot is a member of the guild. Under that
    /// precondition the refusal means the guild has no vanity feature; the
    /// check reports the gate and never fails the funnel. Discord returns
    /// 50001 and 10004 inconsistently for unauthorized guild access (see
    /// discord-api-docs#5840), so without confirmed membership a 50001
    /// cannot be told apart from a removed bot. Any other refusal is a
    /// caller-side fault and fails as [`InviteActionError::Discord`].
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
    /// Sends `POST /channels/{channel_id}/invites` with
    /// `{max_age: 0, max_uses: 0, unique: true}` built through the twilight
    /// request factory. Discord requires a JSON body and defaults an empty
    /// one to a 24-hour expiry (`max_age` 86400, `unique` false), so an empty
    /// body can never yield a permanent invite. A reply with `max_age` 0 and
    /// `max_uses` 0 reports the code; any other expiry reports
    /// [`InviteActionError::NotPermanent`]. No automatic retry: an unreadable
    /// success keeps the execution fence (fail closed), exactly like the
    /// scheduled-event executors.
    pub async fn create_permanent_invite(
        &self,
        channel_id: &str,
    ) -> Result<PermanentInvite, InviteActionError> {
        let channel = snowflake::<ChannelMarker>(channel_id)?;
        let request = Self::request_of(
            self.request_factory()
                .create_invite(channel)
                .max_age(0)
                .max_uses(0)
                .unique(true),
        )?;
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
    /// Sends `GET /guilds/{guild_id}/vanity-url`, which requires
    /// `MANAGE_GUILD`. A set code reports [`VanityStatus::Active`], a null
    /// code reports [`VanityStatus::Absent`], and a Missing Access refusal
    /// (Discord code 50001) reports [`VanityStatus::CapabilityGated`]
    /// instead of failing — but only when the caller has confirmed the bot
    /// is a member of the guild, in which case the refusal means the guild
    /// has no vanity feature. Discord returns 50001 and 10004
    /// inconsistently for unauthorized guild access, so no error code on
    /// its own proves the removed-bot case; every other refusal, including
    /// any other Discord code or an unreadable refusal, is a fixable
    /// caller-side problem and fails as [`InviteActionError::Discord`].
    pub async fn check_vanity_url(
        &self,
        guild_id: &str,
    ) -> Result<VanityStatus, InviteActionError> {
        let guild = snowflake::<GuildMarker>(guild_id)?;
        let route = Route::GetGuildVanityUrl {
            guild_id: guild.get(),
        };
        // Only the Missing Access refusal observed while the caller has
        // confirmed bot membership is a capability answer that consumes
        // completion like a validated read. Every other refusal is a fixable
        // setup problem: it fails instead of reporting a silent gate, so a
        // misconfigured or removed bot is never mistaken for a guild that
        // cannot have a vanity URL without the caller first confirming
        // membership.
        let mut response = self
            .call_once_raw(Request::from_route(&route), &[200, 403, 404])
            .await?;
        if response.status == 403 || response.status == 404 {
            let discord_code = serde_json::from_slice::<serde_json::Value>(&response.body)
                .ok()
                .and_then(|body| body.get("code").and_then(serde_json::Value::as_i64));
            response.complete().await;
            if discord_code == Some(50001) {
                return Ok(VanityStatus::CapabilityGated);
            }
            let detail = match discord_code {
                Some(code) => format!(
                    "vanity URL refused: HTTP {} Discord code {code}",
                    response.status
                ),
                None => format!("vanity URL refused: HTTP {}", response.status),
            };
            return Err(InviteActionError::Discord(DiscordError::Rejected(detail)));
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
