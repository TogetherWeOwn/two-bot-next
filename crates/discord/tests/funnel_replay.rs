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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

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
    ChannelClass, FactsSink, FunnelHandlers, GateClearedInput, InviteState, JoinInput, MemStore,
    MemberJoinFact, MessageFact, MessageInput, NoopLeveling, RulesAcceptedFact, StoredRow,
    VoiceEndedFact, VoiceInput, VoiceStartedFact, WEB_ONE_CLICK_SOURCE,
};
use two_bot_discord::{
    DeferredCommunityFacts, JoinObservation, JoinObserver, MemPipeline, NoClassification, Pipeline,
    ScriptedInvites,
};

const GUILD: u64 = 100_000_000_000_000_001;
const A: u64 = 900_000_000_000_001_111;
const B: u64 = 900_000_000_000_002_222;
const C: u64 = 900_000_000_000_003_333;
const BOT: u64 = 900_000_000_000_000_099;
const CH_TEXT: u64 = 200_000_000_000_000_001;
const CH_VOICE_A: u64 = 300_000_000_000_000_001;
const CH_VOICE_B: u64 = 300_000_000_000_000_002;
const CH_VOICE_C: u64 = 300_000_000_000_000_003;
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

fn rules_pipeline() -> (
    Pipeline<
        two_bot_core::MemStore,
        two_bot_core::NoopLeveling,
        DeferredCommunityFacts,
        ScriptedInvites,
        NoClassification,
    >,
    DeferredCommunityFacts,
) {
    let buffer = DeferredCommunityFacts::new();
    let pipeline = Pipeline::new(
        two_bot_core::MemStore::new(),
        Some(two_bot_core::NoopLeveling),
        Some(buffer.clone()),
        ScriptedInvites::new(),
        NoClassification,
    );
    (pipeline, buffer)
}

fn member_update(member_id: u64, pending: bool, joined: &str, nick: Option<&str>) -> Event {
    Event::MemberUpdate(Box::new(MemberUpdate {
        avatar: None,
        communication_disabled_until: None,
        guild_id: Id::new(GUILD),
        flags: None,
        deaf: None,
        joined_at: Some(ts(joined)),
        mute: None,
        nick: nick.map(str::to_owned),
        pending,
        premium_since: None,
        roles: vec![],
        user: user(member_id, false),
    }))
}

/// Gate-clear buffers exactly one rules fact per member: the pending flip
/// captures on the receipt stamp, and a burst of later updates adds nothing.
#[test]
fn pipeline_gate_clear_buffers_one_rules_fact_per_member() {
    let (pipeline, buffer) = rules_pipeline();
    let observed_at = stamp("12:35:00");

    // Pending arrival buffers nothing.
    pipeline.handle_at(&join_event(B, true, "12:30:00"), &observed_at);
    assert!(buffer.is_empty());

    // Pending true→false buffers one fact on the receipt stamp.
    pipeline.handle_at(&member_update(B, false, "12:30:00", None), &observed_at);
    assert_eq!(buffer.len(), 1);
    let write = buffer.take().pop().expect("one buffered fact");
    assert_eq!((write.guild_id, write.member_id), (GUILD, B));
    assert!(!write.is_bot);
    assert_eq!(write.occurred_at, observed_at);
    assert_eq!(write.source, "gateway");
    assert_eq!(write.source_event_id, format!("{GUILD}:{B}:rules"));

    // A burst of non-transition updates buffers nothing more, and the funnel
    // holds exactly one gate row for the member.
    for nick in ["al", "bo"] {
        pipeline.handle_at(
            &member_update(B, false, "12:30:00", Some(nick)),
            &observed_at,
        );
    }
    assert!(buffer.is_empty());
    assert_eq!(
        pipeline
            .handlers()
            .store()
            .rows()
            .iter()
            .filter(
                |r| r.event_type == two_bot_core::EventType::GateCleared && r.member_id == Some(B)
            )
            .count(),
        1,
        "burst writes one funnel row"
    );
}

/// The fastest members arrive with the gate already cleared: the fact carries
/// Discord's `joined_at`, not the receipt time. Bots buffer too (captured,
/// never funnel-counted).
#[test]
fn pipeline_instant_gate_clear_buffers_rules_fact_on_join_stamp() {
    let (pipeline, buffer) = rules_pipeline();
    let observed_at = stamp("12:10:00");
    pipeline.handle_at(&join_event(A, false, "12:00:00"), &observed_at);
    let mut bot_join = join_event(BOT, false, "12:00:30");
    if let Event::MemberAdd(ref mut add) = bot_join {
        add.member.user.bot = true;
    }
    pipeline.handle_at(&bot_join, &observed_at);

    let writes = buffer.take();
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[0].occurred_at, stamp("12:00:00"));
    assert_eq!(writes[0].source, "gateway");
    assert!(!writes[0].is_bot);
    assert_eq!((writes[1].member_id, writes[1].is_bot), (BOT, true));
    assert!(
        pipeline
            .handlers()
            .store()
            .rows()
            .iter()
            .all(|r| r.member_id != Some(BOT)),
        "bots write no funnel rows"
    );
}

/// A cleared member seen for the first time (no cache entry, never joined)
/// still clears: the missing pre-update flag reads as pending.
#[test]
fn pipeline_first_sighting_of_cleared_member_buffers_rules_fact() {
    let (pipeline, buffer) = rules_pipeline();
    pipeline.handle_at(
        &member_update(C, false, "12:30:00", None),
        &stamp("12:35:00"),
    );
    assert_eq!(buffer.len(), 1);
}

#[test]
fn pipeline_receipt_time_is_fallback_not_payload_override() {
    let pipeline = MemPipeline::for_replay();
    let observed_at = stamp("12:10:00");
    pipeline.handle_at(&join_event(A, false, "12:00:00"), &observed_at);
    pipeline.handle_at(&message_event(A, 1, "12:01:00"), &observed_at);
    let mut unstamped = join_event(B, false, "12:00:00");
    if let Event::MemberAdd(ref mut add) = unstamped {
        add.member.joined_at = None;
    }
    pipeline.handle_at(&unstamped, &observed_at);
    let rows = pipeline.handlers().store().rows();
    for (kind, member_id, at) in [
        (two_bot_core::EventType::MemberJoin, A, stamp("12:00:00")),
        (two_bot_core::EventType::GateCleared, A, stamp("12:00:00")),
        (two_bot_core::EventType::FirstMessage, A, stamp("12:01:00")),
        (two_bot_core::EventType::MemberJoin, B, observed_at.clone()),
        (two_bot_core::EventType::GateCleared, B, observed_at),
    ] {
        let row = rows
            .iter()
            .find(|row| row.event_type == kind && row.member_id == Some(member_id))
            .unwrap();
        assert_eq!(row.occurred_at, at);
    }
}

#[test]
fn pipeline_voice_move_and_server_leave_share_receipt_boundaries() {
    let pipeline = MemPipeline::for_replay();
    pipeline.handle_at(&voice_event(A, Some(CH_VOICE_A)), &stamp("12:00:00"));
    pipeline.handle_at(&voice_event(A, Some(CH_VOICE_B)), &stamp("12:00:03"));
    pipeline.handle_at(
        &Event::MemberRemove(MemberRemove {
            guild_id: Id::new(GUILD),
            user: user(A, false),
        }),
        &stamp("12:00:05"),
    );
    let rows = pipeline.handlers().store().rows();
    let ends: Vec<_> = rows
        .iter()
        .filter(|row| row.event_type == two_bot_core::EventType::VoiceSessionEnd)
        .collect();
    assert_eq!(ends.len(), 2);
    assert_eq!(ends[0].occurred_at, stamp("12:00:03"));
    assert_eq!(ends[0].metadata.as_ref().unwrap()["durationSeconds"], 3);
    assert_eq!(ends[1].occurred_at, stamp("12:00:05"));
    assert_eq!(ends[1].metadata.as_ref().unwrap()["durationSeconds"], 2);
    assert!(rows.iter().any(|row| {
        row.event_type == two_bot_core::EventType::VoiceSessionStart
            && row.source == format!("channel:{CH_VOICE_B}")
            && row.occurred_at == ends[0].occurred_at
    }));
    assert!(rows.iter().any(|row| {
        row.event_type == two_bot_core::EventType::MemberLeave
            && row.occurred_at == ends[1].occurred_at
    }));
}

struct RecordedJoins(std::sync::Mutex<Vec<JoinObservation>>);

impl JoinObserver for RecordedJoins {
    fn observe_join(&self, join: JoinObservation) {
        self.0.lock().unwrap().push(join);
    }
}

/// The raid-watch seam: non-bot joins reach the observer after the funnel
/// rows, with Discord's `joined_at` (receipt time when absent) in epoch ms.
#[test]
fn pipeline_join_observer_sees_non_bot_joins_after_the_funnel() {
    let pipeline = MemPipeline::for_replay();
    let seen = std::sync::Arc::new(RecordedJoins(std::sync::Mutex::new(Vec::new())));
    pipeline.set_join_observer(seen.clone());
    let observed_at = stamp("12:10:00");

    pipeline.handle_at(&join_event(A, true, "12:00:00"), &observed_at);
    let mut unstamped = join_event(B, true, "12:00:00");
    if let Event::MemberAdd(ref mut add) = unstamped {
        add.member.joined_at = None;
    }
    pipeline.handle_at(&unstamped, &observed_at);
    let mut bot_join = join_event(BOT, true, "12:00:30");
    if let Event::MemberAdd(ref mut add) = bot_join {
        add.member.user.bot = true;
    }
    pipeline.handle_at(&bot_join, &observed_at);

    let joins = seen.0.lock().unwrap().clone();
    assert_eq!(joins.len(), 2, "the bot join is never observed");
    assert_eq!(joins[0].guild_id, GUILD);
    assert_eq!(joins[0].member_id, A);
    assert_eq!(joins[0].joined_at_ms, 1_789_905_600_000, "12:00:00Z");
    assert_eq!(joins[0].source, "unknown");
    assert_eq!(joins[1].member_id, B);
    assert_eq!(
        joins[1].joined_at_ms, 1_789_906_200_000,
        "receipt 12:10:00Z"
    );
    // The funnel recorded the joins the observer was told about.
    let rows = pipeline.handlers().store().rows();
    for member_id in [A, B] {
        assert!(rows.iter().any(|row| {
            row.event_type == two_bot_core::EventType::MemberJoin
                && row.member_id == Some(member_id)
        }));
    }
}

/// First registration wins; a pipeline with no observer behaves as before.
#[test]
fn pipeline_join_observer_registration_is_first_wins() {
    let pipeline = MemPipeline::for_replay();
    pipeline.handle_at(&join_event(A, true, "12:00:00"), &stamp("12:10:00"));
    let first = std::sync::Arc::new(RecordedJoins(std::sync::Mutex::new(Vec::new())));
    let second = std::sync::Arc::new(RecordedJoins(std::sync::Mutex::new(Vec::new())));
    pipeline.set_join_observer(first.clone());
    pipeline.set_join_observer(second.clone());
    pipeline.handle_at(&join_event(B, true, "12:01:00"), &stamp("12:10:00"));
    assert_eq!(first.0.lock().unwrap().len(), 1);
    assert!(second.0.lock().unwrap().is_empty());
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

/// A fresh session (READY after re-identify, not RESUMED) also drops open
/// voice sessions (legacy 7da2c15): the leave that follows an outage ends
/// unknown-start instead of spanning the gap. A first connect has nothing to
/// drop and writes no rows.
#[test]
fn pipeline_fresh_session_ready_drops_open_voice_sessions() {
    let ready = Event::Ready(
        serde_json::from_value(serde_json::json!({
            "v": 10,
            "user": {"id": "999", "username": "mock-bot", "discriminator": "0", "bot": true, "mfa_enabled": false},
            "session_id": "fresh-session",
            "resume_gateway_url": "wss://gateway.discord.gg",
            "guilds": [{"id": GUILD.to_string(), "unavailable": true}],
            "application": {"id": "1111", "flags": 0}
        }))
        .expect("ready fixture"),
    );
    let open_sessions = |pipeline: &MemPipeline| {
        pipeline
            .handlers()
            .voice_sessions
            .lock()
            .expect("lock")
            .open_count()
    };

    let pipeline = MemPipeline::for_replay();
    pipeline.handle(&ready);
    assert_eq!(open_sessions(&pipeline), 0, "first READY is a no-op");
    assert!(pipeline.handlers().store().rows().is_empty());

    pipeline.handle(&voice_event(A, Some(CH_VOICE_A)));
    assert_eq!(open_sessions(&pipeline), 1);
    pipeline.handle(&ready);
    assert_eq!(open_sessions(&pipeline), 0, "READY drops the open session");

    pipeline.handle(&voice_event(A, None));
    let rows = pipeline.handlers().store().rows();
    let end = rows
        .iter()
        .find(|r| r.event_type == two_bot_core::EventType::VoiceSessionEnd)
        .expect("leave after READY writes an unknown-start end");
    let metadata = end.metadata.as_ref().expect("end metadata");
    assert_eq!(metadata["startKnown"], false);
    assert!(
        metadata["durationSeconds"].is_null(),
        "no duration is invented across the reconnect: {metadata}"
    );
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

/// Voice boundary rows (start/end only) in insert order.
fn voice_boundaries(pipeline: &MemPipeline) -> Vec<StoredRow> {
    pipeline
        .handlers()
        .store()
        .rows()
        .into_iter()
        .filter(|r| {
            matches!(
                r.event_type,
                two_bot_core::EventType::VoiceSessionStart
                    | two_bot_core::EventType::VoiceSessionEnd
            )
        })
        .collect()
}

/// Legacy `59965d0`, same-tick burst: three frames for one member that all
/// carry ONE processing stamp. Every end resolves against the start the
/// previous frame just wrote (`startKnown:true`, zero seconds), and the
/// channel-scoped keys keep each boundary a row of its own instead of letting
/// the same-instant end(A)/end(B) pair collapse.
#[test]
fn pipeline_same_instant_voice_burst_keeps_every_boundary_as_its_own_row() {
    let pipeline = MemPipeline::for_replay();
    let at = stamp("12:00:00");
    for channel in [CH_VOICE_A, CH_VOICE_B, CH_VOICE_C] {
        pipeline.handle_at(&voice_event(A, Some(channel)), &at);
    }

    let rows = voice_boundaries(&pipeline);
    let boundaries: Vec<_> = rows
        .iter()
        .map(|r| (r.event_type, r.source.as_str()))
        .collect();
    let (start, end) = (
        two_bot_core::EventType::VoiceSessionStart,
        two_bot_core::EventType::VoiceSessionEnd,
    );
    assert_eq!(
        boundaries,
        [
            (start, format!("channel:{CH_VOICE_A}").as_str()),
            (end, format!("channel:{CH_VOICE_A}").as_str()),
            (start, format!("channel:{CH_VOICE_B}").as_str()),
            (end, format!("channel:{CH_VOICE_B}").as_str()),
            (start, format!("channel:{CH_VOICE_C}").as_str()),
        ]
    );
    let keys: std::collections::HashSet<_> = rows.iter().map(|r| &r.idempotency_key).collect();
    assert_eq!(keys.len(), rows.len(), "no two boundaries share a key");
    for row in &rows {
        assert_eq!(row.occurred_at, at, "the whole burst is one instant");
        if row.event_type == end {
            assert_eq!(
                row.metadata,
                Some(serde_json::json!({
                    "startKnown": true,
                    "startedAt": at,
                    "durationSeconds": 0,
                })),
                "end on {} is measured, not unknown-start",
                row.source
            );
        }
    }
    let sessions = pipeline.handlers().voice_sessions.lock().expect("lock");
    assert_eq!(sessions.open_count(), 1);
    assert_eq!(
        sessions.peek(GUILD, A).map(|s| s.channel_id),
        Some(CH_VOICE_C)
    );
}

/// Same burst, but the member comes back to the channel they started in:
/// join A, move to B, move back to A at ONE instant. Both ends are measured
/// and land as separate rows (their keys differ by channel). The return
/// start(A) shares its key with the opening start(A) (same member, instant and
/// channel; the key has no session component, as in legacy), so the store
/// keeps the first row and the tracker still opens the new session.
#[test]
fn pipeline_same_instant_voice_return_to_origin_dedupes_only_the_repeat_start() {
    let pipeline = MemPipeline::for_replay();
    let at = stamp("12:00:00");
    for channel in [CH_VOICE_A, CH_VOICE_B, CH_VOICE_A] {
        pipeline.handle_at(&voice_event(A, Some(channel)), &at);
    }

    let rows = voice_boundaries(&pipeline);
    let (start, end) = (
        two_bot_core::EventType::VoiceSessionStart,
        two_bot_core::EventType::VoiceSessionEnd,
    );
    let ends: Vec<_> = rows.iter().filter(|r| r.event_type == end).collect();
    assert_eq!(ends.len(), 2, "end(A) and end(B) both land");
    assert_ne!(ends[0].idempotency_key, ends[1].idempotency_key);
    assert_eq!(
        ends.iter().map(|r| r.source.as_str()).collect::<Vec<_>>(),
        [
            format!("channel:{CH_VOICE_A}").as_str(),
            format!("channel:{CH_VOICE_B}").as_str()
        ]
    );
    for row in ends {
        assert_eq!(
            row.metadata,
            Some(serde_json::json!({
                "startKnown": true,
                "startedAt": at,
                "durationSeconds": 0,
            }))
        );
    }
    let starts: Vec<_> = rows
        .iter()
        .filter(|r| r.event_type == start)
        .map(|r| r.source.as_str())
        .collect();
    assert_eq!(
        starts,
        [
            format!("channel:{CH_VOICE_A}").as_str(),
            format!("channel:{CH_VOICE_B}").as_str()
        ],
        "the return start(A) repeats the opening start(A) key"
    );
    let sessions = pipeline.handlers().voice_sessions.lock().expect("lock");
    assert_eq!(
        sessions.peek(GUILD, A).map(|s| s.channel_id),
        Some(CH_VOICE_A),
        "the member is tracked in A again"
    );
}

/// Facts double that freezes the first voice end mid-move (the tracker has
/// closed A, start(B) has not run) until another frame reaches the same hook
/// or `GRACE` lapses. Rendezvous waits are bounded and cancelled when either
/// frame exits, including a panic before the hook.
struct MoveGate {
    ends: AtomicUsize,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    state: Mutex<MoveGateState>,
    changed: Condvar,
}

#[derive(Default)]
struct MoveGateState {
    arrivals: usize,
    cancelled: bool,
    overlap: bool,
}

struct CancelMoveOnExit<'a>(&'a MoveGate);

impl Drop for CancelMoveOnExit<'_> {
    fn drop(&mut self) {
        // Cleanup must also wake the peer when a panic poisoned the mutex.
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.cancelled = true;
        self.0.changed.notify_all();
    }
}

impl MoveGate {
    const RENDEZVOUS_TIMEOUT: Duration = Duration::from_secs(5);
    /// Long enough for a runnable thread to reach the hook; the lock-held path
    /// waits it out in full. This is not a timing-free mutation proof.
    const GRACE: Duration = Duration::from_millis(400);

    fn new() -> Self {
        Self {
            ends: AtomicUsize::new(0),
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
            state: Mutex::new(MoveGateState::default()),
            changed: Condvar::new(),
        }
    }

    fn cancel_on_exit(&self) -> CancelMoveOnExit<'_> {
        CancelMoveOnExit(self)
    }

    fn wait_for_waiting_peer(&self) {
        let state = self.state.lock().expect("move gate");
        let (state, _) = self
            .changed
            .wait_timeout_while(state, Self::RENDEZVOUS_TIMEOUT, |s| {
                s.arrivals == 0 && !s.cancelled
            })
            .expect("move gate");
        assert_eq!(state.arrivals, 1, "peer reached its rendezvous wait");
    }

    fn rendezvous(&self, timeout: Duration) -> Result<(), &'static str> {
        let mut state = self.state.lock().expect("move gate");
        if state.cancelled {
            return Err("voice move rendezvous cancelled");
        }
        state.arrivals += 1;
        self.changed.notify_all();
        let (mut state, _) = self
            .changed
            .wait_timeout_while(state, timeout, |s| s.arrivals < 2 && !s.cancelled)
            .expect("move gate");
        if state.arrivals == 2 {
            // Once both arrived, a later frame exit does not undo the handshake.
            Ok(())
        } else if state.cancelled {
            Err("voice move rendezvous cancelled")
        } else {
            state.cancelled = true;
            self.changed.notify_all();
            Err("voice move rendezvous timed out")
        }
    }
}

impl FactsSink for &MoveGate {
    fn record_member_join(&self, _: MemberJoinFact<'_>) {}
    fn record_rules_accepted(&self, _: RulesAcceptedFact<'_>) {}
    fn record_message(&self, _: MessageFact<'_>) {}
    fn record_voice_started(&self, _: VoiceStartedFact<'_>) -> Option<String> {
        None
    }
    fn record_voice_ended(&self, _: VoiceEndedFact<'_>) {
        let nth = self.ends.fetch_add(1, Ordering::SeqCst);
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(now, Ordering::SeqCst);
        if nth == 0 {
            // Mid-move: release the competing frame, then hold the window open.
            self.rendezvous(MoveGate::RENDEZVOUS_TIMEOUT)
                .expect("first move rendezvous");
            let state = self.state.lock().expect("move gate");
            drop(
                self.changed
                    .wait_timeout_while(state, MoveGate::GRACE, |s| !s.overlap && !s.cancelled)
                    .expect("move gate"),
            );
        } else {
            let mut state = self.state.lock().expect("move gate");
            state.overlap = true;
            self.changed.notify_all();
        }
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

#[test]
fn move_gate_cancels_when_first_frame_exits_before_the_hook() {
    for panic_before_hook in [false, true] {
        let gate = MoveGate::new();
        let pipeline = Pipeline::new(
            MemStore::new(),
            Some(NoopLeveling),
            Some(&gate),
            ScriptedInvites::new(),
            NoClassification,
        );
        std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                let _cancel = gate.cancel_on_exit();
                gate.wait_for_waiting_peer();
                if panic_before_hook {
                    panic!("first frame exited before its voice-end hook");
                }
            });
            let second = scope.spawn(|| {
                let _cancel = gate.cancel_on_exit();
                gate.rendezvous(MoveGate::RENDEZVOUS_TIMEOUT)?;
                pipeline.handle_at(&voice_event(A, Some(CH_VOICE_C)), &stamp("12:00:20"));
                Ok(())
            });
            assert_eq!(first.join().is_err(), panic_before_hook);
            assert_eq!(
                second.join().expect("competing frame joins"),
                Err("voice move rendezvous cancelled")
            );
        });
        assert!(pipeline.handlers().store().rows().is_empty());
        assert_eq!(gate.ends.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn move_gate_cancels_when_competing_frame_exits_before_rendezvous() {
    for panic_before_rendezvous in [false, true] {
        let gate = MoveGate::new();
        std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                let _cancel = gate.cancel_on_exit();
                gate.rendezvous(MoveGate::RENDEZVOUS_TIMEOUT)
            });
            let second = scope.spawn(|| {
                let _cancel = gate.cancel_on_exit();
                gate.wait_for_waiting_peer();
                if panic_before_rendezvous {
                    panic!("competing frame exited before rendezvous");
                }
            });
            assert_eq!(second.join().is_err(), panic_before_rendezvous);
            assert_eq!(
                first.join().expect("first frame joins"),
                Err("voice move rendezvous cancelled")
            );
        });
    }
}

#[test]
fn move_gate_times_out_without_a_peer_and_cancels_late_arrivals() {
    let gate = MoveGate::new();
    assert_eq!(
        gate.rendezvous(Duration::from_millis(20)),
        Err("voice move rendezvous timed out")
    );
    assert_eq!(
        gate.rendezvous(MoveGate::RENDEZVOUS_TIMEOUT),
        Err("voice move rendezvous cancelled")
    );
}

/// Legacy `59965d0`, the per-member serial chain (`VoiceChains`). Frame 1
/// (A to B) is frozen between its end(A) and start(B); only then does a second
/// frame for the SAME member (B to C) start. With the chain, frame 2 waits for
/// frame 1 to finish, so its end(B) finds the start frame 1 wrote. Without it,
/// frame 2 runs inside frame 1's window, finds the tracker empty and writes an
/// unknown-start end (and the two frames overlap in the hook).
#[test]
fn pipeline_serializes_same_member_voice_frames() {
    let gate = MoveGate::new();
    let pipeline = Pipeline::new(
        MemStore::new(),
        Some(NoopLeveling),
        Some(&gate),
        ScriptedInvites::new(),
        NoClassification,
    );
    pipeline.handle_at(&voice_event(A, Some(CH_VOICE_A)), &stamp("12:00:00"));

    std::thread::scope(|scope| {
        scope.spawn(|| {
            let _cancel = gate.cancel_on_exit();
            pipeline.handle_at(&voice_event(A, Some(CH_VOICE_B)), &stamp("12:00:10"));
        });
        scope.spawn(|| {
            let _cancel = gate.cancel_on_exit();
            // Released from inside frame 1's hook: frame 1 is provably mid-move.
            gate.rendezvous(MoveGate::RENDEZVOUS_TIMEOUT)
                .expect("competing frame rendezvous");
            pipeline.handle_at(&voice_event(A, Some(CH_VOICE_C)), &stamp("12:00:20"));
        });
    });

    assert_eq!(
        gate.max_in_flight.load(Ordering::SeqCst),
        1,
        "two frames for one member were inside the voice critical section at once"
    );
    let rows = pipeline.handlers().store().rows();
    let boundaries: Vec<_> = rows
        .iter()
        .filter(|r| {
            matches!(
                r.event_type,
                two_bot_core::EventType::VoiceSessionStart
                    | two_bot_core::EventType::VoiceSessionEnd
            )
        })
        .map(|r| (r.event_type, r.source.clone(), r.occurred_at.clone()))
        .collect();
    let (start, end) = (
        two_bot_core::EventType::VoiceSessionStart,
        two_bot_core::EventType::VoiceSessionEnd,
    );
    assert_eq!(
        boundaries,
        [
            (start, format!("channel:{CH_VOICE_A}"), stamp("12:00:00")),
            (end, format!("channel:{CH_VOICE_A}"), stamp("12:00:10")),
            (start, format!("channel:{CH_VOICE_B}"), stamp("12:00:10")),
            (end, format!("channel:{CH_VOICE_B}"), stamp("12:00:20")),
            (start, format!("channel:{CH_VOICE_C}"), stamp("12:00:20")),
        ],
        "frames apply whole and in order"
    );
    for row in rows.iter().filter(|r| r.event_type == end) {
        assert_eq!(
            row.metadata
                .as_ref()
                .and_then(|m| m.get("startKnown"))
                .and_then(serde_json::Value::as_bool),
            Some(true),
            "end on {} lost its start to an interleaved frame",
            row.source
        );
        assert_eq!(
            row.metadata
                .as_ref()
                .and_then(|m| m.get("durationSeconds"))
                .and_then(serde_json::Value::as_i64),
            Some(10)
        );
    }
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

/// Vanity-guild joins with no invite growth attribute `vanity` (legacy catch
/// path): a clean read showing no movement, and a failed read alike. The
/// join still records — attribution never blocks it.
#[test]
fn pipeline_vanity_guild_join_attributes_vanity() {
    let pipeline = MemPipeline::for_replay();
    pipeline.set_guild_vanity(GUILD, true);
    // Successful empty read: nothing grew → vanity.
    pipeline.invite_source().push(GUILD, vec![]);
    pipeline.handle_at(&join_event(A, false, "12:00:00"), &stamp("12:00:00"));
    let rows = pipeline.handlers().store().rows();
    let join = rows
        .iter()
        .find(|r| r.event_type == two_bot_core::EventType::MemberJoin && r.member_id == Some(A))
        .expect("member_join");
    assert_eq!(join.source, "vanity");
    // Failed read on a vanity guild: still vanity, row still written.
    pipeline.handle_at(&join_event(B, false, "12:30:00"), &stamp("12:30:00"));
    let rows = pipeline.handlers().store().rows();
    let join = rows
        .iter()
        .find(|r| r.event_type == two_bot_core::EventType::MemberJoin && r.member_id == Some(B))
        .expect("member_join");
    assert_eq!(join.source, "vanity");
}

/// A baseline older than the staleness bound is re-seeded, not diffed: the
/// stale window files `unknown` and the next join measures against the fresh
/// baseline (TOG-11716).
#[test]
fn pipeline_stale_baseline_reseeds_instead_of_crediting_drift() {
    const D: u64 = 900_000_000_000_004_444;
    let pipeline = MemPipeline::for_replay();
    // Baseline at 12:00 (first read: stored, nothing to credit).
    pipeline.invite_source().push(GUILD, invite_snapshot(5));
    pipeline.handle_at(&join_event(A, false, "12:00:00"), &stamp("12:00:00"));
    // Fresh growth at 12:30 credits normally.
    pipeline.invite_source().push(GUILD, invite_snapshot(6));
    pipeline.handle_at(&join_event(B, false, "12:30:00"), &stamp("12:30:00"));
    let rows = pipeline.handlers().store().rows();
    let join = rows
        .iter()
        .find(|r| r.event_type == two_bot_core::EventType::MemberJoin && r.member_id == Some(B))
        .expect("member_join");
    assert_eq!(join.source, "invite:twodev01");
    // Stale read at 13:30:01 (>1h after the 12:30 baseline): re-seed, no
    // credit — this window files `unknown`, never drift.
    pipeline.invite_source().push(GUILD, invite_snapshot(9));
    pipeline.handle_at(&join_event(C, false, "13:30:01"), &stamp("13:30:01"));
    let rows = pipeline.handlers().store().rows();
    let join = rows
        .iter()
        .find(|r| r.event_type == two_bot_core::EventType::MemberJoin && r.member_id == Some(C))
        .expect("member_join");
    assert_eq!(join.source, "unknown");
    // Next join measures against the re-seeded baseline (9 → 10).
    pipeline.invite_source().push(GUILD, invite_snapshot(10));
    pipeline.handle_at(&join_event(D, false, "13:31:00"), &stamp("13:31:00"));
    let rows = pipeline.handlers().store().rows();
    let join = rows
        .iter()
        .find(|r| r.event_type == two_bot_core::EventType::MemberJoin && r.member_id == Some(D))
        .expect("member_join");
    assert_eq!(join.source, "invite:twodev01");
}

#[allow(dead_code)]
fn _classifier_check(_: &NoClassification) {}
