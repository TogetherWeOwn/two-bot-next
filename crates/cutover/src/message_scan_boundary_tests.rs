//! Acceptance tests: channel-name classifier token boundaries.
//!
//! Innocent names that merely contain a bot-output needle as part of a
//! larger word must route to conversation; true bot-output tokens must
//! still skip. The digit-dash rule, the category path and forum/thread
//! gating are unchanged.

use super::{is_conversation_channel, is_log_channel, is_loggy};

/// The innocent names named in the acceptance criteria.
const SPEC_INNOCENT: [&str; 4] = ["catalog", "dialogue", "blog", "catalogue"];

#[test]
fn spec_innocent_names_are_not_loggy() {
    for name in SPEC_INNOCENT {
        assert!(!is_loggy(name), "{name} must not be loggy");
        assert!(
            !is_log_channel(name, "Community"),
            "{name} must not be a log channel"
        );
    }
}

#[test]
fn spec_innocent_names_route_to_conversation() {
    for name in SPEC_INNOCENT {
        // Text (0) and announcement (5) channels both hold conversation.
        for channel_type in [0_u8, 5] {
            assert!(
                is_conversation_channel(name, "Community", channel_type),
                "{name} (type {channel_type}) must route to conversation"
            );
        }
    }
}

#[test]
fn hyphenated_innocent_tokens_are_not_loggy() {
    for name in [
        "catalog-discussion",
        "team-blog",
        "blog-posts",
        "dialogue-corner",
    ] {
        assert!(!is_loggy(name), "{name} must not be loggy");
        assert!(
            is_conversation_channel(name, "Community", 0),
            "{name} must route to conversation"
        );
    }
}

#[test]
fn needle_substrings_inside_larger_words_are_not_loggy() {
    // Each name contains a needle (`log`, `audit` or `wick`) only as part
    // of a larger word, never as a whole token.
    for name in [
        "logic",
        "prologue",
        "login",
        "blogs",
        "catalogs",
        "auditions",
        "auditorium",
        "wicked",
        "candlewick",
    ] {
        assert!(!is_loggy(name), "{name} must not be loggy");
        assert!(
            is_conversation_channel(name, "Community", 0),
            "{name} must route to conversation"
        );
    }
}

#[test]
fn true_log_tokens_still_skip() {
    for name in [
        "log",
        "logs",
        "mod-log",
        "audit-log",
        "bot-logs",
        "audit-logs",
        "server-log",
        "Mod-Log",
        "AUDIT-LOG",
        "wick-logs",
        "modmail",
        "network-status",
        "network-status-updates",
    ] {
        assert!(is_loggy(name), "{name} must still be loggy");
        assert!(
            is_log_channel(name, "Community"),
            "{name} must still be a log channel"
        );
        assert!(
            !is_conversation_channel(name, "Community", 0),
            "{name} must still skip conversation"
        );
    }
}

#[test]
fn compound_log_names_still_skip() {
    // Regression: the legacy substring matcher skipped compound bot-output
    // names without a separator, and the token-boundary fix must not start
    // scanning these high-volume channels as conversation.
    for name in [
        "modlogs",
        "modlog",
        "auditlog",
        "auditlogs",
        "botlogs",
        "botlog",
        "serverlogs",
        "serverlog",
        "messagelogs",
        "joinlogs",
        "wicklogs",
        "logging",
        "mod-logging",
        "modmails",
        "audits",
        "changelog",
        "MODLOGS",
        "AuditLog",
    ] {
        assert!(is_loggy(name), "{name} must still be loggy");
        assert!(
            is_log_channel(name, "Community"),
            "{name} must still be a log channel"
        );
        assert!(
            !is_conversation_channel(name, "Community", 0),
            "{name} must still skip conversation"
        );
    }
}

#[test]
fn innocent_suffix_words_stay_conversation() {
    // `catalog`, `blog` and `dialog` end a segment with `log`/`logs` but
    // are member conversation; the compound-suffix rule carves them out.
    for name in ["catalog", "catalogs", "blog", "blogs", "dialog", "dialogs"] {
        assert!(!is_loggy(name), "{name} must not be loggy");
        assert!(
            is_conversation_channel(name, "Community", 0),
            "{name} must route to conversation"
        );
    }
}

#[test]
fn partial_network_status_words_are_conversation() {
    // Only the full `network-status` phrase is a bot-output token; its
    // halves on their own must not be broadened into needles.
    for name in ["network", "networking", "status"] {
        assert!(!is_loggy(name), "{name} must not be loggy");
        assert!(
            is_conversation_channel(name, "Community", 0),
            "{name} must route to conversation"
        );
    }
}

#[test]
fn category_path_uses_same_token_boundaries() {
    // An innocent category name must not skip an innocent channel.
    for category in ["Catalog", "dialogue", "Community"] {
        assert!(
            !is_log_channel("general", category),
            "category {category} must not make general a log channel"
        );
        assert!(
            is_conversation_channel("general", category, 0),
            "general under {category} must route to conversation"
        );
    }
    assert!(is_conversation_channel("catalog", "", 0));

    // A true log category still skips, whichever side carries the token.
    assert!(is_log_channel("catalog", "audit-log"));
    assert!(!is_conversation_channel("catalog", "audit-log", 0));
    assert!(is_log_channel("general", "wick-logs"));
    assert!(!is_conversation_channel("general", "wick-logs", 0));
    assert!(is_log_channel("mod-log", "Community"));
}

#[test]
fn digit_dash_rule_is_unchanged() {
    // `^\d+-[a-z]` names still skip.
    for name in ["3-general", "42-chat"] {
        assert!(is_loggy(name), "{name} must still be loggy");
        assert!(
            !is_conversation_channel(name, "Community", 0),
            "{name} must still skip conversation"
        );
    }
    // A trailing digit is not the digit-dash prefix rule.
    assert!(!is_loggy("general-3"));
    assert!(is_conversation_channel("general-3", "Community", 0));
}

#[test]
fn forum_and_nontext_gating_is_unchanged() {
    // Forum channels (15) hold conversation unless the name is loggy.
    assert!(is_conversation_channel("catalog", "Community", 15));
    assert!(is_conversation_channel("forum-posts", "Community", 15));
    assert!(!is_conversation_channel("mod-log", "Community", 15));

    // Voice (2) and thread (10, 11, 12) types never route through this
    // predicate, even with an innocent name.
    for channel_type in [2_u8, 10, 11, 12] {
        assert!(
            !is_conversation_channel("catalog", "Community", channel_type),
            "type {channel_type} must not route to conversation"
        );
    }
}
