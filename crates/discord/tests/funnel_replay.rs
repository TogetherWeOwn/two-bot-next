//! S3 acceptance: mock-discord event replay yields the same funnel rows as
//! two-bot for covered events.
//!
//! Two layers:
//!
//! 1. `handlers_replay_matches_legacy_oracle` — drives the core
//!    [`FunnelHandlers`] directly with the oracle's pinned stamps (the same
//!    inputs the legacy adapter fed its handlers) and asserts all 17 oracle
//!    rows match exactly on `(event_type, member_id, occurred_at, source,
//!    metadata)` — plus idempotency keys.
//! 2. `pipeline_*` — drives twilight `Event`s through [`MemPipeline`] and
//!    asserts the event→input translation (attribution, gate-clear triggers,
//!    voice boundary detection, DM drop, reconnect drop, invite seeding).
//!
//! The oracle (`oracle-tog9808.ts` in the run scratch dir) drives the REAL
//! legacy handlers headlessly and dumps the `events` rows. Regenerate with:
//!   cd <two-bot clone> && TWO_TEST_DATABASE_URL=<scratch pg> \
//!     node --experimental-strip-types oracle-tog9808.ts 2>/dev/null
//! then paste the JSON array into `ORACLE_ROWS` (key order + spacing must be
//! canonical — the test re-canonicalizes both sides before comparing).

use std::str::FromStr;

use twilight_model::{
    channel::message::{MessageFlags, MessageType},
    channel::Message,
    gateway::{
        event::Event,
        payload::incoming::{
            MemberAdd, MemberRemove, MemberUpdate, MessageCreate, VoiceStateUpdate,
        },
    },
    guild::{Member, MemberFlags},
    id::Id,
    user::User,
    util::Timestamp,
    voice::VoiceState,
};
use two_bot_core::{
    ChannelClass, FunnelHandlers, GateClearedInput, InviteState, JoinInput, MemStore, MessageInput,
    StoredRow, VoiceInput, WEB_ONE_CLICK_SOURCE,
};
use two_bot_discord::{MemPipeline, NoClassification};

const GUILD: u64 = 100_000_000_000_000_001;
const A: u64 = 900_000_000_000_001_111;
const B: u64 = 900_000_000_000_002_222;
const C: u64 = 900_000_000_000_003_333;
const BOT: u64 = 900_000_000_000_000_099;
const CH_TEXT: u64 = 200_000_000_000_000_001;
const CH_VOICE_A: u64 = 300_000_000_000_000_001;
const CH_VOICE_B: u64 = 300_000_000_000_000_002;
const INVITER: u64 = 900_000_000_000_000_099;

fn stamp(s: &str) -> String {
    format!("2026-09-20T{s}.000Z")
}

fn ts(s: &str) -> Timestamp {
    Timestamp::from_str(&format!("2026-09-20T{s}.000+00:00")).expect("fixture stamp")
}

fn user(id: u64, bot: bool) -> User {
    User {
        accent_color: None,
        avatar: None,
        avatar_decoration: None,
        avatar_decoration_data: None,
        banner: None,
        bot,
        discriminator: 0,
        email: None,
        flags: None,
        global_name: None,
        id: Id::new(id),
        locale: None,
        mfa_enabled: None,
        name: "member".to_owned(),
        premium_type: None,
        primary_guild: None,
        public_flags: None,
        system: None,
        verified: None,
    }
}

fn member(user_id: u64, pending: bool, joined: &str) -> Member {
    Member {
        avatar: None,
        avatar_decoration_data: None,
        banner: None,
        communication_disabled_until: None,
        deaf: false,
        flags: MemberFlags::empty(),
        joined_at: Some(ts(joined)),
        mute: false,
        nick: None,
        pending,
        premium_since: None,
        roles: vec![],
        user: user(user_id, false),
    }
}

fn join_event(member_id: u64, pending: bool, joined: &str) -> Event {
    Event::MemberAdd(Box::new(MemberAdd {
        guild_id: Id::new(GUILD),
        member: member(member_id, pending, joined),
    }))
}

/// The oracle rows: (event_type, member_id, occurred_at, source, metadata).
/// Regenerated from the real legacy handlers — see header.
const ORACLE_ROWS: &[(&str, &str, &str, &str, &str)] = &[
    (
        "member_join",
        "900000000000001111",
        "2026-09-20T12:00:00.000Z",
        "invite:twodev01",
        "{\"inviterId\":\"900000000000000099\"}",
    ),
    (
        "gate_cleared",
        "900000000000001111",
        "2026-09-20T12:00:00.000Z",
        "gateway",
        "",
    ),
    (
        "first_message",
        "900000000000001111",
        "2026-09-20T12:01:00.000Z",
        "channel:200000000000000001",
        "",
    ),
    (
        "second_message",
        "900000000000001111",
        "2026-09-20T12:02:00.000Z",
        "channel:200000000000000001",
        "",
    ),
    (
        "third_message",
        "900000000000001111",
        "2026-09-20T12:03:00.000Z",
        "channel:200000000000000001",
        "",
    ),
    (
        "voice_session_start",
        "900000000000001111",
        "2026-09-20T12:10:00.000Z",
        "channel:300000000000000001",
        "",
    ),
    (
        "first_voice_session",
        "900000000000001111",
        "2026-09-20T12:10:00.000Z",
        "channel:300000000000000001",
        "",
    ),
    (
        "voice_session_end",
        "900000000000001111",
        "2026-09-20T12:20:00.000Z",
        "channel:300000000000000001",
        "{\"startKnown\":true,\"startedAt\":\"2026-09-20T12:10:00.000Z\",\"durationSeconds\":600}",
    ),
    (
        "voice_session_start",
        "900000000000001111",
        "2026-09-20T12:20:00.000Z",
        "channel:300000000000000002",
        "",
    ),
    (
        "voice_session_end",
        "900000000000001111",
        "2026-09-20T12:25:30.000Z",
        "channel:300000000000000002",
        "{\"startKnown\":true,\"startedAt\":\"2026-09-20T12:20:00.000Z\",\"durationSeconds\":330}",
    ),
    (
        "member_leave",
        "900000000000001111",
        "2026-09-20T13:00:00.000Z",
        "gateway",
        "",
    ),
    (
        "member_join",
        "900000000000002222",
        "2026-09-20T12:30:00.000Z",
        "unknown",
        "",
    ),
    (
        "gate_cleared",
        "900000000000002222",
        "2026-09-20T12:35:00.000Z",
        "gateway",
        "",
    ),
    (
        "first_message",
        "900000000000002222",
        "2026-09-20T12:36:00.000Z",
        "channel:200000000000000001",
        "",
    ),
    (
        "voice_session_end",
        "900000000000002222",
        "2026-09-20T12:40:00.000Z",
        "channel:300000000000000001",
        "{\"startKnown\":false,\"startedAt\":null,\"durationSeconds\":null}",
    ),
    (
        "member_join",
        "900000000000003333",
        "2026-09-20T12:45:00.000Z",
        "web:one_click",
        "",
    ),
    (
        "gate_cleared",
        "900000000000003333",
        "2026-09-20T12:45:00.000Z",
        "gateway",
        "",
    ),
];

/// Canonical projection: (type, member, at, source, metadata value, raw
/// metadata string, key). Values compare order-insensitively (JSON objects
/// are unordered); raw strings compare byte-for-byte against the oracle
/// (Postgres TEXT comparison is byte-wise).
#[allow(clippy::type_complexity)]
fn canonical(
    rows: &[StoredRow],
) -> Vec<(
    String,
    String,
    String,
    String,
    serde_json::Value,
    String,
    String,
)> {
    rows.iter()
        .map(|r| {
            (
                r.event_type.as_str().to_owned(),
                r.member_id.map(|m| m.to_string()).unwrap_or_default(),
                r.occurred_at.clone(),
                r.source.clone(),
                r.metadata.clone().unwrap_or(serde_json::Value::Null),
                r.metadata
                    .as_ref()
                    .map(std::string::ToString::to_string)
                    .unwrap_or_default(),
                r.idempotency_key.clone(),
            )
        })
        .collect()
}

#[allow(clippy::type_complexity)]
fn want() -> Vec<(
    String,
    String,
    String,
    String,
    serde_json::Value,
    String,
    String,
)> {
    ORACLE_ROWS
        .iter()
        .map(|(t, m, at, s, md)| {
            let value: serde_json::Value = if md.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::from_str(md).expect("oracle metadata parses")
            };
            (
                (*t).to_owned(),
                (*m).to_owned(),
                (*at).to_owned(),
                (*s).to_owned(),
                value,
                (*md).to_owned(),
                String::new(), // keys compared separately
            )
        })
        .collect()
}

/// The exact inputs the legacy adapter fed its handlers on this journey
/// (mirrors the oracle script's call sequence).
#[test]
fn handlers_replay_matches_legacy_oracle() {
    let h: FunnelHandlers = FunnelHandlers::new(
        MemStore::new(),
        Some(two_bot_core::NoopLeveling),
        Some(two_bot_core::NoopFacts),
    );

    // --- Member A ----------------------------------------------------------
    h.on_join(JoinInput {
        guild_id: GUILD,
        member_id: A,
        is_bot: false,
        source: "invite:twodev01".to_owned(),
        occurred_at: Some(stamp("12:00:00")),
        inviter_id: Some(INVITER),
        source_event_id: Some(format!("{GUILD}:{A}:{}", stamp("12:00:00"))),
    });
    h.on_gate_cleared(GateClearedInput {
        guild_id: GUILD,
        member_id: A,
        is_bot: false,
        occurred_at: Some(stamp("12:00:00")),
        source: None,
    });
    for (i, at) in ["12:01:00", "12:02:00", "12:03:00", "12:04:00"]
        .iter()
        .enumerate()
    {
        h.on_message(MessageInput {
            guild_id: GUILD,
            member_id: A,
            is_bot: false,
            message_id: Some(format!("msg-a-{i}")),
            webhook_id: None,
            is_staff_automation: false,
            channel_id: CH_TEXT,
            channel_class: ChannelClass::Human,
            capture_only: false,
            occurred_at: Some(stamp(at)),
        });
    }
    h.on_voice_join(VoiceInput {
        guild_id: GUILD,
        member_id: A,
        is_bot: false,
        channel_id: CH_VOICE_A,
        occurred_at: Some(stamp("12:10:00")),
    });
    h.on_voice_leave(VoiceInput {
        guild_id: GUILD,
        member_id: A,
        is_bot: false,
        channel_id: CH_VOICE_A,
        occurred_at: Some(stamp("12:20:00")),
    });
    h.on_voice_join(VoiceInput {
        guild_id: GUILD,
        member_id: A,
        is_bot: false,
        channel_id: CH_VOICE_B,
        occurred_at: Some(stamp("12:20:00")),
    });
    h.on_voice_leave(VoiceInput {
        guild_id: GUILD,
        member_id: A,
        is_bot: false,
        channel_id: CH_VOICE_B,
        occurred_at: Some(stamp("12:25:30")),
    });
    h.on_leave(GUILD, A, Some(stamp("13:00:00")), Some(false));

    // --- Member B: pending join → late gate clear → unknown attribution -----
    h.on_join(JoinInput {
        guild_id: GUILD,
        member_id: B,
        is_bot: false,
        source: "unknown".to_owned(),
        occurred_at: Some(stamp("12:30:00")),
        inviter_id: None,
        source_event_id: Some(format!("{GUILD}:{B}:{}", stamp("12:30:00"))),
    });
    h.on_gate_cleared(GateClearedInput {
        guild_id: GUILD,
        member_id: B,
        is_bot: false,
        occurred_at: Some(stamp("12:35:00")),
        source: None,
    });
    h.on_message(MessageInput {
        guild_id: GUILD,
        member_id: B,
        is_bot: false,
        message_id: Some("msg-b-0".to_owned()),
        webhook_id: None,
        is_staff_automation: false,
        channel_id: CH_TEXT,
        channel_class: ChannelClass::Welcome,
        capture_only: false,
        occurred_at: Some(stamp("12:36:00")),
    });
    h.on_voice_leave(VoiceInput {
        guild_id: GUILD,
        member_id: B,
        is_bot: false,
        channel_id: CH_VOICE_A,
        occurred_at: Some(stamp("12:40:00")),
    });

    // --- Member C: one-click (expected note beats the diff) -----------------
    h.on_join(JoinInput {
        guild_id: GUILD,
        member_id: C,
        is_bot: false,
        source: WEB_ONE_CLICK_SOURCE.to_owned(),
        occurred_at: Some(stamp("12:45:00")),
        inviter_id: None,
        source_event_id: Some(format!("{GUILD}:{C}:{}", stamp("12:45:00"))),
    });
    h.on_gate_cleared(GateClearedInput {
        guild_id: GUILD,
        member_id: C,
        is_bot: false,
        occurred_at: Some(stamp("12:45:00")),
        source: None,
    });

    // --- Bot join: no funnel row ---------------------------------------------
    h.on_join(JoinInput {
        guild_id: GUILD,
        member_id: BOT,
        is_bot: true,
        source: "unknown".to_owned(),
        occurred_at: Some(stamp("12:50:00")),
        inviter_id: None,
        source_event_id: None,
    });

    let got = canonical(&h.store().rows());
    let want = want();
    assert_eq!(got.len(), want.len(), "row count must match the oracle");
    for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
        // Type/member/stamp/source compare directly; metadata as parsed Value
        // (key order is not significant) AND as raw bytes (Postgres TEXT is).
        assert_eq!(
            (&g.0, &g.1, &g.2, &g.3, &g.4),
            (&w.0, &w.1, &w.2, &w.3, &w.4),
            "row {i} mismatch"
        );
        assert_eq!(&g.5, &w.5, "row {i} metadata bytes must match the oracle");
    }
    // Idempotency keys match the legacy format exactly (replay-safe).
    for row in h.store().rows() {
        assert_eq!(
            row.idempotency_key,
            two_bot_core::idempotency_key(&two_bot_core::FunnelEvent {
                guild_id: row.guild_id,
                member_id: row.member_id,
                event_type: row.event_type,
                occurred_at: row.occurred_at.clone(),
                source: row.source.clone(),
                metadata: row.metadata.clone(),
                dedupe_token: row.dedupe_token.clone(),
            }),
            "key must be the pure function of the row"
        );
    }
    // Spot-check the legacy key shapes.
    let rows = h.store().rows();
    assert!(rows
        .iter()
        .any(|r| r.idempotency_key == "100000000000000001:900000000000001111:gate_cleared"));
}

// --- pipeline translation tests -----------------------------------------------

fn seed_pending_cache(pipeline: &MemPipeline) {
    for (id, joined) in [(A, "12:00:00"), (B, "12:30:00")] {
        pipeline
            .cache()
            .update(&Event::MemberAdd(Box::new(MemberAdd {
                guild_id: Id::new(GUILD),
                member: member(id, true, joined),
            })));
    }
}

fn invite_snapshot(uses: u64) -> Vec<InviteState> {
    vec![InviteState {
        code: "twodev01".to_owned(),
        uses,
        inviter_id: Some(INVITER),
        channel_id: Some(CH_TEXT),
    }]
}

fn message_event(member_id: u64, seq: u64, at: &str) -> Event {
    Event::MessageCreate(Box::new(MessageCreate(Message {
        activity: None,
        application: None,
        application_id: None,
        attachments: vec![],
        author: user(member_id, false),
        call: None,
        channel_id: Id::new(CH_TEXT),
        components: vec![],
        content: "hello".to_owned(),
        edited_timestamp: None,
        embeds: vec![],
        flags: Some(MessageFlags::empty()),
        guild_id: Some(Id::new(GUILD)),
        id: Id::new(4_000_000_000_000_000_000 + seq),
        // `interaction` is deprecated in favour of `interaction_metadata`
        // but still a required literal field on 0.17.1.
        #[allow(deprecated)]
        interaction: None,
        interaction_metadata: None,
        kind: MessageType::Regular,
        member: None,
        mention_channels: vec![],
        mention_everyone: false,
        mention_roles: vec![],
        mentions: vec![],
        message_snapshots: vec![],
        pinned: false,
        poll: None,
        reactions: vec![],
        reference: None,
        referenced_message: None,
        role_subscription_data: None,
        sticker_items: vec![],
        timestamp: ts(at),
        thread: None,
        tts: false,
        webhook_id: None,
    })))
}

fn voice_event(member_id: u64, channel: Option<u64>) -> Event {
    Event::VoiceStateUpdate(Box::new(VoiceStateUpdate(VoiceState {
        channel_id: channel.map(Id::new),
        deaf: false,
        guild_id: Some(Id::new(GUILD)),
        member: Some(member(member_id, false, "12:00:00")),
        mute: false,
        self_deaf: false,
        self_mute: false,
        self_stream: false,
        self_video: false,
        session_id: "mock-voice".to_owned(),
        suppress: false,
        user_id: Id::new(member_id),
        request_to_speak_timestamp: None,
    })))
}

/// Join attribution through the pipeline: invite growth wins, and the
/// expected-join note beats the diff.
#[test]
fn pipeline_join_attribution_and_instant_gate_clear() {
    let pipeline = MemPipeline::for_replay();
    pipeline.invite_source().push(GUILD, invite_snapshot(5));
    pipeline.prime_invite_snapshot(GUILD);
    pipeline.invite_source().push(GUILD, invite_snapshot(6));

    // A arrives gate-cleared: join + instant gate_cleared.
    pipeline.handle(&join_event(A, false, "12:00:00"));
    let rows = pipeline.handlers().store().rows();
    let join = rows
        .iter()
        .find(|r| r.event_type == two_bot_core::EventType::MemberJoin && r.member_id == Some(A))
        .expect("member_join");
    assert_eq!(join.source, "invite:twodev01");
    assert_eq!(join.occurred_at, stamp("12:00:00"));
    assert!(
        rows.iter().any(|r| r.event_type == two_bot_core::EventType::GateCleared
            && r.member_id == Some(A)),
        "instant gate clear for !pending join"
    );

    // C: one-click note beats a concurrent invite growth.
    pipeline.expect_join(GUILD, C, WEB_ONE_CLICK_SOURCE.to_owned());
    pipeline.invite_source().push(GUILD, invite_snapshot(9));
    pipeline.handle(&join_event(C, false, "12:45:00"));
    let rows = pipeline.handlers().store().rows();
    let join = rows
        .iter()
        .find(|r| r.event_type == two_bot_core::EventType::MemberJoin && r.member_id == Some(C))
        .expect("member_join C");
    assert_eq!(join.source, WEB_ONE_CLICK_SOURCE);
}

/// Pending join → no instant clear; MemberUpdate flip → gate_cleared.
/// Reads the PRE-update flag (cache holds the old member).
#[test]
fn pipeline_gate_clear_on_pending_flip() {
    let pipeline = MemPipeline::for_replay();
    pipeline.invite_source().push(GUILD, invite_snapshot(5));
    pipeline.prime_invite_snapshot(GUILD);
    pipeline.invite_source().push(GUILD, invite_snapshot(5));
    seed_pending_cache(&pipeline);

    pipeline.handle(&join_event(B, true, "12:30:00"));
    let rows = pipeline.handlers().store().rows();
    assert!(
        !rows.iter().any(|r| r.event_type == two_bot_core::EventType::GateCleared
            && r.member_id == Some(B)),
        "pending join must not clear the gate"
    );

    // Rules accepted: pending true→false.
    pipeline.handle(&Event::MemberUpdate(Box::new(MemberUpdate {
        avatar: None,
        communication_disabled_until: None,
        guild_id: Id::new(GUILD),
        flags: None,
        deaf: None,
        joined_at: Some(ts("12:30:00")),
        mute: None,
        nick: None,
        pending: false,
        premium_since: None,
        roles: vec![],
        user: user(B, false),
    })));
    let rows = pipeline.handlers().store().rows();
    assert!(
        rows.iter().any(|r| r.event_type == two_bot_core::EventType::GateCleared
            && r.member_id == Some(B)),
        "pending flip must clear the gate"
    );

    // A no-op update (same pending=false, cache now cleared) clears nothing new.
    let before = rows.len();
    pipeline.handle(&Event::MemberUpdate(Box::new(MemberUpdate {
        avatar: None,
        communication_disabled_until: None,
        guild_id: Id::new(GUILD),
        flags: None,
        deaf: None,
        joined_at: Some(ts("12:30:00")),
        mute: None,
        nick: Some("newnick".to_owned()),
        pending: false,
        premium_since: None,
        roles: vec![],
        user: user(B, false),
    })));
    assert_eq!(
        pipeline.handlers().store().rows().len(),
        before,
        "non-transition update writes no funnel row"
    );
}

/// Messages: guild rows advance the ladder with the frame stamp; DMs drop.
#[test]
fn pipeline_messages_and_dm_drop() {
    let pipeline = MemPipeline::for_replay();
    pipeline.handle(&message_event(A, 1, "12:01:00"));
    pipeline.handle(&message_event(A, 2, "12:02:00"));
    let rows = pipeline.handlers().store().rows();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].occurred_at, stamp("12:01:00"));
    assert_eq!(rows[0].source, format!("channel:{CH_TEXT}"));

    // DM: no guild_id → dropped.
    let mut dm = message_event(A, 3, "12:03:00");
    if let Event::MessageCreate(ref mut m) = dm {
        m.guild_id = None;
    }
    pipeline.handle(&dm);
    assert_eq!(pipeline.handlers().store().rows().len(), 2);
}

/// Voice: join→leave→move through the pipeline; mute-only frames ignored;
/// reconnect drops open sessions.
#[test]
fn pipeline_voice_boundaries_and_reconnect_drop() {
    let pipeline = MemPipeline::for_replay();
    pipeline.handle(&voice_event(A, Some(CH_VOICE_A)));
    assert_eq!(
        pipeline
            .handlers()
            .voice_sessions
            .lock()
            .expect("lock")
            .open_count(),
        1
    );
    // Mute-only frame (same channel): no boundary.
    pipeline.handle(&voice_event(A, Some(CH_VOICE_A)));
    assert_eq!(pipeline.handlers().store().rows().len(), 2); // start + first

    // Move A→B: end(A) then start(B).
    pipeline.handle(&voice_event(A, Some(CH_VOICE_B)));
    let rows = pipeline.handlers().store().rows();
    let ends: Vec<&StoredRow> = rows
        .iter()
        .filter(|r| r.event_type == two_bot_core::EventType::VoiceSessionEnd)
        .collect();
    assert_eq!(ends.len(), 1);
    assert_eq!(ends[0].source, format!("channel:{CH_VOICE_A}"));

    // Reconnect drops the open session: the next leave is startKnown:false.
    pipeline.handle(&Event::Resumed);
    assert_eq!(
        pipeline
            .handlers()
            .voice_sessions
            .lock()
            .expect("lock")
            .open_count(),
        0
    );
    pipeline.handle(&voice_event(A, None));
    let rows = pipeline.handlers().store().rows();
    let last_end = rows
        .iter()
        .rev()
        .find(|r| r.event_type == two_bot_core::EventType::VoiceSessionEnd)
        .expect("unknown end");
    assert!(last_end.metadata.as_ref().is_some_and(|m| {
        m.get("startKnown").and_then(serde_json::Value::as_bool) == Some(false)
    }));
}

/// Server leave closes the open voice session first (TOG-6122).
#[test]
fn pipeline_leave_closes_voice_first() {
    let pipeline = MemPipeline::for_replay();
    pipeline.handle(&voice_event(A, Some(CH_VOICE_A)));
    pipeline.handle(&Event::MemberRemove(MemberRemove {
        guild_id: Id::new(GUILD),
        user: user(A, false),
    }));
    let rows = pipeline.handlers().store().rows();
    assert!(
        rows.iter()
            .any(|r| r.event_type == two_bot_core::EventType::VoiceSessionEnd
                && r.member_id == Some(A)),
        "leave must close the open session"
    );
    assert!(
        rows.iter().any(|r| r.event_type == two_bot_core::EventType::MemberLeave
            && r.member_id == Some(A)),
        "leave row recorded"
    );
}

/// InviteCreate seeds the baseline so the next join measures growth.
#[test]
fn pipeline_invite_create_seeds_baseline() {
    use twilight_model::gateway::payload::incoming::InviteCreate as IC;
    let pipeline = MemPipeline::for_replay();
    pipeline.handle(&Event::InviteCreate(Box::new(IC {
        channel_id: Id::new(CH_TEXT),
        code: "fresh01".to_owned(),
        created_at: ts("12:00:00"),
        guild_id: Id::new(GUILD),
        inviter: None,
        max_age: 0,
        max_uses: 0,
        target_user_type: None,
        target_user: None,
        temporary: false,
        uses: 0,
    })));
    // The next snapshot read shows uses=1: growth of 1, not of the whole counter.
    pipeline.invite_source().push(
        GUILD,
        vec![InviteState {
            code: "fresh01".to_owned(),
            uses: 1,
            inviter_id: Some(INVITER),
            channel_id: Some(CH_TEXT),
        }],
    );
    pipeline.handle(&join_event(A, false, "12:00:00"));
    let rows = pipeline.handlers().store().rows();
    let join = rows
        .iter()
        .find(|r| r.event_type == two_bot_core::EventType::MemberJoin && r.member_id == Some(A))
        .expect("member_join");
    assert_eq!(join.source, "invite:fresh01");
}

/// Bot joins and bot messages write no funnel rows.
#[test]
fn pipeline_ignores_bots() {
    let pipeline = MemPipeline::for_replay();
    pipeline.invite_source().push(GUILD, invite_snapshot(5));
    pipeline.prime_invite_snapshot(GUILD);
    let mut join = join_event(BOT, false, "12:50:00");
    if let Event::MemberAdd(ref mut add) = join {
        add.user.bot = true;
    }
    pipeline.handle(&join);
    assert!(pipeline
        .handlers()
        .store()
        .rows()
        .iter()
        .all(|r| r.member_id != Some(BOT) || r.event_type == two_bot_core::EventType::MemberLeave));
}

/// Failed invite reads keep the old snapshot: joins still record (`unknown`).
#[test]
fn pipeline_failed_invite_read_still_records() {
    let pipeline = MemPipeline::for_replay();
    // No snapshot queued → read fails → source unknown, row still written.
    pipeline.handle(&join_event(B, true, "12:30:00"));
    let rows = pipeline.handlers().store().rows();
    let join = rows
        .iter()
        .find(|r| r.event_type == two_bot_core::EventType::MemberJoin && r.member_id == Some(B))
        .expect("member_join");
    assert_eq!(join.source, "unknown");
}

#[allow(dead_code)]
fn _classifier_check(_: &NoClassification) {}
