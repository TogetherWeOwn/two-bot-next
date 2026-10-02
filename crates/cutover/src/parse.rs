//! Parsers for the log channels the TWO server already keeps.
//!
//! Ports `src/backfill/parse.ts`. Discord exposes no voice history and no
//! per-member invite record, but years of logging bots wrote joins, leaves
//! and voice sessions into ordinary channels. These are pure string
//! functions — log formats are decided by third-party bots and change
//! without warning, so they are unit-tested against captured samples and
//! the backfill reports zero rather than failing when they move.
//!
//! No `regex` crate: the few patterns needed are hand-rolled so the cutover
//! crate adds no dependency weight to the workspace lockfile.

/// Discord epoch millis (2015-01-01T00:00:00Z) for snowflake conversion.
const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

/// Minimal embed view: what the parsers actually read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmbedView {
    pub title: Option<String>,
    pub description: Option<String>,
    pub footer_text: Option<String>,
}

/// Minimal message view: what the parsers actually read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageView {
    pub id: String,
    pub timestamp: String,
    pub content: Option<String>,
    pub author_id: Option<String>,
    pub author_is_bot: bool,
    pub embeds: Vec<EmbedView>,
}

fn is_snowflake_token(tok: &str) -> bool {
    let len = tok.len();
    (15..=25).contains(&len) && tok.bytes().all(|b| b.is_ascii_digit())
}

/// First snowflake run in `s` (15–25 digits), or `None` (legacy `SNOWFLAKE`).
#[must_use]
pub fn first_snowflake(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let mut j = i;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j - i >= 15 && j - i <= 25 {
                return Some(s[i..j].to_owned());
            }
            i = j;
        } else {
            i += 1;
        }
    }
    None
}

/// Footer `ID: <snowflake>` wins; fall back to the first `<@id>` mention in
/// the description (legacy `memberIdFromEmbed`).
pub fn member_id_from_embed(e: &EmbedView) -> Option<String> {
    if let Some(footer) = e.footer_text.as_deref() {
        if let Some(pos) = footer.find("ID:") {
            let rest = footer[pos + 3..].trim_start();
            let digits: String = rest
                .bytes()
                .take_while(|b| b.is_ascii_digit())
                .map(char::from)
                .collect();
            if is_snowflake_token(&digits) {
                return Some(digits);
            }
        }
        // "ID:" present but followed by no snowflake: fall through to the
        // mention fallback, mirroring the legacy regex (footer match fails →
        // description match attempted).
        if footer.contains("ID:") {
            // still try the mention below
        }
    }
    let desc = e.description.as_deref().unwrap_or("");
    // `<@id>` / `<@!id>` nickname form.
    let mut search = desc;
    while let Some(pos) = search.find("<@") {
        let rest = &search[pos + 2..];
        let rest = rest.strip_prefix('!').unwrap_or(rest);
        let digits: String = rest
            .bytes()
            .take_while(|b| b.is_ascii_digit())
            .map(char::from)
            .collect();
        if is_snowflake_token(&digits) {
            return Some(digits);
        }
        search = &search[pos + 2..];
        if search.is_empty() {
            break;
        }
    }
    None
}

/// First `<#id>` channel mention in the description (legacy
/// `channelIdFromEmbed`).
pub fn channel_id_from_embed(e: &EmbedView) -> Option<String> {
    let desc = e.description.as_deref().unwrap_or("");
    let mut search = desc;
    while let Some(pos) = search.find("<#") {
        let rest = &search[pos + 2..];
        let digits: String = rest
            .bytes()
            .take_while(|b| b.is_ascii_digit())
            .map(char::from)
            .collect();
        if is_snowflake_token(&digits) {
            return Some(digits);
        }
        search = &search[pos + 2..];
        if search.is_empty() {
            break;
        }
    }
    None
}

/// Voice session kind (legacy `VoiceKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceKind {
    Join,
    Change,
    Leave,
}

/// One parsed voice log line (legacy `VoiceRecord`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceRecord {
    pub member_id: String,
    pub channel_id: Option<String>,
    pub kind: VoiceKind,
    pub occurred_at: String,
}

/// Parse a voice log embed. Two logger formats: the current Logger (titled
/// "Member joined voice channel" + "**name** joined #channel") and historic
/// Wick (titleless "**<@id> joined voice channel <#id>**"). Both key the
/// member off the footer snowflake.
pub fn parse_voice_message(msg: &MessageView) -> Option<VoiceRecord> {
    let e = msg.embeds.first()?;
    let title = e.title.as_deref().unwrap_or("").to_lowercase();
    let desc = e.description.as_deref().unwrap_or("").to_lowercase();

    let kind = if title.contains("joined voice")
        || (title.is_empty() && desc.contains("joined voice channel"))
    {
        VoiceKind::Join
    } else if title.contains("changed voice")
        || (title.is_empty() && desc.contains("moved voice channel"))
    {
        VoiceKind::Change
    } else if title.contains("left voice")
        || (title.is_empty() && desc.contains("left voice channel"))
    {
        VoiceKind::Leave
    } else {
        return None;
    };
    let member_id = member_id_from_embed(e)?;
    Some(VoiceRecord {
        member_id,
        channel_id: channel_id_from_embed(e),
        kind,
        occurred_at: msg.timestamp.clone(),
    })
}

/// Member-log kind (legacy `MemberLogKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberLogKind {
    Join,
    Leave,
}

/// One parsed join/leave log line (legacy `MemberLogRecord`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberLogRecord {
    pub member_id: String,
    pub kind: MemberLogKind,
    pub occurred_at: String,
}

/// Parse a join/leave log embed. `channel_kind` carries what a dedicated
/// channel means for titleless embeds; a titled embed we do not recognise is
/// a different event type (role changes, nickname edits) and must NOT count.
pub fn parse_member_log_message(
    msg: &MessageView,
    channel_kind: Option<MemberLogKind>,
) -> Option<MemberLogRecord> {
    let e = msg.embeds.first()?;
    let title = e.title.as_deref().unwrap_or("").to_lowercase();
    let kind = if title == "member joined" {
        MemberLogKind::Join
    } else if title == "member left" || title == "member banned" {
        MemberLogKind::Leave
    } else if title.is_empty() {
        channel_kind?
    } else {
        return None;
    };
    let member_id = member_id_from_embed(e)?;
    Some(MemberLogRecord {
        member_id,
        kind,
        occurred_at: msg.timestamp.clone(),
    })
}

/// Map a channel name to the event its titleless logs carry (legacy
/// `memberLogKindForChannel`).
pub fn member_log_kind_for_channel(name: &str) -> Option<MemberLogKind> {
    let lower = name.to_lowercase();
    let is_boundary = |i: usize, len: usize| {
        let before = if i == 0 {
            None
        } else {
            lower.as_bytes().get(i - 1).copied()
        };
        let after = lower.as_bytes().get(i + len).copied();
        let is_letter = |b: Option<u8>| b.is_some_and(|c| c.is_ascii_alphabetic());
        !is_letter(before) && !is_letter(after)
    };
    for needle in ["member-join"] {
        if let Some(i) = lower.find(needle) {
            if is_boundary(i, needle.len()) {
                return Some(MemberLogKind::Join);
            }
        }
    }
    for needle in ["member-leave", "member-ban"] {
        if let Some(i) = lower.find(needle) {
            if is_boundary(i, needle.len()) {
                return Some(MemberLogKind::Leave);
            }
        }
    }
    None
}

/// One parsed #invites leave line: username only, never a snowflake
/// (legacy `LeaveAttributionRecord`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaveAttributionRecord {
    pub username: String,
    pub joined_via: String,
    pub occurred_at: String,
}

/// Byte offset of the first occurrence of `needle` in `haystack`, comparing
/// bytes case-insensitively over ASCII only. Unicode case folding changes
/// byte lengths (the Kelvin sign lowercases to `k`; `İ` grows to `i` plus a
/// combining dot), so an offset found in a lowercased copy can point inside
/// another character of the original. Matching raw bytes keeps every match
/// boundary a real character boundary in `haystack`.
fn find_ignore_ascii_case(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    let needle = needle.as_bytes();
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
}

/// Parse the invite-tracker bot's leave line. Good for counting churn only —
/// it never logs joins and never records a snowflake.
pub fn parse_leave_attribution(msg: &MessageView) -> Option<LeaveAttributionRecord> {
    let content = msg.content.as_deref()?;
    let marker = "left the server.";
    let pos = find_ignore_ascii_case(content, marker)?;
    let username = content[..pos].trim().to_owned();
    if username.is_empty() {
        return None;
    }
    let tail = content[pos + marker.len()..].to_lowercase();
    let joined_via = if tail.contains("vanity") {
        "vanity"
    } else if tail.contains("oauth") {
        "oauth"
    } else {
        "unknown"
    };
    Some(LeaveAttributionRecord {
        username,
        joined_via: joined_via.to_owned(),
        occurred_at: msg.timestamp.clone(),
    })
}

/// Discord snowflake → creation time (legacy `snowflakeToDate`).
#[must_use]
pub fn snowflake_to_date_ms(id: &str) -> Option<u64> {
    let n: u64 = id.parse().ok()?;
    Some((n >> 22) + DISCORD_EPOCH_MS)
}

/// Inverse: smallest snowflake at or after `millis` (legacy
/// `dateToSnowflake`).
#[must_use]
pub fn date_to_snowflake(millis: u64) -> String {
    ((millis.saturating_sub(DISCORD_EPOCH_MS)) << 22).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const AT: &str = "2026-08-19T19:32:15.762000+00:00";

    fn msg(embed: EmbedView) -> MessageView {
        MessageView {
            id: "1".to_owned(),
            timestamp: AT.to_owned(),
            embeds: vec![embed],
            ..Default::default()
        }
    }

    fn embed(title: &str, description: &str, footer: &str) -> EmbedView {
        EmbedView {
            title: if title.is_empty() {
                None
            } else {
                Some(title.to_owned())
            },
            description: if description.is_empty() {
                None
            } else {
                Some(description.to_owned())
            },
            footer_text: if footer.is_empty() {
                None
            } else {
                Some(footer.to_owned())
            },
        }
    }

    #[test]
    fn titled_member_joined_is_join() {
        let r = parse_member_log_message(
            &msg(embed(
                "Member joined",
                "<@1539711683898118154> 107th to join\nNEW ACCOUNT",
                "ID: 1539711683898118154",
            )),
            None,
        )
        .unwrap();
        assert_eq!(r.member_id, "1539711683898118154");
        assert_eq!(r.kind, MemberLogKind::Join);
    }

    #[test]
    fn titled_member_left_keys_on_footer_not_roles() {
        let r = parse_member_log_message(
            &msg(embed(
                "Member left",
                "<@720351927581278219> joined 3 years ago\n**Roles:** <@&1060912046012633148> <@&1101170473108242452>",
                "ID: 720351927581278219",
            )),
            None,
        )
        .unwrap();
        assert_eq!(r.kind, MemberLogKind::Leave);
        assert_eq!(r.member_id, "720351927581278219");
    }

    #[test]
    fn unrecognised_titled_embed_is_not_join() {
        assert_eq!(
            parse_member_log_message(
                &msg(embed(
                    "Role added",
                    "<@&1144789677057003636>",
                    "ID: 1539711683898118154"
                )),
                Some(MemberLogKind::Join),
            ),
            None
        );
    }

    #[test]
    fn titleless_needs_channel_context() {
        let m = msg(embed(
            "",
            "<@1329713790065049701> danielarellano0911",
            "ID: 1329713790065049701",
        ));
        assert_eq!(parse_member_log_message(&m, None), None);
        let r = parse_member_log_message(&m, Some(MemberLogKind::Join)).unwrap();
        assert_eq!(r.member_id, "1329713790065049701");
    }

    #[test]
    fn nickname_mention_form_resolves() {
        let r = parse_member_log_message(
            &msg(embed(
                "",
                "<@!1298143954834817030> jefferyrichard9703",
                "ID: 1298143954834817030",
            )),
            Some(MemberLogKind::Leave),
        )
        .unwrap();
        assert_eq!(r.member_id, "1298143954834817030");
        assert_eq!(r.kind, MemberLogKind::Leave);
    }

    #[test]
    fn channel_names_map_to_dedicated_events() {
        assert_eq!(
            member_log_kind_for_channel("member-join"),
            Some(MemberLogKind::Join)
        );
        assert_eq!(
            member_log_kind_for_channel("member-leave"),
            Some(MemberLogKind::Leave)
        );
        assert_eq!(member_log_kind_for_channel("general"), None);
    }

    #[test]
    fn logger_voice_titled_format() {
        let r = parse_voice_message(&msg(embed(
            "Member joined voice channel",
            "**name** joined #general",
            "ID: 1539711683898118154",
        )))
        .unwrap();
        assert_eq!(r.kind, VoiceKind::Join);
        assert_eq!(r.member_id, "1539711683898118154");
    }

    #[test]
    fn wick_voice_titleless_format_keeps_channel() {
        let r = parse_voice_message(&msg(embed(
            "",
            "**<@1539711683898118154> joined voice channel <#1060912046012633148>**",
            "ID: 1539711683898118154",
        )))
        .unwrap();
        assert_eq!(r.kind, VoiceKind::Join);
        assert_eq!(r.channel_id.as_deref(), Some("1060912046012633148"));
    }

    #[test]
    fn non_voice_embed_is_not_voice() {
        assert_eq!(
            parse_voice_message(&msg(embed(
                "Member joined",
                "<@1>",
                "ID: 1539711683898118154"
            ))),
            None
        );
    }

    #[test]
    fn leave_attribution_parses_and_limits() {
        let r = parse_leave_attribution(&leave_msg("someuser left the server. joined via vanity"))
            .unwrap();
        assert_eq!(
            (r.username.as_str(), r.joined_via.as_str()),
            ("someuser", "vanity")
        );
    }

    fn leave_msg(content: &str) -> MessageView {
        MessageView {
            content: Some(content.to_owned()),
            timestamp: AT.to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn leave_attribution_returns_multibyte_usernames_byte_exact() {
        // U+212A KELVIN SIGN lowercases to `k` (3 bytes → 1) and U+0130 grows
        // to `i` plus a combining dot (2 → 3): offsets found in a lowercased
        // copy of these lines land inside the username. Each accepted case
        // must return the original username exactly, never a mid-character
        // slice.
        let cases = [
            (
                "\u{212A}é left the server. joined via vanity",
                "\u{212A}é",
                "vanity",
            ),
            (
                "\u{0130}stanbul left the server. joined via oauth",
                "\u{0130}stanbul",
                "oauth",
            ),
            (
                "Ünicode left the server. joined via vanity",
                "Ünicode",
                "vanity",
            ),
            (
                "日本語ユーザー left the server. joined via oauth",
                "日本語ユーザー",
                "oauth",
            ),
            (
                "🦀かに left the server. joined via unknown",
                "🦀かに",
                "unknown",
            ),
        ];
        for (content, username, joined_via) in cases {
            let r = parse_leave_attribution(&leave_msg(content))
                .unwrap_or_else(|| panic!("no record for {content:?}"));
            assert_eq!(r.username, username, "username for {content:?}");
            assert_eq!(r.joined_via, joined_via, "attribution for {content:?}");
        }
    }

    #[test]
    fn leave_attribution_matches_marker_regardless_of_case() {
        let cases = [
            (
                "Alice LEFT THE SERVER. joined via vanity",
                "Alice",
                "vanity",
            ),
            ("Bob LeFt ThE SeRvEr. Joined via OAUTH", "Bob", "oauth"),
            (
                "\u{0130}van LEFT THE SERVER. joined via vanity",
                "\u{0130}van",
                "vanity",
            ),
        ];
        for (content, username, joined_via) in cases {
            let r = parse_leave_attribution(&leave_msg(content))
                .unwrap_or_else(|| panic!("no record for {content:?}"));
            assert_eq!(r.username, username, "username for {content:?}");
            assert_eq!(r.joined_via, joined_via, "attribution for {content:?}");
        }
    }

    #[test]
    fn leave_attribution_needs_content_marker_and_username() {
        let no_content = MessageView {
            timestamp: AT.to_owned(),
            ..Default::default()
        };
        assert_eq!(parse_leave_attribution(&no_content), None);
        for content in [
            "someuser left the guild. joined via vanity",
            "  left the server. joined via vanity",
            "\n\tleft the server. joined via oauth",
        ] {
            assert_eq!(
                parse_leave_attribution(&leave_msg(content)),
                None,
                "for {content:?}"
            );
        }
    }

    #[test]
    fn snowflake_round_trips_to_the_second() {
        // The bot's own account, created 2026-08-19.
        let ms = snowflake_to_date_ms("1539711683898118154").unwrap();
        // Back to a snowflake at the same second-or-later position.
        let back = date_to_snowflake(ms);
        let ms2 = snowflake_to_date_ms(&back).unwrap();
        assert!(ms2 <= ms && ms - ms2 < 1000);
    }

    #[test]
    fn first_snowflake_finds_runs() {
        assert_eq!(
            first_snowflake("ID: 1539711683898118154"),
            Some("1539711683898118154".to_owned())
        );
        assert_eq!(first_snowflake("no digits here"), None);
    }
}
