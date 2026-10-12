//! Stream funnel: Twitch live alerts + YouTube VOD links (pure helpers, no I/O).
//!
//! Scope and quota confirmed 2026-10-12 against the official references:
//!
//! - Twitch EventSub `stream.online` version 1: no OAuth scope is required,
//!   an app access token from the client-credentials flow authorizes the
//!   subscription, the condition carries `broadcaster_user_id`, and the
//!   transports are webhook, WebSocket and Conduit. EventSub delivery itself
//!   carries no per-event quota units.
//! - YouTube Data API v3 `videos.insert`: 1,600 quota units per upload against
//!   the default 10,000 units/day project quota (about six uploads a day;
//!   increases go through the quota-extension form). There is no
//!   Community-post creation endpoint, so this module never plans one.
//!
//! Push-only by design: live alerts consume EventSub push notifications, and
//! VOD discovery reuses the existing no-quota Atom/RSS polling in `feeds`.
//! This module provides no Helix polling, no `youtube.search` loop and no
//! network, secret, database or Discord client of its own. Transport wiring
//! (webhook receiver, secret handling, announcements-channel posting) is a
//! separate reviewed slice with its own CISO/Operator gates.

use sha2::{Digest, Sha256};
use thiserror::Error;

/// EventSub subscription type accepted here.
pub const STREAM_ONLINE_TYPE: &str = "stream.online";
/// EventSub subscription version accepted here.
pub const STREAM_ONLINE_VERSION: &str = "1";
/// YouTube `videos.insert` quota cost in units per upload.
pub const YOUTUBE_UPLOAD_COST_UNITS: u32 = 1_600;
/// YouTube default daily project quota in units.
pub const YOUTUBE_DEFAULT_DAILY_QUOTA_UNITS: u32 = 10_000;
/// Discord message content ceiling in UTF-16 units.
const MESSAGE_LIMIT_UNITS: usize = 2000;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum StreamFunnelError {
    #[error("Event is not a stream.online notification.")]
    WrongType,
    #[error("Event is missing a required field.")]
    MissingField,
    #[error("Broadcaster id is invalid.")]
    InvalidBroadcasterId,
    #[error("Broadcaster login is invalid.")]
    InvalidBroadcasterLogin,
    #[error("Event timestamp is invalid.")]
    InvalidStartedAt,
    #[error("Discord invite URL is invalid.")]
    InvalidInviteUrl,
    #[error("Title is too long to fit the announcement.")]
    TitleTooLong,
}

/// A validated Twitch `stream.online` go-live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamOnline {
    /// Twitch broadcaster user id (numeric string).
    pub broadcaster_user_id: String,
    /// Twitch broadcaster login (lowercase, URL-safe).
    pub broadcaster_login: String,
    /// Go-live instant from the event payload.
    pub started_at: String,
}

fn event_string(event: &serde_json::Value, field: &str) -> Result<String, StreamFunnelError> {
    event
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or(StreamFunnelError::MissingField)
}

/// Validate a Twitch broadcaster user id: 1-32 ASCII digits.
fn valid_broadcaster_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 32 && value.bytes().all(|b| b.is_ascii_digit())
}

/// Validate a Twitch broadcaster login: 1-25 ASCII alphanumerics/underscore.
fn valid_broadcaster_login(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 25
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn valid_started_at(value: &str) -> bool {
    !value.is_empty() && value.len() <= 64 && crate::funnel::parse_iso_millis(value).is_some()
}

fn invite_url_valid(url: &str) -> bool {
    let parsed = url::Url::parse(url).ok().filter(|parsed| {
        parsed.scheme() == "https"
            && parsed.host().is_some()
            && parsed.username().is_empty()
            && parsed.password().is_none()
    });
    parsed.is_some_and(|parsed| {
        let text = parsed.as_str();
        !crate::message_safety::contains_mass_mention(text) && !text.contains("](")
    })
}

fn truncate_utf16(value: &str, limit: usize) -> String {
    let mut units = 0usize;
    value
        .chars()
        .take_while(|ch| {
            units += ch.len_utf16();
            units <= limit
        })
        .collect()
}

/// Parse a full EventSub notification (`{ subscription, event }`) into a
/// validated [`StreamOnline`]. Rejects any other subscription type or version
/// so a misrouted `stream.offline` or `channel.update` can never post LIVE.
pub fn parse_stream_online(
    notification: &serde_json::Value,
) -> Result<StreamOnline, StreamFunnelError> {
    let subscription = notification
        .get("subscription")
        .ok_or(StreamFunnelError::MissingField)?;
    let kind = subscription
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or(StreamFunnelError::MissingField)?;
    if kind != STREAM_ONLINE_TYPE {
        return Err(StreamFunnelError::WrongType);
    }
    let version = subscription
        .get("version")
        .and_then(serde_json::Value::as_str)
        .ok_or(StreamFunnelError::MissingField)?;
    if version != STREAM_ONLINE_VERSION {
        return Err(StreamFunnelError::WrongType);
    }
    let event = notification
        .get("event")
        .ok_or(StreamFunnelError::MissingField)?;
    let broadcaster_user_id = event_string(event, "broadcaster_user_id")?;
    if !valid_broadcaster_id(&broadcaster_user_id) {
        return Err(StreamFunnelError::InvalidBroadcasterId);
    }
    let broadcaster_login = event_string(event, "broadcaster_user_login")?;
    if !valid_broadcaster_login(&broadcaster_login) {
        return Err(StreamFunnelError::InvalidBroadcasterLogin);
    }
    let started_at = event_string(event, "started_at")?;
    if !valid_started_at(&started_at) {
        return Err(StreamFunnelError::InvalidStartedAt);
    }
    if let Some(condition_id) = subscription
        .get("condition")
        .and_then(|condition| condition.get("broadcaster_user_id"))
        .and_then(serde_json::Value::as_str)
    {
        if condition_id != broadcaster_user_id {
            return Err(StreamFunnelError::InvalidBroadcasterId);
        }
    }
    Ok(StreamOnline {
        broadcaster_user_id,
        broadcaster_login,
        started_at,
    })
}

/// Stable dedup key for one go-live: `stream.online` + broadcaster + instant.
/// Successive EventSub redeliveries of the same instant collapse to one key;
/// a new `started_at` is a new stream and posts again.
pub fn live_dedup_key(stream: &StreamOnline) -> String {
    hex::encode(Sha256::digest(
        format!(
            "{}\0{}\0{}",
            STREAM_ONLINE_TYPE, stream.broadcaster_user_id, stream.started_at
        )
        .as_bytes(),
    ))
}

/// Render the announcements-channel LIVE post for a validated go-live.
///
/// The Twitch URL is the canonical `https://www.twitch.tv/<login>` page and
/// the Discord link is the caller-supplied invite. Mentions in the optional
/// title are neutralized and the result never exceeds Discord's message cap.
pub fn live_announcement(
    stream: &StreamOnline,
    title: Option<&str>,
    discord_invite_url: &str,
) -> Result<String, StreamFunnelError> {
    if !invite_url_valid(discord_invite_url) {
        return Err(StreamFunnelError::InvalidInviteUrl);
    }
    let login = crate::message_safety::neutralize_mentions(&stream.broadcaster_login);
    let twitch_url = format!("https://www.twitch.tv/{login}");
    let clean_title: Option<String> = title
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(crate::message_safety::neutralize_mentions);
    let headline = match clean_title.as_deref() {
        Some(value) => {
            let fixed = format!("LIVE: **** is live on Twitch!\n{twitch_url}\n").len();
            let budget = MESSAGE_LIMIT_UNITS
                .checked_sub(fixed)
                .and_then(|rest| rest.checked_sub(discord_invite_url.len() + 19))
                .ok_or(StreamFunnelError::TitleTooLong)?;
            let short = truncate_title(value, budget)?;
            format!("LIVE: **{short}** is live on Twitch!")
        }
        None => format!("LIVE: **{login}** is live on Twitch!"),
    };
    let content = format!("{headline}\n{twitch_url}\nWatch together: {discord_invite_url}");
    if content.encode_utf16().count() > MESSAGE_LIMIT_UNITS {
        return Err(StreamFunnelError::TitleTooLong);
    }
    Ok(content)
}

fn truncate_title(value: &str, budget: usize) -> Result<String, StreamFunnelError> {
    let flat: String = value
        .chars()
        .map(|ch| if ch == '\r' || ch == '\n' { ' ' } else { ch })
        .collect();
    if flat.encode_utf16().count() <= budget {
        return Ok(flat);
    }
    if budget == 0 {
        return Err(StreamFunnelError::TitleTooLong);
    }
    let mut kept = truncate_utf16(&flat, budget.saturating_sub(1));
    if kept.encode_utf16().count() >= budget {
        return Err(StreamFunnelError::TitleTooLong);
    }
    kept.push('…');
    Ok(kept)
}

/// Footer appended to game-night VOD descriptions so every upload links back
/// to the Discord. The uploader pastes the result into the description box;
/// no API call here, hence no quota spend.
pub fn vod_description_footer(discord_invite_url: &str) -> Result<String, StreamFunnelError> {
    if !invite_url_valid(discord_invite_url) {
        return Err(StreamFunnelError::InvalidInviteUrl);
    }
    Ok(format!(
        "Come hang out live with us: {discord_invite_url}\nNew here? Say hi after you join."
    ))
}

/// Pinned-comment text for a VOD upload. Same convention as the description
/// footer: Discord link first, optional neutralized title for context.
pub fn vod_pinned_comment(
    video_title: Option<&str>,
    discord_invite_url: &str,
) -> Result<String, StreamFunnelError> {
    if !invite_url_valid(discord_invite_url) {
        return Err(StreamFunnelError::InvalidInviteUrl);
    }
    let clean: Option<String> = video_title
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(crate::message_safety::neutralize_mentions)
        .map(|value| truncate_utf16(&value, 200));
    let content = match clean.as_deref() {
        Some(title) => format!(
            "Thanks for watching {title}! Full VOD list lives here, and we hang out live on Discord: {discord_invite_url}"
        ),
        None => format!(
            "Thanks for watching! Full VOD list lives here, and we hang out live on Discord: {discord_invite_url}"
        ),
    };
    if content.encode_utf16().count() > MESSAGE_LIMIT_UNITS {
        return Err(StreamFunnelError::TitleTooLong);
    }
    Ok(content)
}

/// How many `videos.insert` uploads fit in a quota budget. The default
/// 10,000-unit project quota holds about six uploads at 1,600 units each.
#[must_use]
pub fn max_uploads_for_quota(quota_units: u32) -> u32 {
    quota_units / YOUTUBE_UPLOAD_COST_UNITS
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn notification() -> serde_json::Value {
        json!({
            "subscription": {
                "type": "stream.online",
                "version": "1",
                "condition": {"broadcaster_user_id": "123456"},
            },
            "event": {
                "broadcaster_user_id": "123456",
                "broadcaster_user_login": "ourgamenight",
                "broadcaster_user_name": "OurGameNight",
                "started_at": "2026-10-12T20:00:00Z",
            }
        })
    }

    #[test]
    fn parses_valid_stream_online() {
        let stream = parse_stream_online(&notification()).expect("valid");
        assert_eq!(stream.broadcaster_user_id, "123456");
        assert_eq!(stream.broadcaster_login, "ourgamenight");
        assert_eq!(stream.started_at, "2026-10-12T20:00:00Z");
    }

    #[test]
    fn rejects_wrong_subscription_type_and_version() {
        let mut wrong = notification();
        wrong["subscription"]["type"] = json!("stream.offline");
        assert_eq!(
            parse_stream_online(&wrong),
            Err(StreamFunnelError::WrongType)
        );
        let mut version = notification();
        version["subscription"]["version"] = json!("2");
        assert_eq!(
            parse_stream_online(&version),
            Err(StreamFunnelError::WrongType)
        );
    }

    #[test]
    fn rejects_bad_broadcaster_and_timestamp() {
        let mut bad_id = notification();
        bad_id["event"]["broadcaster_user_id"] = json!("abc");
        assert_eq!(
            parse_stream_online(&bad_id),
            Err(StreamFunnelError::InvalidBroadcasterId)
        );
        let mut mismatch = notification();
        mismatch["subscription"]["condition"]["broadcaster_user_id"] = json!("999");
        assert_eq!(
            parse_stream_online(&mismatch),
            Err(StreamFunnelError::InvalidBroadcasterId)
        );
        let mut bad_login = notification();
        bad_login["event"]["broadcaster_user_login"] = json!("not a login!");
        assert_eq!(
            parse_stream_online(&bad_login),
            Err(StreamFunnelError::InvalidBroadcasterLogin)
        );
        let mut bad_time = notification();
        bad_time["event"]["started_at"] = json!("yesterday-ish");
        assert_eq!(
            parse_stream_online(&bad_time),
            Err(StreamFunnelError::InvalidStartedAt)
        );
        let mut missing = notification();
        missing.as_object_mut().expect("object").remove("event");
        assert_eq!(
            parse_stream_online(&missing),
            Err(StreamFunnelError::MissingField)
        );
    }

    #[test]
    fn dedup_key_stable_per_instant() {
        let stream = parse_stream_online(&notification()).expect("valid");
        let same = live_dedup_key(&stream);
        assert_eq!(same, live_dedup_key(&stream));
        assert_eq!(same.len(), 64);
        let mut later = stream.clone();
        later.started_at = "2026-10-19T20:00:00Z".to_owned();
        assert_ne!(same, live_dedup_key(&later));
    }

    #[test]
    fn live_post_links_twitch_and_discord() {
        let stream = parse_stream_online(&notification()).expect("valid");
        let post = live_announcement(
            &stream,
            Some("Game night: finals"),
            "https://discord.gg/example",
        )
        .expect("post");
        assert!(post.contains("https://www.twitch.tv/ourgamenight"));
        assert!(post.contains("https://discord.gg/example"));
        assert!(post.encode_utf16().count() <= 2000);
        let bare =
            live_announcement(&stream, None, "https://discord.gg/example").expect("bare post");
        assert!(bare.contains("ourgamenight"));
    }

    #[test]
    fn live_post_neutralizes_title_mentions_and_refuses_bad_invite() {
        let stream = parse_stream_online(&notification()).expect("valid");
        let post = live_announcement(
            &stream,
            Some("@everyone finals tonight"),
            "https://discord.gg/example",
        )
        .expect("post");
        assert!(!crate::message_safety::contains_mass_mention(&post));
        assert_eq!(
            live_announcement(&stream, None, "http://discord.gg/example"),
            Err(StreamFunnelError::InvalidInviteUrl)
        );
        assert_eq!(
            live_announcement(&stream, None, "https://discord.gg/@everyone"),
            Err(StreamFunnelError::InvalidInviteUrl)
        );
        assert_eq!(
            live_announcement(&stream, None, "https://discord.gg/a](b"),
            Err(StreamFunnelError::InvalidInviteUrl)
        );
    }

    #[test]
    fn vod_helpers_link_discord_and_neutralize_titles() {
        let footer = vod_description_footer("https://discord.gg/example").expect("footer");
        assert!(footer.contains("https://discord.gg/example"));
        let pinned = vod_pinned_comment(Some("@everyone finals"), "https://discord.gg/example")
            .expect("pinned");
        assert!(pinned.contains("https://discord.gg/example"));
        assert!(!crate::message_safety::contains_mass_mention(&pinned));
        assert_eq!(
            vod_description_footer("not a url"),
            Err(StreamFunnelError::InvalidInviteUrl)
        );
    }

    #[test]
    fn default_quota_holds_about_six_uploads() {
        assert_eq!(max_uploads_for_quota(YOUTUBE_DEFAULT_DAILY_QUOTA_UNITS), 6);
        assert_eq!(YOUTUBE_UPLOAD_COST_UNITS, 1_600);
    }
}
