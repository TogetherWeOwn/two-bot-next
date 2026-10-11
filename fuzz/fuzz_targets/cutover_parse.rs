#![no_main]

use libfuzzer_sys::fuzz_target;
use two_bot_cutover::parse::{
    member_log_kind_for_channel, parse_leave_attribution, parse_member_log_message,
    parse_voice_message, EmbedView, MemberLogKind, MessageView, VoiceKind,
};

/// Bounds keep every allocation a function of the (libFuzzer-capped) input,
/// following the `rsvp.rs` pattern: at most this many NUL/newline-separated
/// fields are read, each truncated to this many chars before reaching a
/// constructor. Newline splitting is deliberate: multiline leave bodies stay
/// covered by the `multiline_leave_content_still_attributes` unit test, while
/// the fuzzer stresses hostile single-line field content.
const MAX_FIELDS: usize = 8;
const MAX_FIELD_CHARS: usize = 256;

/// Field order in the raw input: title, description, footer, leave content,
/// channel name. Missing trailing fields read as empty; extras are ignored.
const TITLE_IDX: usize = 0;
const DESCRIPTION_IDX: usize = 1;
const FOOTER_IDX: usize = 2;
const CONTENT_IDX: usize = 3;
const CHANNEL_IDX: usize = 4;

/// Synthetic identities only; the parsers never see a store, Discord client,
/// network or secret, so any fixed strings would do. Kept snowflake-shaped
/// for readability.
const MEMBER_ID: &str = "11111111111111111";
const CHANNEL_ID: &str = "22222222222222222";
const MESSAGE_ID: &str = "33333333333333333";
const TIMESTAMP: &str = "2026-08-19T19:32:15.762000+00:00";

fn truncate_chars(s: &str) -> String {
    s.chars().take(MAX_FIELD_CHARS).collect()
}

/// The parsers key attribution on 15–25 digit runs; a returned id outside that
/// shape would be a half record, never a clean refusal.
fn is_snowflake_shaped(id: &str) -> bool {
    (15..=25).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit())
}

fn message(
    title: Option<&str>,
    description: Option<&str>,
    footer: Option<&str>,
    content: Option<&str>,
) -> MessageView {
    MessageView {
        id: MESSAGE_ID.to_owned(),
        timestamp: TIMESTAMP.to_owned(),
        content: content.map(str::to_owned),
        author_id: None,
        author_is_bot: false,
        embeds: vec![EmbedView {
            title: title.map(str::to_owned),
            description: description.map(str::to_owned),
            footer_text: footer.map(str::to_owned),
        }],
    }
}

fuzz_target!(|data: &[u8]| {
    // Input-independent base cases: known-good Logger/Wick/member/leave rows
    // still parse, placeholders and id-less rows still refuse. Any parser
    // behavior change must update this target on its own card; silent drift
    // fails the fuzzer instead.
    let empty = MessageView {
        id: MESSAGE_ID.to_owned(),
        timestamp: TIMESTAMP.to_owned(),
        ..Default::default()
    };
    assert_eq!(parse_voice_message(&empty), None);
    assert_eq!(parse_member_log_message(&empty, None), None);
    assert_eq!(
        parse_member_log_message(&empty, Some(MemberLogKind::Join)),
        None
    );
    assert_eq!(parse_leave_attribution(&empty), None);

    let logger = message(
        Some("Member joined voice channel"),
        Some("**fuzz-name** joined #general"),
        Some("ID: 11111111111111111"),
        None,
    );
    let voice = parse_voice_message(&logger).expect("logger voice parses");
    assert_eq!(voice.member_id, MEMBER_ID);
    assert_eq!(voice.kind, VoiceKind::Join);
    assert_eq!(voice.channel_id, None);
    assert_eq!(voice.occurred_at, TIMESTAMP);

    let wick = message(
        None,
        Some("**<@11111111111111111> joined voice channel <#22222222222222222>**"),
        Some("ID: 11111111111111111"),
        None,
    );
    let voice = parse_voice_message(&wick).expect("wick voice parses");
    assert_eq!(voice.member_id, MEMBER_ID);
    assert_eq!(voice.kind, VoiceKind::Join);
    assert_eq!(voice.channel_id.as_deref(), Some(CHANNEL_ID));

    // Titled Logger descriptions carry a display name, never a mention: a
    // stray mention beside an id-less footer mints nothing.
    let stray = message(
        Some("Member joined voice channel"),
        Some("**ghostly.og** joined #general, thanks <@22222222222222222>!"),
        Some("ID: n/a"),
        None,
    );
    assert_eq!(parse_voice_message(&stray), None);

    let join = message(
        Some("Member joined"),
        Some("<@11111111111111111> 107th to join"),
        Some("ID: 11111111111111111"),
        None,
    );
    let record = parse_member_log_message(&join, None).expect("titled join parses");
    assert_eq!(record.member_id, MEMBER_ID);
    assert_eq!(record.kind, MemberLogKind::Join);
    assert_eq!(record.occurred_at, TIMESTAMP);

    // A whitespace-only title is a titled embed the parsers do not recognise:
    // the channel hint mints nothing from it.
    let blank = message(
        Some("   "),
        Some("<@11111111111111111> hi"),
        Some("ID: 11111111111111111"),
        None,
    );
    assert_eq!(
        parse_member_log_message(&blank, Some(MemberLogKind::Join)),
        None
    );

    let leave = message(
        None,
        None,
        None,
        Some("SomeUser left the server. joined via vanity"),
    );
    let attribution = parse_leave_attribution(&leave).expect("vanity leave parses");
    assert_eq!(attribution.username, "SomeUser");
    assert_eq!(attribution.joined_via, "vanity");
    assert_eq!(attribution.occurred_at, TIMESTAMP);
    assert_eq!(
        parse_leave_attribution(&message(
            None,
            None,
            None,
            Some("TODO left the server. vanity")
        )),
        None,
        "placeholder names are truncated rows, not churn"
    );

    // Discord message fields arrive as UTF-8 text. Invalid UTF-8 is discarded,
    // matching the other `&str` harnesses.
    let Ok(raw) = std::str::from_utf8(data) else {
        return;
    };
    let mut fields: Vec<String> = raw.split(['\0', '\n']).map(truncate_chars).collect();
    if fields.len() > MAX_FIELDS {
        fields.truncate(MAX_FIELDS);
    }
    for field in &fields {
        assert!(field.chars().count() <= MAX_FIELD_CHARS);
    }
    let field = |idx: usize| fields.get(idx).map(String::as_str).unwrap_or("");
    let opt = |idx: usize| {
        let value = field(idx);
        if value.is_empty() {
            None
        } else {
            Some(value)
        }
    };
    let msg = message(
        opt(TITLE_IDX),
        opt(DESCRIPTION_IDX),
        opt(FOOTER_IDX),
        opt(CONTENT_IDX),
    );
    let channel_name = field(CHANNEL_IDX);

    // The channel-name mapping drives one member-log call; a cycled explicit
    // hint drives a second, so the no-context, join-context and leave-context
    // paths all stay reachable even when the fuzzed name maps to nothing.
    let derived_hint = member_log_kind_for_channel(channel_name);
    let cycled_hint = match data.len() % 3 {
        0 => None,
        1 => Some(MemberLogKind::Join),
        _ => Some(MemberLogKind::Leave),
    };

    // No database, Discord client, network or secret is touched below; only
    // these pure string parsers run.
    let voice = parse_voice_message(&msg);
    if let Some(record) = &voice {
        assert!(
            is_snowflake_shaped(&record.member_id),
            "voice member id is a full snowflake, never a prefix"
        );
        if let Some(channel_id) = &record.channel_id {
            assert!(is_snowflake_shaped(channel_id));
        }
        assert_eq!(record.occurred_at, TIMESTAMP);
    }
    assert_eq!(
        voice,
        parse_voice_message(&msg),
        "voice parse is deterministic"
    );

    for hint in [derived_hint, cycled_hint] {
        let logged = parse_member_log_message(&msg, hint);
        if let Some(record) = &logged {
            assert!(
                is_snowflake_shaped(&record.member_id),
                "member-log id is a full snowflake, never a prefix"
            );
            assert_eq!(record.occurred_at, TIMESTAMP);
        }
        assert_eq!(
            logged,
            parse_member_log_message(&msg, hint),
            "member-log parse is deterministic"
        );
    }

    let leave = parse_leave_attribution(&msg);
    if let Some(record) = &leave {
        assert!(!record.username.is_empty());
        assert!(
            msg.content
                .as_deref()
                .is_some_and(|content| content.contains(record.username.as_str())),
            "username is a byte-exact slice of the content"
        );
        assert!(matches!(
            record.joined_via.as_str(),
            "vanity" | "oauth" | "unknown"
        ));
        assert_eq!(record.occurred_at, TIMESTAMP);
    }
    assert_eq!(
        leave,
        parse_leave_attribution(&msg),
        "leave parse is deterministic"
    );
});
