#![cfg(feature = "db")]
//! Gateway + interaction integration uses agent-testdb/CI and mock Discord only.
#[allow(dead_code)]
mod common;

#[path = "support/deferred_capture_compat.rs"]
mod deferred_capture_compat;
use deferred_capture_compat::Healthy;

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::{sync::Arc, time::Duration};
use twilight_model::{application::interaction::Interaction, gateway::event::Event};
use two_bot_core::{
    leveling::{leaderboard_reply_for_season, rank_text, season_id_for_ms, LevelRoleReward},
    leveling_store as store, HandlerId, MemStore, OnboardingGates, OnboardingMode,
};
use two_bot_discord::leveling_runtime::LevelingRuntimeError;
use two_bot_discord::{
    pipeline::MessageEligibility, ActionExecutor, LevelingRuntime, OrderedLevelingPipeline,
};
use two_bot_testsupport::TestDatabase;

const GUILD: u64 = 1545644954272137297;
const MEMBER: u64 = 100000000000000001;
const ROLE: &str = "200000000000000001";
const CHANNEL: u64 = 300000000000000001;

fn at(seconds: i64) -> String {
    two_bot_core::format_iso_millis(1_786_771_200_000 + seconds * 1000)
}
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_millis() as i64
}
fn user(id: u64) -> Value {
    json!({"id":id.to_string(),"username":"username","global_name":"Global name","discriminator":"0","avatar":null,"bot":false})
}
fn member() -> Value {
    json!({"user":user(MEMBER),"roles":[],"joined_at":null,"deaf":false,"mute":false,"pending":false,"flags":0,"permissions":"0"})
}
fn runtime(pool: PgPool, mock: &MockRest, mode: OnboardingMode) -> LevelingRuntime {
    LevelingRuntime::new(
        pool,
        Arc::new(
            ActionExecutor::with_proxy("mock-only-token".into(), Some(mock.origin())).unwrap(),
        ),
        GUILD,
        OnboardingGates {
            mode,
            dry_run: false,
        },
    )
}
fn message(seconds: i64) -> Event {
    Event::MessageCreate(Box::new(serde_json::from_value(json!({
        "id":(4_000_000_000_000_000_000_u64 + seconds as u64).to_string(),"guild_id":GUILD.to_string(),"channel_id":CHANNEL.to_string(),"author":user(MEMBER),"content":"hello there","timestamp":at(seconds).replace('Z', "+00:00"),"edited_timestamp":null,"tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],"attachments":[],"embeds":[],"pinned":false,"type":0,"components":[]
    })).unwrap()))
}
fn voice(channel: Option<u64>) -> Event {
    Event::VoiceStateUpdate(Box::new(serde_json::from_value(json!({
        "guild_id":GUILD.to_string(),"channel_id":channel.map(|c|c.to_string()),"user_id":MEMBER.to_string(),"member":member(),"session_id":"mock-voice","deaf":false,"mute":false,"self_deaf":false,"self_mute":false,"self_stream":false,"self_video":false,"suppress":false
    })).unwrap()))
}
fn interaction(name: &str, target: bool) -> Interaction {
    let mut data = json!({"id":"1","name":name,"type":1,"options":[]});
    if target {
        data["options"] = json!([{"name":"member","type":6,"value":(MEMBER+1).to_string()}]);
        data["resolved"] = json!({"users":{(MEMBER+1).to_string():user(MEMBER+1)}});
        data["resolved"]["users"][&(MEMBER + 1).to_string()]["global_name"] = Value::Null;
    }
    serde_json::from_value(json!({"id":"7","application_id":"8","type":2,"token":"mock-callback","version":1,"guild_id":GUILD.to_string(),"member":member(),"locale":"en-US","data":data,"entitlements":[],"authorizing_integration_owners":{"0":GUILD.to_string()}})).unwrap()
}

struct TestDb {
    fixture: TestDatabase,
    pool: PgPool,
}
impl TestDb {
    async fn new() -> Self {
        let url = std::env::var("TWO_TEST_DATABASE_URL")
            .expect("explicit DB test requires TWO_TEST_DATABASE_URL (test bootstrap only)");
        let fixture = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create migrated test database; no credential fallback");
        let pool = fixture.pool().clone();
        Self { fixture, pool }
    }
    async fn seed(&self, xp: i64) {
        sqlx::query(
            "INSERT INTO member_levels (guild_id,member_id,xp,imported_xp,updated_at) VALUES ($1,$2,$3,$3,NOW())",
        )
        .bind(GUILD.to_string())
        .bind(MEMBER.to_string())
        .bind(xp)
        .execute(&self.pool)
        .await
        .unwrap();
        store::replace_role_rewards(
            &self.pool,
            &GUILD.to_string(),
            &[LevelRoleReward {
                level: 1,
                role_id: ROLE.into(),
            }],
        )
        .await
        .unwrap();
    }
    async fn close(self) {
        self.fixture.close().await.expect("drop test database");
    }
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or credential-free CI service"]
async fn routed_rank_optional_member_and_fixed_public_top_ten_match_legacy() {
    let db = TestDb::new().await;
    let edit = || ScriptedResponse::json(200, json!({"id": "99"}));
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            edit(),
            ScriptedResponse::status(204),
            edit(),
            ScriptedResponse::status(204),
            edit(),
            ScriptedResponse::status(204),
            edit(),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(db.pool.clone(), &mock, OnboardingMode::Legacy);
    runtime
        .handle_interaction(&interaction("rank", false), HandlerId::Rank)
        .await
        .unwrap();
    runtime
        .handle_interaction(&interaction("rank", true), HandlerId::Rank)
        .await
        .unwrap();
    runtime
        .handle_interaction(&interaction("leaderboard", false), HandlerId::Leaderboard)
        .await
        .unwrap();
    // The routed board reads the current season only: lifetime stays empty,
    // so every row below must come from `member_season_xp`.
    let season = season_id_for_ms(now_ms());
    for offset in 0..12_u64 {
        sqlx::query("INSERT INTO member_season_xp (guild_id,season_id,member_id,seasonal_xp,updated_at) VALUES ($1,$2,$3,$4,NOW())")
            .bind(GUILD.to_string())
            .bind(&season)
            .bind((MEMBER + offset).to_string())
            .bind(100_i64 + offset as i64)
            .execute(&db.pool)
            .await
            .unwrap();
    }
    runtime
        .handle_interaction(&interaction("leaderboard", false), HandlerId::Leaderboard)
        .await
        .unwrap();
    let mut wrong_guild = interaction("rank", false);
    wrong_guild.guild_id = Some(twilight_model::id::Id::new(1234));
    assert!(!runtime
        .handle_interaction(&wrong_guild, HandlerId::Rank)
        .await
        .unwrap());
    assert!(!runtime
        .handle_interaction(&interaction("feed-list", false), HandlerId::FeedList)
        .await
        .unwrap());
    let requests = mock.requests();
    assert_eq!(
        requests.len(),
        8,
        "one defer plus one completion edit per routed command"
    );
    let callbacks: Vec<_> = requests
        .iter()
        .filter(|r| r.method == "POST" && r.path.ends_with("/callback"))
        .collect();
    let edits: Vec<_> = requests
        .iter()
        .filter(|r| r.method == "PATCH" && r.path.ends_with("/messages/@original"))
        .collect();
    assert_eq!(callbacks.len(), 4, "one acknowledgement per command");
    assert_eq!(edits.len(), 4, "one completion edit per command");
    let defers: Vec<Value> = callbacks
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    for defer in &defers {
        assert_eq!(defer["type"], 5, "leveling acknowledges with a defer");
    }
    assert_eq!(defers[0]["data"]["flags"], 64, "rank defers ephemerally");
    assert_eq!(defers[1]["data"]["flags"], 64, "rank defers ephemerally");
    assert_ne!(
        defers[2]["data"]["flags"], 64,
        "leaderboard defers publicly"
    );
    assert_ne!(
        defers[3]["data"]["flags"], 64,
        "leaderboard defers publicly"
    );
    let body: Vec<Value> = edits
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(body[0]["content"], rank_text("Global name", 0, None, 0, 0));
    assert_eq!(body[1]["content"], rank_text("username", 0, None, 0, 0));
    assert_eq!(
        body[2]["content"],
        leaderboard_reply_for_season(&[], &season).content
    );
    assert_eq!(body[3]["allowed_mentions"]["parse"], json!([]));
    assert_eq!(
        body[3]["content"].as_str().unwrap().lines().count(),
        11,
        "no invented paging on the completed board"
    );
    assert!(callbacks
        .iter()
        .all(|r| r.path.starts_with("/api/v10/interactions/7/")));
    assert!(edits
        .iter()
        .all(|r| r.path == "/api/v10/webhooks/8/mock-callback/messages/@original"));
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires approved agent-testdb or credential-free CI service"]
async fn ordered_gateway_concurrent_sources_award_once_and_read_back_rewards() {
    let db = TestDb::new().await;
    db.seed(90).await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!({"roles":[]})),
            ScriptedResponse::status(204),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let pipeline = OrderedLevelingPipeline::new(
        MemStore::new(),
        Some(runtime(db.pool.clone(), &mock, OnboardingMode::Legacy)),
    );
    pipeline
        .handle_at(&voice(Some(CHANNEL)), &at(0), MessageEligibility::default())
        .await
        .unwrap();
    let msg = message(120);
    let leave = voice(None);
    let instant = at(120);
    let (message_award, voice_award) = tokio::join!(
        pipeline.handle_at(&msg, &instant, MessageEligibility::default()),
        pipeline.handle_at(&leave, &instant, MessageEligibility::default())
    );
    let message_award = message_award.unwrap();
    let voice_award = voice_award.unwrap();
    assert_eq!(message_award[0].awarded, 15);
    assert_eq!(voice_award[0].awarded, 10);
    assert_eq!(
        usize::from(message_award[0].leveled_up) + usize::from(voice_award[0].leveled_up),
        1
    );
    assert_eq!(
        pipeline
            .handle_at(&msg, &at(120), MessageEligibility::default())
            .await
            .unwrap()[0]
            .awarded,
        0
    );
    assert!(pipeline
        .handle_at(&leave, &at(120), MessageEligibility::default())
        .await
        .unwrap()
        .is_empty());
    let profile = store::profile(&db.pool, &GUILD.to_string(), &MEMBER.to_string())
        .await
        .unwrap();
    assert_eq!(
        (
            profile.xp,
            profile.message_xp,
            profile.voice_xp,
            profile.imported_xp
        ),
        (115, 15, 10, 90)
    );
    let awards: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM xp_awards")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(awards, 2);
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[1].method, "PUT");
    assert!(requests[1].path.ends_with(&format!("/roles/{ROLE}")));
    // Fresh member-role readback produces no grants after the first plan applied.
    let ladder = store::role_rewards(&db.pool, &GUILD.to_string())
        .await
        .unwrap();
    assert!(two_bot_core::leveling::plan_reward_roles(
        profile.level,
        &ladder,
        &[ROLE.into()],
        true,
        false
    )
    .unwrap()
    .grant
    .is_empty());
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or credential-free CI service"]
async fn eligibility_unknown_duration_reconnect_and_session_mode_are_preserved() {
    let db = TestDb::new().await;
    db.seed(90).await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let pipeline = OrderedLevelingPipeline::new(
        MemStore::new(),
        Some(runtime(db.pool.clone(), &mock, OnboardingMode::Session)),
    );
    for eligibility in [
        MessageEligibility {
            capture_only: true,
            is_staff_automation: false,
        },
        MessageEligibility {
            capture_only: false,
            is_staff_automation: true,
        },
    ] {
        assert!(pipeline
            .handle_at(&message(0), &at(0), eligibility)
            .await
            .unwrap()
            .is_empty());
    }
    let mut bot = message(0);
    if let Event::MessageCreate(m) = &mut bot {
        m.author.bot = true;
    }
    let mut webhook = message(0);
    if let Event::MessageCreate(m) = &mut webhook {
        m.webhook_id = Some(twilight_model::id::Id::new(7777));
    }
    let mut dm = message(0);
    if let Event::MessageCreate(m) = &mut dm {
        m.guild_id = None;
    }
    for event in [bot, webhook, dm] {
        assert!(pipeline
            .handle_at(&event, &at(0), MessageEligibility::default())
            .await
            .unwrap()
            .is_empty());
    }
    pipeline
        .handle_at(&voice(Some(CHANNEL)), &at(0), MessageEligibility::default())
        .await
        .unwrap();
    pipeline.handle(&Event::Resumed).await.unwrap();
    assert!(pipeline
        .handle_at(&voice(None), &at(600), MessageEligibility::default())
        .await
        .unwrap()
        .is_empty());
    let award = pipeline
        .handle_at(&message(600), &at(600), MessageEligibility::default())
        .await
        .unwrap();
    assert!(award[0].leveled_up);
    assert_eq!(award[0].awarded, 15);
    assert!(mock.requests().is_empty());
    let profile = store::profile(&db.pool, &GUILD.to_string(), &MEMBER.to_string())
        .await
        .unwrap();
    assert_eq!(profile.voice_xp, 0);
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or credential-free CI service"]
async fn dry_run_and_guild_fence_preserve_xp_without_private_interaction_dispatch() {
    let db = TestDb::new().await;
    db.seed(90).await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
    let runtime = LevelingRuntime::new(
        db.pool.clone(),
        Arc::new(
            ActionExecutor::with_proxy("mock-only-token".into(), Some(mock.origin())).unwrap(),
        ),
        GUILD,
        OnboardingGates {
            mode: OnboardingMode::Legacy,
            dry_run: true,
        },
    );
    let pipeline = OrderedLevelingPipeline::new(MemStore::new(), Some(runtime));
    let mut wrong_guild = message(0);
    if let Event::MessageCreate(m) = &mut wrong_guild {
        m.guild_id = Some(twilight_model::id::Id::new(1234));
    }
    assert!(pipeline.handle(&wrong_guild).await.unwrap().is_empty());
    let awards = pipeline.handle(&message(0)).await.unwrap();
    assert_eq!(awards[0].awarded, 15);
    assert!(awards[0].leveled_up);
    assert!(mock.requests().is_empty());
    let event = Event::InteractionCreate(Box::new(
        twilight_model::gateway::payload::incoming::InteractionCreate(interaction("rank", false)),
    ));
    assert!(pipeline.handle(&event).await.unwrap().is_empty());
    // Only CommandRuntime may route/respond; ordered awards must stay silent.
    assert!(mock.requests().is_empty());
    let profile = store::profile(&db.pool, &GUILD.to_string(), &MEMBER.to_string())
        .await
        .unwrap();
    assert_eq!(profile.xp, 105);
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires approved agent-testdb or credential-free CI service"]
async fn slow_reward_reads_yield_tokio_and_do_not_reorder_later_member_events() {
    let db = TestDb::new().await;
    db.seed(90).await;
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, json!({"roles":[ROLE]}))
            .delayed(Duration::from_millis(300))],
        ScriptedResponse::status(500),
    )
    .await;
    let pipeline = OrderedLevelingPipeline::new(
        MemStore::new(),
        Some(runtime(db.pool.clone(), &mock, OnboardingMode::Legacy)),
    );
    let first = message(0);
    let second = message(60);
    let first_at = at(0);
    let second_at = at(60);
    let sequence = async {
        let (a, b) = tokio::join!(
            pipeline.handle_at(&first, &first_at, MessageEligibility::default()),
            pipeline.handle_at(&second, &second_at, MessageEligibility::default())
        );
        assert_eq!(a.unwrap()[0].total_xp, 105);
        assert_eq!(b.unwrap()[0].total_xp, 120);
    };
    tokio::pin!(sequence);
    tokio::select! { _ = &mut sequence => panic!("slow read completed before timer"), _ = tokio::time::sleep(Duration::from_millis(50)) => {} }
    sequence.await;
    let rows: Vec<i64> = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM occurred_at)::bigint FROM xp_awards ORDER BY id",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(rows[1] - rows[0], 60);
    assert_eq!(mock.requests().len(), 1);
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or credential-free CI service"]
async fn gateway_reward_and_interaction_executor_errors_propagate() {
    let db = TestDb::new().await;
    db.seed(90).await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(403)).await;
    let pipeline = OrderedLevelingPipeline::new(
        MemStore::new(),
        Some(runtime(db.pool.clone(), &mock, OnboardingMode::Legacy)),
    );
    let error = pipeline
        .handle_at(&message(0), &at(0), MessageEligibility::default())
        .await
        .unwrap_err();
    // The held-roles read fails after the award committed: the error says so
    // and carries the committed award.
    assert_eq!(
        error.to_string(),
        "leveling role application failed after the XP award committed: role application Discord operation failed"
    );
    assert_eq!(error.committed_awards().len(), 1);
    assert!(error.committed_awards()[0].leveled_up);
    let runtime = runtime(db.pool.clone(), &mock, OnboardingMode::Legacy);
    assert!(runtime
        .handle_interaction(&interaction("rank", false), HandlerId::Rank)
        .await
        .is_err());
    assert_eq!(mock.requests().len(), 2);
    mock.shutdown().await;
    db.close().await;
}

/// Gateway voice capture: a human join inserts one
/// `voice_session_started` fact keyed `voice-start:{session_key}`, the leave
/// inserts one honestly-measured `voice_session_ended` fact (330 s for the
/// 5.5-minute session) carrying `sessionKey`, `startedAt`,
/// `durationSeconds` and `startKnown:true`, and a redelivered join at the
/// original stamp dedupes to zero inserts.
#[tokio::test]
#[ignore = "requires approved agent-testdb or credential-free CI service"]
async fn community_voice_fact_capture_round_trip() {
    let db = TestDb::new().await;
    let pipeline = OrderedLevelingPipeline::new(MemStore::new(), None);
    pipeline.enable_community_facts(db.pool.clone()).healthy();
    const VOICE_CHANNEL: u64 = 300000000000000002;

    // Join: one start fact, and the durable session key travels with it.
    let requests = pipeline
        .collect_at(
            &voice(Some(VOICE_CHANNEL)),
            &at(10),
            MessageEligibility::default(),
        )
        .healthy();
    pipeline.drain(requests).await.unwrap();
    assert_eq!(pipeline.drain_facts().await.unwrap(), 1);
    let (session_key, metadata): (String, String) = sqlx::query_as(
        "SELECT source_event_id, metadata FROM community_facts WHERE guild_id = $1 AND event_type = 'voice_session_started'",
    )
    .bind(GUILD.to_string())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert!(
        session_key.starts_with(&format!("{GUILD}:{MEMBER}:")),
        "session key binds guild and member"
    );
    assert!(
        session_key.ends_with(&format!(":{VOICE_CHANNEL}")),
        "session key binds channel"
    );
    assert!(
        metadata.contains(&format!("\"sessionKey\":\"{session_key}\"")),
        "start carries its session key"
    );

    // Leave 330 s later: one measured end with honest duration math.
    let requests = pipeline
        .collect_at(&voice(None), &at(340), MessageEligibility::default())
        .healthy();
    pipeline.drain(requests).await.unwrap();
    assert_eq!(pipeline.drain_facts().await.unwrap(), 1);

    // Redelivered join at the original stamp reuses the same session key, so
    // the `voice-start:{session_key}` row dedupes to zero inserts (a
    // same-channel frame without a leave would never reach the sink at all).
    let requests = pipeline
        .collect_at(
            &voice(Some(VOICE_CHANNEL)),
            &at(10),
            MessageEligibility::default(),
        )
        .healthy();
    pipeline.drain(requests).await.unwrap();
    assert_eq!(pipeline.drain_facts().await.unwrap(), 0);
    let row: (String, String, String) = sqlx::query_as(
        "SELECT event_type, source_event_id, metadata FROM community_facts WHERE guild_id = $1 AND event_type = 'voice_session_ended'",
    )
    .bind(GUILD.to_string())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(row.1, session_key, "end reuses the start's session key");
    for field in [
        format!("\"sessionKey\":\"{session_key}\"").as_str(),
        "\"durationSeconds\":330",
        "\"startKnown\":true",
    ] {
        assert!(row.2.contains(field), "end carries expected field");
    }

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM community_facts")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, 2, "one start plus one end, no double row");
    db.close().await;
}

/// Gateway `MessageCreate` capture: a human message inserts one
/// `message_created` fact with its classifier verdict, a duplicate delivery
/// inserts nothing, a bot message is captured as `bot` while queuing no
/// award, and a DM inserts nothing.
#[tokio::test]
#[ignore = "requires approved agent-testdb or credential-free CI service"]
async fn community_message_fact_capture_round_trip() {
    let db = TestDb::new().await;
    let pipeline = OrderedLevelingPipeline::new(MemStore::new(), None);
    pipeline.enable_community_facts(db.pool.clone()).healthy();

    // Human message: one fact, and the classifier verdict travels with it.
    let requests = pipeline
        .collect_at(&message(10), &at(10), MessageEligibility::default())
        .healthy();
    pipeline.drain(requests).await.unwrap();
    assert_eq!(pipeline.drain_facts().await.unwrap(), 1);
    let row: (String, String) =
        sqlx::query_as("SELECT classification, metadata FROM community_facts WHERE guild_id = $1")
            .bind(GUILD.to_string())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(row.0, "eligible_human");
    assert!(row.1.contains("\"channelClass\""), "channel class travels");
    assert!(!row.1.contains("hello there"), "no message content stored");

    // Duplicate delivery: the same message id dedupes to zero inserts.
    let requests = pipeline
        .collect_at(&message(10), &at(10), MessageEligibility::default())
        .healthy();
    pipeline.drain(requests).await.unwrap();
    assert_eq!(pipeline.drain_facts().await.unwrap(), 0);

    // Bot message: captured as `bot` but queues no award.
    let mut bot = message(11);
    if let Event::MessageCreate(m) = &mut bot {
        m.author.bot = true;
    }
    let requests = pipeline
        .collect_at(&bot, &at(11), MessageEligibility::default())
        .healthy();
    assert!(requests.is_empty(), "bots never funnel-count");
    pipeline.drain(requests).await.unwrap();
    assert_eq!(pipeline.drain_facts().await.unwrap(), 1);

    // DM: dropped in the pipeline, never reaches the sink.
    let mut dm = message(12);
    if let Event::MessageCreate(m) = &mut dm {
        m.guild_id = None;
    }
    let requests = pipeline
        .collect_at(&dm, &at(12), MessageEligibility::default())
        .healthy();
    pipeline.drain(requests).await.unwrap();
    assert_eq!(pipeline.drain_facts().await.unwrap(), 0);

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM community_facts")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, 2, "human plus bot rows, no double row, no DM row");
    let bot_class: String =
        sqlx::query_scalar("SELECT classification FROM community_facts WHERE source_event_id = $1")
            .bind((4_000_000_000_000_000_000_u64 + 11).to_string())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(bot_class, "bot");
    db.close().await;
}

// Foreign-guild zero-rows proof: reply silence alone does not prove that a
// foreign guild left no persistence behind. These tests drive every leveling
// entry point (`/rank`, `/leaderboard`, message XP, voice XP) from a guild
// other than the runtime's configured guild and assert the store stays
// empty, while an own-guild control proves the same composition still
// writes. A test that would catch a removed guild fence must see rows here.
const FOREIGN_GUILD: u64 = 1111111111111111111;

async fn leveling_row_counts(pool: &PgPool, guild: u64) -> (i64, i64) {
    sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM member_levels WHERE guild_id = $1),
                (SELECT COUNT(*) FROM xp_awards WHERE guild_id = $1)",
    )
    .bind(guild.to_string())
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn total_leveling_rows(pool: &PgPool) -> (i64, i64) {
    sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM member_levels),
                (SELECT COUNT(*) FROM xp_awards)",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

fn foreign_interaction(name: &str, target: bool) -> Interaction {
    let mut interaction = interaction(name, target);
    interaction.guild_id = Some(twilight_model::id::Id::new(FOREIGN_GUILD));
    interaction
}

fn foreign_message(seconds: i64) -> Event {
    let mut event = message(seconds);
    if let Event::MessageCreate(m) = &mut event {
        m.guild_id = Some(twilight_model::id::Id::new(FOREIGN_GUILD));
    }
    event
}

fn foreign_voice(channel: Option<u64>) -> Event {
    let mut event = voice(channel);
    if let Event::VoiceStateUpdate(v) = &mut event {
        v.guild_id = Some(twilight_model::id::Id::new(FOREIGN_GUILD));
    }
    event
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or credential-free CI service"]
async fn foreign_guild_rank_and_leaderboard_leave_zero_rows_and_no_reply() {
    let db = TestDb::new().await;
    let edit = || ScriptedResponse::json(200, json!({"id": "99"}));
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            edit(),
            ScriptedResponse::status(204),
            edit(),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(db.pool.clone(), &mock, OnboardingMode::Legacy);

    // Foreign-guild /rank (self and targeted) and /leaderboard, plus a
    // missing-guild /rank: none is handled, none replies, none writes.
    for (interaction, handler) in [
        (foreign_interaction("rank", false), HandlerId::Rank),
        (foreign_interaction("rank", true), HandlerId::Rank),
        (
            foreign_interaction("leaderboard", false),
            HandlerId::Leaderboard,
        ),
    ] {
        assert!(
            !runtime
                .handle_interaction(&interaction, handler)
                .await
                .unwrap(),
            "foreign-guild interaction must not be handled"
        );
    }
    let mut guildless = interaction("rank", false);
    guildless.guild_id = None;
    assert!(!runtime
        .handle_interaction(&guildless, HandlerId::Rank)
        .await
        .unwrap());
    assert!(
        mock.requests().is_empty(),
        "foreign-guild interactions send no reply"
    );
    assert_eq!(leveling_row_counts(&db.pool, FOREIGN_GUILD).await, (0, 0));
    assert_eq!(
        total_leveling_rows(&db.pool).await,
        (0, 0),
        "foreign-guild interactions leave zero member_levels/xp_awards rows"
    );

    // Own-guild control: the same handlers run and reply, and the read paths
    // themselves still write nothing.
    assert!(runtime
        .handle_interaction(&interaction("rank", false), HandlerId::Rank)
        .await
        .unwrap());
    assert!(runtime
        .handle_interaction(&interaction("leaderboard", false), HandlerId::Leaderboard)
        .await
        .unwrap());
    assert_eq!(
        mock.requests().len(),
        4,
        "one defer plus one completion edit per own-guild command"
    );
    assert_eq!(leveling_row_counts(&db.pool, GUILD).await, (0, 0));
    assert_eq!(total_leveling_rows(&db.pool).await, (0, 0));
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or credential-free CI service"]
async fn foreign_guild_message_and_voice_xp_leave_zero_rows_own_guild_writes() {
    let db = TestDb::new().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let pipeline = OrderedLevelingPipeline::new(
        MemStore::new(),
        Some(runtime(db.pool.clone(), &mock, OnboardingMode::Legacy)),
    );

    // Foreign-guild message XP: no award, no rows.
    assert!(pipeline
        .handle_at(&foreign_message(0), &at(0), MessageEligibility::default())
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        total_leveling_rows(&db.pool).await,
        (0, 0),
        "foreign-guild message leaves zero rows"
    );

    // Foreign-guild voice XP: a full ten-minute session awards nothing and
    // writes nothing.
    pipeline
        .handle_at(
            &foreign_voice(Some(CHANNEL)),
            &at(60),
            MessageEligibility::default(),
        )
        .await
        .unwrap();
    assert!(pipeline
        .handle_at(
            &foreign_voice(None),
            &at(660),
            MessageEligibility::default()
        )
        .await
        .unwrap()
        .is_empty());
    assert_eq!(leveling_row_counts(&db.pool, FOREIGN_GUILD).await, (0, 0));
    assert_eq!(
        total_leveling_rows(&db.pool).await,
        (0, 0),
        "foreign-guild voice session leaves zero rows"
    );

    // Own-guild control: the same pipeline and runtime do write. One message
    // (15 XP) and one ten-minute voice session (10 min * 5 XP = 50 XP) land
    // as one member_levels row and two xp_awards rows.
    let message_awards = pipeline
        .handle_at(&message(1200), &at(1200), MessageEligibility::default())
        .await
        .unwrap();
    assert_eq!(message_awards.len(), 1);
    assert_eq!(message_awards[0].awarded, 15);
    pipeline
        .handle_at(
            &voice(Some(CHANNEL)),
            &at(1300),
            MessageEligibility::default(),
        )
        .await
        .unwrap();
    let voice_awards = pipeline
        .handle_at(&voice(None), &at(1900), MessageEligibility::default())
        .await
        .unwrap();
    assert_eq!(voice_awards.len(), 1);
    assert_eq!(voice_awards[0].awarded, 50);
    assert_eq!(leveling_row_counts(&db.pool, GUILD).await, (1, 2));
    assert_eq!(total_leveling_rows(&db.pool).await, (1, 2));
    let own_xp: i64 = sqlx::query_scalar("SELECT xp FROM member_levels WHERE guild_id = $1")
        .bind(GUILD.to_string())
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(own_xp, 65);

    // Foreign attempts after the control add nothing and leave the
    // own-guild row untouched.
    assert!(pipeline
        .handle_at(
            &foreign_message(2000),
            &at(2000),
            MessageEligibility::default()
        )
        .await
        .unwrap()
        .is_empty());
    pipeline
        .handle_at(
            &foreign_voice(Some(CHANNEL)),
            &at(2100),
            MessageEligibility::default(),
        )
        .await
        .unwrap();
    assert!(pipeline
        .handle_at(
            &foreign_voice(None),
            &at(2700),
            MessageEligibility::default()
        )
        .await
        .unwrap()
        .is_empty());
    assert_eq!(leveling_row_counts(&db.pool, FOREIGN_GUILD).await, (0, 0));
    assert_eq!(total_leveling_rows(&db.pool).await, (1, 2));
    let own_xp_after: i64 = sqlx::query_scalar("SELECT xp FROM member_levels WHERE guild_id = $1")
        .bind(GUILD.to_string())
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(own_xp_after, own_xp);
    mock.shutdown().await;
    db.close().await;
}

// Short-message rule from real gateway metadata: a trimmed message shorter
// than 8 Unicode scalars with no embed/attachment earns zero while
// consuming the message cooldown once. These tests drive real
// `MessageCreate` events through the ordered pipeline into the store, so
// the rule is proven gateway-to-store, not only in the pure policy. They
// pin the existing public seams only (pipeline, store rows); the builder
// chooses how the minimal transient shape is threaded internally. Message
// bodies must never be persisted or logged to satisfy them.

fn shaped_message(seconds: i64, content: &str, variant: &str) -> Event {
    let mut value = json!({
        "id":(4_000_000_000_000_000_000_u64 + seconds as u64).to_string(),"guild_id":GUILD.to_string(),"channel_id":CHANNEL.to_string(),"author":user(MEMBER),"content":content,"timestamp":at(seconds).replace('Z', "+00:00"),"edited_timestamp":null,"tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],"attachments":[],"embeds":[],"pinned":false,"type":0,"components":[]
    });
    match variant {
        "attachment" => {
            value["attachments"] = json!([{
                "id": "888888888888888888", "filename": "note.txt", "size": 1,
                "url": "http://mock.invalid/file", "proxy_url": "http://mock.invalid/file"
            }]);
        }
        "embed" => {
            value["embeds"] = json!([{"type": "link", "url": "http://mock.invalid/"}]);
        }
        _ => {}
    }
    Event::MessageCreate(Box::new(serde_json::from_value(value).unwrap()))
}

fn delivered_pipeline(pool: PgPool, mock: &MockRest) -> OrderedLevelingPipeline<MemStore> {
    let pipeline = OrderedLevelingPipeline::new(
        MemStore::new(),
        Some(runtime(pool, mock, OnboardingMode::Legacy)),
    );
    // The shard feeding these events subscribed MESSAGE_CONTENT.
    pipeline.set_message_content_delivered(true);
    pipeline
}

async fn message_award_rows(pool: &PgPool) -> Vec<(i32, Option<String>)> {
    sqlx::query_as("SELECT xp, decision FROM xp_awards WHERE source = 'message' ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or credential-free CI service"]
async fn short_gateway_message_earns_zero_and_consumes_cooldown_once() {
    let db = TestDb::new().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let pipeline = delivered_pipeline(db.pool.clone(), &mock);

    // Seven trimmed scalars, no embed/attachment: earns zero.
    let short = pipeline
        .handle_at(
            &shaped_message(0, "1234567", "plain"),
            &at(0),
            MessageEligibility::default(),
        )
        .await
        .unwrap();
    assert_eq!(short.len(), 1, "the award attempt still resolves");
    assert_eq!(
        short[0].awarded, 0,
        "a 7-char gateway message earns zero XP"
    );
    let profile = store::profile(&db.pool, &GUILD.to_string(), &MEMBER.to_string())
        .await
        .unwrap();
    assert_eq!(
        (
            profile.xp,
            profile.message_xp,
            profile.voice_xp,
            profile.imported_xp
        ),
        (0, 0, 0, 0),
        "a short message grants no lifetime XP"
    );

    // The short message consumed the cooldown once: a valid follow-up ten
    // seconds later is still blocked, and that blocked attempt must not
    // restart the clock.
    let blocked = pipeline
        .handle_at(
            &shaped_message(10, "12345678", "plain"),
            &at(10),
            MessageEligibility::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        blocked[0].awarded, 0,
        "a quick valid follow-up cannot bypass the consumed cooldown"
    );

    // Exactly 60 s after the short message the cooldown has lapsed.
    let after = pipeline
        .handle_at(
            &shaped_message(60, "12345678", "plain"),
            &at(60),
            MessageEligibility::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        after[0].awarded, 15,
        "an 8-char message after the cooldown earns the normal award"
    );

    // Audit semantics: the short attempt leaves exactly one decision-marked
    // 0-XP row naming the short-message reason; the cooldown-blocked
    // follow-up stays silent; the valid message leaves one granted row.
    let rows = message_award_rows(&db.pool).await;
    assert_eq!(rows.len(), 2, "short audit row plus one granted row");
    assert_eq!(rows[0].0, 0, "the short audit row grants no XP");
    assert_eq!(
        rows[0].1.as_deref(),
        Some("short_message"),
        "the short attempt is decision-marked as short-message: {rows:?}"
    );
    assert_eq!(rows[1], (15, None), "the granted row carries no decision");

    let profile = store::profile(&db.pool, &GUILD.to_string(), &MEMBER.to_string())
        .await
        .unwrap();
    assert_eq!((profile.xp, profile.message_xp), (15, 15));
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or credential-free CI service"]
async fn short_message_boundaries_use_trimmed_scalar_counts() {
    let db = TestDb::new().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let pipeline = delivered_pipeline(db.pool.clone(), &mock);

    // (seconds, content, expected award). Messages sit 700 s apart: outside
    // the 60 s cooldown and outside the 10-minute same-channel burst
    // window, and both controls land inside the first five daily ladder
    // rungs, so each expectation isolates the short-message rule alone.
    let cases: [(i64, &str, u64); 7] = [
        // Known empty content delivered by the gateway: short.
        (0, "", 0),
        // Whitespace-only trims to empty: short.
        (700, "   \t  ", 0),
        // Eight multibyte scalars (32 UTF-8 bytes): control earns.
        (
            1400,
            "\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}",
            15,
        ),
        // Padding does not count: 7 trimmed scalars behind spaces is short.
        (2100, "  1234567  ", 0),
        // Exactly 8 trimmed scalars behind padding: control earns.
        (2800, "  12345678  ", 15),
        // Seven multibyte scalars (10 UTF-8 bytes): short by scalar count,
        // even though the byte length clears the floor.
        (3500, "caf\u{00E9} \u{2615}x", 0),
        // Seven trimmed scalars with an interior space: still short.
        (4200, "abc def", 0),
    ];
    for (seconds, content, expected) in cases {
        let awards = pipeline
            .handle_at(
                &shaped_message(seconds, content, "plain"),
                &at(seconds),
                MessageEligibility::default(),
            )
            .await
            .unwrap();
        assert_eq!(awards.len(), 1);
        assert_eq!(
            awards[0].awarded,
            expected,
            "content with {} trimmed scalars at {seconds}s",
            content.trim().chars().count()
        );
    }
    let granted: i64 = sqlx::query_scalar("SELECT COALESCE(SUM(xp), 0)::bigint FROM xp_awards")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(granted, 30, "only the two >=8-scalar controls earn");
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or credential-free CI service"]
async fn embed_and_attachment_messages_keep_existing_award_behavior() {
    let db = TestDb::new().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let pipeline = delivered_pipeline(db.pool.clone(), &mock);

    // Empty content with an attachment is not a short message.
    let attached = pipeline
        .handle_at(
            &shaped_message(0, "", "attachment"),
            &at(0),
            MessageEligibility::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        attached[0].awarded, 15,
        "an attachment-only message keeps the existing award"
    );

    // A 2-char body with an embed is not a short message either.
    let embedded = pipeline
        .handle_at(
            &shaped_message(120, "hi", "embed"),
            &at(120),
            MessageEligibility::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        embedded[0].awarded, 15,
        "a short body with an embed keeps the existing award"
    );

    assert_eq!(
        message_award_rows(&db.pool).await,
        vec![(15, None), (15, None)],
        "both controls leave plain granted rows, no short-message audit row"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or credential-free CI service"]
async fn unavailable_content_keeps_unavailable_not_known_short_behavior() {
    let db = TestDb::new().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    // No set_message_content_delivered(true): this pipeline's shard did not
    // subscribe MESSAGE_CONTENT, so the shape is unknown rather than a
    // known empty/short message. Existing unavailable-content behavior is
    // preserved: the message awards as before, with no new intent asked.
    let pipeline = OrderedLevelingPipeline::new(
        MemStore::new(),
        Some(runtime(db.pool.clone(), &mock, OnboardingMode::Legacy)),
    );

    let unknown = pipeline
        .handle_at(
            &shaped_message(0, "hi", "plain"),
            &at(0),
            MessageEligibility::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        unknown[0].awarded, 15,
        "unknown shape (content not delivered) keeps the existing award"
    );
    assert_eq!(
        message_award_rows(&db.pool).await,
        vec![(15, None)],
        "no short-message audit row for an unknown shape"
    );
    mock.shutdown().await;
    db.close().await;
}

// Hermetic double-failure precedence (no database: the refused loopback pool
// fails the read at once, like the bot crate's immediate-error acceptance).
// The acknowledged store error is the root cause the caller must log, so it
// propagates even when the best-effort completion edit fails alongside it.
#[tokio::test]
async fn acknowledged_store_failure_reports_the_store_error_when_the_completion_edit_fails() {
    let pool = PgPoolOptions::new()
        .min_connections(0)
        .max_connections(1)
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("lazy pool");
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204), ScriptedResponse::status(500)],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(pool, &mock, OnboardingMode::Legacy);
    let error = runtime
        .handle_interaction(&interaction("rank", false), HandlerId::Rank)
        .await
        .expect_err("the double failure still reports");
    assert!(
        matches!(error, LevelingRuntimeError::Store(_)),
        "the store error wins over the failed completion edit, got {error:?}"
    );
    assert_eq!(
        mock.requests().len(),
        2,
        "one defer plus one completion-edit attempt"
    );
    mock.shutdown().await;
}
