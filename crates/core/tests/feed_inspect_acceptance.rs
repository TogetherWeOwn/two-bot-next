//! Saved-relay inspect acceptance: honest last-attempt freshness.
//!
//! Behavioral contract for a private stable-ID inspect operation inside the
//! existing feed management surface. A saved relay ID, kind, destination,
//! enabled state, safe source detail and last attempted poll render through
//! the real command registry, help, list paging and store seams. A saved
//! message or source is never proof of current remote availability.
//!
//! Baseline shape: the inspect command, plan variants and renderer do not
//! exist yet, so the RED tests below fail on assertions, not on compilation.
//! `BaselineFeedInspect` provides the legacy single-row rendering as the
//! fallback; an inherent `FeedRelay::inspect_text(&self) -> String` with the
//! same signature takes precedence once implemented, and the legacy list
//! renderer stays backward compatible for short lists.
//!
//! Builder contract (one slice):
//! - Register `feed-inspect` in the announcement set with a required string
//!   `id` option (max 128), Manage Server permission, guild-only, no DMs.
//!   Announcement count moves 7 to 8; the merged feature set and the publish
//!   set each grow by one; help lists the new command under Feed relays.
//! - Add `Inspect { id }` plan routing with the same enabled, configured-guild
//!   and Manage Server gates as the existing trio, plus the same ID shape.
//! - Render one bounded detail reply (at most 2000 UTF-16 units) with the
//!   exact ID, kind, channel destination, enabled state, safe source and last
//!   attempted poll using Discord-localized time (`<t:secs:>`). Never label
//!   the attempt as healthy, successful, fetched or delivered: failed fetches
//!   advance the stored timestamp.
//! - Expose never-attempted, attempted-at and invalid or unknown legacy
//!   timestamps with the same attempt vocabulary in list rows and help.
//!   Invalid values render as unknown, never panic and never claim success.
//! - Redact userinfo, credential-like query values and fragments with an
//!   explicit notice, keep enough non-secret identity to distinguish relays,
//!   suppress mentions and hostile markdown, and never log raw source input.
//! - Keep inspect a pure guild-scoped read: no fetch, schedule change, relay
//!   mutation, audit write, claim renewal or delivery reset. Absent,
//!   wrong-guild, invalid-ID and store-failure paths stay distinct, private
//!   and mutation-free. List paging keeps every whole row reachable.
//!
//! No live Discord, no live services, no sleeps. Store tests use an owned
//! disposable fixture and skip locally when the bootstrap variable is absent.

use two_bot_core::feeds::{
    feed_list_page, feed_list_text, normalize_source, plan_command, FeedCommand,
    FeedCommandContext, FeedCommandPlan, FeedError, FeedKind, FeedRelay,
};
use two_bot_core::{announcement_commands, help_text};

/// Expected inspect command name within the feed management surface.
const INSPECT_COMMAND: &str = "feed-inspect";

/// Words that must never label the stored last-attempt timestamp.
const FORBIDDEN_ATTEMPT_LABELS: [&str; 4] = ["healthy", "success", "fetched", "delivered"];

trait BaselineFeedInspect {
    fn inspect_text(&self) -> String;
}

impl BaselineFeedInspect for FeedRelay {
    fn inspect_text(&self) -> String {
        feed_list_text(std::slice::from_ref(self))
    }
}

fn relay(id: &str, source: &str, last_checked_at: Option<i64>, enabled: bool) -> FeedRelay {
    FeedRelay {
        id: id.into(),
        guild_id: "100000000000000001".into(),
        channel_id: "123456789012345678".into(),
        kind: FeedKind::Rss,
        source: source.into(),
        enabled,
        last_checked_at,
        created_by: "100000000000000002".into(),
        created_at: 1_700_000_000_000,
        updated_at: 1_700_000_000_000,
    }
}

fn page_relay(index: usize) -> FeedRelay {
    FeedRelay {
        id: format!("{index:032x}"),
        guild_id: "100000000000000001".into(),
        channel_id: "123456789012345678".into(),
        kind: FeedKind::Rss,
        source: format!("https://example.org/news/section-{index:02}/updates?format=rss"),
        enabled: !index.is_multiple_of(5),
        last_checked_at: None,
        created_by: "100000000000000002".into(),
        created_at: index as i64,
        updated_at: 0,
    }
}

fn attempt_context() -> FeedCommandContext<'static> {
    FeedCommandContext {
        enabled: true,
        configured_guild_id: "100000000000000001",
        guild_id: "100000000000000001",
        channel_id: "123456789012345678",
        actor_id: "100000000000000002",
        can_manage_guild: true,
        now_ms: 1_700_000_000_000,
    }
}

fn assert_no_success_labels(text: &str) {
    let lowered = text.to_ascii_lowercase();
    for label in FORBIDDEN_ATTEMPT_LABELS {
        assert!(
            !lowered.contains(label),
            "attempt must not claim {label}: {text}"
        );
    }
}

fn assert_bounded(text: &str) {
    let units = text.encode_utf16().count();
    assert!(
        units <= 2000,
        "reply exceeds 2000 UTF-16 units ({units}): {text}"
    );
}

fn assert_mentions_suppressed(text: &str) {
    assert!(
        !text.contains("@everyone"),
        "raw mass mention survives: {text}"
    );
    assert!(!text.contains("@here"), "raw mass mention survives: {text}");
}

#[test]
fn inspect_command_is_registered_with_stable_id_and_manage_guild() {
    let definitions = announcement_commands();
    assert_eq!(
        definitions.len(),
        8,
        "the feed management surface grows by exactly the inspect command"
    );
    let definition = definitions
        .iter()
        .find(|command| command.name == INSPECT_COMMAND)
        .expect("inspect command is registered");
    assert_eq!(
        definition.default_member_permissions.as_deref(),
        Some("32"),
        "inspect keeps the Manage Server gate"
    );
    assert!(!definition.dm_permission, "inspect stays guild-only");
    assert!(!definition.description.trim().is_empty());
    assert_eq!(
        definition.options.len(),
        1,
        "the only input is the relay id"
    );
    let id = &definition.options[0];
    assert_eq!(id.name, "id");
    assert_eq!(id.kind, 3, "Discord String, like the remove command");
    assert_eq!(id.required, Some(true));
    assert_eq!(id.max_length, Some(128));
    assert!(!id.description.trim().is_empty());
}

#[test]
fn feed_trio_controls_preserve_existing_registry_shapes() {
    let definitions = announcement_commands();
    for name in ["feed-add", "feed-remove", "feed-list"] {
        let definition = definitions
            .iter()
            .find(|command| command.name == name)
            .expect("existing feed command stays registered");
        assert_eq!(
            definition.default_member_permissions.as_deref(),
            Some("32"),
            "{name} keeps Manage Server"
        );
        assert!(!definition.dm_permission, "{name} stays guild-only");
    }
    let list = definitions
        .iter()
        .find(|command| command.name == "feed-list")
        .expect("list stays registered");
    assert_eq!(list.options.len(), 1);
    assert_eq!(list.options[0].name, "page");
    assert_eq!(list.options[0].kind, 4);
}

#[test]
fn plan_gates_preserve_management_and_configured_guild_fences() {
    let mut context = FeedCommandContext {
        enabled: false,
        configured_guild_id: "100000000000000001",
        guild_id: "100000000000000001",
        channel_id: "123456789012345678",
        actor_id: "100000000000000002",
        can_manage_guild: true,
        now_ms: 1_700_000_000_000,
    };
    assert!(matches!(
        plan_command(&context, FeedCommand::List),
        Err(FeedError::Disabled)
    ));
    context.enabled = true;
    context.can_manage_guild = false;
    assert!(matches!(
        plan_command(&context, FeedCommand::List),
        Err(FeedError::PermissionRequired)
    ));
    context.can_manage_guild = true;
    context.guild_id = "999999999999999999";
    assert!(matches!(
        plan_command(&context, FeedCommand::List),
        Err(FeedError::Disabled)
    ));
    context.guild_id = "100000000000000001";
    assert!(matches!(
        plan_command(&context, FeedCommand::List),
        Ok(FeedCommandPlan::List { .. })
    ));
    assert!(matches!(
        plan_command(
            &context,
            FeedCommand::Remove {
                id: "not an id".into(),
            }
        ),
        Err(FeedError::InvalidId)
    ));
}

#[test]
fn help_feed_group_advertises_inspect() {
    let text = help_text(&announcement_commands());
    let section = text
        .split("**Feed relays**")
        .nth(1)
        .expect("Feed relays group renders");
    let group = section.split("**").next().unwrap_or(section);
    assert!(
        group.contains(&format!("/{INSPECT_COMMAND}")),
        "Feed relays help must list /{INSPECT_COMMAND}: {text}"
    );
}

#[test]
fn list_pages_advertise_inspect_and_attempt_vocabulary() {
    let rows: Vec<FeedRelay> = (1..=3).map(page_relay).collect();
    let single = feed_list_page(&rows, 1);
    assert!(
        single.contains(INSPECT_COMMAND),
        "short list must point at inspect: {single}"
    );
    assert!(
        single.to_ascii_lowercase().contains("attempt"),
        "short list must use attempt vocabulary: {single}"
    );
    let many: Vec<FeedRelay> = (1..=25).map(page_relay).collect();
    let first = feed_list_page(&many, 1);
    assert!(
        first.contains(INSPECT_COMMAND),
        "paged list must point at inspect: {first}"
    );
    assert!(
        first.to_ascii_lowercase().contains("attempt"),
        "paged list must use attempt vocabulary: {first}"
    );
    assert_no_success_labels(&single);
    assert_no_success_labels(&first);
}

#[test]
fn inspect_shows_identity_destination_enabled_safe_source_and_attempt() {
    let feed = relay(
        "relay-identity-1",
        "https://example.org/news/updates?format=rss",
        Some(1_700_000_000_000),
        true,
    );
    let text = feed.inspect_text();
    assert_bounded(&text);
    assert!(
        text.contains("`relay-identity-1`"),
        "exact ID stays copyable: {text}"
    );
    assert!(text.contains("rss"), "kind stays visible: {text}");
    assert!(
        text.contains("<#123456789012345678>"),
        "destination stays visible: {text}"
    );
    assert!(
        text.to_ascii_lowercase().contains("enabled"),
        "enabled state stays visible: {text}"
    );
    assert!(
        text.contains("example.org"),
        "non-secret source identity stays visible: {text}"
    );
    assert!(
        text.to_ascii_lowercase().contains("attempt"),
        "last attempt stays visible: {text}"
    );
    assert!(
        text.contains("<t:1700000000:"),
        "attempt uses Discord-localized time: {text}"
    );
    assert_no_success_labels(&text);
    assert_mentions_suppressed(&text);
}

#[test]
fn inspect_never_attempted_is_truthful_without_success_labels() {
    let feed = relay(
        "relay-never-1",
        "https://example.org/news/never?format=rss",
        None,
        true,
    );
    let text = feed.inspect_text();
    let lowered = text.to_ascii_lowercase();
    assert!(
        lowered.contains("never") && lowered.contains("attempt"),
        "never-attempted must say so plainly: {text}"
    );
    assert_no_success_labels(&text);
    assert!(
        !text.contains("<t:"),
        "never-attempted carries no timestamp: {text}"
    );
}

#[test]
fn inspect_attempted_at_uses_discord_time_without_success_labels() {
    let feed = relay(
        "relay-attempted-1",
        "https://example.org/news/attempted?format=rss",
        Some(1_700_000_123_000),
        false,
    );
    let text = feed.inspect_text();
    assert!(
        text.to_ascii_lowercase().contains("attempt"),
        "attempted-at must use attempt vocabulary: {text}"
    );
    assert!(
        text.contains("<t:1700000123:"),
        "attempted-at uses Discord-localized time: {text}"
    );
    assert!(
        text.to_ascii_lowercase().contains("disabled"),
        "disabled state stays visible: {text}"
    );
    assert_no_success_labels(&text);
}

#[test]
fn inspect_failed_fetch_advance_is_attempt_not_success() {
    // The poller advances the stored timestamp even when the fetch fails, so
    // inspect must label that timestamp as an attempt, never as a fetch or
    // delivery. The dedicated poller control pins the advance itself.
    let feed = relay(
        "relay-failed-fetch-1",
        "https://example.org/news/failed?format=rss",
        Some(1_700_000_456_000),
        true,
    );
    let text = feed.inspect_text();
    assert!(
        text.to_ascii_lowercase().contains("attempt"),
        "a failed-fetch timestamp is still an attempt: {text}"
    );
    assert!(
        text.contains("<t:1700000456:"),
        "failed-fetch attempt keeps its timestamp: {text}"
    );
    assert_no_success_labels(&text);
}

#[test]
fn inspect_invalid_legacy_timestamps_are_unknown_not_healthy() {
    for (name, stamp) in [
        ("relay-legacy-negative", -1),
        ("relay-legacy-far-future", i64::MAX),
    ] {
        let feed = relay(
            name,
            "https://example.org/news/legacy?format=rss",
            Some(stamp),
            true,
        );
        let text = feed.inspect_text();
        assert_bounded(&text);
        let lowered = text.to_ascii_lowercase();
        assert!(
            lowered.contains("unknown") && lowered.contains("attempt"),
            "invalid legacy stamp must read as unknown attempt: {text}"
        );
        assert_no_success_labels(&text);
        assert!(
            text.contains(&format!("`{name}`")),
            "invalid stamp keeps the exact ID: {text}"
        );
    }
}

#[test]
fn attempt_vocabulary_never_uses_success_labels_in_list_controls() {
    // Qualified call keeps the baseline trait live after an inherent method lands.
    let feed = relay(
        "relay-keepalive-1",
        "https://example.org/news/keepalive?format=rss",
        Some(1_700_000_000_000),
        true,
    );
    assert_eq!(
        <FeedRelay as BaselineFeedInspect>::inspect_text(&feed),
        feed_list_text(std::slice::from_ref(&feed))
    );
    let rows: Vec<FeedRelay> = (1..=3).map(page_relay).collect();
    let single = feed_list_page(&rows, 1);
    let many: Vec<FeedRelay> = (1..=25).map(page_relay).collect();
    let first = feed_list_page(&many, 1);
    assert_no_success_labels(&single);
    assert_no_success_labels(&first);
    assert_bounded(&single);
    assert_bounded(&first);
}

#[test]
fn inspect_redacts_userinfo_credential_query_and_fragment_with_notice() {
    let hostile = "https://fixture-user:fixture-password@example.org/news/feed.xml?token=fixture-query-secret&api_key=fixture-key-secret&channel_id=UC0123456789#fixture-fragment-secret";
    let feed = relay("relay-hostile-1", hostile, Some(1_700_000_000_000), true);
    let text = feed.inspect_text();
    assert_bounded(&text);
    for secret in [
        "fixture-user",
        "fixture-password",
        "fixture-query-secret",
        "fixture-key-secret",
        "fixture-fragment-secret",
    ] {
        assert!(
            !text.contains(secret),
            "secret value must not survive rendering: {text}"
        );
    }
    // The channel destination (`<#id>`) legitimately contains '#', so the
    // fragment ban applies to the source projection only.
    let source_line = text.lines().find(|line| line.starts_with("Source:"));
    assert!(
        source_line.is_some(),
        "detail must project a Source line: {text}"
    );
    assert!(
        !source_line.is_some_and(|line| line.contains('#')),
        "fragments must not survive the source projection: {text}"
    );
    assert!(
        text.to_ascii_lowercase().contains("redact"),
        "redaction must be explicit: {text}"
    );
    assert!(
        text.contains("example.org"),
        "safe host identity stays visible: {text}"
    );
    assert!(
        text.contains("UC0123456789"),
        "non-secret identity distinguishes relays: {text}"
    );
    assert_no_success_labels(&text);
}

fn assert_normalized_query_credentials_redacted(name: &str) {
    let secret = "fixture-encoded-credential-value";
    let source = format!(
        "https://example.org/{}/feed?{name}={secret}&channel_id=UC0123456789&format=rss",
        "a".repeat(160)
    );
    assert!(
        source.find(secret).expect("fixture has credential") > 128,
        "credential must be beyond the legacy source preview"
    );
    let normalized = normalize_source(FeedKind::Rss, &source)
        .expect("encoded and compound credential names are valid saved sources");
    assert!(
        normalized.contains(secret),
        "normalization is not redaction"
    );
    let text = relay("relay-query-redaction", &normalized, None, true).inspect_text();
    assert_bounded(&text);
    assert!(
        !text.contains(secret),
        "credential value for {name} must not survive inspect: {text}"
    );
    assert!(
        text.to_ascii_lowercase().contains("redact"),
        "credential removal for {name} must have an explicit notice: {text}"
    );
    let source_line = text
        .lines()
        .find(|line| line.starts_with("Source: "))
        .expect("detail projects a source line");
    for identity in ["example.org", r"channel\_id=UC0123456789", "format=rss"] {
        assert!(
            source_line.contains(identity),
            "safe identity {identity} survives credential redaction: {text}"
        );
    }
    assert_no_success_labels(&text);
}

#[test]
fn inspect_redacts_lower_hex_encoded_credential_names_after_normalization() {
    for name in ["to%6ben", "%74oken", "client%53ecret"] {
        assert_normalized_query_credentials_redacted(name);
    }
}

#[test]
fn inspect_redacts_upper_hex_encoded_credential_names_after_normalization() {
    for name in ["to%6Ben", "access%54oken", "client%53ecret"] {
        assert_normalized_query_credentials_redacted(name);
    }
}

#[test]
fn inspect_redacts_compound_credential_names_after_normalization() {
    for name in ["accessToken", "clientSecret"] {
        assert_normalized_query_credentials_redacted(name);
    }
}

#[test]
fn inspect_redacts_case_variant_credential_names_after_normalization() {
    for name in ["AccessToken", "ACCESSTOKEN", "ClientSecret", "CLIENTSECRET"] {
        assert_normalized_query_credentials_redacted(name);
    }
}

#[test]
fn inspect_plain_token_redaction_preserves_safe_query_identity() {
    assert_normalized_query_credentials_redacted("token");
}

fn assert_semicolon_query_credentials_redacted(
    raw_query: &str,
    secrets: &[&str],
    expected_identities: &[&str],
) {
    let source = format!("https://example.org/{}/feed?{raw_query}", "a".repeat(160));
    for secret in secrets {
        assert!(
            source.find(secret).expect("fixture has credential") > 128,
            "credential must be beyond the legacy source preview"
        );
    }
    let normalized = normalize_source(FeedKind::Rss, &source)
        .expect("semicolon-separated credential query is a valid saved source");
    for secret in secrets {
        assert!(
            normalized.contains(secret),
            "normalization is not redaction"
        );
    }
    let text = relay("relay-query-redaction", &normalized, None, true).inspect_text();
    assert_bounded(&text);
    for secret in secrets {
        assert!(
            !text.contains(secret),
            "semicolon-separated credential value must not survive inspect: {text}"
        );
    }
    assert!(
        text.to_ascii_lowercase().contains("redact"),
        "semicolon credential removal must have an explicit notice: {text}"
    );
    let source_line = text
        .lines()
        .find(|line| line.starts_with("Source: "))
        .expect("detail projects a source line");
    for identity in expected_identities {
        assert!(
            source_line.contains(identity),
            "safe identity {identity} survives semicolon credential redaction: {text}"
        );
    }
    assert!(
        source_line.contains("format=rss;"),
        "original semicolon separator survives in the projection: {text}"
    );
    assert_no_success_labels(&text);
}

#[test]
fn inspect_redacts_semicolon_separated_credentials_preserving_separator() {
    assert_semicolon_query_credentials_redacted(
        "format=rss;token=fixture-semicolon-secret-value",
        &["fixture-semicolon-secret-value"],
        &["example.org", "format=rss"],
    );
    assert_semicolon_query_credentials_redacted(
        "format=rss;token=fixture-mixed-secret-one&channel_id=UC0123456789",
        &["fixture-mixed-secret-one"],
        &["example.org", "format=rss", r"channel\_id=UC0123456789"],
    );
}

#[test]
fn inspect_public_ipv6_source_has_exactly_one_bracket_pair() {
    let source = "https://[2606:4700:4700::1111]/feed";
    let normalized = normalize_source(FeedKind::Rss, source)
        .expect("default-port public IPv6 source passes the saved-source validator");
    assert_eq!(normalized, source);
    // Nondefault ports exercise legacy saved rows, not new-source admission.
    for source in [
        normalized.as_str(),
        "https://[2606:4700:4700::1111]:8443/feed",
    ] {
        let text = relay("relay-ipv6", source, None, true).inspect_text();
        let source_lines: Vec<&str> = text
            .lines()
            .filter(|line| line.starts_with("Source: "))
            .collect();
        assert_eq!(source_lines, vec![format!("Source: {source}")]);
        assert_bounded(&text);
        assert_no_success_labels(&text);
    }
}

#[test]
fn inspect_suppresses_mentions_embeds_and_hostile_markdown() {
    let hostile =
        "https://example.org/@everyone/news**](https://attacker.example/@here)?format=rss";
    let feed = relay("relay-markdown-1", hostile, None, true);
    let text = feed.inspect_text();
    assert_bounded(&text);
    assert_mentions_suppressed(&text);
    assert!(
        !text.contains("]("),
        "masked-link markdown must not survive: {text}"
    );
    assert!(
        !text.contains("attacker.example"),
        "hostile markdown target must not survive as a link: {text}"
    );
    assert!(
        text.contains("`relay-markdown-1`"),
        "exact ID stays reachable under hostile source: {text}"
    );
}

#[test]
fn inspect_source_stays_bounded_with_explicit_truncation_and_identity() {
    let long_tail = format!(
        "https://example.org/{}/videos.xml?channel_id=UC9876543210&format=rss",
        "a".repeat(1800)
    );
    assert!(long_tail.len() <= 2048);
    let feed = relay("relay-long-1", &long_tail, Some(1_700_000_000_000), true);
    let text = feed.inspect_text();
    assert_bounded(&text);
    assert!(
        text.contains('…') || text.to_ascii_lowercase().contains("truncat"),
        "source truncation must be explicit: {text}"
    );
    assert!(
        text.contains("UC9876543210"),
        "distinguishing tail survives truncation: {text}"
    );
    let astral = format!(
        "https://example.org/{}/feed.xml?format=rss",
        "🦀".repeat(120)
    );
    let astral_feed = relay("relay-astral-1", &astral, None, true);
    let astral_text = astral_feed.inspect_text();
    assert_bounded(&astral_text);
    assert!(
        astral_text.contains("`relay-astral-1`"),
        "astral source keeps the exact ID: {astral_text}"
    );
    assert_no_success_labels(&text);
    assert_no_success_labels(&astral_text);
}

#[test]
fn list_pages_preserve_every_id_at_capacity_with_deterministic_unicode_paging() {
    let rows: Vec<FeedRelay> = (1..=25).map(page_relay).collect();
    let first = feed_list_page(&rows, 1);
    assert_bounded(&first);
    // Deterministic packing: the same rows always render the same first page.
    assert_eq!(first, feed_list_page(&rows, 1));
    // Every ID stays discoverable across the advertised pages.
    let mut seen = Vec::new();
    let mut page = 1;
    loop {
        let text = feed_list_page(&rows, page);
        assert_bounded(&text);
        for line in text.lines().filter(|line| line.starts_with('`')) {
            let id = line
                .strip_prefix('`')
                .and_then(|rest| rest.split_once('`').map(|(id, _)| id))
                .expect("whole row keeps the exact ID");
            seen.push(id.to_owned());
        }
        if text.contains("Page ") && text.contains(" of ") {
            page += 1;
            if page > 25 {
                break;
            }
            continue;
        }
        break;
    }
    let expected: Vec<String> = rows.iter().map(|row| row.id.clone()).collect();
    assert_eq!(
        seen, expected,
        "every exact ID occurs once, in stable order"
    );
}

fn advertised_list_rows(rows: &[FeedRelay]) -> Vec<String> {
    let first = feed_list_page(rows, 1);
    let total = first
        .lines()
        .find_map(|line| line.strip_prefix("Page 1 of "))
        .map(|rest| {
            rest.split_once('.')
                .expect("page footer has a complete page count")
                .0
                .parse::<usize>()
                .expect("advertised page count is numeric")
        })
        .unwrap_or(1);
    assert!((1..=rows.len()).contains(&total));
    let mut rendered = Vec::new();
    for page in 1..=total {
        let page = i64::try_from(page).expect("bounded page count fits command input");
        let text = feed_list_page(rows, page);
        assert_bounded(&text);
        assert_no_success_labels(&text);
        assert_eq!(text, feed_list_page(rows, page), "paging is deterministic");
        if total > 1 {
            assert!(text.contains(&format!("Page {page} of {total}.")));
        }
        rendered.extend(
            text.lines()
                .filter(|line| line.starts_with('`'))
                .map(str::to_owned),
        );
    }
    let seen: Vec<&str> = rendered
        .iter()
        .map(|line| {
            line.strip_prefix('`')
                .and_then(|rest| rest.split_once('`').map(|(id, _)| id))
                .expect("whole row keeps its complete stable ID")
        })
        .collect();
    let expected: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(
        seen, expected,
        "every ID occurs once across advertised pages"
    );
    rendered
}

fn assert_list_attempt_suffix(line: &str, expected: &str) {
    assert_eq!(
        line.rsplit_once(" · ").map(|(_, suffix)| suffix),
        Some(expected),
        "whole row must end in the exact attempt phrase: {line}"
    );
    assert_eq!(line.matches("Last attempt:").count(), 1);
}

#[test]
fn list_pages_apply_same_attempt_vocabulary_without_losing_rows() {
    let mut rows: Vec<FeedRelay> = (1..=25).map(page_relay).collect();
    let attempts = [
        (None, "Last attempt: never"),
        (Some(1_700_000_000_000), "Last attempt: <t:1700000000:R>"),
        (Some(-1), "Last attempt: unknown"),
    ];
    for (index, row) in rows.iter_mut().enumerate() {
        row.last_checked_at = attempts[index % attempts.len()].0;
        row.source = format!("https://example.org/{}/feed-{index}", "🦀".repeat(40));
    }
    assert!(feed_list_page(&rows, 1).contains("Page 1 of "));
    let rendered = advertised_list_rows(&rows);
    for (index, row) in rows.iter().enumerate() {
        let prefix = format!("`{}` ", row.id);
        let line = rendered
            .iter()
            .find(|line| line.starts_with(&prefix))
            .expect("exact stable-ID row stays reachable across advertised pages");
        assert_list_attempt_suffix(line, attempts[index % attempts.len()].1);
    }
}

// Literal expected phrases make each millisecond boundary independent of the
// renderer's range predicate and integer division. In particular MAX-1 must
// not round up, and MAX+1 must not become credible after division by 1000.
const ATTEMPT_BOUNDARIES: [(i64, &str); 7] = [
    (-1, "Last attempt: unknown"),
    (0, "Last attempt: <t:0:R>"),
    (1, "Last attempt: <t:0:R>"),
    (253_402_300_798_999, "Last attempt: <t:253402300798:R>"),
    (253_402_300_799_000, "Last attempt: <t:253402300799:R>"),
    (253_402_300_799_001, "Last attempt: unknown"),
    (i64::MAX, "Last attempt: unknown"),
];

#[test]
fn inspect_attempt_timestamp_millisecond_boundaries_are_exact() {
    for (stamp, expected) in ATTEMPT_BOUNDARIES {
        let feed = relay(
            "relay-timestamp-boundary",
            "https://example.org/feed?format=rss",
            Some(stamp),
            true,
        );
        let text = feed.inspect_text();
        let phrases: Vec<&str> = text
            .lines()
            .filter(|line| line.starts_with("Last attempt:"))
            .collect();
        assert_eq!(phrases, vec![expected], "detail timestamp {stamp}: {text}");
        assert_bounded(&text);
        assert_no_success_labels(&text);
    }
}

#[test]
fn list_attempt_timestamp_millisecond_boundaries_are_exact() {
    let rows: Vec<FeedRelay> = ATTEMPT_BOUNDARIES
        .iter()
        .enumerate()
        .map(|(index, (stamp, _))| {
            let mut feed = page_relay(index + 1);
            feed.last_checked_at = Some(*stamp);
            feed
        })
        .collect();
    let rendered = advertised_list_rows(&rows);
    for (row, (_, expected)) in rows.iter().zip(ATTEMPT_BOUNDARIES) {
        let prefix = format!("`{}` ", row.id);
        let line = rendered
            .iter()
            .find(|line| line.starts_with(&prefix))
            .expect("exact boundary row remains reachable");
        assert_list_attempt_suffix(line, expected);
    }
}

#[test]
fn inspect_stays_private_guild_scoped_and_validated_at_plan_time() {
    let context = attempt_context();
    assert!(matches!(
        plan_command(&context, FeedCommand::List),
        Ok(FeedCommandPlan::List { .. })
    ));
    let mut foreign = attempt_context();
    foreign.guild_id = "999999999999999999";
    assert!(matches!(
        plan_command(&foreign, FeedCommand::List),
        Err(FeedError::Disabled)
    ));
    assert!(matches!(
        plan_command(
            &context,
            FeedCommand::Remove {
                id: "bad id".into(),
            }
        ),
        Err(FeedError::InvalidId)
    ));
    // The inspect plan must reuse these exact gates; the registry test above
    // fails until the inspect command exists to route through them.
    let definitions = announcement_commands();
    assert!(
        definitions
            .iter()
            .any(|command| command.name == INSPECT_COMMAND),
        "inspect routing is required for plan coverage"
    );
}

#[cfg(feature = "db")]
#[tokio::test]
async fn inspect_store_guild_isolation_and_no_mutation_on_read() {
    use two_bot_core::feeds_store;
    use two_bot_testsupport::TestDatabase;

    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        eprintln!("SKIP inspect store isolation: bootstrap variable is absent");
        return;
    };
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create migrated disposable fixture");
    let own = relay(
        "store-own-1",
        "https://example.org/store/own.xml",
        Some(1_700_000_000_000),
        true,
    );
    let mut foreign = relay(
        "store-foreign-1",
        "https://example.org/store/foreign.xml",
        None,
        true,
    );
    foreign.guild_id = "999999999999999999".into();
    feeds_store::add_feed(db.pool(), &own)
        .await
        .expect("seed own relay");
    feeds_store::add_feed(db.pool(), &foreign)
        .await
        .expect("seed foreign relay");
    let before: serde_json::Value = sqlx::query_scalar(
        "SELECT jsonb_build_object('relays', (SELECT coalesce(jsonb_agg(to_jsonb(r) ORDER BY id), '[]'::jsonb) FROM feed_relays r), 'deliveries', (SELECT coalesce(jsonb_agg(to_jsonb(d) ORDER BY feed_id, item_key), '[]'::jsonb) FROM feed_deliveries d), 'audit', (SELECT coalesce(jsonb_agg(to_jsonb(a) ORDER BY id), '[]'::jsonb) FROM announcements_audit_log a))",
    )
    .fetch_one(db.pool())
    .await
    .expect("snapshot before read");
    let listed = feeds_store::list_feeds(db.pool(), &own.guild_id, false)
        .await
        .expect("guild-scoped list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "store-own-1");
    assert!(
        !listed.iter().any(|row| row.id == "store-foreign-1"),
        "foreign relays never leak into the owning guild read"
    );
    // Pure rendering over the stored row: the detail must carry attempt
    // vocabulary once implemented, without touching the store.
    let text = listed[0].inspect_text();
    assert!(
        text.to_ascii_lowercase().contains("attempt"),
        "stored-row inspect uses attempt vocabulary: {text}"
    );
    let after: serde_json::Value = sqlx::query_scalar(
        "SELECT jsonb_build_object('relays', (SELECT coalesce(jsonb_agg(to_jsonb(r) ORDER BY id), '[]'::jsonb) FROM feed_relays r), 'deliveries', (SELECT coalesce(jsonb_agg(to_jsonb(d) ORDER BY feed_id, item_key), '[]'::jsonb) FROM feed_deliveries d), 'audit', (SELECT coalesce(jsonb_agg(to_jsonb(a) ORDER BY id), '[]'::jsonb) FROM announcements_audit_log a))",
    )
    .fetch_one(db.pool())
    .await
    .expect("snapshot after read");
    assert_eq!(before, after, "inspect reads change no stored column");
    db.close().await.expect("verify teardown");
}

#[test]
fn inspect_invalid_and_disabled_shapes_are_distinct_at_plan_time() {
    let context = attempt_context();
    assert!(matches!(
        plan_command(
            &context,
            FeedCommand::Remove {
                id: "bad id".into(),
            }
        ),
        Err(FeedError::InvalidId)
    ));
    let mut disabled = attempt_context();
    disabled.enabled = false;
    assert!(matches!(
        plan_command(&disabled, FeedCommand::List),
        Err(FeedError::Disabled)
    ));
    // Absent, invalid and store-failure replies must stay distinct once the
    // inspect route exists; the registry assertion fails until it does.
    let definitions = announcement_commands();
    assert!(
        definitions
            .iter()
            .any(|command| command.name == INSPECT_COMMAND),
        "distinct inspect recovery copy requires the inspect route"
    );
}

#[cfg(feature = "db")]
#[tokio::test]
async fn inspect_store_preserves_delivery_ledger_and_fences_on_read() {
    use two_bot_core::feeds_store;
    use two_bot_testsupport::TestDatabase;

    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        eprintln!("SKIP inspect ledger preservation: bootstrap variable is absent");
        return;
    };
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create migrated disposable fixture");
    let feed = relay(
        "store-ledger-1",
        "https://example.org/store/ledger.xml",
        Some(1_700_000_000_000),
        true,
    );
    feeds_store::add_feed(db.pool(), &feed)
        .await
        .expect("seed relay");
    sqlx::query(
        "INSERT INTO feed_deliveries (feed_id, item_key, nonce, state, first_seen_at) VALUES ($1, 'item-0', 'nonce-store-ledger-1', 'pending', '2030-01-01T00:00:00Z')",
    )
    .bind("store-ledger-1")
    .execute(db.pool())
    .await
    .expect("seed pending delivery");
    let before: serde_json::Value = sqlx::query_scalar(
        "SELECT jsonb_build_object('relays', (SELECT coalesce(jsonb_agg(to_jsonb(r) ORDER BY id), '[]'::jsonb) FROM feed_relays r), 'deliveries', (SELECT coalesce(jsonb_agg(to_jsonb(d) ORDER BY feed_id, item_key), '[]'::jsonb) FROM feed_deliveries d), 'audit', (SELECT coalesce(jsonb_agg(to_jsonb(a) ORDER BY id), '[]'::jsonb) FROM announcements_audit_log a))",
    )
    .fetch_one(db.pool())
    .await
    .expect("snapshot before read");
    let listed = feeds_store::list_feeds(db.pool(), &feed.guild_id, false)
        .await
        .expect("guild-scoped list");
    assert_eq!(listed.len(), 1);
    let text = listed[0].inspect_text();
    assert!(
        text.contains("`store-ledger-1`"),
        "ledger read keeps the exact ID: {text}"
    );
    let after: serde_json::Value = sqlx::query_scalar(
        "SELECT jsonb_build_object('relays', (SELECT coalesce(jsonb_agg(to_jsonb(r) ORDER BY id), '[]'::jsonb) FROM feed_relays r), 'deliveries', (SELECT coalesce(jsonb_agg(to_jsonb(d) ORDER BY feed_id, item_key), '[]'::jsonb) FROM feed_deliveries d), 'audit', (SELECT coalesce(jsonb_agg(to_jsonb(a) ORDER BY id), '[]'::jsonb) FROM announcements_audit_log a))",
    )
    .fetch_one(db.pool())
    .await
    .expect("snapshot after read");
    assert_eq!(
        before, after,
        "inspect reads never renew claims or reset fences"
    );
    db.close().await.expect("verify teardown");
}
