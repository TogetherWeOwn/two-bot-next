//! Game-activity analytics acceptance (test-first).
//!
//! Pins the planned `two_bot_core::game_activity` contract through the public
//! API only. This file is intentionally RED: the module does not exist yet,
//! so this test target fails to compile until the Build card lands it.
//! That compile failure IS the handoff signal.
//!
//! Pure and offline: fixtures only, no Discord client, no SQL, no network,
//! no staging or production database. Guild-level aggregates only (never a
//! per-member row); the series is never published (presence-probe
//! containment precedent: guild aggregates, no gateway presence intent,
//! no public-surface reader).
//!
//! Contract pinned here (Build card implements exactly this):
//! - Presence deltas carry `activities[]` with name + numeric type
//!   (Playing / Streaming / Listening / Watching / Custom / Competing),
//!   timestamps, `application_id`, details / state / party / assets.
//!   Buttons are label-only (no URL survives). Custom status never creates
//!   a session (withheld in large guilds). Streaming URLs survive only for
//!   Twitch / YouTube.
//! - Play intervals sessionize per user + game from those deltas: contiguous
//!   Playing deltas for the same game merge; a game change, a gap timeout,
//!   or a non-Playing activity closes the interval.
//! - Each session carries an in-voice overlap flag derived from voice
//!   sessions (join/leave spans plus Discord `self_stream`); streaming
//!   sessions are flagged as streaming.
//! - The only durable output is guild-level aggregates keyed by
//!   (guild, game): total play ms, in-voice ms, session counts. No
//!   per-member row, no per-member reader, no public-surface view.

use two_bot_core::game_activity::{
    aggregate_guild, is_allowed_stream_url, sanitize_activity, sessionize, ActivitySnapshot,
    ActivityType, GuildGameAggregate, PresenceDelta, VoiceSpan,
};

// Fixed clock so interval math is deterministic across runs.
const T0: i64 = 1_750_000_000_000;
const MIN: i64 = 60_000;

fn playing(name: &str) -> ActivitySnapshot {
    ActivitySnapshot {
        activity_type: ActivityType::Playing,
        name: name.to_owned(),
        application_id: Some("123".to_owned()),
        details: Some("Ranked".to_owned()),
        state: Some("In queue".to_owned()),
        party_size: Some([2, 3]),
        assets: true,
        button_labels: vec!["Join".to_owned()],
        button_urls: vec!["https://evil.example/join".to_owned()],
        stream_url: None,
    }
}

fn delta(at_ms: i64, activities: Vec<ActivitySnapshot>) -> PresenceDelta {
    PresenceDelta {
        user_key: "user-1".to_owned(),
        observed_at_ms: at_ms,
        activities,
    }
}

fn voice_span(join_ms: i64, leave_ms: i64) -> VoiceSpan {
    VoiceSpan {
        user_key: "user-1".to_owned(),
        join_ms,
        leave_ms,
        self_stream: false,
    }
}

#[test]
fn contiguous_playing_deltas_merge_into_one_interval() {
    let deltas = vec![
        delta(T0, vec![playing("Apex Legends")]),
        delta(T0 + 5 * MIN, vec![playing("Apex Legends")]),
        delta(T0 + 10 * MIN, vec![]),
    ];
    let sessions = sessionize(&deltas, &[voice_span(T0 - MIN, T0 + 20 * MIN)], 15 * MIN);
    assert_eq!(sessions.len(), 1, "one merged play interval");
    assert_eq!(sessions[0].game.as_deref(), Some("Apex Legends"));
    assert_eq!(sessions[0].started_at_ms, T0);
    assert_eq!(sessions[0].ended_at_ms, T0 + 5 * MIN);
    assert!(!sessions[0].is_streaming, "play is not streaming");
    assert!(
        sessions[0].in_voice,
        "interval inside the voice span overlaps"
    );
}

#[test]
fn game_change_or_gap_timeout_splits_intervals() {
    let deltas = vec![
        delta(T0, vec![playing("Apex Legends")]),
        delta(T0 + 5 * MIN, vec![playing("Minecraft")]),
        delta(T0 + 60 * MIN, vec![playing("Minecraft")]),
    ];
    let sessions = sessionize(&deltas, &[], 15 * MIN);
    assert_eq!(sessions.len(), 3, "game change splits; long gap splits");
    assert_eq!(sessions[0].game.as_deref(), Some("Apex Legends"));
    assert_eq!(sessions[1].game.as_deref(), Some("Minecraft"));
    assert_eq!(sessions[2].game.as_deref(), Some("Minecraft"));
}

#[test]
fn non_playing_activity_types_never_open_play_sessions() {
    for activity_type in [
        ActivityType::Listening,
        ActivityType::Watching,
        ActivityType::Competing,
        ActivityType::Custom,
    ] {
        let snapshot = ActivitySnapshot {
            activity_type,
            ..playing("Spotify")
        };
        let sessions = sessionize(&[delta(T0, vec![snapshot])], &[], 15 * MIN);
        assert!(
            sessions.is_empty(),
            "activity type {activity_type:?} must not sessionize"
        );
    }
}

#[test]
fn streaming_activity_flags_streaming_session_not_play() {
    let snapshot = ActivitySnapshot {
        activity_type: ActivityType::Streaming,
        stream_url: Some("https://twitch.tv/someone".to_owned()),
        ..playing("Twitch")
    };
    let sessions = sessionize(&[delta(T0, vec![snapshot])], &[], 15 * MIN);
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0].is_streaming, "streaming sessions are flagged");
}

#[test]
fn buttons_are_label_only_and_custom_never_sessions() {
    let snapshot = playing("Apex Legends");
    let clean = sanitize_activity(&snapshot);
    assert_eq!(clean.button_labels, vec!["Join".to_owned()]);
    assert!(
        clean.button_urls.is_empty(),
        "button URLs must never survive"
    );
    assert!(clean.assets, "assets survive sanitizing");
    let custom = ActivitySnapshot {
        activity_type: ActivityType::Custom,
        ..playing("Custom Status")
    };
    let sessions = sessionize(&[delta(T0, vec![custom])], &[], 15 * MIN);
    assert!(sessions.is_empty(), "Custom never creates a session");
}

#[test]
fn streaming_url_allowlist_is_twitch_and_youtube_only() {
    assert!(is_allowed_stream_url("https://twitch.tv/someone"));
    assert!(is_allowed_stream_url("https://www.youtube.com/watch?v=x"));
    assert!(is_allowed_stream_url("https://youtu.be/x"));
    assert!(!is_allowed_stream_url("https://evil.example/stream"));
    assert!(!is_allowed_stream_url("https://twitch.evil.example/x"));
    assert!(!is_allowed_stream_url("not a url"));
}

#[test]
fn in_voice_overlap_requires_shared_time_with_voice_span() {
    let deltas = vec![delta(T0, vec![playing("Apex Legends")])];
    let inside = sessionize(&deltas, &[voice_span(T0 - MIN, T0 + MIN)], 15 * MIN);
    assert!(inside[0].in_voice);
    let outside = sessionize(
        &deltas,
        &[voice_span(T0 + 60 * MIN, T0 + 70 * MIN)],
        15 * MIN,
    );
    assert!(
        !outside[0].in_voice,
        "disjoint voice span must not flag overlap"
    );
    let other_user = VoiceSpan {
        user_key: "user-2".to_owned(),
        ..voice_span(T0 - MIN, T0 + MIN)
    };
    let stranger = sessionize(&deltas, &[other_user], 15 * MIN);
    assert!(
        !stranger[0].in_voice,
        "another member's voice span must not flag overlap"
    );
}

#[test]
fn self_stream_voice_span_marks_streaming_overlap() {
    let snapshot = ActivitySnapshot {
        activity_type: ActivityType::Streaming,
        stream_url: Some("https://twitch.tv/someone".to_owned()),
        ..playing("Twitch")
    };
    let span = VoiceSpan {
        self_stream: true,
        ..voice_span(T0 - MIN, T0 + MIN)
    };
    let sessions = sessionize(&[delta(T0, vec![snapshot])], &[span], 15 * MIN);
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0].is_streaming);
    assert!(sessions[0].in_voice);
}

#[test]
fn aggregates_are_guild_level_only_never_per_member() {
    let deltas = vec![
        delta(T0, vec![playing("Apex Legends")]),
        delta(T0 + 5 * MIN, vec![playing("Apex Legends")]),
        PresenceDelta {
            user_key: "user-2".to_owned(),
            ..delta(T0, vec![playing("Apex Legends")])
        },
    ];
    let aggregates: Vec<GuildGameAggregate> =
        aggregate_guild("guild-1", &sessionize(&deltas, &[], 15 * MIN));
    assert_eq!(aggregates.len(), 1, "one row per (guild, game)");
    let row = &aggregates[0];
    assert_eq!(row.guild_id, "guild-1");
    assert_eq!(row.game.as_deref(), Some("Apex Legends"));
    assert!(row.total_play_ms > 0);
    // Per-user sessionization: user-1's two deltas merge into one interval
    // and user-2's delta is a singleton, so two sessions total.
    assert_eq!(row.session_count, 2);
    let debug = format!("{row:?}");
    assert!(
        !debug.contains("user-1") && !debug.contains("user-2"),
        "aggregate debug must carry no member key, got: {debug}"
    );
    let json = serde_json::to_string(row).expect("aggregate serializes");
    assert!(
        !json.contains("user-1") && !json.contains("user-2"),
        "aggregate JSON must carry no member key, got: {json}"
    );
}

#[test]
fn aggregates_never_publish_per_member_rows() {
    // The pipeline's only durable output is the guild aggregate above: this
    // test fails until the Build card lands without adding any per-member
    // store, reader, or public-surface view alongside it.
    let sessions = sessionize(&[delta(T0, vec![playing("Apex Legends")])], &[], 15 * MIN);
    let aggregates = aggregate_guild("guild-1", &sessions);
    assert_eq!(aggregates.len(), 1);
    assert!(
        aggregates[0].in_voice_ms <= aggregates[0].total_play_ms,
        "in-voice time is a subset of play time"
    );
}
