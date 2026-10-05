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

/// Footer-only variant of [`member_id_from_embed`]: no mention fallback.
fn member_id_from_footer_only(e: &EmbedView) -> Option<String> {
    member_id_from_embed(&EmbedView {
        title: None,
        description: None,
        footer_text: e.footer_text.clone(),
    })
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
    // Titled Logger descriptions carry a display name, never a member mention:
    // with no footer id, a mention in the text is incidental (a thank-you, a
    // quoted reply) and must not mint attribution. Only the titleless Wick
    // format keeps the footer-then-mention fallback (legacy ce3c0e2).
    let member_id = if title.is_empty() {
        member_id_from_embed(e)?
    } else {
        member_id_from_footer_only(e)?
    };
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
    // Padded titles still match (stray logger padding must not drop real
    // joins), but the channel fallback keys on the RAW title: a
    // whitespace-only title is a titled embed we do not recognise, so the
    // channel hint mints nothing from it (legacy ce3c0e2).
    let raw_title = e.title.as_deref().unwrap_or("").to_lowercase();
    let title = raw_title.trim();
    let kind = if title == "member joined" {
        MemberLogKind::Join
    } else if title == "member left" || title == "member banned" {
        MemberLogKind::Leave
    } else if raw_title.is_empty() {
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
    // Any occurrence at a letter boundary counts, as the legacy regex does:
    // a near-miss earlier in the name (`premember-join`) must not hide a real
    // one later (`premember-join member-join`).
    let has_bounded = |needle: &str| {
        lower
            .match_indices(needle)
            .any(|(i, _)| is_boundary(i, needle.len()))
    };
    if has_bounded("member-join") {
        return Some(MemberLogKind::Join);
    }
    if has_bounded("member-leave") || has_bounded("member-ban") {
        return Some(MemberLogKind::Leave);
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

/// Whole-name placeholders hand-edited fixtures leave in the username slot
/// (legacy `/^(todo|tbd|fixme|xxx|\?+|placeholder|unknown(\s+user)?)$/i`).
/// Only the whole name matches: `TodoFan99` is a real username.
fn is_placeholder_username(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "todo" | "tbd" | "fixme" | "xxx" | "placeholder" | "unknown"
    ) {
        return true;
    }
    if !lower.is_empty() && lower.bytes().all(|b| b == b'?') {
        return true;
    }
    lower
        .strip_prefix("unknown")
        .and_then(|rest| rest.strip_prefix(char::is_whitespace))
        .is_some_and(|rest| rest.trim_start() == "user")
}

/// Parse the invite-tracker bot's leave line. Good for counting churn only —
/// it never logs joins and never records a snowflake.
pub fn parse_leave_attribution(msg: &MessageView) -> Option<LeaveAttributionRecord> {
    let content = msg.content.as_deref()?;
    let marker = "left the server.";
    let pos = find_ignore_ascii_case(content, marker)?;
    let username = content[..pos].trim().to_owned();
    // An empty or placeholder name is a truncated or hand-edited row, not a
    // departure: it would write churn the funnel cannot join to anyone.
    if username.is_empty() || is_placeholder_username(&username) {
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

    const MID: &str = "1539711683898118154";

    fn msg_with(title: &str, description: &str, footer: &str) -> MessageView {
        msg(embed(title, description, footer))
    }

    // --- legacy 860557f / ce3c0e2 malformed-row contract -------------------
    //
    // The legacy suite fed `unknown` JSON into the parsers. Here the inputs
    // are typed twilight messages decoded before they reach `MessageView`, so
    // non-string titles, non-date timestamps and garbage message shapes cannot
    // arrive; the remaining contract is "a bad row is `None`, never a half
    // record", pinned below.

    #[test]
    fn titled_voice_with_no_footer_id_ignores_a_stray_description_mention() {
        // Titled Logger descriptions carry a display name, never a mention; a
        // mention beside an id-less footer is incidental and must not mint a
        // voice session.
        assert_eq!(
            parse_voice_message(&msg_with(
                "Member joined voice channel",
                "**ghostly.og** joined #general, thanks <@1298143954834817030>!",
                "ID: n/a",
            )),
            None
        );
        // The footer path is untouched.
        assert_eq!(
            parse_voice_message(&msg_with(
                "Member joined voice channel",
                "**ghostly.og** joined #general",
                &format!("ID: {MID}"),
            ))
            .map(|r| r.member_id),
            Some(MID.to_owned())
        );
        // Titleless Wick keeps the footer-then-mention fallback, also with an
        // explicitly empty (not absent) title.
        let wick = format!("**<@{MID}> joined voice channel <#1175127344072118405>**");
        assert_eq!(
            parse_voice_message(&msg_with("", &wick, "n/a")).map(|r| r.member_id),
            Some(MID.to_owned())
        );
    }

    #[test]
    fn placeholder_usernames_are_truncated_rows_not_churn() {
        for name in [
            "TODO",
            "todo",
            "TBD",
            "FIXME",
            "xxx",
            "???",
            "?",
            "Placeholder",
            "UNKNOWN",
            "Unknown User",
            "unknown   user",
        ] {
            assert_eq!(
                parse_leave_attribution(&leave_msg(&format!("{name} left the server. vanity"))),
                None,
                "placeholder refused: {name}"
            );
        }
        // Whole-name match only: real usernames containing a keyword parse.
        for name in ["TodoFan99", "X#1", "unknownuser", "unknown users", "??a"] {
            assert_eq!(
                parse_leave_attribution(&leave_msg(&format!("{name} left the server. vanity")))
                    .map(|r| r.username),
                Some(name.to_owned()),
                "real username kept: {name}"
            );
        }
    }

    #[test]
    fn padded_member_titles_parse_and_whitespace_only_titles_guess_nothing() {
        let padded = |title: &str| msg_with(title, &format!("<@{MID}> hi"), &format!("ID: {MID}"));
        assert_eq!(
            parse_member_log_message(&padded("Member joined "), None).map(|r| r.kind),
            Some(MemberLogKind::Join)
        );
        assert_eq!(
            parse_member_log_message(&padded("  Member left  "), None).map(|r| r.kind),
            Some(MemberLogKind::Leave)
        );
        assert_eq!(
            parse_member_log_message(&padded("\tMEMBER BANNED\n"), None).map(|r| r.kind),
            Some(MemberLogKind::Leave)
        );
        // A whitespace-only title is a titled embed we do not recognise: the
        // channel hint must not mint a join from it.
        assert_eq!(
            parse_member_log_message(&padded("   "), Some(MemberLogKind::Join)),
            None
        );
    }

    #[test]
    fn truncated_footers_and_adversarial_ids_refuse_cleanly() {
        let id_of =
            |footer: &str, description: &str| member_id_from_embed(&embed("", description, footer));
        // Footer cut mid-id: too short to be a snowflake, and no mention.
        assert_eq!(id_of("ID: 15397", ""), None);
        // A 26-digit run is not a snowflake: refuse, never key on a prefix.
        assert_eq!(id_of(&format!("ID: {MID}1234567"), ""), None);
        // Boundary: exactly 15 and 25 digits count, 14 does not.
        assert_eq!(
            id_of("ID: 123456789012345", ""),
            Some("123456789012345".to_owned())
        );
        assert_eq!(
            id_of("ID: 1234567890123456789012345", ""),
            Some("1234567890123456789012345".to_owned())
        );
        assert_eq!(id_of("ID: 12345678901234", ""), None);
        // The mention fallback still resolves when the footer carries no id.
        assert_eq!(
            id_of("n/a", "<@!1298143954834817030> name"),
            Some("1298143954834817030".to_owned())
        );
        // Adversarial footers cannot inject an id.
        assert_eq!(id_of("ID: ", ""), None);
        assert_eq!(id_of(&format!("ID: abc{MID}"), ""), None);
        assert_eq!(id_of(&format!("ID:\n{MID}"), ""), Some(MID.to_owned()));
        // TODO-marker rows resolve to nothing.
        assert_eq!(id_of("ID: TODO", "TODO"), None);
        assert_eq!(channel_id_from_embed(&embed("", "TODO", "TODO")), None);
    }

    #[test]
    fn truncated_logger_rows_refuse_instead_of_half_parsing() {
        // TODO-marker titled embed.
        assert_eq!(parse_voice_message(&msg_with("TODO", "TODO", "TODO")), None);
        // Mid-phrase truncation of a Wick description: no complete kind phrase.
        assert_eq!(
            parse_voice_message(&msg_with(
                "",
                &format!("**<@{MID}> joined voice cha"),
                &format!("ID: {MID}"),
            )),
            None
        );
        // Footer truncated below snowflake width, no mention: no record.
        assert_eq!(
            parse_voice_message(&msg_with("Member joined voice channel", "x", "ID: 15397")),
            None
        );
        // A drifted channel hint on a titleless embed mints nothing when the
        // row itself carries no usable id.
        assert_eq!(
            parse_member_log_message(&msg_with("", "TODO", "ID: TODO"), Some(MemberLogKind::Join)),
            None
        );
    }

    #[test]
    fn channel_names_refuse_near_miss_suffixes() {
        let kind = member_log_kind_for_channel;
        assert_eq!(kind("member-join-log"), Some(MemberLogKind::Join));
        assert_eq!(kind("member-join2"), Some(MemberLogKind::Join));
        assert_eq!(kind("MEMBER-JOIN"), Some(MemberLogKind::Join));
        assert_eq!(kind("member-ban"), Some(MemberLogKind::Leave));
        assert_eq!(kind("member-joinx"), None);
        assert_eq!(kind("premember-join"), None);
        assert_eq!(kind("member-leavex"), None);
        // A near miss earlier in the name must not hide a bounded match later.
        assert_eq!(
            kind("premember-join member-join"),
            Some(MemberLogKind::Join)
        );
        assert_eq!(
            kind("xmember-leave logs-member-leave"),
            Some(MemberLogKind::Leave)
        );
    }

    #[test]
    fn title_case_and_wick_move_variants_keep_their_kinds() {
        assert_eq!(
            parse_voice_message(&msg_with(
                "MEMBER JOINED VOICE CHANNEL",
                "x",
                &format!("ID: {MID}"),
            ))
            .map(|r| r.kind),
            Some(VoiceKind::Join)
        );
        // Wick's historic "moved" phrasing is a channel change, not a join.
        assert_eq!(
            parse_voice_message(&msg_with(
                "",
                &format!("**<@{MID}> moved voice channel <#1175127344072118405>**"),
                &format!("ID: {MID}"),
            ))
            .map(|r| r.kind),
            Some(VoiceKind::Change)
        );
    }

    #[test]
    fn multiline_leave_content_still_attributes() {
        // A literal newline in the username slot (copy-paste) still attributes
        // and the tail still classifies.
        let r = parse_leave_attribution(&leave_msg("SomeUser\nleft the server. vanity")).unwrap();
        assert_eq!(
            (r.username.as_str(), r.joined_via.as_str()),
            ("SomeUser", "vanity")
        );
    }

    #[test]
    fn corrupt_snowflake_bounds_are_none_never_a_panic() {
        let overflow = "9".repeat(100);
        for bad in ["abc", "", "  ", "-1", "12.5", "0x123", overflow.as_str()] {
            assert_eq!(snowflake_to_date_ms(bad), None, "id refused: {bad:?}");
        }
        // Valid conversions are unchanged and the inverse never panics.
        assert!(snowflake_to_date_ms(MID).is_some());
        assert_eq!(date_to_snowflake(0), "0");
        let _ = date_to_snowflake(u64::MAX);
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
