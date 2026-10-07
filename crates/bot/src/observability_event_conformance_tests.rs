//! Structured-log event-name conformance: offline test-only pin of the
//! gateway/session observability vocabularies to
//! `docs/observability-event-catalog.md`.
//!
//! The catalog records every structured-log event name on the gateway cutover
//! path plus the adjacent bounded vocabularies (metric event labels, gateway
//! failure classes, startup error classes, `voice_event` field values). These
//! tests assert the code and the catalog agree exactly, and fail closed on
//! unknowns: adding, removing or renaming an event without updating the
//! catalog (and the const below) turns the suite red. No behavior changes;
//! nothing here touches staging, secrets or the network.

use std::collections::BTreeSet;

// Gateway lifecycle events, partitioned by the file the catalog attributes
// them to. The message string (last argument to the `tracing` macro) is the
// event name; fields such as `sequence` or `guild_id` are context, not names.

/// `crates/bot/src/gateway.rs`: all fifteen traced in the file are cataloged.
const GATEWAY_RS_EVENTS: [&str; 15] = [
    "cold resume committed; requesting voice snapshot via identify",
    "gateway reconnect failed; Twilight will retry",
    "gateway shard loop started",
    "gateway leveling dispatch failed",
    "gateway ready; checkpoint committed",
    "onboarding interaction interrupted; member must reselect",
    "invite counter read unavailable; retaining snapshot",
    "TWO_VOICE=1 but no discord token; voice rooms disabled",
    "TWO_VOICE=1 but no database URL; voice rooms disabled",
    "voice database unavailable; voice rooms disabled",
    "voice rooms enabled; gateway sink attached",
    "voice HTTP setup failed; voice rooms disabled",
    "interaction acknowledgement blocked; advancing past lost callback",
    "interaction response failed; not replaying command",
    "READY identity differs from boot token; ordered identity not armed",
];

/// `crates/bot/src/main.rs`: the seven cataloged `tracing` messages.
const MAIN_RS_TRACED_EVENTS: [&str; 7] = [
    "durable gateway initialized; shard connecting",
    "gateway prerequisites missing; gateway parked, /readyz reports down",
    "container service failed",
    "durable gateway failed; checkpoint unchanged, readiness unavailable",
    "shutdown_deadline_exceeded: abandoning in-flight work",
    "feature gates invalid; ordered interaction surface parked",
    "moderation gates invalid; ordered interaction surface parked",
];

/// `crates/bot/src/main.rs`: cataloged drain outcomes that surface as
/// `std::io::Error` text from `supervise_gateway_bounded`, not as `tracing`
/// messages, so they are presence-checked but excluded from the traced scan.
const MAIN_RS_ERROR_STRINGS: [&str; 2] = [
    "gateway task stopped; container restart required",
    "gateway drain failed; restart required",
];

/// `crates/bot/src/server.rs`.
const SERVER_RS_EVENTS: [&str; 5] = [
    "listening",
    "SIGTERM received; draining",
    "SIGINT received; draining",
    "shutdown_completed",
    "response failed",
];

/// `crates/bot/src/shutdown.rs`.
const SHUTDOWN_RS_EVENTS: [&str; 2] = [
    "invalid shutdown timeout; using default",
    "second shutdown signal received; exiting immediately",
];

/// `crates/bot/src/jobs.rs`.
const JOBS_RS_EVENTS: [&str; 1] = ["periodic job failed"];

/// `crates/bot/src/voice_rooms.rs`: the five cataloged voice messages.
const VOICE_RS_CATALOG_MESSAGES: [&str; 5] = [
    "voice_operation succeeded",
    "voice_operation failed",
    "voice_reconcile planned",
    "voice action dead-lettered",
    "voice creator orphan needs manual deletion",
];

/// `voice_event` field values (`crates/bot/src/voice_rooms.rs`), the bounded
/// voice vocabulary that rides the gateway sink.
const VOICE_EVENT_VALUES: [&str; 4] = [
    "voice_operation",
    "voice_reconcile",
    "voice_dead_letter",
    "voice_creator_orphan",
];

/// Metric event labels, in `two_bot_core::metrics::EVENTS` order. Unknown
/// dispatch types collapse to `other`; scrapers match these spellings.
const METRIC_EVENT_LABELS: [&str; 18] = [
    "READY",
    "RESUMED",
    "GUILD_CREATE",
    "GUILD_DELETE",
    "GUILD_UPDATE",
    "GUILD_MEMBER_ADD",
    "GUILD_MEMBER_REMOVE",
    "GUILD_MEMBER_UPDATE",
    "MESSAGE_CREATE",
    "MESSAGE_UPDATE",
    "MESSAGE_DELETE",
    "VOICE_STATE_UPDATE",
    "INVITE_CREATE",
    "INVITE_DELETE",
    "INTERACTION_CREATE",
    "HEARTBEAT_ACK",
    "GATEWAY_CLOSE",
    "other",
];

/// Gateway failure classes (`gateway_failure::FailureClass::as_str`), served
/// on `/readyz` as `{"phase":"durable_gateway","class":"…"}`.
const FAILURE_CLASSES: [&str; 12] = [
    "store_unavailable",
    "gateway_pool_connect_failed",
    "checkpoint_load_failed",
    "onboarding_gates_invalid",
    "onboarding_init_failed",
    "custom_commands_init_failed",
    "milestones_load_failed",
    "automod_config_invalid",
    "automod_executor_failed",
    "raid_executor_failed",
    "gateway_runtime_failed",
    "gateway_task_panicked",
];

/// Startup `error_class` values logged before fatal exits (see
/// `docs/startup-diagnostics.md`): inline literals in `main.rs`, so pinned by
/// presence, not by an enum.
const STARTUP_ERROR_CLASSES: [&str; 8] = [
    "listener_bind_failed",
    "gateway_override_invalid",
    "database_connect_failed",
    "receiver_config_invalid",
    "receiver_prerequisites_invalid",
    "receiver_bind_failed",
    "moderation_disable_unknown",
    "container_service_failed",
];

// Adjacent emissions: real log lines in the scanned files that belong to a
// neighboring surface (startup/boot wiring, voice command UX), not to the
// gateway session catalog. Listed explicitly so a brand-new message still
// fails closed instead of blending in; each group names its owning area.

// `crates/bot/src/main.rs`: boot/startup diagnostics owned by
// `docs/startup-diagnostics.md`.
const MAIN_RS_ADJACENT: [&str; 13] = [
    "internal-action startup refused",
    "config invalid; continuing with safe defaults",
    "container listener failed",
    "DISCORD_GATEWAY_URL must be valid UTF-8",
    "DISCORD_GATEWAY_URL must be a loopback mock websocket address",
    "boot command registry synchronization failed",
    "database initialization failed; exiting for supervisor restart",
    "moderation_disable_refused: boot refused with moderation/automation disabled while releases are owed; complete or cancel them, or set TWO_ALLOW_OWED_RELEASES=1 to override",
    "moderation_disable_override: booting with moderation/automation disabled while releases are owed; members may stay banned and channels locked",
    "moderation_disable_refused: owed-release state unreadable while moderation/automation is disabled; refusing boot",
    "command runtime parked; onboarding runtime disabled",
    "automod configuration rejected",
    "automod activation wired into the gateway loop",
];

// `crates/bot/src/voice_rooms.rs`: per-command voice UX receipts, not gateway
// session events.
const VOICE_RS_ADJACENT: [&str; 10] = [
    "voice succession refused",
    "voice notice settings unreadable",
    "voice notice had no working destination",
    "voice create reservation settle failed",
    "import preview planned an apply; refusing without writing",
    "voice vote response failed; not retried",
    "voice ballot response failed; not retried",
    "voice name modal response failed; not retried",
    "voice acknowledgement failed; command not executed",
    "voice response completion failed; not retried",
];

// Test-only emissions inside the scanned files' `#[cfg(test)]` modules. They
// never run in production, but `include_str!` sees the whole file, so they
// join the known set explicitly instead of failing the scan.
const TEST_ONLY_EMISSIONS: [&str; 1] = [
    // `server.rs` capture-subscriber self-check: proves the test recorder
    // observes events; asserts on the text, ships no behavior.
    "capture remains active",
];

/// The six catalog-scoped source files, read at compile time so the suite is
/// fully offline.
const SCOPED_SOURCES: [(&str, &str); 6] = [
    ("gateway.rs", include_str!("gateway.rs")),
    ("main.rs", include_str!("main.rs")),
    ("server.rs", include_str!("server.rs")),
    ("shutdown.rs", include_str!("shutdown.rs")),
    ("jobs.rs", include_str!("jobs.rs")),
    ("voice_rooms.rs", include_str!("voice_rooms.rs")),
];

/// Macro names that emit a structured-log event. The message is the last
/// string literal inside the invocation; every current callsite keeps the
/// message last, so a callsite that puts a field after the message fails the
/// scan until the callsite (or this extractor) is updated.
const LOG_MACROS: [&str; 5] = ["info", "warn", "error", "debug", "trace"];

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// The log-macro name ending at `bang` (the index of `!`), or `None`.
/// Accepts both `warn!(…)` and `tracing::warn!(…)`; rejects longer
/// identifiers that merely end with a macro name (e.g. `my_warn!`).
fn macro_name_before(source: &str, bang: usize) -> Option<&'static str> {
    let prefix = &source[..bang];
    LOG_MACROS.into_iter().find(|name| {
        prefix.ends_with(*name)
            && (prefix.len() == name.len()
                || !is_name_byte(prefix.as_bytes()[prefix.len() - name.len() - 1]))
    })
}

/// Byte index of the `)` closing the paren opened at `open`, skipping
/// over string literals, character literals and `//` line comments so
/// parens inside them do not affect the depth.
fn closing_paren(source: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open;
    while i < source.len() {
        match source[i] {
            b'"' => {
                i += 1;
                while i < source.len() {
                    if source[i] == b'\\' {
                        i += 2;
                    } else if source[i] == b'"' {
                        break;
                    } else {
                        i += 1;
                    }
                }
            }
            b'\'' => {
                i += 1;
                while i < source.len() {
                    if source[i] == b'\\' {
                        i += 2;
                    } else if source[i] == b'\'' {
                        break;
                    } else {
                        i += 1;
                    }
                }
            }
            b'/' if i + 1 < source.len() && source[i + 1] == b'/' => {
                while i < source.len() && source[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// String literals inside `body`, without their quotes, in order. Handles the
/// standard `\"` escapes; a literal spanning lines would fail closed below.
fn string_literals(body: &str) -> Vec<&str> {
    let bytes = body.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            let mut j = i + 1;
            while j < bytes.len() {
                if bytes[j] == b'\\' {
                    j += 2;
                } else if bytes[j] == b'"' {
                    break;
                } else {
                    j += 1;
                }
            }
            if j < bytes.len() {
                out.push(&body[i + 1..j]);
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    out
}

/// Every `tracing` message literal in `source`, in file order: for each log
/// macro invocation, the last string literal is the event name. Panics on a
/// macro with no literal so a new emission style fails closed instead of
/// passing silently.
fn traced_messages(source: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut search_from = 0;
    while let Some(rel) = source[search_from..].find("!(") {
        let bang = search_from + rel;
        let found = macro_name_before(source, bang);
        // The byte after `!` is `(`, so the invocation opens at `bang + 1`.
        if found.is_some() {
            let open = bang + 1;
            if let Some(close) = closing_paren(source.as_bytes(), open) {
                let body = &source[open + 1..close];
                let literals = string_literals(body);
                match literals.last() {
                    Some(message) => out.push(*message),
                    None => panic!(
                        "log macro without a message literal fails closed; body starts: {}",
                        body.chars().take(80).collect::<String>()
                    ),
                }
                search_from = close + 1;
                continue;
            }
        }
        search_from = bang + 2;
    }
    out
}

/// `voice_event = "…"` field values in `voice_rooms.rs`, in file order.
fn voice_event_values(source: &str) -> Vec<&str> {
    const MARKER: &str = "voice_event = \"";
    let mut out = Vec::new();
    let mut search_from = 0;
    while let Some(rel) = source[search_from..].find(MARKER) {
        let start = search_from + rel + MARKER.len();
        let rest = &source[start..];
        match rest.find('"') {
            Some(end) => {
                out.push(&rest[..end]);
                search_from = start + end + 1;
            }
            None => panic!("unterminated voice_event value fails closed"),
        }
    }
    out
}

fn sorted_unique(items: Vec<&str>) -> Vec<&str> {
    let set: BTreeSet<&str> = items.into_iter().collect();
    set.into_iter().collect()
}

/// Every cataloged event appears in the file the catalog attributes it to,
/// so a moved or deleted emission is caught with its documented callsite.
#[test]
fn catalog_entries_appear_at_their_documented_files() {
    let by_name: std::collections::HashMap<&str, &str> = SCOPED_SOURCES.iter().copied().collect();
    let source = |file: &str| -> &str {
        by_name
            .get(file)
            .copied()
            .unwrap_or_else(|| panic!("no source for {file}"))
    };
    let expectations: [(&str, &[&str]); 6] = [
        ("gateway.rs", &GATEWAY_RS_EVENTS),
        ("main.rs", &MAIN_RS_TRACED_EVENTS),
        ("server.rs", &SERVER_RS_EVENTS),
        ("shutdown.rs", &SHUTDOWN_RS_EVENTS),
        ("jobs.rs", &JOBS_RS_EVENTS),
        ("voice_rooms.rs", &VOICE_RS_CATALOG_MESSAGES),
    ];
    for (file, events) in expectations {
        let text = source(file);
        for event in events {
            assert!(
                text.contains(event),
                "catalog event missing from {file}: {event}"
            );
        }
    }
    let main = source("main.rs");
    for error in MAIN_RS_ERROR_STRINGS {
        assert!(
            main.contains(error),
            "catalog drain outcome missing from main.rs: {error}"
        );
    }
    for class in STARTUP_ERROR_CLASSES {
        assert!(
            main.contains(class),
            "startup error class missing from main.rs: {class}"
        );
    }
}

#[test]
fn emitted_event_names_match_the_catalog_unknowns_fail() {
    let mut scanned = Vec::new();
    for (_, text) in SCOPED_SOURCES {
        scanned.extend(traced_messages(text));
    }
    let mut known = Vec::new();
    known.extend(GATEWAY_RS_EVENTS);
    known.extend(MAIN_RS_TRACED_EVENTS);
    known.extend(SERVER_RS_EVENTS);
    known.extend(SHUTDOWN_RS_EVENTS);
    known.extend(JOBS_RS_EVENTS);
    known.extend(VOICE_RS_CATALOG_MESSAGES);
    known.extend(MAIN_RS_ADJACENT);
    known.extend(VOICE_RS_ADJACENT);
    known.extend(TEST_ONLY_EMISSIONS);
    assert_eq!(
        sorted_unique(scanned),
        sorted_unique(known),
        "unknown or stale structured-log event: an emitted message is not in the catalog, or a cataloged message is no longer emitted. \
         Added a log line: record it in docs/observability-event-catalog.md and in the matching const here \
         (gateway-path events join the per-file catalog consts, others join the adjacent consts with their owning doc). \
         Removed or renamed one: update the catalog and the consts together."
    );
}

#[test]
fn metric_event_labels_match_the_catalog() {
    assert_eq!(
        two_bot_core::metrics::EVENTS,
        &METRIC_EVENT_LABELS[..],
        "metrics::EVENTS drifted from the catalog labels; update both together"
    );
}

#[test]
fn failure_classes_match_the_catalog() {
    let actual: Vec<&str> = crate::gateway_failure::FailureClass::ALL
        .iter()
        .map(|class| class.as_str())
        .collect();
    assert_eq!(
        actual, FAILURE_CLASSES,
        "FailureClass variants drifted from the catalog classes; update both together"
    );
}

#[test]
fn voice_event_values_are_bounded_by_the_catalog() {
    let voice = SCOPED_SOURCES
        .iter()
        .find_map(|(file, text)| (*file == "voice_rooms.rs").then_some(*text))
        .expect("voice_rooms.rs is a scoped source");
    assert_eq!(
        sorted_unique(voice_event_values(voice)),
        sorted_unique(VOICE_EVENT_VALUES.to_vec()),
        "new voice_event value fails closed: record it in docs/observability-event-catalog.md and VOICE_EVENT_VALUES"
    );
}
