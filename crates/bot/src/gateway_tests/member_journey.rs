//! Hand-computed legacy row contracts: docs/member-journey-parity.md.
//! This uses the real pipeline and dispatch transaction, never seeded events.

use super::{checkpoint, TestDb, GUILD};
use futures_util::FutureExt as _;
use serde_json::{json, Value};
use std::panic::{resume_unwind, AssertUnwindSafe};
use twilight_gateway::{Event, EventTypeFlags};
use two_bot_core::gateway_funnel::{FunnelBatch, GatewayFunnelBuffer, InviteSnapshotWrite};
use two_bot_core::gateway_session::{dispatch_action, DispatchAction};
use two_bot_core::{InviteState, NoopFacts, NoopLeveling};
use two_bot_cutover::gateway_session::GatewaySessionStore;
use two_bot_discord::{NoClassification, Pipeline, ScriptedInvites};

type Journey = Pipeline<GatewayFunnelBuffer, NoopLeveling, NoopFacts, ScriptedInvites>;

fn pipeline() -> Journey {
    Pipeline::new(
        GatewayFunnelBuffer::default(),
        None,
        None,
        ScriptedInvites::new(),
        NoClassification,
    )
}

fn stamp(clock: &str) -> String {
    format!("2026-09-30T{clock}.000Z")
}

fn payload_stamp(clock: &str) -> String {
    stamp(clock).replace('Z', "+00:00")
}

fn user(id: &str) -> Value {
    json!({"id":id,"username":"fixture-member","discriminator":"0","bot":false})
}

fn member(guild: &str, pending: bool, joined: &str) -> Value {
    json!({"guild_id":guild,"user":user("77"),"roles":[],"joined_at":payload_stamp(joined),
        "pending":pending,"deaf":false,"mute":false,"flags":0})
}

fn invite(guild: &str, code: &str, inviter: &str) -> Value {
    json!({"guild_id":guild,"code":code,"uses":0,"channel_id":"4444",
        "created_at":payload_stamp("11:58:00"),"inviter":user(inviter),
        "max_age":0,"max_uses":0,"temporary":false})
}

fn counter(code: &str, uses: u64, inviter: u64) -> InviteState {
    InviteState {
        code: code.into(),
        uses,
        inviter_id: Some(inviter),
        channel_id: Some(4444),
    }
}

fn message(id: &str, clock: &str) -> Value {
    json!({"guild_id":GUILD,"id":id,"channel_id":"4444","author":user("77"),
        "timestamp":payload_stamp(clock),"type":0,"content":"hello","attachments":[],
        "embeds":[],"mentions":[],"mention_roles":[],"mention_everyone":false,
        "pinned":false,"tts":false})
}

fn voice(channel: Option<&str>, mute: bool) -> Value {
    json!({"guild_id":GUILD,"user_id":"77","channel_id":channel,"session_id":"voice",
        "deaf":false,"mute":false,"self_deaf":false,"self_mute":mute,
        "self_video":false,"suppress":false,"member":member(GUILD,false,"12:00:00")})
}

async fn dispatch(
    pipeline: &Journey,
    store: &GatewaySessionStore,
    seq: u64,
    kind: &str,
    data: Value,
    clock: &str,
) -> DispatchAction {
    let saved = checkpoint("journey", seq, "ws://fixture");
    // Same guard as the shard runner: redelivery must not mutate cache/trackers.
    if dispatch_action(
        store.load().await.expect("checkpoint").as_ref(),
        &saved.session_id,
        seq,
    ) == DispatchAction::Duplicate
    {
        return DispatchAction::Duplicate;
    }
    let packet = json!({"op":0,"s":seq,"t":kind,"d":data}).to_string();
    let parsed = twilight_gateway::parse(packet, EventTypeFlags::all())
        .expect("scripted dispatch parses")
        .expect("known dispatch");
    pipeline.handle_at(&Event::from(parsed), &stamp(clock));
    store
        .commit_dispatch(&saved, pipeline.handlers().store().take_batch())
        .await
        .expect("commit pipeline effects")
}

async fn rows(db: &TestDb, sql: &str) -> Value {
    let rows: Vec<sqlx::types::Json<Value>> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .fetch_all(&db.pool)
        .await
        .expect("read persisted rows");
    Value::Array(rows.into_iter().map(|v| v.0).collect())
}

const EVENTS: &str = r#"SELECT to_jsonb(t) FROM (
    SELECT guild_id, member_id, event_type,
    to_char(occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS occurred_at,
    source, metadata::jsonb AS metadata, idempotency_key FROM events ORDER BY id
) t"#;

const MEMBERS: &str = r#"SELECT to_jsonb(t) FROM (
    SELECT guild_id, member_id, join_source, is_bot,
    to_char(joined_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS joined_at,
    to_char(first_message_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS first_message_at,
    to_char(third_message_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS third_message_at,
    to_char(first_voice_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS first_voice_at,
    to_char(last_active_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS last_active_at,
    to_char(left_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS left_at,
    to_char(inactive_flagged_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS inactive_flagged_at,
    to_char(gate_cleared_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS gate_cleared_at
    FROM members ORDER BY guild_id, member_id
) t"#;

const INVITES: &str = r#"SELECT to_jsonb(t) FROM (
    SELECT guild_id, code, uses, inviter_id, channel_id,
    to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS updated_at
    FROM invite_snapshots ORDER BY guild_id, code
) t"#;

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn gateway_member_journey_matches_legacy_golden() {
    let db = TestDb::new().await;
    let result = AssertUnwindSafe(member_journey(&db)).catch_unwind().await;
    db.close().await;
    if let Err(panic) = result {
        resume_unwind(panic);
    }
}

async fn member_journey(db: &TestDb) {
    let golden: Value = serde_json::from_str(include_str!("member_journey.golden.json"))
        .expect("hand-computed golden");
    let foreign = pipeline();
    let foreign_store = GatewaySessionStore::new(db.pool.clone(), "3333".into(), 0);
    dispatch(
        &foreign,
        &foreign_store,
        1,
        "INVITE_CREATE",
        invite("3333", "journey", "88"),
        "11:59:00",
    )
    .await;
    foreign
        .invite_source()
        .push(3333, vec![counter("journey", 8, 88)]);
    dispatch(
        &foreign,
        &foreign_store,
        2,
        "GUILD_MEMBER_ADD",
        member("3333", true, "11:59:30"),
        "11:59:30",
    )
    .await;

    let pipeline = pipeline();
    dispatch(
        &pipeline,
        &db.store,
        1,
        "INVITE_CREATE",
        invite(GUILD, "side", "99"),
        "11:59:40",
    )
    .await;
    dispatch(
        &pipeline,
        &db.store,
        2,
        "INVITE_CREATE",
        invite(GUILD, "journey", "99"),
        "11:59:50",
    )
    .await;
    assert_eq!(
        rows(db, INVITES).await,
        golden["invite_seed"],
        "creation must preserve other codes and guilds"
    );
    pipeline.invite_source().push(
        2222,
        vec![counter("journey", 1, 99), counter("side", 0, 99)],
    );
    dispatch(
        &pipeline,
        &db.store,
        3,
        "GUILD_MEMBER_ADD",
        member(GUILD, true, "12:00:00"),
        "12:00:05",
    )
    .await;
    let gates: i64 =
        sqlx::query_scalar("SELECT count(*) FROM events WHERE event_type = 'gate_cleared'")
            .fetch_one(&db.pool)
            .await
            .expect("gate count");
    assert_eq!(gates, 0, "pending joins are recorded without gate clear");
    assert_eq!(
        db.count().await,
        2,
        "one join per guild, no invite funnel event"
    );
    dispatch(
        &pipeline,
        &db.store,
        4,
        "GUILD_MEMBER_UPDATE",
        member(GUILD, false, "12:00:00"),
        "12:01:00",
    )
    .await;
    dispatch(
        &pipeline,
        &db.store,
        5,
        "GUILD_MEMBER_UPDATE",
        member(GUILD, false, "12:00:00"),
        "12:01:30",
    )
    .await;
    for (seq, id, clock) in [
        (6, "600", "12:02:00"),
        (7, "700", "12:03:00"),
        (8, "800", "12:04:00"),
        (9, "900", "12:05:00"),
    ] {
        // Observation time intentionally differs: message.timestamp is truth.
        dispatch(
            &pipeline,
            &db.store,
            seq,
            "MESSAGE_CREATE",
            message(id, clock),
            "12:06:00",
        )
        .await;
    }
    dispatch(
        &pipeline,
        &db.store,
        10,
        "VOICE_STATE_UPDATE",
        voice(Some("5555"), false),
        "12:10:00",
    )
    .await;
    dispatch(
        &pipeline,
        &db.store,
        11,
        "VOICE_STATE_UPDATE",
        voice(Some("5555"), true),
        "12:11:00",
    )
    .await;
    dispatch(
        &pipeline,
        &db.store,
        12,
        "VOICE_STATE_UPDATE",
        voice(Some("6666"), false),
        "12:20:00",
    )
    .await;
    assert_eq!(
        dispatch(
            &pipeline,
            &db.store,
            12,
            "VOICE_STATE_UPDATE",
            voice(None, false),
            "12:21:00"
        )
        .await,
        DispatchAction::Duplicate
    );
    dispatch(
        &pipeline,
        &db.store,
        13,
        "VOICE_STATE_UPDATE",
        voice(None, false),
        "12:25:30",
    )
    .await;
    dispatch(
        &pipeline,
        &db.store,
        14,
        "VOICE_STATE_UPDATE",
        voice(Some("5555"), false),
        "12:30:00",
    )
    .await;
    assert_eq!(
        pipeline
            .handlers()
            .voice_sessions
            .lock()
            .expect("voice")
            .open_count(),
        1
    );
    let before_resume = db.count().await;
    dispatch(&pipeline, &db.store, 15, "RESUMED", json!({}), "12:40:00").await;
    assert_eq!(
        pipeline
            .handlers()
            .voice_sessions
            .lock()
            .expect("voice")
            .open_count(),
        0,
        "resume drops unproven sessions"
    );
    assert_eq!(db.count().await, before_resume, "resume invents no end row");
    dispatch(
        &pipeline,
        &db.store,
        16,
        "VOICE_STATE_UPDATE",
        voice(None, false),
        "12:41:00",
    )
    .await;
    dispatch(
        &pipeline,
        &db.store,
        17,
        "GUILD_MEMBER_REMOVE",
        json!({"guild_id":GUILD,"user":user("77")}),
        "13:00:00",
    )
    .await;
    assert_eq!(rows(db, EVENTS).await, golden["events"]);
    assert_eq!(rows(db, MEMBERS).await, golden["members"]);
    assert_eq!(rows(db, INVITES).await, golden["invite_snapshots"]);
    assert_eq!(
        db.store
            .load()
            .await
            .expect("load")
            .expect("saved")
            .sequence,
        17
    );
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn gateway_invite_snapshot_transaction_and_read_failure_contract() {
    let db = TestDb::new().await;
    let result = AssertUnwindSafe(invite_snapshot_contract(&db))
        .catch_unwind()
        .await;
    db.close().await;
    if let Err(panic) = result {
        resume_unwind(panic);
    }
}

async fn invite_snapshot_contract(db: &TestDb) {
    let pipeline = pipeline();
    dispatch(
        &pipeline,
        &db.store,
        1,
        "INVITE_CREATE",
        invite(GUILD, "journey", "99"),
        "11:59:00",
    )
    .await;
    let baseline = rows(db, INVITES).await;
    // Failed/unavailable reads do not authorize deleting the previous snapshot.
    dispatch(
        &pipeline,
        &db.store,
        2,
        "GUILD_MEMBER_ADD",
        member(GUILD, true, "12:00:00"),
        "12:00:00",
    )
    .await;
    assert_eq!(rows(db, INVITES).await, baseline);
    let before = db.count().await;
    let before_members = rows(db, MEMBERS).await;
    // Snapshot failure occurs after the event/project writes, and rolls back all.
    let mut batch = FunnelBatch {
        events: vec![super::event(
            two_bot_core::EventType::MemberLeave,
            &stamp("12:01:00"),
        )],
        ..Default::default()
    };
    batch.invite_snapshots.push(InviteSnapshotWrite {
        guild_id: 2222,
        states: vec![counter("journey", 2, 99)],
        observed_at: "not-a-timestamp".into(),
        replace_all: true,
    });
    assert!(db
        .store
        .commit_dispatch(&checkpoint("journey", 3, "ws://fixture"), batch)
        .await
        .is_err());
    assert_eq!(db.count().await, before);
    assert_eq!(rows(db, MEMBERS).await, before_members);
    assert_eq!(rows(db, INVITES).await, baseline);
    assert_eq!(
        db.store
            .load()
            .await
            .expect("load")
            .expect("saved")
            .sequence,
        2
    );
    let foreign = FunnelBatch {
        invite_snapshots: vec![InviteSnapshotWrite {
            guild_id: 3333,
            states: vec![counter("journey", 2, 99)],
            observed_at: stamp("12:01:00"),
            replace_all: true,
        }],
        ..Default::default()
    };
    assert!(db
        .store
        .commit_dispatch(&checkpoint("journey", 3, "ws://fixture"), foreign)
        .await
        .is_err());
    assert_eq!(rows(db, INVITES).await, baseline);
    // A successful empty REST read, unlike None, really does remove stale codes.
    pipeline.invite_source().push(2222, vec![]);
    dispatch(
        &pipeline,
        &db.store,
        3,
        "GUILD_MEMBER_ADD",
        member(GUILD, true, "12:02:00"),
        "12:02:00",
    )
    .await;
    assert_eq!(rows(db, INVITES).await, json!([]));
}
