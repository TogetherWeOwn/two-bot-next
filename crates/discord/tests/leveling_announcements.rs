#![cfg(feature = "db")]
//! Level-up channel announcements: failing-first protected acceptance.
//!
//! Contract (roadmap item 2; paired builder card builds on this branch and
//! must never edit this file):
//! - A message-XP award that crosses a level boundary posts exactly one
//!   mention-safe celebration in the source channel. The copy names the new
//!   level as `level {n}` (case-insensitive), stays concise (<= 200 chars),
//!   never pings (`allowed_mentions.parse == []`, no `@everyone`/`@here`).
//! - Decay audit rows, role-grant/revoke reconciliation and bot traffic post
//!   nothing. Voice level-ups post nothing (voice is out of scope).
//! - A failed post never blocks or corrupts the award; a drain that crosses
//!   several levels collapses to one message naming the highest level.
//! - No inline-reply shape is required: the contract is a channel post. A
//!   builder that adds `message_reference` still satisfies every assertion.
//!
//! Gateway/runtime fixtures on the leveling drain + `MockRest` executor
//! doubles; no live Discord.

#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::sync::Arc;
use twilight_model::{application::interaction::Interaction, gateway::event::Event};
use two_bot_core::{
    format_iso_millis,
    leveling::{
        level_for_xp, plan_reward_roles, rank_reply, season_id_for_ms, total_xp_for_level,
        LevelRoleReward,
    },
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
const RECEIPT: &str = "900000000000000001";
/// Concise celebration copy stays far under Discord's 2000-char ceiling.
const MAX_ANNOUNCEMENT_CHARS: usize = 200;

fn base_ms() -> i64 {
    1_786_771_200_000
}
fn at(seconds: i64) -> String {
    format_iso_millis(base_ms() + seconds * 1000)
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
fn message_in(seconds: i64, channel: u64) -> Event {
    Event::MessageCreate(Box::new(serde_json::from_value(json!({
        "id":(4_000_000_000_000_000_000_u64 + seconds as u64).to_string(),"guild_id":GUILD.to_string(),"channel_id":channel.to_string(),"author":user(MEMBER),"content":"hello there","timestamp":at(seconds).replace('Z', "+00:00"),"edited_timestamp":null,"tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],"attachments":[],"embeds":[],"pinned":false,"type":0,"components":[]
    })).unwrap()))
}
fn message(seconds: i64) -> Event {
    message_in(seconds, CHANNEL)
}
fn voice(channel: Option<u64>) -> Event {
    Event::VoiceStateUpdate(Box::new(serde_json::from_value(json!({
        "guild_id":GUILD.to_string(),"channel_id":channel.map(|c|c.to_string()),"user_id":MEMBER.to_string(),"member":member(),"session_id":"mock-voice","deaf":false,"mute":false,"self_deaf":false,"self_mute":false,"self_stream":false,"self_video":false,"suppress":false
    })).unwrap()))
}
fn interaction(name: &str) -> Interaction {
    serde_json::from_value(json!({"id":"7","application_id":"8","type":2,"token":"mock-callback","version":1,"guild_id":GUILD.to_string(),"member":member(),"locale":"en-US","data":{"id":"1","name":name,"type":1,"options":[]},"entitlements":[],"authorizing_integration_owners":{"0":GUILD.to_string()}})).unwrap()
}

/// `POST /channels/{c}/messages` requests only: role grants (PUT), member
/// reads (GET) and interaction callbacks never count as announcements.
fn channel_posts(requests: &[common::RestRequest]) -> Vec<&common::RestRequest> {
    requests
        .iter()
        .filter(|r| {
            r.method == "POST"
                && r.path.starts_with("/api/v10/channels/")
                && r.path.ends_with("/messages")
        })
        .collect()
}

/// Pin the announcement copy contract: names `level {n}`, concise, silent.
fn assert_announcement(request: &common::RestRequest, channel: u64, level: u64) {
    assert_eq!(
        request.path,
        format!("/api/v10/channels/{channel}/messages"),
        "the announcement lands in the source channel"
    );
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    let content = body["content"].as_str().expect("text content");
    assert!(
        content.to_lowercase().contains(&format!("level {level}")),
        "copy names the new level: {content:?}"
    );
    assert!(
        content.chars().count() <= MAX_ANNOUNCEMENT_CHARS,
        "copy stays concise: {content:?}"
    );
    assert!(
        !content.contains("@everyone") && !content.contains("@here"),
        "copy never mass-pings: {content:?}"
    );
    assert_eq!(
        body["allowed_mentions"]["parse"],
        json!([]),
        "the post is mention-safe"
    );
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
    }
    async fn seed_rewards(&self) {
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

fn receipt_default() -> ScriptedResponse {
    // One body satisfies every executor read: the POST receipt (`id`), the
    // member role readback (`roles`) and the idempotent PUT (200 or 204).
    ScriptedResponse::json(200, json!({"id": RECEIPT, "roles": []}))
}

#[tokio::test]
async fn message_level_up_posts_exactly_one_mention_safe_announcement_in_source_channel() {
    assert_eq!(
        (total_xp_for_level(1), total_xp_for_level(2)),
        (100, 255),
        "the MEE6 floors this test seeds against"
    );
    let db = TestDb::new().await;
    db.seed(90).await;
    let mock = MockRest::start(vec![], receipt_default()).await;
    let pipeline = OrderedLevelingPipeline::new(
        MemStore::new(),
        Some(runtime(db.pool.clone(), &mock, OnboardingMode::Legacy)),
    );
    let awards = pipeline
        .handle_at(&message(0), &at(0), MessageEligibility::default())
        .await
        .unwrap();
    assert_eq!(awards.len(), 1);
    assert_eq!(awards[0].awarded, 15);
    assert!(awards[0].leveled_up, "90 + 15 crosses the level-1 floor");
    assert_eq!((awards[0].total_xp, awards[0].level), (105, 1));
    let requests = mock.requests();
    let posts = channel_posts(&requests);
    assert_eq!(posts.len(), 1, "exactly one celebration post");
    assert_announcement(posts[0], CHANNEL, 1);
    assert_eq!(
        mock.requests().len(),
        1,
        "no role ladder is seeded, so the announcement is the only wire call"
    );
    let profile = store::profile(&db.pool, &GUILD.to_string(), &MEMBER.to_string())
        .await
        .unwrap();
    assert_eq!((profile.xp, profile.level), (105, 1));
    assert_eq!(level_for_xp(105), 1);
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn role_grant_reconciliation_adds_no_extra_message_and_rank_composes() {
    let db = TestDb::new().await;
    db.seed(90).await;
    db.seed_rewards().await;
    let mock = MockRest::start(vec![], receipt_default()).await;
    let runtime = runtime(db.pool.clone(), &mock, OnboardingMode::Legacy);
    let pipeline = OrderedLevelingPipeline::new(MemStore::new(), Some(runtime.clone()));
    let awards = pipeline
        .handle_at(&message(0), &at(0), MessageEligibility::default())
        .await
        .unwrap();
    assert!(awards[0].leveled_up);
    let requests = mock.requests();
    let puts = requests.iter().filter(|r| r.method == "PUT").count();
    assert_eq!(puts, 1, "the level-1 role grant still applies");
    assert!(
        requests
            .iter()
            .any(|r| r.method == "PUT" && r.path.ends_with(&format!("/roles/{ROLE}"))),
        "the grant targets the ladder role"
    );
    let posts = channel_posts(&requests);
    assert_eq!(
        posts.len(),
        1,
        "role reconciliation adds no channel message of its own"
    );
    assert_announcement(posts[0], CHANNEL, 1);
    // /rank composes: the same award answers ephemerally without a second post.
    assert!(runtime
        .handle_interaction(&interaction("rank"), HandlerId::Rank)
        .await
        .unwrap());
    let requests = mock.requests();
    assert_eq!(
        channel_posts(&requests).len(),
        1,
        "rank adds a callback, never another channel post"
    );
    let callback = requests
        .iter()
        .find(|r| r.path.starts_with("/api/v10/interactions/7/"))
        .expect("the rank callback");
    let body: Value = serde_json::from_slice(&callback.body).unwrap();
    let profile = store::profile(&db.pool, &GUILD.to_string(), &MEMBER.to_string())
        .await
        .unwrap();
    assert_eq!(
        body["data"]["content"],
        json!(rank_reply(&profile, "Global name").content)
    );
    assert_eq!(body["data"]["flags"], 64);
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn multi_level_burst_in_one_drain_collapses_to_single_highest_level_message() {
    let db = TestDb::new().await;
    db.seed(90).await;
    let mock = MockRest::start(vec![], receipt_default()).await;
    let pipeline = OrderedLevelingPipeline::new(
        MemStore::new(),
        Some(runtime(db.pool.clone(), &mock, OnboardingMode::Legacy)),
    );
    // Day one (05:20 UTC base): five full ladder rungs plus five half rungs,
    // each in its own channel so the same-channel burst halving never fires.
    // Day two (past the UTC midnight at +67,200 s): five fresh full rungs.
    // 90 + 75 + 25 + 75 = 265 crosses level 1, then level 2, in one drain.
    let mut stamps: Vec<(i64, u64)> = (0..10).map(|i| (i * 120, CHANNEL + i as u64)).collect();
    stamps.extend((0..5).map(|j| (68_000 + j * 120, CHANNEL + 10 + j as u64)));
    let mut batch = Vec::new();
    for (seconds, channel) in &stamps {
        batch.extend(pipeline.collect_at(
            &message_in(*seconds, *channel),
            &at(*seconds),
            MessageEligibility::default(),
        ));
    }
    assert_eq!(batch.len(), 15, "every message queues one award request");
    let awards = pipeline.drain(batch).await.unwrap();
    let expected: Vec<u64> = [15, 15, 15, 15, 15, 5, 5, 5, 5, 5, 15, 15, 15, 15, 15].into();
    let actual: Vec<u64> = awards.iter().map(|a| a.awarded).collect();
    assert_eq!(actual, expected, "ladder, caps and days all behave");
    assert_eq!(
        awards.iter().filter(|a| a.leveled_up).count(),
        2,
        "the burst crosses two floors"
    );
    let last = awards.last().unwrap();
    assert_eq!((last.total_xp, last.level), (265, 2));
    let requests = mock.requests();
    let posts = channel_posts(&requests);
    assert_eq!(
        posts.len(),
        1,
        "rapid multi-level gains collapse to one message"
    );
    let body: Value = serde_json::from_slice(&posts[0].body).unwrap();
    let content = body["content"].as_str().expect("text content");
    assert!(
        content.to_lowercase().contains("level 2"),
        "the survivor names the highest level: {content:?}"
    );
    assert!(
        content.chars().count() <= MAX_ANNOUNCEMENT_CHARS
            && !content.contains("@everyone")
            && !content.contains("@here"),
        "the collapsed copy stays concise and silent: {content:?}"
    );
    assert_eq!(body["allowed_mentions"]["parse"], json!([]));
    let channels: Vec<u64> = stamps.iter().map(|(_, c)| *c).collect();
    let posted = posts[0]
        .path
        .trim_start_matches("/api/v10/channels/")
        .trim_end_matches("/messages")
        .parse::<u64>()
        .expect("numeric channel in the post path");
    assert!(
        channels.contains(&posted),
        "the survivor posts in one of the burst's source channels"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn announcement_post_failure_keeps_award() {
    let db = TestDb::new().await;
    db.seed(90).await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let pipeline = OrderedLevelingPipeline::new(
        MemStore::new(),
        Some(runtime(db.pool.clone(), &mock, OnboardingMode::Legacy)),
    );
    let awards = pipeline
        .handle_at(&message(0), &at(0), MessageEligibility::default())
        .await
        .expect("a failed post never fails the award");
    assert_eq!(awards.len(), 1);
    assert!(awards[0].leveled_up);
    assert_eq!((awards[0].awarded, awards[0].total_xp), (15, 105));
    let profile = store::profile(&db.pool, &GUILD.to_string(), &MEMBER.to_string())
        .await
        .unwrap();
    assert_eq!(
        (profile.xp, profile.message_xp, profile.level),
        (105, 15, 1),
        "the award persists uncorrupted"
    );
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM xp_awards")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 1, "exactly one audit row, no partial write");
    let requests = mock.requests();
    let posts = channel_posts(&requests);
    assert_eq!(
        posts.len(),
        1,
        "one bounded post attempt, swallowed on failure"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn award_without_crossing_posts_nothing() {
    let db = TestDb::new().await;
    let mock = MockRest::start(vec![], receipt_default()).await;
    let pipeline = OrderedLevelingPipeline::new(
        MemStore::new(),
        Some(runtime(db.pool.clone(), &mock, OnboardingMode::Legacy)),
    );
    let awards = pipeline
        .handle_at(&message(0), &at(0), MessageEligibility::default())
        .await
        .unwrap();
    assert_eq!(awards.len(), 1);
    assert_eq!(awards[0].awarded, 15);
    assert!(!awards[0].leveled_up, "15 XP stays on level 0");
    assert!(
        mock.requests().is_empty(),
        "an award that crosses nothing stays silent"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn decay_revoke_and_bot_traffic_post_nothing() {
    let db = TestDb::new().await;
    let mock = MockRest::start(vec![], receipt_default()).await;
    let pipeline = OrderedLevelingPipeline::new(
        MemStore::new(),
        Some(runtime(db.pool.clone(), &mock, OnboardingMode::Legacy)),
    );
    // One earning award mirrors into the seasonal projection; backdate that
    // row 60 days so the decay tick finds four overdue weeks past the grace.
    pipeline
        .handle_at(&message(0), &at(0), MessageEligibility::default())
        .await
        .unwrap();
    let season = season_id_for_ms(base_ms());
    sqlx::query(
        "UPDATE member_season_xp SET updated_at = $1::text::timestamptz
         WHERE guild_id = $2 AND season_id = $3 AND member_id = $4",
    )
    .bind(format_iso_millis(base_ms() - 60 * 86_400_000))
    .bind(GUILD.to_string())
    .bind(&season)
    .bind(MEMBER.to_string())
    .execute(&db.pool)
    .await
    .unwrap();
    let decayed = store::apply_seasonal_decay(&db.pool, &GUILD.to_string(), &season, &at(0))
        .await
        .unwrap();
    assert_eq!(decayed, 1, "the tick really decayed the member");
    let decay_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM xp_awards WHERE source = 'decay'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(decay_rows, 1, "one decay audit row exists");
    let profile = store::profile(&db.pool, &GUILD.to_string(), &MEMBER.to_string())
        .await
        .unwrap();
    assert_eq!(profile.xp, 15, "decay never touches lifetime XP");
    assert!(mock.requests().is_empty(), "seasonal decay posts nothing");
    // A held ladder role above the member's level plans a revoke; the plan
    // itself posts nothing.
    let plan = plan_reward_roles(
        0,
        &[LevelRoleReward {
            level: 1,
            role_id: ROLE.into(),
        }],
        &[ROLE.into()],
        true,
        true,
    )
    .expect("one revoke stays within the role-write cap");
    assert_eq!(plan.revoke, vec![ROLE.to_owned()], "a revoke is planned");
    assert!(
        mock.requests().is_empty(),
        "reward reconciliation posts nothing"
    );
    // Bot traffic never funnels: no award, no post.
    let mut bot = message(60);
    if let Event::MessageCreate(m) = &mut bot {
        m.author.bot = true;
    }
    assert!(pipeline
        .handle_at(&bot, &at(60), MessageEligibility::default())
        .await
        .unwrap()
        .is_empty());
    assert!(mock.requests().is_empty(), "bot traffic posts nothing");
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn gates_and_voice_level_ups_stay_silent() {
    // Dry runs award but never write to Discord: no roles, no announcements.
    let db = TestDb::new().await;
    db.seed(90).await;
    let mock = MockRest::start(vec![], receipt_default()).await;
    let dry = LevelingRuntime::new(
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
    let pipeline = OrderedLevelingPipeline::new(MemStore::new(), Some(dry));
    let awards = pipeline
        .handle_at(&message(0), &at(0), MessageEligibility::default())
        .await
        .unwrap();
    assert!(awards[0].leveled_up, "dry runs still cross");
    assert!(mock.requests().is_empty(), "dry-run level-ups post nothing");
    mock.shutdown().await;
    db.close().await;

    // Ineligible traffic funnels nothing, so nothing can announce.
    let db = TestDb::new().await;
    db.seed(90).await;
    let mock = MockRest::start(vec![], receipt_default()).await;
    let pipeline = OrderedLevelingPipeline::new(
        MemStore::new(),
        Some(runtime(db.pool.clone(), &mock, OnboardingMode::Legacy)),
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
    assert!(mock.requests().is_empty(), "gated traffic posts nothing");

    // Voice level-ups are out of scope: the award lands, silently.
    pipeline
        .handle_at(&voice(Some(CHANNEL)), &at(0), MessageEligibility::default())
        .await
        .unwrap();
    let awards = pipeline
        .handle_at(&voice(None), &at(600), MessageEligibility::default())
        .await
        .unwrap();
    assert_eq!(awards.len(), 1);
    assert_eq!(awards[0].awarded, 50, "ten voice minutes at 5 XP");
    assert!(awards[0].leveled_up, "90 + 50 crosses the level-1 floor");
    assert!(mock.requests().is_empty(), "voice level-ups post nothing");
    let profile = store::profile(&db.pool, &GUILD.to_string(), &MEMBER.to_string())
        .await
        .unwrap();
    assert_eq!((profile.xp, profile.voice_xp), (140, 50));
    mock.shutdown().await;
    db.close().await;
}
