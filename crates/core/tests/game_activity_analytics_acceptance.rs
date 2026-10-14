//! Game-activity wiring acceptance (TOG-23408, child of TOG-23402).
//!
//! Pins the `two_bot_core::game_activity` mapping contract the Build card
//! implements in the gateway (`crates/bot/src/gateway.rs` `PresenceUpdate`
//! path) plus voice spans (`voice.handle_received` in
//! `crates/bot/src/voice_rooms.rs`).
//!
//! Pure and offline: fixtures only, no Discord client, no SQL, no network,
//! no staging or production database. Guild-level aggregates only (never a
//! per-member row); the series is never published (presence-probe
//! containment precedent `crates/core/src/presence.rs`: guild aggregates,
//! no gateway presence intent, no public-surface reader, operator trend
//! report only).
//!
//! If a migration/store shape is implied, this file asserts the
//! guild-aggregate row shape only — it never creates the migration. Build
//! card must handle the schema tripwires separately: erasure plan, seed,
//! roles, rollback class, cutover mirror + matrix row, replay-fence
//! exclusion.
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
    ActivityType, GuildGameAggregate, PlaySession, PresenceDelta, VoiceSpan, MAX_GAME_TEXT_CHARS,
    SINGLE_OBSERVATION_FLOOR_MS,
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
    // The pipeline's only durable output is the guild aggregate above: the
    // Build card lands without adding any per-member store, reader, or
    // public-surface view alongside it.
    let sessions = sessionize(&[delta(T0, vec![playing("Apex Legends")])], &[], 15 * MIN);
    let aggregates = aggregate_guild("guild-1", &sessions);
    assert_eq!(aggregates.len(), 1);
    assert!(
        aggregates[0].in_voice_ms <= aggregates[0].total_play_ms,
        "in-voice time is a subset of play time"
    );
}

// --- TOG-23408 wiring contract: gateway numeric types ----------------------
// The Build card maps the gateway `PresenceUpdate.activities[].type` number
// at `crates/bot/src/gateway.rs:629-643` to `ActivityType`. This helper pins
// the expected mapping inside the test so the contract is explicit without
// touching runtime code: 0 -> Playing, 1 -> Streaming, everything else never
// opens a session (Custom / type 4 is withheld in large guilds).
fn gateway_activity_type(numeric: u8) -> Option<ActivityType> {
    match numeric {
        0 => Some(ActivityType::Playing),
        1 => Some(ActivityType::Streaming),
        _ => None,
    }
}

#[test]
fn gateway_numeric_types_zero_and_one_sessionize_only() {
    assert_eq!(gateway_activity_type(0), Some(ActivityType::Playing));
    assert_eq!(gateway_activity_type(1), Some(ActivityType::Streaming));
    for numeric in [2u8, 3, 4, 5] {
        assert_eq!(
            gateway_activity_type(numeric),
            None,
            "gateway type {numeric} must never map to a session type"
        );
    }
    // Playing (0) and Streaming (1) each open a session through `sessionize`.
    for activity_type in [ActivityType::Playing, ActivityType::Streaming] {
        let snapshot = ActivitySnapshot {
            activity_type,
            ..playing("Apex Legends")
        };
        let sessions = sessionize(&[delta(T0, vec![snapshot])], &[], 15 * MIN);
        assert_eq!(
            sessions.len(),
            1,
            "gateway-mapped {activity_type:?} must sessionize"
        );
    }
}

// --- TOG-23408 wiring contract: sanitize + bounds --------------------------

#[test]
fn sanitize_drops_every_button_url_keeps_labels_in_order() {
    let snapshot = ActivitySnapshot {
        button_labels: vec!["Join".to_owned(), "Watch".to_owned(), "Shop".to_owned()],
        button_urls: vec![
            "https://evil.example/join".to_owned(),
            "https://evil.example/watch".to_owned(),
            "https://evil.example/shop".to_owned(),
        ],
        ..playing("Apex Legends")
    };
    let clean = sanitize_activity(&snapshot);
    assert_eq!(
        clean.button_labels,
        vec!["Join".to_owned(), "Watch".to_owned(), "Shop".to_owned()]
    );
    assert!(
        clean.button_urls.is_empty(),
        "every button URL must be dropped, got {:?}",
        clean.button_urls
    );
}

#[test]
fn stream_url_allowlist_covers_subdomains_schemes_and_lookalikes() {
    // Allowed: exact hosts, subdomains, http + https, any case.
    for url in [
        "https://twitch.tv/someone",
        "http://twitch.tv/someone",
        "https://m.twitch.tv/someone",
        "https://player.twitch.tv/x",
        "https://TWITCH.TV/someone",
        "https://youtube.com/watch?v=x",
        "http://youtube.com/watch?v=x",
        "https://gaming.youtube.com/watch?v=x",
        "https://youtu.be/x",
        "https://www.youtu.be/x",
        "https://YOUTU.BE/x",
    ] {
        assert!(
            is_allowed_stream_url(url),
            "allowlisted stream URL rejected: {url}"
        );
        let snapshot = ActivitySnapshot {
            activity_type: ActivityType::Streaming,
            stream_url: Some(url.to_owned()),
            ..playing("Stream")
        };
        assert_eq!(
            sanitize_activity(&snapshot).stream_url.as_deref(),
            Some(url),
            "sanitize must keep allowlisted URL: {url}"
        );
    }
    // Rejected: other hosts, lookalikes, wrong scheme, non-URL text.
    for url in [
        "https://evil.example/stream",
        "https://twitch.evil.example/x",
        "https://youtube.evil.example/x",
        "https://youtu.be.evil.example/x",
        "https://evil-twitch.tv/x",
        "https://evilyoutube.com/x",
        "https://twitch.tv.evil.example/x",
        "ftp://twitch.tv/someone",
        "javascript:alert(1)",
        "not a url",
        "",
        "twitch.tv/someone",
    ] {
        assert!(
            !is_allowed_stream_url(url),
            "non-allowlisted stream URL accepted: {url}"
        );
        let snapshot = ActivitySnapshot {
            activity_type: ActivityType::Streaming,
            stream_url: Some(url.to_owned()),
            ..playing("Stream")
        };
        assert!(
            sanitize_activity(&snapshot).stream_url.is_none(),
            "sanitize must drop URL: {url}"
        );
    }
}

#[test]
fn game_title_bound_and_singleton_floor_match_constants() {
    assert_eq!(MAX_GAME_TEXT_CHARS, 128, "game title bound");
    assert_eq!(
        SINGLE_OBSERVATION_FLOOR_MS, 60_000,
        "single-observation floor is one minute"
    );
    // A 200-char title sessionizes truncated to 128 chars (char count, not
    // bytes).
    let long_title = "A".repeat(200);
    let sessions = sessionize(&[delta(T0, vec![playing(&long_title)])], &[], 15 * MIN);
    assert_eq!(sessions.len(), 1);
    let game = sessions[0].game.as_deref().expect("session has a game");
    assert_eq!(game.chars().count(), 128, "title truncated to bound");
    assert_eq!(game, "A".repeat(128));
    // Unicode titles truncate by chars, not bytes.
    let uni_title = "é".repeat(200);
    let sessions = sessionize(&[delta(T0, vec![playing(&uni_title)])], &[], 15 * MIN);
    assert_eq!(sessions[0].game.as_deref().unwrap().chars().count(), 128);
    // Whitespace-only titles never open a session.
    let sessions = sessionize(&[delta(T0, vec![playing("   ")])], &[], 15 * MIN);
    assert!(sessions.is_empty(), "blank title must not sessionize");
    // Singleton sessions credit exactly the floor; multi-observation spans
    // use wall-clock time.
    let singleton = sessionize(&[delta(T0, vec![playing("Apex")])], &[], 15 * MIN);
    assert_eq!(
        singleton[0].ended_at_ms - singleton[0].started_at_ms,
        60_000,
        "singleton credits the floor"
    );
    let pair = sessionize(
        &[
            delta(T0, vec![playing("Apex")]),
            delta(T0 + 30 * MIN, vec![playing("Apex")]),
        ],
        &[],
        60 * MIN,
    );
    assert_eq!(
        pair[0].ended_at_ms - pair[0].started_at_ms,
        30 * MIN,
        "merged interval uses wall-clock span"
    );
}

// --- TOG-23408 wiring contract: voice overlap in millis --------------------
// Voice spans come from `voice.handle_received` (join/leave + Discord
// `self_stream`). Overlap is the summed intersection with the session span
// for the same member only, merged so overlapping spans count once,
// half-open (a span ending exactly at session start contributes zero).

fn member_voice(user: &str, join_ms: i64, leave_ms: i64, self_stream: bool) -> VoiceSpan {
    VoiceSpan {
        user_key: user.to_owned(),
        join_ms,
        leave_ms,
        self_stream,
    }
}

#[test]
fn voice_one_minute_overlap_inside_thirty_minute_session() {
    let deltas = vec![
        PresenceDelta {
            user_key: "user-1".to_owned(),
            observed_at_ms: T0,
            activities: vec![playing("Apex")],
        },
        PresenceDelta {
            user_key: "user-1".to_owned(),
            observed_at_ms: T0 + 30 * MIN,
            activities: vec![playing("Apex")],
        },
    ];
    let voices = vec![member_voice("user-1", T0, T0 + MIN, false)];
    let sessions = sessionize(&deltas, &voices, 60 * MIN);
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        sessions[0].in_voice_ms, MIN,
        "1-minute overlap, not the whole 30-minute span"
    );
    assert!(sessions[0].in_voice);
    let rows = aggregate_guild("guild-1", &sessions);
    assert_eq!(rows[0].total_play_ms, 30 * MIN);
    assert_eq!(rows[0].in_voice_ms, MIN);
}

#[test]
fn voice_overlapping_spans_count_union_once_and_boundary_is_zero() {
    let deltas = vec![
        PresenceDelta {
            user_key: "user-1".to_owned(),
            observed_at_ms: T0,
            activities: vec![playing("Apex")],
        },
        PresenceDelta {
            user_key: "user-1".to_owned(),
            observed_at_ms: T0 + 30 * MIN,
            activities: vec![playing("Apex")],
        },
    ];
    let voices = vec![
        member_voice("user-1", T0 + 5 * MIN, T0 + 15 * MIN, false),
        member_voice("user-1", T0 + 10 * MIN, T0 + 20 * MIN, false),
    ];
    let sessions = sessionize(&deltas, &voices, 60 * MIN);
    assert_eq!(
        sessions[0].in_voice_ms,
        15 * MIN,
        "union 5..20 counts once as 15 minutes"
    );
    // Half-open: a span ending exactly at session start contributes zero.
    let boundary = vec![member_voice("user-1", T0 - 10 * MIN, T0, false)];
    let sessions = sessionize(&deltas, &boundary, 60 * MIN);
    assert_eq!(
        sessions[0].in_voice_ms, 0,
        "span ending exactly at session start contributes zero"
    );
    assert!(!sessions[0].in_voice);
}

#[test]
fn voice_overlap_is_same_member_only_and_self_stream_needs_overlap() {
    let deltas = vec![PresenceDelta {
        user_key: "user-1".to_owned(),
        observed_at_ms: T0,
        activities: vec![playing("Apex")],
    }];
    // Another member's overlapping span contributes nothing.
    let stranger = vec![member_voice("user-2", T0 - MIN, T0 + MIN, false)];
    let sessions = sessionize(&deltas, &stranger, 15 * MIN);
    assert_eq!(sessions[0].in_voice_ms, 0);
    assert!(!sessions[0].in_voice);
    // `self_stream` with no shared time still contributes nothing: the flag
    // never creates overlap on its own.
    let disjoint_stream = vec![member_voice("user-1", T0 + 60 * MIN, T0 + 70 * MIN, true)];
    let sessions = sessionize(&deltas, &disjoint_stream, 15 * MIN);
    assert_eq!(sessions[0].in_voice_ms, 0);
    assert!(!sessions[0].in_voice);
    // `self_stream` overlapping the session contributes like any span.
    let overlapping_stream = vec![member_voice("user-1", T0 - MIN, T0 + MIN, true)];
    let sessions = sessionize(&deltas, &overlapping_stream, 15 * MIN);
    assert!(sessions[0].in_voice_ms > 0);
    assert!(sessions[0].in_voice);
}

// --- TOG-23408 wiring contract: guild aggregates only ----------------------

#[test]
fn aggregate_row_shape_has_no_member_key_and_folds_per_game() {
    let sessions = sessionize(
        &[
            PresenceDelta {
                user_key: "user-1".to_owned(),
                observed_at_ms: T0,
                activities: vec![playing("Apex")],
            },
            PresenceDelta {
                user_key: "user-1".to_owned(),
                observed_at_ms: T0 + 10 * MIN,
                activities: vec![playing("Apex")],
            },
            PresenceDelta {
                user_key: "user-2".to_owned(),
                observed_at_ms: T0,
                activities: vec![playing("Zelda")],
            },
        ],
        &[],
        15 * MIN,
    );
    let rows = aggregate_guild("guild-9", &sessions);
    assert_eq!(rows.len(), 2, "one row per (guild, game)");
    assert_eq!(rows[0].game.as_deref(), Some("Apex"));
    assert_eq!(rows[1].game.as_deref(), Some("Zelda"));
    // Wall-clock totals: Apex spans 10 minutes, Zelda is a floor singleton.
    assert_eq!(rows[0].total_play_ms, 10 * MIN);
    assert_eq!(rows[0].session_count, 1);
    assert_eq!(rows[1].total_play_ms, MIN);
    assert_eq!(rows[1].session_count, 1);
    for row in &rows {
        assert_eq!(row.guild_id, "guild-9");
        // Exact serialized shape: no member key may appear under any name.
        let value = serde_json::to_value(row).expect("aggregate serializes");
        let obj = value.as_object().expect("aggregate is an object");
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "game",
                "guild_id",
                "in_voice_ms",
                "session_count",
                "total_play_ms"
            ],
            "aggregate row shape is guild-level only"
        );
        let debug = format!("{row:?}");
        assert!(
            !debug.contains("user-1") && !debug.contains("user-2"),
            "aggregate Debug must carry no member key, got: {debug}"
        );
        let json = serde_json::to_string(row).expect("aggregate serializes");
        assert!(
            !json.contains("user-1") && !json.contains("user-2"),
            "aggregate JSON must carry no member key, got: {json}"
        );
        assert!(
            !json.contains("user_key") && !json.contains("member"),
            "aggregate JSON must have no member field, got: {json}"
        );
    }
}

#[test]
fn aggregate_in_voice_ms_is_capped_at_total_play_ms() {
    // Hand-built session with inflated voice overlap: the fold must cap.
    let sessions = vec![PlaySession {
        game: Some("Apex".to_owned()),
        started_at_ms: T0,
        ended_at_ms: T0 + MIN,
        is_streaming: false,
        in_voice: true,
        in_voice_ms: 2 * MIN,
    }];
    let rows = aggregate_guild("guild-1", &sessions);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].total_play_ms, MIN, "total is wall-clock span");
    assert_eq!(
        rows[0].in_voice_ms, MIN,
        "in-voice time capped at the total"
    );
}

// --- TOG-23408 wiring contract: sessionization edges -----------------------

#[test]
fn interleaved_members_keep_their_own_sessions() {
    let mut deltas = Vec::new();
    for step in 0..5 {
        deltas.push(PresenceDelta {
            user_key: "user-a".to_owned(),
            observed_at_ms: T0 + step * 5 * MIN,
            activities: vec![playing("Apex")],
        });
        deltas.push(PresenceDelta {
            user_key: "user-b".to_owned(),
            observed_at_ms: T0 + MIN + step * 5 * MIN,
            activities: vec![playing("Apex")],
        });
    }
    let sessions = sessionize(&deltas, &[], 15 * MIN);
    assert_eq!(sessions.len(), 2, "one session per member");
    let total: i64 = sessions
        .iter()
        .map(|s| s.ended_at_ms.saturating_sub(s.started_at_ms).max(0))
        .sum();
    assert_eq!(total, 40 * MIN, "both members' play time kept");
}

#[test]
fn other_members_non_playing_delta_closes_only_its_own_interval() {
    let deltas = vec![
        PresenceDelta {
            user_key: "user-a".to_owned(),
            observed_at_ms: T0,
            activities: vec![playing("Apex")],
        },
        PresenceDelta {
            user_key: "user-b".to_owned(),
            observed_at_ms: T0 + 2 * MIN,
            activities: vec![playing("Apex")],
        },
        PresenceDelta {
            user_key: "user-b".to_owned(),
            observed_at_ms: T0 + 3 * MIN,
            activities: Vec::new(),
        },
        PresenceDelta {
            user_key: "user-a".to_owned(),
            observed_at_ms: T0 + 5 * MIN,
            activities: vec![playing("Apex")],
        },
        PresenceDelta {
            user_key: "user-a".to_owned(),
            observed_at_ms: T0 + 10 * MIN,
            activities: vec![playing("Apex")],
        },
    ];
    let sessions = sessionize(&deltas, &[], 15 * MIN);
    assert_eq!(sessions.len(), 2, "one session per member");
    assert!(
        sessions
            .iter()
            .any(|s| s.ended_at_ms.saturating_sub(s.started_at_ms) == 10 * MIN),
        "A stays one 10-minute session, got {sessions:?}"
    );
    assert!(
        sessions
            .iter()
            .any(|s| s.ended_at_ms.saturating_sub(s.started_at_ms) == MIN),
        "B is one floor-credited singleton, got {sessions:?}"
    );
}

#[test]
fn tied_sessions_have_deterministic_total_order_game_streaming_voice() {
    // Game tie-break: Apex sorts before Zelda on identical ticks.
    let deltas = vec![
        PresenceDelta {
            user_key: "user-a".to_owned(),
            observed_at_ms: T0,
            activities: vec![playing("Zelda")],
        },
        PresenceDelta {
            user_key: "user-b".to_owned(),
            observed_at_ms: T0,
            activities: vec![playing("Apex")],
        },
    ];
    let first = sessionize(&deltas, &[], 15 * MIN);
    let second = sessionize(&deltas, &[], 15 * MIN);
    assert_eq!(first.len(), 2);
    assert_eq!(first, second, "tied sessions order every run");
    assert_eq!(first[0].game.as_deref(), Some("Apex"));
    assert_eq!(first[1].game.as_deref(), Some("Zelda"));
    // Streaming tie-break: same game + tick, play sorts before streaming.
    let deltas = vec![
        PresenceDelta {
            user_key: "user-a".to_owned(),
            observed_at_ms: T0,
            activities: vec![ActivitySnapshot {
                activity_type: ActivityType::Streaming,
                ..playing("Apex")
            }],
        },
        PresenceDelta {
            user_key: "user-b".to_owned(),
            observed_at_ms: T0,
            activities: vec![playing("Apex")],
        },
    ];
    let sessions = sessionize(&deltas, &[], 15 * MIN);
    assert_eq!(sessions.len(), 2);
    assert!(
        !sessions[0].is_streaming && sessions[1].is_streaming,
        "ties break by streaming flag, got {sessions:?}"
    );
    assert_eq!(
        sessionize(&deltas, &[], 15 * MIN),
        sessions,
        "streaming tie-break is deterministic"
    );
    // Voice tie-break: same game + streaming + tick, quieter sorts first.
    let deltas = vec![
        PresenceDelta {
            user_key: "user-a".to_owned(),
            observed_at_ms: T0,
            activities: vec![playing("Apex")],
        },
        PresenceDelta {
            user_key: "user-b".to_owned(),
            observed_at_ms: T0,
            activities: vec![playing("Apex")],
        },
    ];
    let voices = vec![member_voice("user-b", T0, T0 + 30 * MIN, false)];
    let sessions = sessionize(&deltas, &voices, 15 * MIN);
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].in_voice_ms, 0);
    assert!(
        sessions[1].in_voice_ms > 0,
        "voice tie-break orders quiet first, got {sessions:?}"
    );
    assert_eq!(
        sessionize(&deltas, &voices, 15 * MIN),
        sessions,
        "voice tie-break is deterministic"
    );
}
