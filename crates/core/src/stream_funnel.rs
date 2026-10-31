//! Stream funnel: Twitch live alerts + YouTube VOD links (pure helpers, no I/O).
//!
//! Scope and quota confirmed 2026-10-12 against the official references:
//!
//! - Twitch EventSub `stream.online` version 1: no OAuth scope is required.
//!   Webhook and Conduit subscriptions use an app access token from the
//!   client-credentials flow; WebSocket subscriptions require a user access
//!   token instead (no scope for `stream.online`) and fail with an app token.
//!   The condition carries `broadcaster_user_id`, and the transports are
//!   webhook, WebSocket and Conduit. EventSub delivery itself carries no
//!   per-event quota units.
//! - YouTube Data API v3 `videos.insert`: the method has its own quota bucket
//!   (default 100 calls/day, cost 1 per call), separate from the default
//!   10,000 units/day general project quota. There is no Community-post
//!   creation endpoint, so this module never plans one.
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
/// YouTube `videos.insert` default daily call limit. The method has its own
/// quota bucket (default 100 calls/day, cost 1 per call), separate from the
/// general 10,000 units/day project quota.
pub const YOUTUBE_INSERT_DAILY_CALL_LIMIT: u32 = 100;
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
pub(crate) fn valid_broadcaster_id(value: &str) -> bool {
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

/// Validate a configured Discord invite URL with the exact rule the
/// post renderer applies, so delivery configuration can refuse an
/// unpostable invite at startup instead of failing every go-live.
pub fn validated_invite_url(url: &str) -> Result<String, StreamFunnelError> {
    message_invite_url(url)
}

/// Validate a caller-supplied Discord invite URL and return the exact string
/// that is safe to post. The mass-mention and masked-link checks run on the
/// parsed (`Url::as_str`) form, and that same form is what callers output, so
/// the check can never be bypassed by characters the URL parser strips or
/// percent-encodes (tabs, newlines, spaces, `<`/`>`). Input whose parsed form
/// differs from the trimmed input is rejected outright.
fn message_invite_url(url: &str) -> Result<String, StreamFunnelError> {
    let trimmed = url.trim();
    let parsed = url::Url::parse(trimmed)
        .ok()
        .filter(|parsed| {
            parsed.scheme() == "https"
                && parsed.host().is_some()
                && parsed.username().is_empty()
                && parsed.password().is_none()
        })
        .ok_or(StreamFunnelError::InvalidInviteUrl)?;
    let normalized = parsed.as_str().to_owned();
    if normalized != trimmed {
        return Err(StreamFunnelError::InvalidInviteUrl);
    }
    if crate::message_safety::contains_mass_mention(&normalized) || normalized.contains("](") {
        return Err(StreamFunnelError::InvalidInviteUrl);
    }
    Ok(normalized)
}

/// Escape ASCII punctuation and flatten line breaks before inserting
/// broadcaster-controlled text into Discord Markdown message content.
/// Mirrors `feeds::escape_markdown`: a Twitch title such as
/// `[Free Nitro](https://evil.example/)` must not reach the wire as a masked
/// link posted under the bot's name.
fn escape_markdown(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '\r' | '\n') {
            escaped.push(' ');
        } else {
            if ch.is_ascii_punctuation() {
                escaped.push('\\');
            }
            escaped.push(ch);
        }
    }
    escaped
}

fn truncate_escaped_markdown(value: &str, limit: usize) -> String {
    let escaped = escape_markdown(value);
    if escaped.encode_utf16().count() <= limit {
        return escaped;
    }
    if limit == 0 {
        return String::new();
    }

    let mut truncated = truncate_utf16(&escaped, limit - 1);
    let trailing_backslashes = truncated.chars().rev().take_while(|ch| *ch == '\\').count();
    if trailing_backslashes % 2 == 1 {
        truncated.pop();
    }
    truncated.push('…');
    truncated
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

/// Collapse line breaks and cut plain (non-Discord) text to `limit` UTF-16
/// units with an ellipsis. Unlike [`truncate_escaped_markdown`], this adds no
/// backslash escapes: YouTube comments render backslashes literally, so
/// Discord escaping garbles titles there while protecting nothing (YouTube
/// has no masked links).
fn truncate_plain_text(value: &str, limit: usize) -> String {
    let flat: String = value
        .chars()
        .map(|ch| if matches!(ch, '\r' | '\n') { ' ' } else { ch })
        .collect();
    if flat.encode_utf16().count() <= limit {
        return flat;
    }
    if limit == 0 {
        return String::new();
    }
    let mut truncated = truncate_utf16(&flat, limit - 1);
    truncated.push('…');
    truncated
}

/// Parse a full EventSub notification (`{ subscription, event }`) into a
/// validated [`StreamOnline`]. Rejects any other subscription type or version
/// so a misrouted `stream.offline` or `channel.update` can never post LIVE.
/// A present `event.type` must be `live`: Twitch documents `playlist`,
/// `watch_party`, `premiere` and `rerun` as further values, and none of them
/// is a genuine go-live. An absent `type` is accepted for older payloads.
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
    if let Some(event_type) = event.get("type").and_then(serde_json::Value::as_str) {
        if event_type != "live" {
            return Err(StreamFunnelError::WrongType);
        }
    }
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
/// the Discord link is the caller-supplied invite in its validated normalized
/// form. Broadcaster-controlled text is mass-mention neutralized and
/// Markdown-escaped (mirroring `feeds::plan_post`) so a Twitch title cannot
/// smuggle a masked link or formatting into the post. Role and user mentions
/// stay readable here, so the caller must post the result with mentions
/// suppressed (no `allowed_mentions` parse), exactly like
/// `FeedPost.suppress_mentions`. The result never exceeds Discord's message cap.
pub fn live_announcement(
    stream: &StreamOnline,
    title: Option<&str>,
    discord_invite_url: &str,
) -> Result<String, StreamFunnelError> {
    let invite = message_invite_url(discord_invite_url)?;
    let login = crate::message_safety::neutralize_mentions(&stream.broadcaster_login);
    let safe_login = escape_markdown(&login);
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
                .and_then(|rest| rest.checked_sub(invite.len() + 19))
                .ok_or(StreamFunnelError::TitleTooLong)?;
            let short = truncate_escaped_markdown(value, budget);
            format!("LIVE: **{short}** is live on Twitch!")
        }
        None => format!("LIVE: **{safe_login}** is live on Twitch!"),
    };
    let content = format!("{headline}\n{twitch_url}\nWatch together: {invite}");
    if content.encode_utf16().count() > MESSAGE_LIMIT_UNITS {
        return Err(StreamFunnelError::TitleTooLong);
    }
    Ok(content)
}

/// Footer appended to game-night VOD descriptions so every upload links back
/// to the Discord. The uploader pastes the result into the description box;
/// no API call here, hence no quota spend.
pub fn vod_description_footer(discord_invite_url: &str) -> Result<String, StreamFunnelError> {
    let invite = message_invite_url(discord_invite_url)?;
    Ok(format!(
        "Come hang out live with us: {invite}\nNew here? Say hi after you join."
    ))
}

/// Pinned-comment text for a VOD upload. Same convention as the description
/// footer: Discord link first, optional neutralized title for context. The
/// title is plain text with NO Markdown escaping: this posts to YouTube,
/// which renders backslashes literally, so Discord escaping would garble it
/// (YouTube has no masked links to defend against). Mass mentions stay
/// neutralized — the zero-width break is invisible on YouTube and keeps the
/// text safe if quoted back into Discord.
pub fn vod_pinned_comment(
    video_title: Option<&str>,
    discord_invite_url: &str,
) -> Result<String, StreamFunnelError> {
    let invite = message_invite_url(discord_invite_url)?;
    let clean: Option<String> = video_title
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(crate::message_safety::neutralize_mentions)
        .map(|value| truncate_plain_text(&value, 200));
    let content = match clean.as_deref() {
        Some(title) => format!(
            "Thanks for watching {title}! Full VOD list lives here, and we hang out live on Discord: {invite}"
        ),
        None => format!(
            "Thanks for watching! Full VOD list lives here, and we hang out live on Discord: {invite}"
        ),
    };
    if content.encode_utf16().count() > MESSAGE_LIMIT_UNITS {
        return Err(StreamFunnelError::TitleTooLong);
    }
    Ok(content)
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
    fn live_post_escapes_title_markdown() {
        let stream = parse_stream_online(&notification()).expect("valid");
        let post = live_announcement(
            &stream,
            Some("[Free Nitro](https://evil.example/) **# x"),
            "https://discord.gg/example",
        )
        .expect("post");
        assert!(
            !post.contains("](https://evil.example/)"),
            "masked link must not reach the wire: {post}"
        );
        assert!(
            !post.contains("**# x"),
            "raw bold markers must not reach the wire: {post}"
        );
    }

    #[test]
    fn invite_url_rejects_parser_stripped_or_encoded_bypass() {
        let stream = parse_stream_online(&notification()).expect("valid");
        for probe in [
            "https://discord.gg/x/\nFree Nitro <@&123>",
            "https://discord.gg/x/\t<@123>",
            "https://discord.gg/x <@&123>",
        ] {
            assert_eq!(
                live_announcement(&stream, None, probe),
                Err(StreamFunnelError::InvalidInviteUrl),
                "probe must be rejected: {probe:?}"
            );
            assert_eq!(
                vod_description_footer(probe),
                Err(StreamFunnelError::InvalidInviteUrl),
                "probe must be rejected: {probe:?}"
            );
            assert_eq!(
                vod_pinned_comment(None, probe),
                Err(StreamFunnelError::InvalidInviteUrl),
                "probe must be rejected: {probe:?}"
            );
        }
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
    fn insert_bucket_defaults_to_one_hundred_daily_calls() {
        assert_eq!(YOUTUBE_INSERT_DAILY_CALL_LIMIT, 100);
    }

    #[test]
    fn pinned_comment_keeps_youtube_title_unescaped() {
        let pinned = vod_pinned_comment(
            Some("Game Night #3: Finals (part 1)!"),
            "https://discord.gg/example",
        )
        .expect("pinned");
        assert!(
            pinned.contains("Game Night #3: Finals (part 1)!"),
            "YouTube title must appear verbatim, no backslash escapes: {pinned}"
        );
        assert!(
            !pinned.contains('\\'),
            "no Discord backslash escapes in YouTube text: {pinned}"
        );
    }

    #[test]
    fn rejects_non_live_event_type() {
        for event_type in ["rerun", "playlist", "watch_party", "premiere"] {
            let mut probe = notification();
            probe["event"]["type"] = json!(event_type);
            assert_eq!(
                parse_stream_online(&probe),
                Err(StreamFunnelError::WrongType),
                "event type {event_type:?} is not a go-live"
            );
        }
        let mut live = notification();
        live["event"]["type"] = json!("live");
        parse_stream_online(&live).expect("live event type parses");
    }
}
