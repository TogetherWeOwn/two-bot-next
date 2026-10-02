#![cfg(feature = "db")]
//! Gateway + interaction integration uses agent-testdb/CI and mock Discord only.
#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::{sync::Arc, time::Duration};
use twilight_model::{application::interaction::Interaction, gateway::event::Event};
use two_bot_core::{
    leveling::{rank_text, LevelRoleReward},
    leveling_store as store, HandlerId, MemStore, OnboardingGates, OnboardingMode,
};
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
        "id":(4_000_000_000_000_000_000_u64 + seconds as u64).to_string(),"guild_id":GUILD.to_string(),"channel_id":CHANNEL.to_string(),"author":user(MEMBER),"content":"hello","timestamp":at(seconds).replace('Z', "+00:00"),"edited_timestamp":null,"tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],"attachments":[],"embeds":[],"pinned":false,"type":0,"components":[]
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
    let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
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
    for offset in 0..12_u64 {
        sqlx::query("INSERT INTO member_levels (guild_id,member_id,xp,imported_xp,updated_at) VALUES ($1,$2,$3,$3,NOW())")
            .bind(GUILD.to_string())
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
    assert_eq!(requests.len(), 4);
    let body: Vec<Value> = requests
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(
        body[0]["data"]["content"],
        rank_text("Global name", 0, None, 0, 0)
    );
    assert_eq!(
        body[1]["data"]["content"],
        rank_text("username", 0, None, 0, 0)
    );
    assert_eq!(body[0]["data"]["flags"], 64);
    assert_eq!(body[2]["data"]["content"], "No XP has been earned yet.");
    assert_eq!(body[3]["data"]["allowed_mentions"]["parse"], json!([]));
    assert_eq!(
        body[3]["data"]["content"].as_str().unwrap().lines().count(),
        11
    );
    assert!(body[3]["data"]["flags"].is_null());
    assert!(body[3]["data"]["components"].is_null());
    assert!(requests
        .iter()
        .all(|r| r.path.starts_with("/api/v10/interactions/7/")));
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
    assert_eq!(error.to_string(), "leveling Discord operation failed");
    let runtime = runtime(db.pool.clone(), &mock, OnboardingMode::Legacy);
    assert!(runtime
        .handle_interaction(&interaction("rank", false), HandlerId::Rank)
        .await
        .is_err());
    assert_eq!(mock.requests().len(), 2);
    mock.shutdown().await;
    db.close().await;
}
