//! Bounded rejection telemetry acceptance (TOG-12018, threat-model F4).
//!
//! Pure: no database, no network. The receiver wiring lives with TOG-10603;
//! this pins the core contract: exhaustive classification, scalar records,
//! bounded suppression, and no marker escape under hostile input.

use two_bot_core::internal_actions::{ErrorCode, IMPLEMENTED_ACTIONS, MAX_BODY_BYTES};
use two_bot_core::rejection_telemetry::{
    ActionLabel, KeyLabel, RecordKind, Rejection, RejectionClass, RejectionRecord,
    RejectionTelemetry, TelemetryConfig, DEFAULT_MAX_TRACKED, DEFAULT_SAMPLES_PER_WINDOW,
    DEFAULT_WINDOW_MS,
};

fn configured(id: &str) -> KeyLabel {
    KeyLabel::new(id, true)
}

fn known(action: &'static str) -> ActionLabel {
    ActionLabel::new(Some(action))
}

fn telemetry() -> RejectionTelemetry {
    RejectionTelemetry::new(TelemetryConfig::default())
}

fn record_all(
    tele: &mut RejectionTelemetry,
    rejection: Rejection,
    now_ms: u64,
) -> Vec<RejectionRecord> {
    tele.record(rejection, now_ms)
}

/// Every `ErrorCode` maps to its class; the two codes that split on labels
/// cover every label. Adding a variant to `ErrorCode` without extending this
/// table is the review-time tripwire; the compile-time one is the wildcard-free
/// match in `RejectionClass::classify`.
#[test]
fn every_error_code_has_a_class() {
    let cases: Vec<(ErrorCode, KeyLabel, ActionLabel, RejectionClass)> = vec![
        (
            ErrorCode::Unauthorized,
            configured("web"),
            ActionLabel::Unknown,
            RejectionClass::AuthFailure,
        ),
        (
            ErrorCode::Unauthorized,
            KeyLabel::Unknown,
            ActionLabel::Unknown,
            RejectionClass::UnknownKey,
        ),
        (
            ErrorCode::Unauthorized,
            KeyLabel::Invalid,
            ActionLabel::Unknown,
            RejectionClass::UnknownKey,
        ),
        (
            ErrorCode::Unauthorized,
            KeyLabel::Other,
            ActionLabel::Unknown,
            RejectionClass::UnknownKey,
        ),
        (
            ErrorCode::StaleRequest,
            configured("web"),
            ActionLabel::Unknown,
            RejectionClass::ClockSkew,
        ),
        (
            ErrorCode::Replayed,
            configured("web"),
            ActionLabel::Unknown,
            RejectionClass::NonceReplay,
        ),
        (
            ErrorCode::RateLimited,
            configured("web"),
            known("role.assign"),
            RejectionClass::RateLimit,
        ),
        (
            ErrorCode::ActionNotAllowed,
            configured("web"),
            known("role.assign"),
            RejectionClass::ActionDisabled,
        ),
        (
            ErrorCode::ActionNotAllowed,
            configured("web"),
            ActionLabel::Unknown,
            RejectionClass::UnknownAction,
        ),
        (
            ErrorCode::ActionNotAllowed,
            configured("web"),
            ActionLabel::Other,
            RejectionClass::UnknownAction,
        ),
        (
            ErrorCode::Malformed,
            configured("web"),
            ActionLabel::Unknown,
            RejectionClass::MalformedBody,
        ),
        (
            ErrorCode::VersionConflict,
            configured("web"),
            known("settings.set"),
            RejectionClass::Conflict,
        ),
        (
            ErrorCode::InProgress,
            configured("web"),
            known("event.upsert"),
            RejectionClass::Conflict,
        ),
        (
            ErrorCode::DiscordRejected,
            configured("web"),
            known("moderation.ban"),
            RejectionClass::Upstream,
        ),
        (
            ErrorCode::DiscordUnavailable,
            configured("web"),
            known("moderation.ban"),
            RejectionClass::Upstream,
        ),
        (
            ErrorCode::UpstreamTimeout,
            configured("web"),
            known("moderation.ban"),
            RejectionClass::Upstream,
        ),
        (
            ErrorCode::Internal,
            configured("web"),
            ActionLabel::Unknown,
            RejectionClass::Internal,
        ),
    ];
    for (code, key, action, expected) in cases {
        let rejection = Rejection::new(code, key.clone(), action);
        assert_eq!(rejection.class(), expected, "code {code:?}");
        assert_eq!(rejection.key(), &key);
        assert_eq!(rejection.action(), action);
    }
}

#[test]
fn key_label_echoes_only_configured_ids() {
    assert_eq!(configured("web").as_str(), "web");
    assert_eq!(configured("bot-signer_2.0").as_str(), "bot-signer_2.0");
    // Valid shape but not in the ring: caller-chosen, never printed.
    assert_eq!(KeyLabel::new("attacker-key", false).as_str(), "unknown");
    assert_eq!(KeyLabel::Unknown.as_str(), "unknown");
    // No shape, no echo.
    assert_eq!(KeyLabel::new("", true).as_str(), "invalid");
    assert_eq!(KeyLabel::new("has space", true).as_str(), "invalid");
    assert_eq!(KeyLabel::new("semi;colon", true).as_str(), "invalid");
    assert_eq!(KeyLabel::new("sha256=abc", true).as_str(), "invalid");
    assert_eq!(KeyLabel::Invalid.as_str(), "invalid");
    assert_eq!(KeyLabel::Other.as_str(), "other");
    // 64 chars is the longest printable id; 65 is not an id.
    let long = "k".repeat(64);
    assert_eq!(KeyLabel::new(&long, true).as_str(), long);
    assert_eq!(KeyLabel::new(&"k".repeat(65), true).as_str(), "invalid");
}

#[test]
fn action_label_echoes_only_catalog_names() {
    // The card's 19-name catalog, pinned so a quiet addition is a loud diff.
    assert_eq!(IMPLEMENTED_ACTIONS.len(), 19);
    for name in IMPLEMENTED_ACTIONS {
        assert_eq!(ActionLabel::new(Some(name)), ActionLabel::Known(name));
        assert_eq!(ActionLabel::new(Some(name)).as_str(), name);
    }
    assert_eq!(ActionLabel::new(None), ActionLabel::Unknown);
    assert_eq!(ActionLabel::new(Some("")), ActionLabel::Unknown);
    assert_eq!(
        ActionLabel::new(Some("moderation.nuke")),
        ActionLabel::Unknown
    );
    assert_eq!(ActionLabel::Unknown.as_str(), "unknown");
    assert_eq!(ActionLabel::Other.as_str(), "other");
}

#[test]
fn from_body_extracts_only_catalog_actions() {
    let known = br#"{"action":"role.assign","guild_id":"123"}"#;
    assert_eq!(ActionLabel::from_body(known), known("role.assign"));
    for raw in [
        br#"{"action":"moderation.nuke"}"#.as_slice(),
        br#"{"action":42}"#,
        br#"{}"#,
        br#"["action"]"#,
        b"not json at all",
        b"",
    ] {
        assert_eq!(ActionLabel::from_body(raw), ActionLabel::Unknown);
    }
    // Oversized bodies are never parsed.
    let big = vec![b'x'; MAX_BODY_BYTES + 1];
    assert_eq!(ActionLabel::from_body(&big), ActionLabel::Unknown);
}

// ---------------------------------------------------------------------------
// Marker fixtures: hostile caller-controlled text that must never reach a
// record. Tokens, bodies, SQL and paths.
// ---------------------------------------------------------------------------

const TOKEN_MARKERS: &[&str] = &[
    "sha256=deadbeef0123456789abcdef0123456789abcdef0123456789abcdef",
    "Bearer secret-caller-token",
    "access_token",
    "TWO_INTERNAL_KEYS=web:super-secret-value-that-is-long-enough",
];

const BODY_MARKERS: &[&str] = &[
    r#"{"action":"role.assign","access_token":"oauth-secret"}"#,
    "<script>alert(1)</script>",
    "guild_id=123456789",
];

const SQL_MARKERS: &[&str] = &[
    "SELECT * FROM guilds",
    "'; DROP TABLE members; --",
    "pg_sleep(1)",
    "internal_actions",
];

const PATH_MARKERS: &[&str] = &["/internal/actions", "/guilds/123/members", "X-TWO-Key-Id"];

fn all_markers() -> Vec<&'static str> {
    TOKEN_MARKERS
        .iter()
        .chain(BODY_MARKERS)
        .chain(SQL_MARKERS)
        .chain(PATH_MARKERS)
        .copied()
        .collect()
}

/// Deterministic hostile-input generator (xorshift64, fixed seed): no new
/// dependencies, same sweep on every run.
struct Hostile {
    state: u64,
}

impl Hostile {
    fn new() -> Self {
        Self {
            state: 0x9E37_79B9_7F4A_7C15,
        }
    }

    fn next(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// A caller-controlled string: marker embedded in hostile padding, drawn
    /// from shapes that stress the label validators (spaces, `=`, `;`, quotes,
    /// overlong runs).
    fn string(&mut self, marker: &str) -> String {
        const PADS: &[&str] = &[" ", "=", ";", "\"", "'", ":", "\n", "x"];
        let pad = PADS[(self.next() as usize) % PADS.len()];
        let reps = 1 + (self.next() % 70) as usize;
        format!("{}{marker}{}", pad.repeat(reps), pad.repeat(reps >> 2))
    }
}

fn record_text(record: &RejectionRecord) -> String {
    format!("{record} {record:?}")
}

/// No token, body, SQL or path marker appears in any emitted record,
/// however hostile the caller-controlled input.
#[test]
fn markers_never_escape_into_records() {
    let markers = all_markers();
    let mut hostile = Hostile::new();
    let mut tele = telemetry();
    let mut checked = 0usize;

    for round in 0..2_000 {
        let marker = markers[round % markers.len()];
        let text = hostile.string(marker);
        let now_ms = (round / 500) as u64 * DEFAULT_WINDOW_MS;

        // Hostile key id through the real constructor: configured ids come
        // from the operator's ring, so only "web" is ever configured here.
        let key = KeyLabel::new(&text, text == "web");
        // Hostile bodies through the real extractor.
        let body = format!(r#"{{"action":"{text}","token":"{marker}"}}"#);
        let action = ActionLabel::from_body(body.as_bytes());
        let code = match round % 12 {
            0 => ErrorCode::Unauthorized,
            1 => ErrorCode::StaleRequest,
            2 => ErrorCode::Replayed,
            3 => ErrorCode::RateLimited,
            4 => ErrorCode::ActionNotAllowed,
            5 => ErrorCode::Malformed,
            6 => ErrorCode::VersionConflict,
            7 => ErrorCode::InProgress,
            8 => ErrorCode::DiscordRejected,
            9 => ErrorCode::DiscordUnavailable,
            10 => ErrorCode::UpstreamTimeout,
            _ => ErrorCode::Internal,
        };
        for out in record_all(&mut tele, Rejection::new(code, key, action), now_ms) {
            let text = record_text(&out);
            for marker in &markers {
                assert!(
                    !text.contains(marker),
                    "marker {marker:?} escaped into {text:?}"
                );
            }
            checked += 1;
        }
    }
    for out in tele.close_window() {
        let text = record_text(&out);
        for marker in &markers {
            assert!(
                !text.contains(marker),
                "marker {marker:?} escaped into summary {text:?}"
            );
        }
        checked += 1;
    }
    assert!(checked > 0, "sweep must emit at least one record");
}

/// A hostile key id is never echoed even when its shape is valid: only the
/// operator's ring membership earns print.
#[test]
fn valid_shaped_unknown_key_never_echoes() {
    let hostile_id = "attacker-controlled-id";
    assert!(two_bot_core::rejection_telemetry::valid_key_id_shape(
        hostile_id
    ));
    let rejection = Rejection::new(
        ErrorCode::Unauthorized,
        KeyLabel::new(hostile_id, false),
        ActionLabel::Unknown,
    );
    assert_eq!(rejection.class(), RejectionClass::UnknownKey);
    let mut tele = telemetry();
    let out = record_all(&mut tele, rejection, 0);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].key.as_str(), "unknown");
    assert!(!record_text(&out[0]).contains(hostile_id));
}

#[test]
fn suppression_emits_sample_then_one_summary() {
    let config = TelemetryConfig::new(60_000, 1, 8).expect("valid config");
    let mut tele = RejectionTelemetry::new(config);
    let rejection = || Rejection::new(ErrorCode::Replayed, configured("web"), ActionLabel::Unknown);

    let first = record_all(&mut tele, rejection(), 0);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].kind, RecordKind::Sample);
    assert_eq!(first[0].count, 1);
    assert_eq!(first[0].suppressed, 0);

    for _ in 0..9 {
        assert!(record_all(&mut tele, rejection(), 1).is_empty());
    }

    let summaries = tele.close_window();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].kind, RecordKind::Summary);
    assert_eq!(summaries[0].count, 10);
    assert_eq!(summaries[0].suppressed, 9);
    assert_eq!(tele.tracked_len(), 0);
}

#[test]
fn overflow_folds_into_other_with_bounded_memory() {
    let config = TelemetryConfig::new(60_000, 1, 2).expect("valid config");
    let mut tele = RejectionTelemetry::new(config);

    for (i, key) in ["a", "b", "c", "d"].iter().enumerate() {
        let out = record_all(
            &mut tele,
            Rejection::new(ErrorCode::Malformed, configured(key), ActionLabel::Unknown),
            i as u64,
        );
        // The first two buckets sample; the overflow bucket samples once,
        // then suppresses.
        if i < 3 {
            assert_eq!(out.len(), 1, "bucket {key} should sample");
            if i == 2 {
                assert_eq!(out[0].key, KeyLabel::Other);
                assert_eq!(out[0].action, ActionLabel::Other);
            }
        } else {
            assert!(out.is_empty(), "overflow sample spent on {key}");
        }
    }
    assert_eq!(tele.tracked_len(), 2);

    let mut summaries = tele.close_window();
    summaries.sort_by(|a, b| a.key.as_str().cmp(b.key.as_str()));
    let other: Vec<_> = summaries
        .iter()
        .filter(|r| r.key == KeyLabel::Other && r.action == ActionLabel::Other)
        .collect();
    assert_eq!(other.len(), 1, "one folded summary: {summaries:?}");
    assert_eq!(other[0].class, RejectionClass::MalformedBody);
    assert_eq!(other[0].count, 2);
    assert_eq!(other[0].suppressed, 1);
}

/// A flood of N rejections emits no more than the documented bound per
/// window, however many distinct keys and actions the flood uses.
#[test]
fn flood_stays_within_documented_bound() {
    let mut tele = telemetry();
    let bound = tele.max_records_per_window();
    // Defaults: (32 tracked + 11 overflow) * (1 sample + 1 summary) = 86.
    assert_eq!(
        bound,
        (DEFAULT_MAX_TRACKED as u64 + RejectionClass::COUNT as u64)
            * (u64::from(DEFAULT_SAMPLES_PER_WINDOW) + 1)
    );

    let mut hostile = Hostile::new();
    let mut emitted = 0usize;
    for round in 0..10_000 {
        let key = format!("flood-key-{}", hostile.next() % 500);
        let action = IMPLEMENTED_ACTIONS[(hostile.next() as usize) % IMPLEMENTED_ACTIONS.len()];
        let code = match round % 5 {
            0 => ErrorCode::Unauthorized,
            1 => ErrorCode::RateLimited,
            2 => ErrorCode::Replayed,
            3 => ErrorCode::ActionNotAllowed,
            _ => ErrorCode::Malformed,
        };
        emitted += record_all(
            &mut tele,
            Rejection::new(code, KeyLabel::new(&key, false), known(action)),
            0,
        )
        .len();
    }
    emitted += tele.close_window().len();
    assert!(
        emitted as u64 <= bound,
        "flood emitted {emitted} records, bound is {bound}"
    );
}

/// The bound holds across windows: each window re-arms samples and closes
/// with its own summaries.
#[test]
fn bound_holds_across_windows() {
    let config = TelemetryConfig::new(1_000, 1, 4).expect("valid config");
    let mut tele = RejectionTelemetry::new(config);
    let bound = tele.max_records_per_window();

    let mut total = 0u64;
    for window in 0..5 {
        let now_ms = window * 1_000;
        let mut per_window = 0usize;
        for i in 0..200 {
            per_window += record_all(
                &mut tele,
                Rejection::new(
                    ErrorCode::Replayed,
                    configured("web"),
                    ActionLabel::new(Some(IMPLEMENTED_ACTIONS[i % 19])),
                ),
                now_ms,
            )
            .len();
        }
        // Rolling into the next window flushes this window's summaries.
        per_window += tele.flush(now_ms + 1_000).len();
        assert!(
            per_window as u64 <= bound,
            "window {window} emitted {per_window}, bound is {bound}"
        );
        total += per_window as u64;
    }
    assert!(total > 0);
}

#[test]
fn display_record_is_scalar() {
    let record = RejectionRecord {
        kind: RecordKind::Summary,
        class: RejectionClass::RateLimit,
        key: configured("web"),
        action: known("guild.add_member"),
        count: 61,
        suppressed: 60,
    };
    assert_eq!(
        record.to_string(),
        "kind=summary class=rate_limit key=web action=guild.add_member count=61 suppressed=60"
    );
}

#[test]
fn defaults_document_the_bound() {
    assert_eq!(DEFAULT_WINDOW_MS, 60_000);
    assert_eq!(DEFAULT_SAMPLES_PER_WINDOW, 1);
    assert_eq!(DEFAULT_MAX_TRACKED, 32);
}
